#![cfg(target_os = "linux")]

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DbFileIdentity {
    fd_targets: BTreeSet<PathBuf>,
    target_names: BTreeSet<OsString>,
    file_ids: BTreeSet<FileId>,
}

impl DbFileIdentity {
    pub(crate) fn from_resolved_path(path: &Path) -> Self {
        let mut identity = Self::default();
        identity.insert_target(path);
        identity
    }

    fn insert_target(&mut self, path: &Path) {
        self.fd_targets.insert(path.to_path_buf());
        self.insert_target_name(path);

        if let Some(file_id) = file_id_for_path(path) {
            self.file_ids.insert(file_id);
        }
    }

    fn insert_target_name(&mut self, path: &Path) {
        if let Some(file_name) = path.file_name() {
            self.target_names.insert(file_name.to_os_string());
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.fd_targets.extend(other.fd_targets);
        self.target_names.extend(other.target_names);
        self.file_ids.extend(other.file_ids);
    }

    pub(crate) fn matches_fd(&self, fd_path: &Path, fd_target: &Path) -> bool {
        if self.fd_targets.contains(fd_target) {
            return true;
        }
        if !fd_target.is_absolute() {
            return false;
        }

        let target_name_matches = self.target_name_matches(fd_target);
        if !target_name_matches {
            return false;
        }

        let fd_file_id = file_id_for_path(fd_path);
        if fd_file_id.is_some_and(|file_id| self.file_ids.contains(&file_id)) {
            return true;
        }

        let canonical_target = if target_is_symlink(fd_target) || fd_file_id.is_none() {
            Some(canonicalize_if_present(fd_target))
        } else {
            None
        };
        if canonical_target
            .as_ref()
            .is_some_and(|target| self.fd_targets.contains(target))
        {
            return true;
        }

        let stripped_target = strip_deleted_suffix(fd_file_id, fd_target);

        self.fd_targets.contains(stripped_target.as_ref())
            || (matches!(stripped_target, Cow::Owned(_))
                && self
                    .fd_targets
                    .contains(&canonicalize_if_present(&stripped_target)))
    }

    fn target_name_matches(&self, path: &Path) -> bool {
        let Some(file_name) = path.file_name() else {
            return false;
        };
        self.target_names.contains(file_name)
            || stripped_deleted_name(file_name)
                .is_some_and(|stripped| self.target_names.contains(&stripped))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DbPathIdentity {
    db_path: PathBuf,
    files: DbFileIdentity,
}

impl DbPathIdentity {
    pub(crate) fn from_existing_db_path(db_path: &Path) -> Option<Self> {
        let absolute_db_path = resolve_configured_path_lossy(db_path);
        let canonical_db_path = canonicalize_db_path_or_parent(absolute_db_path)?;
        Some(Self::from_resolved_db_path(canonical_db_path))
    }

    pub(crate) fn from_resolved_db_path(db_path: PathBuf) -> Self {
        let mut files = DbFileIdentity::from_resolved_path(&db_path);
        for suffix in ["-wal", "-shm"] {
            let sidecar = append_os_suffix(&db_path, suffix);
            files.merge(DbFileIdentity::from_resolved_path(&sidecar));
        }
        Self { db_path, files }
    }

    pub(crate) fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub(crate) fn matches_fd(&self, fd_path: &Path, fd_target: &Path) -> bool {
        self.files.matches_fd(fd_path, fd_target)
    }
}

pub(crate) fn db_file_targets(db_path: &Path) -> Vec<(&'static str, DbFileIdentity)> {
    db_file_targets_from_resolved_db_path(resolve_configured_path_lossy(db_path))
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn db_file_targets_with_cwd(
    db_path: &Path,
    cwd: &Path,
) -> Vec<(&'static str, DbFileIdentity)> {
    db_file_targets_from_resolved_db_path(resolve_path_with_cwd_lossy(db_path, cwd))
}

fn db_file_targets_from_resolved_db_path(db_path: PathBuf) -> Vec<(&'static str, DbFileIdentity)> {
    let resolved_db_path = canonicalize_if_present(&db_path);
    let shm_path = append_os_suffix(&resolved_db_path, "-shm");
    let wal_path = append_os_suffix(&resolved_db_path, "-wal");
    vec![
        ("db", DbFileIdentity::from_resolved_path(&resolved_db_path)),
        ("shm", DbFileIdentity::from_resolved_path(&shm_path)),
        ("wal", DbFileIdentity::from_resolved_path(&wal_path)),
    ]
}

pub(crate) fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(OsStr::new(suffix));
    PathBuf::from(os)
}

fn resolve_configured_path_lossy(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_path_with_cwd_lossy(path, &cwd)
}

fn resolve_path_with_cwd_lossy(path: &Path, cwd: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    canonicalize_if_present(&absolute)
}

fn canonicalize_db_path_or_parent(path: PathBuf) -> Option<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(&path) {
        return Some(canonical);
    }

    let file_name = path.file_name()?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::canonicalize(parent)
        .ok()
        .map(|canonical_parent| canonical_parent.join(file_name))
}

fn canonicalize_if_present(path: &Path) -> PathBuf {
    canonicalize_db_path_or_parent(path.to_path_buf()).unwrap_or_else(|| path.to_path_buf())
}

fn target_is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn file_id_for_path(path: &Path) -> Option<FileId> {
    fs::metadata(path)
        .ok()
        .map(|metadata| FileId::from_metadata(&metadata))
}

const DELETED_SUFFIX: &[u8] = b" (deleted)";

fn stripped_deleted_name(file_name: &OsStr) -> Option<OsString> {
    let bytes = file_name.as_bytes();
    bytes.ends_with(DELETED_SUFFIX).then(|| {
        OsString::from_vec(bytes[..bytes.len().saturating_sub(DELETED_SUFFIX.len())].to_vec())
    })
}

fn strip_deleted_suffix(fd_file_id: Option<FileId>, path: &Path) -> Cow<'_, Path> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(DELETED_SUFFIX) {
        if let Some(target_file_id) = file_id_for_path(path) {
            if fd_file_id == Some(target_file_id) {
                return Cow::Borrowed(path);
            }
        }
        let keep = bytes.len().saturating_sub(DELETED_SUFFIX.len());
        return Cow::Owned(PathBuf::from(OsString::from_vec(bytes[..keep].to_vec())));
    }
    Cow::Borrowed(path)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::symlink;

    #[test]
    fn test_matches_fd_direct_target_does_not_require_fd_metadata() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        File::create(&db_path).expect("db file");
        let identity = DbFileIdentity::from_resolved_path(&db_path);

        assert!(identity.matches_fd(&tmp.path().join("missing-fd"), &db_path));
    }

    #[test]
    fn test_matches_fd_rejects_non_absolute_fd_targets_after_direct_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        File::create(&db_path).expect("db file");
        let identity = DbFileIdentity::from_resolved_path(&db_path);

        assert!(!identity.matches_fd(&tmp.path().join("fd-3"), Path::new("socket:[12345]")));
    }

    #[test]
    fn test_matches_fd_rejects_unrelated_absolute_target_before_metadata_fallback() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let unrelated_path = tmp.path().join("notes.txt");
        File::create(&db_path).expect("db file");
        File::create(&unrelated_path).expect("unrelated file");
        let identity = DbFileIdentity::from_resolved_path(&db_path);

        assert!(!identity.matches_fd(&tmp.path().join("missing-fd"), &unrelated_path));
    }

    #[test]
    fn test_matches_fd_canonical_target_keeps_matching_name_symlink_path_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_dir = tmp.path().join("real");
        let linked_dir = tmp.path().join("linked");
        fs::create_dir(&real_dir).expect("real dir");
        fs::create_dir(&linked_dir).expect("linked dir");
        let real_db_path = real_dir.join("palace.db");
        let linked_db_path = linked_dir.join("palace.db");
        File::create(&real_db_path).expect("db file");
        symlink(&real_db_path, &linked_db_path).expect("db symlink");
        let identity = DbFileIdentity::from_resolved_path(&real_db_path);

        assert!(identity.matches_fd(&tmp.path().join("missing-fd"), &linked_db_path));
    }

    #[test]
    fn test_matches_fd_does_not_strip_literal_deleted_suffix_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let literal_deleted_path = tmp.path().join("palace.db (deleted)");
        File::create(&db_path).expect("db file");
        File::create(&literal_deleted_path).expect("literal deleted suffix file");
        let identity = DbFileIdentity::from_resolved_path(&db_path);

        assert!(
            !identity.matches_fd(&literal_deleted_path, &literal_deleted_path),
            "a real file whose name ends with the kernel suffix text is not the DB"
        );
    }

    #[test]
    fn test_matches_fd_strips_kernel_deleted_suffix_when_target_path_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let fd_path = tmp.path().join("fd-3");
        File::create(&db_path).expect("db file");
        let identity = DbFileIdentity::from_resolved_path(&db_path);
        let deleted_target = append_os_suffix(&db_path, " (deleted)");

        assert!(identity.matches_fd(&fd_path, &deleted_target));
    }

    #[test]
    fn test_from_existing_db_path_uses_canonical_parent_when_db_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let identity = DbPathIdentity::from_existing_db_path(&db_path).expect("db identity");

        assert_eq!(
            identity.db_path(),
            std::fs::canonicalize(tmp.path())
                .expect("canonical tempdir")
                .join("palace.db")
        );
    }

    #[test]
    fn test_canonicalize_if_present_uses_canonical_parent_when_file_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_dir = tmp.path().join("real");
        let linked_dir = tmp.path().join("linked");
        fs::create_dir(&real_dir).expect("real dir");
        symlink(&real_dir, &linked_dir).expect("dir symlink");

        let db_path = linked_dir.join("palace.db");

        assert_eq!(
            canonicalize_if_present(&db_path),
            std::fs::canonicalize(&real_dir)
                .expect("canonical real dir")
                .join("palace.db")
        );
    }
}

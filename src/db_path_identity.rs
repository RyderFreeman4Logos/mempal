#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::ffi::{OsStr, OsString};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileId {
    dev: u64,
    ino: u64,
}

#[cfg(target_os = "linux")]
impl FileId {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default)]
pub(crate) struct DbFileIdentity {
    fd_targets: BTreeSet<PathBuf>,
    file_ids: BTreeSet<FileId>,
}

#[cfg(target_os = "linux")]
impl DbFileIdentity {
    pub(crate) fn from_resolved_path(path: &Path) -> Self {
        let mut identity = Self::default();
        identity.insert_target(path);
        identity
    }

    fn insert_target(&mut self, path: &Path) {
        let canonical = canonicalize_if_present(path);
        self.fd_targets.insert(path.to_path_buf());
        self.fd_targets.insert(canonical.clone());

        if let Some(file_id) = file_id_for_path(path).or_else(|| file_id_for_path(&canonical)) {
            self.file_ids.insert(file_id);
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.fd_targets.extend(other.fd_targets);
        self.file_ids.extend(other.file_ids);
    }

    pub(crate) fn matches_fd(&self, fd_path: &Path, fd_target: &Path) -> bool {
        if file_id_for_path(fd_path).is_some_and(|file_id| self.file_ids.contains(&file_id)) {
            return true;
        }

        let stripped_target = strip_deleted_suffix(fd_path, fd_target);
        let canonical_target = canonicalize_if_present(&stripped_target);

        self.fd_targets.contains(&canonical_target) || self.fd_targets.contains(&stripped_target)
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub(crate) struct DbPathIdentity {
    db_path: PathBuf,
    files: DbFileIdentity,
}

#[cfg(target_os = "linux")]
impl DbPathIdentity {
    pub(crate) fn from_existing_db_path(db_path: &Path) -> Option<Self> {
        let absolute_db_path = resolve_configured_path_lossy(db_path);
        let canonical_db_path = fs::canonicalize(absolute_db_path).ok()?;
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
pub(crate) fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(OsStr::new(suffix));
    PathBuf::from(os)
}

#[cfg(target_os = "linux")]
fn resolve_configured_path_lossy(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_path_with_cwd_lossy(path, &cwd)
}

#[cfg(target_os = "linux")]
fn resolve_path_with_cwd_lossy(path: &Path, cwd: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    canonicalize_if_present(&absolute)
}

#[cfg(target_os = "linux")]
fn canonicalize_if_present(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(target_os = "linux")]
fn file_id_for_path(path: &Path) -> Option<FileId> {
    fs::metadata(path)
        .ok()
        .map(|metadata| FileId::from_metadata(&metadata))
}

#[cfg(target_os = "linux")]
fn strip_deleted_suffix(fd_path: &Path, path: &Path) -> PathBuf {
    const DELETED_SUFFIX: &[u8] = b" (deleted)";
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(DELETED_SUFFIX) {
        let target_file_id = file_id_for_path(path);
        let fd_file_id = file_id_for_path(fd_path);
        if target_file_id.is_some() && target_file_id == fd_file_id {
            return path.to_path_buf();
        }
        let keep = bytes.len().saturating_sub(DELETED_SUFFIX.len());
        return PathBuf::from(OsString::from_vec(bytes[..keep].to_vec()));
    }
    path.to_path_buf()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs::File;

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
}

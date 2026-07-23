//! Sidecar path identity for profile DB admission.

use std::fs;
use std::path::{Path, PathBuf};

use super::db_admission::DbAdmissionError;

#[derive(Debug)]
pub(super) struct AdmissionPaths {
    pub(super) database_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) lock_path: PathBuf,
}

impl AdmissionPaths {
    pub(super) fn new(db_path: &Path) -> Result<Self, DbAdmissionError> {
        // Resolve the parent only after creating it: returning a lexical parent
        // here would let later opens re-traverse a mutable symlink.
        let raw_parent = db_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(raw_parent).map_err(|source| DbAdmissionError::Io {
            path: raw_parent.to_path_buf(),
            source,
        })?;
        let parent = fs::canonicalize(raw_parent).map_err(|source| DbAdmissionError::Io {
            path: raw_parent.to_path_buf(),
            source,
        })?;
        validate_sidecar_parent(&parent)?;

        let lexical_name = db_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or(DbAdmissionError::InvalidRequest(
                "database path must have a UTF-8 file name",
            ))?;

        let full_path = parent.join(lexical_name);
        let (database_path, sidecar_dir, sidecar_name) = match fs::canonicalize(&full_path) {
            Ok(canonical) => {
                // Reject hard-linked regular databases: each link would get
                // an independent admission budget, and SQLite WAL/shm files
                // are not designed for multi-link access. Non-regular paths
                // must reach SQLite so its existing diagnostics classify the
                // underlying path/open failure.
                #[cfg(unix)]
                if let Ok(metadata) = fs::metadata(&canonical) {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.is_file() && metadata.nlink() > 1 {
                        return Err(DbAdmissionError::InvalidRequest(
                            "database file has multiple hard links; admission identity cannot be established safely",
                        ));
                    }
                }
                let dir = canonical.parent().map(Path::to_path_buf).unwrap_or(parent);
                let name = canonical
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or(lexical_name)
                    .to_string();
                (canonical, dir, name)
            }
            Err(_) => (full_path, parent, lexical_name.to_string()),
        };
        validate_sidecar_parent(&sidecar_dir)?;

        Ok(Self {
            database_path,
            state_path: sidecar_dir.join(format!(".{sidecar_name}.admission.json")),
            lock_path: sidecar_dir.join(format!(".{sidecar_name}.admission.lock")),
        })
    }

    pub(super) fn holder_lease_path(&self, token: &str) -> PathBuf {
        let state_name = self.state_name();
        // Never place serialized token text into a path: the state file is a
        // trust boundary, while a fixed hash keeps the lease beside its state.
        let lease_id = blake3::hash(token.as_bytes()).to_hex();
        self.state_path
            .with_file_name(format!("{state_name}.{}.lease", &lease_id[..24]))
    }

    pub(super) fn state_temp_path(&self, token: &str) -> PathBuf {
        self.state_path
            .with_file_name(format!("{}.tmp.{token}", self.state_name()))
    }

    pub(super) fn state_parent(&self) -> &Path {
        self.state_path.parent().unwrap_or_else(|| Path::new("."))
    }

    pub(super) fn is_current_lease_path(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(lease_id) = name
            .strip_prefix(&self.state_name())
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".lease"))
        else {
            return false;
        };
        is_lower_hex(lease_id, 24)
    }

    pub(super) fn is_current_state_temp_path(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(token) = name
            .strip_prefix(&self.state_name())
            .and_then(|rest| rest.strip_prefix(".tmp."))
        else {
            return false;
        };
        is_lower_hex(token, 32)
    }

    fn state_name(&self) -> String {
        self.state_path
            .file_name()
            .expect("validated admission state path has a file name")
            .to_string_lossy()
            .into_owned()
    }
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(unix)]
fn validate_sidecar_parent(parent: &Path) -> Result<(), DbAdmissionError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(parent).map_err(|source| DbAdmissionError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    // SAFETY: `geteuid` has no pointer arguments and reads process credentials.
    let expected_uid = unsafe { libc::geteuid() };
    let reason = if !metadata.is_dir() {
        Some("sidecar parent is not a directory")
    } else if metadata.uid() != expected_uid {
        Some("sidecar parent is not owned by the current user")
    } else if metadata.mode() & 0o022 != 0 {
        Some("sidecar parent is writable by group or other users")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(DbAdmissionError::UnsafeSidecarDirectory {
            path: parent.to_path_buf(),
            reason,
        }),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn validate_sidecar_parent(parent: &Path) -> Result<(), DbAdmissionError> {
    if fs::metadata(parent)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err(DbAdmissionError::UnsafeSidecarDirectory {
            path: parent.to_path_buf(),
            reason: "sidecar parent is not a directory",
        })
    }
}

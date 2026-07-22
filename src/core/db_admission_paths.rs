//! Sidecar path identity for profile DB admission.

use std::fs;
use std::path::{Path, PathBuf};

use super::db_admission::DbAdmissionError;

pub(super) struct AdmissionPaths {
    pub(super) database_path: PathBuf,
    pub(super) state_path: PathBuf,
    pub(super) lock_path: PathBuf,
}

impl AdmissionPaths {
    pub(super) fn new(db_path: &Path) -> Result<Self, DbAdmissionError> {
        // Canonicalize the full database path to prevent symlink aliases
        // from bypassing the profile-wide admission budget.
        let raw_parent = db_path.parent().unwrap_or_else(|| Path::new("."));
        let parent = fs::canonicalize(raw_parent).unwrap_or_else(|_| raw_parent.to_path_buf());
        fs::create_dir_all(&parent).map_err(|source| DbAdmissionError::Io {
            path: parent.clone(),
            source,
        })?;

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
                    let nlink = metadata.nlink();
                    if metadata.is_file() && nlink > 1 {
                        return Err(DbAdmissionError::InvalidRequest(
                            "database file has multiple hard links; admission identity cannot be established safely",
                        ));
                    }
                }
                let dir = canonical
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(parent);
                let name = canonical
                    .file_name()
                    .and_then(|n| n.to_str())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(lexical_name)
                    .to_string();
                (canonical, dir, name)
            }
            Err(_) => (full_path, parent, lexical_name.to_string()),
        };

        Ok(Self {
            database_path,
            state_path: sidecar_dir.join(format!(".{sidecar_name}.admission.json")),
            lock_path: sidecar_dir.join(format!(".{sidecar_name}.admission.lock")),
        })
    }

    pub(super) fn holder_lease_path(&self, token: &str) -> PathBuf {
        let state_name = self
            .state_path
            .file_name()
            .expect("validated admission state path has a file name")
            .to_string_lossy();
        // Never place serialized token text into a path: the state file is a
        // trust boundary, while a fixed hash keeps the lease beside its state.
        let lease_id = blake3::hash(token.as_bytes()).to_hex();
        self.state_path
            .with_file_name(format!("{state_name}.{}.lease", &lease_id[..24]))
    }

    pub(super) fn state_parent(&self) -> &Path {
        self.state_path.parent().unwrap_or_else(|| Path::new("."))
    }

    pub(super) fn is_current_lease_path(&self, path: &Path) -> bool {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let state_name = self
            .state_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let Some(lease_id) = name
            .strip_prefix(state_name)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".lease"))
        else {
            return false;
        };
        lease_id.len() == 24 && lease_id.bytes().all(|byte| byte.is_ascii_hexdigit())
    }
}

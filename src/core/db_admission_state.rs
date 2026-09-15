//! Safe, bounded persistence for profile database admission state.
//!
//! Admission sidecars are a trust boundary. This module keeps every pathname
//! operation fail-closed: the parent is validated by [`AdmissionPaths`], files
//! are opened without following symlinks, and the opened inode is checked
//! against the pathname before it is used or reclaimed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::db_admission::{AdmissionState, DbAdmissionError, imp};
use super::db_admission_paths::AdmissionPaths;

pub(super) const ADMISSION_STATE_SCHEMA_VERSION: u32 = 1;
pub(super) const MAX_ADMISSION_STATE_BYTES: usize = 64 * 1024;
const MAX_STAGED_STATE_CREATE_ATTEMPTS: u8 = 8;
static STAGED_STATE_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(super) struct StateLock {
    file: File,
    #[cfg(target_os = "linux")]
    _fork_guard: std::sync::RwLockReadGuard<'static, ()>,
}

impl Deref for StateLock {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

pub(super) fn lock_state(path: &Path) -> Result<StateLock, DbAdmissionError> {
    let started = std::time::Instant::now();
    let deadline = started
        .checked_add(super::db_admission::ADMISSION_LOCK_TIMEOUT)
        .unwrap_or(started);
    lock_state_until(path, deadline, std::time::Instant::now)
}

fn lock_state_until(
    path: &Path,
    deadline: std::time::Instant,
    mut now: impl FnMut() -> std::time::Instant,
) -> Result<StateLock, DbAdmissionError> {
    #[cfg(target_os = "linux")]
    let fork_guard = super::db_admission::admission_state_fork_guard(path, deadline)?;
    let file = open_sidecar(path, true, true)?;
    verify_sidecar_inode(path, &file)?;
    let busy = || DbAdmissionError::Busy {
        path: path.to_path_buf(),
        timeout_ms: super::db_admission::ADMISSION_LOCK_TIMEOUT.as_millis() as u64,
    };
    let mut waited = false;
    loop {
        if waited && now() >= deadline {
            return Err(busy());
        }
        match imp::try_lock_exclusive(&file) {
            Ok(true) if waited && now() >= deadline => return Err(busy()),
            Ok(true) => {
                return Ok(StateLock {
                    file,
                    #[cfg(target_os = "linux")]
                    _fork_guard: fork_guard,
                });
            }
            Ok(false) if now() < deadline => {
                waited = true;
                std::thread::sleep(
                    super::db_admission::ADMISSION_LOCK_RETRY
                        .min(deadline.saturating_duration_since(now())),
                );
            }
            Ok(false) => return Err(busy()),
            Err(source) => {
                return Err(DbAdmissionError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

pub(super) fn load_state(path: &Path) -> Result<AdmissionState, DbAdmissionError> {
    let mut file = match open_sidecar(path, false, false) {
        Ok(file) => file,
        Err(DbAdmissionError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(AdmissionState::default());
        }
        Err(error) => return Err(error),
    };
    verify_sidecar_inode(path, &file)?;
    let declared_len = file
        .metadata()
        .map_err(|source| DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if declared_len > MAX_ADMISSION_STATE_BYTES as u64 {
        return Err(DbAdmissionError::StateTooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_ADMISSION_STATE_BYTES,
            actual_bytes: declared_len,
        });
    }

    let mut bytes = Vec::with_capacity(declared_len as usize);
    (&mut file)
        .take((MAX_ADMISSION_STATE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_ADMISSION_STATE_BYTES {
        return Err(DbAdmissionError::StateTooLarge {
            path: path.to_path_buf(),
            max_bytes: MAX_ADMISSION_STATE_BYTES,
            actual_bytes: bytes.len() as u64,
        });
    }
    if bytes.is_empty() {
        return Ok(AdmissionState::default());
    }

    let state: AdmissionState =
        serde_json::from_slice(&bytes).map_err(|source| DbAdmissionError::InvalidState {
            path: path.to_path_buf(),
            source,
        })?;
    if state.schema_version != ADMISSION_STATE_SCHEMA_VERSION {
        return Err(DbAdmissionError::UnsupportedStateVersion {
            path: path.to_path_buf(),
            version: state.schema_version,
        });
    }
    Ok(state)
}

pub(super) fn save_state(
    paths: &AdmissionPaths,
    state: &AdmissionState,
) -> Result<(), DbAdmissionError> {
    let encoded = serde_json::to_vec(state).map_err(|source| DbAdmissionError::InvalidState {
        path: paths.state_path.clone(),
        source,
    })?;
    if encoded.len() > MAX_ADMISSION_STATE_BYTES {
        return Err(DbAdmissionError::StateTooLarge {
            path: paths.state_path.clone(),
            max_bytes: MAX_ADMISSION_STATE_BYTES,
            actual_bytes: encoded.len() as u64,
        });
    }

    let (staged_path, mut staged) = create_staged_state_file(paths)?;
    let write_result = (|| {
        staged
            .write_all(&encoded)
            .map_err(|source| DbAdmissionError::Io {
                path: staged_path.clone(),
                source,
            })?;
        staged.sync_all().map_err(|source| DbAdmissionError::Io {
            path: staged_path.clone(),
            source,
        })?;
        #[cfg(test)]
        super::db_admission_fault_injection::exit_if(
            super::db_admission_fault_injection::CrashPoint::StateTempSyncedBeforeRename,
        );
        fs::rename(&staged_path, &paths.state_path).map_err(|source| DbAdmissionError::Io {
            path: paths.state_path.clone(),
            source,
        })?;
        sync_parent_directory(paths.state_parent()).map_err(|source| DbAdmissionError::Io {
            path: paths.state_parent().to_path_buf(),
            source,
        })?;
        let committed = open_sidecar(&paths.state_path, false, false)?;
        verify_sidecar_inode(&paths.state_path, &committed)?;
        Ok(())
    })();
    if write_result.is_err() {
        drop(staged);
        let _ = remove_staged_state_file(&staged_path);
    }
    write_result
}

/// Reclaim only exact grammar-matched, verifiable state write temps.
pub(super) fn sweep_staged_state_files(paths: &AdmissionPaths) -> Result<usize, DbAdmissionError> {
    let entries = fs::read_dir(paths.state_parent()).map_err(|source| DbAdmissionError::Io {
        path: paths.state_parent().to_path_buf(),
        source,
    })?;
    let mut reclaimed = 0usize;
    for entry in entries {
        let entry = entry.map_err(|source| DbAdmissionError::Io {
            path: paths.state_parent().to_path_buf(),
            source,
        })?;
        let candidate = entry.path();
        if !paths.is_current_state_temp_path(&candidate) {
            continue;
        }
        match remove_staged_state_file(&candidate) {
            Ok(true) => reclaimed = reclaimed.saturating_add(1),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "retaining unverifiable admission state temp"),
        }
    }
    Ok(reclaimed)
}

pub(super) fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

fn create_staged_state_file(paths: &AdmissionPaths) -> Result<(PathBuf, File), DbAdmissionError> {
    for _ in 0..MAX_STAGED_STATE_CREATE_ATTEMPTS {
        let path = paths.state_temp_path(&staged_state_token());
        match open_staged_state_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(DbAdmissionError::Io { path, source });
            }
        }
    }
    Err(DbAdmissionError::Io {
        path: paths.state_parent().to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique admission state temp file",
        ),
    })
}

fn staged_state_token() -> String {
    let sequence = STAGED_STATE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&sequence.to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.finalize().to_hex()[..32].to_string()
}

fn remove_staged_state_file(path: &Path) -> io::Result<bool> {
    let file = match open_sidecar_file(path, true, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    verify_current_sidecar_inode(path, &file)?;
    match fs::remove_file(path) {
        Ok(()) => {
            sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn open_sidecar(path: &Path, write: bool, create: bool) -> Result<File, DbAdmissionError> {
    open_sidecar_file(path, write, create).map_err(|source| {
        if source.raw_os_error() == Some(libc::ELOOP) {
            DbAdmissionError::UnsafeSidecar {
                path: path.to_path_buf(),
                reason: "symlink traversal is prohibited",
            }
        } else {
            DbAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

#[cfg(unix)]
fn open_sidecar_file(path: &Path, write: bool, create: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_sidecar_file(path: &Path, write: bool, create: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .create(create)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn open_staged_state_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_staged_state_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

fn verify_sidecar_inode(path: &Path, file: &File) -> Result<(), DbAdmissionError> {
    verify_current_sidecar_inode(path, file).map_err(|source| DbAdmissionError::UnsafeSidecar {
        path: path.to_path_buf(),
        reason: if source.kind() == io::ErrorKind::NotFound {
            "pathname changed while opening sidecar"
        } else {
            "sidecar pathname and opened inode do not match"
        },
    })
}

#[cfg(unix)]
fn verify_current_sidecar_inode(path: &Path, file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    // SAFETY: `geteuid` has no pointer arguments and reads process credentials.
    let expected_uid = unsafe { libc::geteuid() };
    if !opened.is_file()
        || !current.is_file()
        || current.file_type().is_symlink()
        || opened.uid() != expected_uid
        || opened.mode() & 0o022 != 0
        || opened.nlink() != 1
        || current.uid() != expected_uid
        || current.mode() & 0o022 != 0
        || current.nlink() != 1
        || opened.dev() != current.dev()
        || opened.ino() != current.ino()
    {
        return Err(io::Error::other("unsafe sidecar inode"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_current_sidecar_inode(path: &Path, file: &File) -> io::Result<()> {
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !opened.is_file() || !current.is_file() || current.file_type().is_symlink() {
        return Err(io::Error::other("unsafe sidecar inode"));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn file_lock_does_not_admit_after_contended_deadline() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("admission.lock");
        let holder = open_sidecar(&path, true, true).expect("open lock holder");
        assert!(
            imp::try_lock_exclusive(&holder).expect("lock holder"),
            "new lock file must be lockable"
        );
        let started = std::time::Instant::now();
        let deadline = started + super::super::db_admission::ADMISSION_LOCK_TIMEOUT;
        let mut holder = Some(holder);
        let mut clock_reads = 0;

        let result = lock_state_until(&path, deadline, || {
            clock_reads += 1;
            if clock_reads == 1 {
                started
            } else {
                drop(holder.take());
                deadline
            }
        });

        let error = match result {
            Ok(_) => panic!("late file-lock release must not admit after the deadline"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            DbAdmissionError::Busy {
                path: busy_path,
                timeout_ms: 250,
            } if busy_path == path
        ));
    }

    #[test]
    fn file_lock_keeps_nonblocking_first_attempt_at_expired_deadline() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("admission.lock");
        let deadline = std::time::Instant::now();

        let lock = lock_state_until(&path, deadline, || {
            panic!("an uncontended first attempt must not consult the expired deadline")
        })
        .expect("uncontended first attempt remains nonblocking");

        drop(lock);
    }
}

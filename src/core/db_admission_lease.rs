//! Crash-recoverable holder-lease sidecars for profile DB admission.
//!
//! The admission state lock serializes this module with state publication. A
//! lease that is not referenced by the current state is removable only after
//! the inode has passed the checks below; anything uncertain is retained.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

use super::db_admission::{
    AdmissionPaths, DbAdmissionError, DbAdmissionHolder, UnknownHolderReason, imp,
};
use super::db_admission_state::sync_parent_directory;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_UNLINK: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) struct UnlinkFaultGuard {
    previous: Option<std::path::PathBuf>,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(test)]
pub(super) fn fail_next_holder_lease_unlink(path: &Path) -> UnlinkFaultGuard {
    let previous = FAIL_NEXT_UNLINK.with(|slot| slot.replace(Some(path.to_path_buf())));
    assert!(
        previous.is_none(),
        "holder-lease unlink fault already armed"
    );
    UnlinkFaultGuard {
        previous,
        _not_send: std::marker::PhantomData,
    }
}

#[cfg(test)]
fn take_unlink_failure(path: &Path) -> bool {
    FAIL_NEXT_UNLINK.with(|slot| {
        let mut armed = slot.borrow_mut();
        if armed.as_deref() == Some(path) {
            armed.take();
            true
        } else {
            false
        }
    })
}

#[cfg(test)]
impl Drop for UnlinkFaultGuard {
    fn drop(&mut self) {
        FAIL_NEXT_UNLINK.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HolderLiveness {
    Live,
    Dead,
    Unknown(UnknownHolderReason),
}

pub(super) fn create_holder_lease(path: &Path) -> Result<File, DbAdmissionError> {
    let file = open_new_lease(path).map_err(|source| DbAdmissionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    match imp::try_lock_exclusive(&file) {
        Ok(true) => {
            file.sync_all().map_err(|source| DbAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            sync_parent_directory(path.parent().unwrap_or_else(|| Path::new("."))).map_err(
                |source| DbAdmissionError::Io {
                    path: path
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .to_path_buf(),
                    source,
                },
            )?;
            Ok(file)
        }
        Ok(false) => {
            if let Err(error) = remove_holder_lease(path) {
                tracing::warn!(%error, "failed to clean up unexpectedly locked holder lease");
            }
            Err(DbAdmissionError::Io {
                path: path.to_path_buf(),
                source: io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "new database holder lease is unexpectedly locked",
                ),
            })
        }
        Err(source) => {
            if let Err(error) = remove_holder_lease(path) {
                tracing::warn!(%error, "failed to clean up holder lease after lock error");
            }
            Err(DbAdmissionError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    }
}

pub(super) fn remove_holder_lease(path: &Path) -> Result<(), DbAdmissionError> {
    #[cfg(test)]
    if take_unlink_failure(path) {
        return Err(DbAdmissionError::Io {
            path: path.to_path_buf(),
            source: io::Error::other("injected one-time holder lease unlink failure"),
        });
    }
    match fs::remove_file(path) {
        Ok(()) => sync_parent_directory(path.parent().unwrap_or_else(|| Path::new("."))).map_err(
            |source| DbAdmissionError::Io {
                path: path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
                source,
            },
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn holder_lease_liveness(path: &Path) -> HolderLiveness {
    let file = match open_current_lease(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return HolderLiveness::Dead,
        Err(_) => return HolderLiveness::Unknown(UnknownHolderReason::LeaseOpenUnavailable),
    };
    match imp::try_lock_exclusive(&file) {
        Ok(true) => HolderLiveness::Dead,
        Ok(false) => HolderLiveness::Live,
        Err(_) => HolderLiveness::Unknown(UnknownHolderReason::LeaseLockUnavailable),
    }
}

/// Reclaim lease files left by a crash after state publication or removal.
///
/// The caller holds the stable admission state lock. Every current state row
/// protects its deterministic filename, including holders whose version is
/// unknown to this binary. Candidate names must match this database's exact
/// v1 lease grammar, then pass no-follow, metadata, flock, and inode checks.
pub(super) fn sweep_unreferenced_holder_leases(
    paths: &AdmissionPaths,
    holders: &[DbAdmissionHolder],
) -> Result<usize, DbAdmissionError> {
    let protected = holders
        .iter()
        .map(|holder| paths.holder_lease_path(&holder.token))
        .collect::<HashSet<_>>();
    let parent = paths.state_parent();
    let entries = fs::read_dir(parent).map_err(|source| DbAdmissionError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut reclaimed = 0usize;
    for entry in entries {
        let entry = entry.map_err(|source| DbAdmissionError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let candidate = entry.path();
        if !paths.is_current_lease_path(&candidate) || protected.contains(&candidate) {
            continue;
        }
        match remove_unreferenced_lease(&candidate) {
            Ok(true) => reclaimed = reclaimed.saturating_add(1),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "retaining unverifiable database holder lease sidecar")
            }
        }
    }
    Ok(reclaimed)
}

#[cfg(unix)]
fn open_new_lease(path: &Path) -> io::Result<File> {
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
fn open_new_lease(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn open_current_lease(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_current_lease(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

#[cfg(unix)]
fn remove_unreferenced_lease(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let file = match open_current_lease(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: `geteuid` has no pointer arguments and reads only the calling
    // process credential maintained by the kernel.
    let expected_uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != expected_uid || metadata.nlink() != 1 {
        return Ok(false);
    }
    if !imp::try_lock_exclusive(&file)? {
        return Ok(false);
    }
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => {
            sync_parent_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn remove_unreferenced_lease(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

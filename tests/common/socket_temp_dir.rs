use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

use tempfile::TempDir;

pub struct SocketTempDir {
    #[cfg(target_os = "linux")]
    _directory: File,
    storage: TempDir,
    path: PathBuf,
}

impl SocketTempDir {
    pub fn new() -> io::Result<Self> {
        let storage = TempDir::new()?;
        #[cfg(target_os = "linux")]
        let directory = File::open(storage.path())?;
        #[cfg(target_os = "linux")]
        // ponytail: Linux procfs alias; add a platform helper when non-Linux socket gates exist.
        let path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            directory.as_raw_fd()
        ));
        #[cfg(not(target_os = "linux"))]
        // ponytail: non-Linux keeps the configured path; add a native alias if deep roots matter.
        let path = storage.path().to_owned();
        Ok(Self {
            #[cfg(target_os = "linux")]
            _directory: directory,
            storage,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn storage_path(&self) -> &Path {
        self.storage.path()
    }
}

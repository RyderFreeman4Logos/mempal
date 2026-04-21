use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::core::config::Config;

pub mod generator;
pub mod manager;

#[derive(Debug, Clone, Subcommand)]
pub enum HotpatchCommands {
    Review {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value_t = false)]
        include_applied: bool,
        #[arg(long, default_value_t = false)]
        include_dismissed: bool,
    },
    Apply {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long, default_value_t = false)]
        confirm: bool,
    },
    Dismiss {
        #[arg(long)]
        dir: PathBuf,
    },
    Clean {
        #[arg(long, default_value = "30d")]
        older_than: String,
    },
}

pub fn run_command(config: &Config, mempal_home: &Path, command: HotpatchCommands) -> Result<()> {
    match command {
        HotpatchCommands::Review {
            dir,
            include_applied,
            include_dismissed,
        } => {
            let report = manager::review(
                config,
                mempal_home,
                manager::ReviewOptions {
                    dir,
                    include_applied,
                    include_dismissed,
                },
            )?;
            print!("{}", report.stdout);
        }
        HotpatchCommands::Apply { dir, confirm } => {
            let report =
                manager::apply(config, mempal_home, manager::ApplyOptions { dir, confirm })?;
            print!("{}", report.stdout);
        }
        HotpatchCommands::Dismiss { dir } => {
            let report = manager::dismiss(config, mempal_home, manager::DismissOptions { dir })?;
            print!("{}", report.stdout);
        }
        HotpatchCommands::Clean { older_than } => {
            let report = manager::clean(config, mempal_home, manager::CleanOptions { older_than })?;
            print!("{}", report.stdout);
        }
    }
    Ok(())
}

pub(crate) fn short_drawer_id(drawer_id: &str) -> &str {
    let len = drawer_id
        .char_indices()
        .nth(8)
        .map(|(idx, _)| idx)
        .unwrap_or(drawer_id.len());
    &drawer_id[..len]
}

pub(crate) struct FileLock {
    file: File,
}

impl FileLock {
    pub(crate) fn open_exclusive(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // SAFETY: flock operates on the valid file descriptor owned by this
            // File and does not outlive it.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("failed to lock {}", path.display()));
            }
        }
        Ok(Self { file })
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // SAFETY: unlocks the same valid descriptor previously locked.
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

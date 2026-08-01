//! Linux daemon process discovery with process-birth identity validation.
//!
//! Discovery correlates daemon argv, database ownership, and `/proc` start
//! ticks. Callers can therefore reject a reused PID before treating daemon
//! registration or IPC readiness as authoritative.

use std::path::Path;

#[cfg(target_os = "linux")]
use std::ffi::OsStr;

#[cfg(target_os = "linux")]
use crate::daemon_singleton::{MEMPAL_DB_PATH_ENV, is_daemon_argv, parse_proc_cmdline};
#[cfg(target_os = "linux")]
use crate::db_path_identity::DbPathIdentity;

/// A daemon PID paired with the Linux process birth observed during discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonProcess {
    pub pid: i32,
    pub(crate) start_time_ticks: u64,
}

impl DaemonProcess {
    /// Return true only while this PID still names the discovered process birth.
    #[cfg(target_os = "linux")]
    pub fn is_current(&self) -> bool {
        daemon_process_is_current_in_proc(self, Path::new("/proc"))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn is_current(&self) -> bool {
        false
    }

    /// Verify an opaque daemon status identity against this process birth.
    #[cfg(target_os = "linux")]
    pub(crate) fn matches_process_identity(&self, expected: &str) -> bool {
        let Ok(pid) = u32::try_from(self.pid) else {
            return false;
        };
        self.is_current()
            && crate::core::process_identity::process_identity_matches(pid, expected)
                .unwrap_or(true)
    }
}

/// Enumerate every live daemon process with its process-birth identity.
///
/// Linux scans `/proc/<pid>/cmdline`, `/proc/<pid>/fd`, and
/// `/proc/<pid>/environ`. Non-Linux returns an empty collection because robust
/// sibling discovery is a Linux-only refinement.
#[cfg(target_os = "linux")]
pub fn enumerate_daemon_processes(binary_name: &str, db_path: &Path) -> Vec<DaemonProcess> {
    let self_pid = std::process::id() as i32;
    enumerate_daemon_processes_in_proc(binary_name, db_path, Path::new("/proc"), self_pid)
}

#[cfg(target_os = "linux")]
fn enumerate_daemon_processes_in_proc(
    binary_name: &str,
    db_path: &Path,
    proc_root: &Path,
    self_pid: i32,
) -> Vec<DaemonProcess> {
    let mut processes = Vec::new();
    let Some(db_identity) = DbPathIdentity::from_existing_db_path(db_path) else {
        return processes;
    };
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return processes;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Some(start_time_ticks) = process_start_ticks_in_proc(proc_root, pid) else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let argv = parse_proc_cmdline(&raw);
        if is_daemon_argv(&argv, binary_name)
            && process_matches_db_path(&entry.path(), &db_identity)
            && process_start_ticks_in_proc(proc_root, pid) == Some(start_time_ticks)
        {
            processes.push(DaemonProcess {
                pid,
                start_time_ticks,
            });
        }
    }
    processes.sort_unstable_by_key(|process| process.pid);
    processes
}

#[cfg(target_os = "linux")]
fn process_start_ticks_in_proc(proc_root: &Path, pid: i32) -> Option<u64> {
    let stat = std::fs::read(proc_root.join(pid.to_string()).join("stat")).ok()?;
    crate::core::process_identity::parse_start_ticks(&stat)
}

/// Return false for a confirmed dead or zombie process; fail closed otherwise.
#[cfg(target_os = "linux")]
pub fn process_is_live(pid: i32) -> bool {
    process_is_live_in_proc(Path::new("/proc"), pid).unwrap_or(true)
}

#[cfg(target_os = "linux")]
fn process_is_live_in_proc(proc_root: &Path, pid: i32) -> Option<bool> {
    let pid = u32::try_from(pid).ok()?;
    match std::fs::read(proc_root.join(pid.to_string()).join("stat")) {
        Ok(stat) => Some(crate::core::process_identity::parse_start_ticks(&stat).is_some()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn process_is_live(_pid: i32) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn daemon_process_is_current_in_proc(process: &DaemonProcess, proc_root: &Path) -> bool {
    process_start_ticks_in_proc(proc_root, process.pid) == Some(process.start_time_ticks)
}

#[cfg(target_os = "linux")]
pub fn enumerate_daemon_pids(binary_name: &str, db_path: &Path) -> Vec<i32> {
    enumerate_daemon_processes(binary_name, db_path)
        .into_iter()
        .map(|process| process.pid)
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn enumerate_daemon_pids_in_proc(
    binary_name: &str,
    db_path: &Path,
    proc_root: &Path,
    self_pid: i32,
) -> Vec<i32> {
    enumerate_daemon_processes_in_proc(binary_name, db_path, proc_root, self_pid)
        .into_iter()
        .map(|process| process.pid)
        .collect()
}

#[cfg(target_os = "linux")]
fn process_matches_db_path(proc_pid_dir: &Path, db_identity: &DbPathIdentity) -> bool {
    process_has_db_fd(proc_pid_dir, db_identity)
        || process_env_matches_db_path(proc_pid_dir, db_identity.db_path())
}

#[cfg(target_os = "linux")]
fn process_has_db_fd(proc_pid_dir: &Path, db_identity: &DbPathIdentity) -> bool {
    let Ok(entries) = std::fs::read_dir(proc_pid_dir.join("fd")) else {
        return false;
    };

    entries.flatten().any(|entry| {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            return false;
        };
        db_identity.matches_fd(&entry.path(), &target)
    })
}

#[cfg(target_os = "linux")]
fn process_env_matches_db_path(proc_pid_dir: &Path, expected_db_path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(raw) = std::fs::read(proc_pid_dir.join("environ")) else {
        return false;
    };

    raw.split(|byte| *byte == 0)
        .filter_map(|part| part.strip_prefix(MEMPAL_DB_PATH_ENV))
        .map(OsStr::from_bytes)
        .any(|path| env_db_path_matches(path, expected_db_path))
}

#[cfg(target_os = "linux")]
fn env_db_path_matches(path: &OsStr, expected_db_path: &Path) -> bool {
    let path = Path::new(path);
    path == expected_db_path
        || std::fs::canonicalize(path).is_ok_and(|canonical| canonical == expected_db_path)
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_daemon_processes(_binary_name: &str, _db_path: &Path) -> Vec<DaemonProcess> {
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_daemon_pids(_binary_name: &str, _db_path: &Path) -> Vec<i32> {
    Vec::new()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn process_identity_rejects_reused_pid_start_ticks() {
        let temp = tempfile::tempdir().expect("temp dir");
        let proc_root = temp.path().join("proc");
        let pid_dir = proc_root.join("321");
        std::fs::create_dir_all(&pid_dir).expect("pid dir");
        std::fs::write(
            pid_dir.join("stat"),
            format!("321 (mempal) S{} 111", " 0".repeat(18)),
        )
        .expect("initial stat");
        let process = DaemonProcess {
            pid: 321,
            start_time_ticks: 111,
        };
        assert!(daemon_process_is_current_in_proc(&process, &proc_root));

        std::fs::write(
            pid_dir.join("stat"),
            format!("321 (mempal) S{} 222", " 0".repeat(18)),
        )
        .expect("reused stat");
        assert!(!daemon_process_is_current_in_proc(&process, &proc_root));
    }
}

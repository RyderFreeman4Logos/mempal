//! Atomic singleton enforcement and orphan reaping for the background daemon.
//!
//! Two concerns live here, both keyed off the daemon's own argv / a lock file
//! next to `daemon.pid` (#257):
//!
//!   1. **Atomic singleton gate** — the running daemon holds a
//!      process-lifetime `flock(LOCK_EX | LOCK_NB)` on `daemon.lock`. A second
//!      daemon that tries to start while a healthy daemon already holds the
//!      lock gets `EWOULDBLOCK`/`EAGAIN` and exits immediately instead of
//!      daemonizing. The pidfile (written non-atomically) could not provide
//!      this: two `serve --mcp` ensure-checks could both observe "no live
//!      daemon" and both daemonize, converging on duplicate orphans.
//!
//!   2. **Sibling enumeration** — `daemon status` / `daemon stop` scan
//!      `/proc/<pid>/cmdline` for every live `<binary> daemon …` invocation so
//!      `status` reports the true count and `stop` reaps orphans the single
//!      pidfile PID could never see.
//!
//! The `flock` primitive mirrors the proven inline-extern pattern in
//! `src/ingest/lock.rs` (no libc dep on Unix; Windows no-op fallback). The
//! lock is associated with the open file description, so it survives the
//! double-`fork` performed by `daemonize` as long as the guard fd stays open,
//! and is released automatically when the process dies — even on `SIGKILL`,
//! which is precisely how an orphaned daemon's lock frees up for its successor.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Lock file name, written next to `daemon.pid` inside `mempal_home`.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";

/// Sentinel error meaning a healthy daemon already holds the singleton lock.
///
/// Returned (wrapped in `anyhow::Error`) by daemon bootstrap so the race loser
/// can be turned into a clean success exit at the single production call site
/// without disturbing the `Result<DaemonContext>` signature the integration
/// tests depend on. Downcast with [`anyhow::Error::is`].
#[derive(Debug, Error)]
#[error("daemon already running (singleton lock held)")]
pub struct DaemonAlreadyRunning;

#[derive(Debug, Error)]
pub enum DaemonLockError {
    #[error("io error on daemon lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Outcome of a non-blocking attempt to acquire the daemon singleton lock.
#[derive(Debug)]
pub enum DaemonLockAcquisition {
    /// This process now exclusively owns the daemon lock.
    Acquired(DaemonLockGuard),
    /// A healthy daemon already holds the lock; the caller must NOT daemonize.
    AlreadyHeld,
}

/// RAII guard holding the daemon singleton lock for the process lifetime.
///
/// The lock is released only once every duplicate fd (across `fork` /
/// daemonize) is closed, so keeping this guard alive in the running daemon
/// keeps the lock held; dropping it (or process exit, including `SIGKILL`)
/// releases it.
#[derive(Debug)]
pub struct DaemonLockGuard {
    _file: File,
    path: PathBuf,
}

impl DaemonLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Try to acquire the daemon singleton lock at `<mempal_home>/daemon.lock`.
///
/// Non-blocking: on contention returns [`DaemonLockAcquisition::AlreadyHeld`]
/// (no retry) so the race loser can exit instead of spinning. The open file
/// description backing the returned guard survives `fork`/daemonize, so the
/// lock persists into the final daemon process as long as the guard is held.
pub fn try_acquire(mempal_home: &Path) -> Result<DaemonLockAcquisition, DaemonLockError> {
    let lock_path = mempal_home.join(DAEMON_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|source| DaemonLockError::Io {
            path: lock_path.clone(),
            source,
        })?;

    match imp::try_lock_exclusive(&file) {
        Ok(true) => Ok(DaemonLockAcquisition::Acquired(DaemonLockGuard {
            _file: file,
            path: lock_path,
        })),
        Ok(false) => Ok(DaemonLockAcquisition::AlreadyHeld),
        Err(source) => Err(DaemonLockError::Io {
            path: lock_path,
            source,
        }),
    }
}

/// Basename of the currently running executable (e.g. `"mempal"`), used to
/// match sibling daemon processes precisely. Falls back to `None` if the path
/// cannot be resolved; callers should substitute a sensible default.
pub fn current_binary_name() -> Option<String> {
    std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    })
}

/// Split a raw `/proc/<pid>/cmdline` blob (NUL-separated argv) into argv parts,
/// dropping empty trailing fields.
pub fn parse_proc_cmdline(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .filter_map(|part| std::str::from_utf8(part).ok().map(ToOwned::to_owned))
        .collect()
}

/// Precise predicate: is `argv` a `<binary_name> daemon …` invocation?
///
/// Matches only when argv[0]'s basename equals `binary_name` AND argv[1] is
/// exactly `daemon`. This intentionally excludes `mempal serve --mcp`
/// (argv[1] = `serve`), `mempal status` (argv[1] = `status`), and unrelated
/// programs such as `claude -p …` (different basename). Pure and unit-testable;
/// the `/proc` scan in [`enumerate_daemon_pids`] feeds parsed argv through it.
pub fn is_daemon_argv<S: AsRef<str>>(argv: &[S], binary_name: &str) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let Some(subcommand) = argv.get(1) else {
        return false;
    };
    let program_base = Path::new(program.as_ref())
        .file_name()
        .and_then(|name| name.to_str());
    program_base == Some(binary_name) && subcommand.as_ref() == "daemon"
}

/// Enumerate the PIDs of every live `<binary_name> daemon …` process, excluding
/// the calling process itself.
///
/// Linux: scans `/proc/<pid>/cmdline`. Non-Linux: returns empty (the pidfile
/// path still applies; robust sibling reaping is a Linux-only refinement).
#[cfg(target_os = "linux")]
pub fn enumerate_daemon_pids(binary_name: &str) -> Vec<i32> {
    let self_pid = std::process::id() as i32;
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue; // skip non-numeric /proc entries
        };
        if pid == self_pid {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue; // process exited or unreadable; skip
        };
        if raw.is_empty() {
            continue; // kernel threads expose an empty cmdline
        }
        let argv = parse_proc_cmdline(&raw);
        if is_daemon_argv(&argv, binary_name) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_daemon_pids(_binary_name: &str) -> Vec<i32> {
    Vec::new()
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const EWOULDBLOCK: i32 = 35; // macOS; Linux uses EAGAIN (11)

    unsafe extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    /// Returns `Ok(true)` when the exclusive lock was acquired, `Ok(false)` on
    /// contention (`EWOULDBLOCK`/`EAGAIN`), and `Err` for any other OS error.
    pub fn try_lock_exclusive(file: &File) -> Result<bool, io::Error> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid open file descriptor owned by `file` for the
        // duration of this call; `flock` only inspects it and does not retain
        // it past return.
        let ret = unsafe { flock(fd, LOCK_EX | LOCK_NB) };
        if ret == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == EWOULDBLOCK || code == 11 => Ok(false),
            _ => Err(err),
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::io;

    /// Windows fallback: always "acquires". Singleton enforcement via
    /// `LockFileEx` is follow-up work; the pidfile path still applies.
    pub fn try_lock_exclusive(_file: &File) -> Result<bool, io::Error> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn test_second_acquire_blocked_until_first_dropped() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let guard1 = match try_acquire(tmp.path()).expect("first acquire") {
            DaemonLockAcquisition::Acquired(guard) => guard,
            DaemonLockAcquisition::AlreadyHeld => panic!("first acquire should win"),
        };
        assert!(guard1.path().ends_with(DAEMON_LOCK_FILE));

        // A second acquisition while the first guard is held must observe the
        // lock as already held (distinct open file descriptions conflict under
        // flock even within the same process).
        match try_acquire(tmp.path()).expect("second acquire attempt") {
            DaemonLockAcquisition::AlreadyHeld => {}
            DaemonLockAcquisition::Acquired(_) => {
                panic!("second acquire must not win while first guard is held")
            }
        }

        drop(guard1);

        // After release the lock is acquirable again.
        match try_acquire(tmp.path()).expect("third acquire after drop") {
            DaemonLockAcquisition::Acquired(_) => {}
            DaemonLockAcquisition::AlreadyHeld => {
                panic!("acquire after drop should win")
            }
        }
    }

    #[test]
    fn test_is_daemon_argv_matches_daemon_and_rejects_siblings() {
        // Matches the daemon's own invocations (bare path and absolute path,
        // with and without a subcommand).
        assert!(is_daemon_argv(
            &argv(&["mempal", "daemon", "--foreground"]),
            "mempal"
        ));
        assert!(is_daemon_argv(&argv(&["mempal", "daemon"]), "mempal"));
        assert!(is_daemon_argv(
            &argv(&["/usr/local/bin/mempal", "daemon", "start", "--foreground"]),
            "mempal"
        ));
        assert!(is_daemon_argv(
            &argv(&["target/release/mempal", "daemon", "restart"]),
            "mempal"
        ));

        // Must NOT match the MCP server, the top-level status command, the stop
        // command's own argv pattern aside, or unrelated programs.
        assert!(!is_daemon_argv(
            &argv(&["mempal", "serve", "--mcp"]),
            "mempal"
        ));
        assert!(!is_daemon_argv(&argv(&["mempal", "status"]), "mempal"));
        assert!(!is_daemon_argv(
            &argv(&["claude", "-p", "mempal daemon --foreground"]),
            "mempal"
        ));
        assert!(!is_daemon_argv(
            &argv(&["/usr/bin/python3", "daemon.py"]),
            "mempal"
        ));

        // Defensive edges: too-short argv and a different binary name.
        assert!(!is_daemon_argv(&argv(&["mempal"]), "mempal"));
        assert!(!is_daemon_argv::<&str>(&[], "mempal"));
        assert!(!is_daemon_argv(&argv(&["other-tool", "daemon"]), "mempal"));
    }

    #[test]
    fn test_parse_proc_cmdline_splits_nul_separated_argv() {
        assert_eq!(
            parse_proc_cmdline(b"mempal\0daemon\0--foreground\0"),
            vec![
                "mempal".to_string(),
                "daemon".to_string(),
                "--foreground".to_string()
            ]
        );
        // A trailing/duplicate NUL must not yield empty argv entries.
        assert_eq!(
            parse_proc_cmdline(b"mempal\0\0daemon\0"),
            vec!["mempal".to_string(), "daemon".to_string()]
        );
        assert!(parse_proc_cmdline(b"").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_enumerate_excludes_self() {
        // The test runner's argv is not `<name> daemon …`, so scanning for a
        // binary name that matches this process's basename must not return our
        // own PID (enumerate filters self out regardless).
        let self_pid = std::process::id() as i32;
        let name = current_binary_name().unwrap_or_else(|| "mempal".to_string());
        assert!(!enumerate_daemon_pids(&name).contains(&self_pid));
        // Sanity: a binary name that cannot match anything yields no PIDs.
        assert!(enumerate_daemon_pids("\0no-such-binary").is_empty());
    }
}

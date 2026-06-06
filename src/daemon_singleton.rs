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
//!   2. **Sibling enumeration** — `daemon status` / `daemon stop` / `daemon reap`
//!      scan `/proc/<pid>/cmdline` for every live daemon invocation using the
//!      same database. This is intentionally argv/database-based instead of
//!      lock-based so a deleted-inode daemon from an older binary that never
//!      acquired `daemon.lock` can still be found and reaped.
//!
//! The `flock` primitive mirrors the proven inline-extern pattern in
//! `src/ingest/lock.rs` (no libc dep on Unix; Windows no-op fallback). The
//! lock is associated with the open file description, so it survives the
//! double-`fork` performed by `daemonize` as long as the guard fd stays open,
//! and is released automatically when the process dies — even on `SIGKILL`,
//! which is precisely how an orphaned daemon's lock frees up for its successor.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Lock file name, written next to `daemon.pid` inside `mempal_home`.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
const MEMPAL_DB_PATH_ENV: &[u8] = b"MEMPAL_DB_PATH=";

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

/// Precise predicate: is `argv` a daemon process invocation?
///
/// Matches only argv shapes that can become the long-lived daemon after
/// `DaemonContext::bootstrap` daemonizes the current process:
/// `<binary> daemon`, `<binary> daemon --foreground`,
/// `<binary> daemon start [--foreground]`, and `<binary> daemon restart`.
/// This intentionally excludes `mempal serve --mcp`, top-level CLI commands,
/// and daemon management commands that do not call daemon bootstrap.
pub fn is_daemon_argv<S: AsRef<str>>(argv: &[S], binary_name: &str) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let program_base = Path::new(program.as_ref())
        .file_name()
        .and_then(|name| name.to_str());
    if program_base != Some(binary_name) {
        return false;
    }

    let Some((subcommand_index, subcommand)) = first_cli_subcommand(argv) else {
        return false;
    };
    if subcommand != "daemon" {
        return false;
    }

    match argv.get(subcommand_index + 1).map(AsRef::as_ref) {
        None | Some("--foreground") => true,
        Some("start") => matches!(
            argv.get(subcommand_index + 2).map(AsRef::as_ref),
            None | Some("--foreground")
        ),
        Some("restart") => argv.get(subcommand_index + 2).is_none(),
        Some(_) => false,
    }
}

fn first_cli_subcommand<S: AsRef<str>>(argv: &[S]) -> Option<(usize, &str)> {
    let mut skip_next = false;
    for (index, arg) in argv.iter().enumerate().skip(1) {
        let arg = arg.as_ref();
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            return argv.get(index + 1).map(|next| (index + 1, next.as_ref()));
        }
        if arg.starts_with("--") {
            if global_flag_takes_value(arg) {
                skip_next = true;
            }
            continue;
        }
        return Some((index, arg));
    }
    None
}

fn global_flag_takes_value(arg: &str) -> bool {
    // Keep this list aligned with clap global flag parsing so every value-taking
    // global option is skipped when locating the first subcommand.
    const CLI_GLOBAL_VALUE_FLAGS: &[&str] = &["--config", "--config-path"];

    !arg.contains('=') && CLI_GLOBAL_VALUE_FLAGS.contains(&arg)
}

/// Enumerate the PIDs of every live daemon process, excluding this process.
///
/// Linux: scans `/proc/<pid>/cmdline`, `/proc/<pid>/fd`, and
/// `/proc/<pid>/environ`. Non-Linux: returns empty (the pidfile path still
/// applies; robust sibling reaping is a Linux-only refinement).
#[cfg(target_os = "linux")]
pub fn enumerate_daemon_pids(binary_name: &str, db_path: &Path) -> Vec<i32> {
    let self_pid = std::process::id() as i32;
    enumerate_daemon_pids_in_proc(binary_name, db_path, Path::new("/proc"), self_pid)
}

#[cfg(target_os = "linux")]
fn enumerate_daemon_pids_in_proc(
    binary_name: &str,
    db_path: &Path,
    proc_root: &Path,
    self_pid: i32,
) -> Vec<i32> {
    let mut pids = Vec::new();
    let Some(db_identity) = DbPathIdentity::from_db_path(db_path) else {
        return pids;
    };
    let Ok(entries) = std::fs::read_dir(proc_root) else {
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
        if is_daemon_argv(&argv, binary_name)
            && process_matches_db_path(&entry.path(), &db_identity)
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    pids
}

#[cfg(target_os = "linux")]
fn process_matches_db_path(proc_pid_dir: &Path, db_identity: &DbPathIdentity) -> bool {
    process_has_db_fd(proc_pid_dir, db_identity)
        || process_env_matches_db_path(proc_pid_dir, db_identity.db_path())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct DbPathIdentity {
    db_path: PathBuf,
    fd_targets: BTreeSet<PathBuf>,
}

#[cfg(target_os = "linux")]
impl DbPathIdentity {
    fn from_db_path(db_path: &Path) -> Option<Self> {
        let db_path = std::fs::canonicalize(db_path).ok()?;
        let mut fd_targets = BTreeSet::new();
        fd_targets.insert(db_path.clone());
        for suffix in ["-wal", "-shm"] {
            let sidecar = append_os_suffix(&db_path, suffix);
            fd_targets.insert(canonicalize_if_present(&sidecar));
        }
        Some(Self {
            db_path,
            fd_targets,
        })
    }

    fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn matches_fd_target(&self, fd_target: &Path) -> bool {
        self.fd_targets.contains(fd_target)
    }
}

#[cfg(target_os = "linux")]
fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(OsStr::new(suffix));
    PathBuf::from(os)
}

#[cfg(target_os = "linux")]
fn canonicalize_if_present(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
        let Ok(canonical_target) = std::fs::canonicalize(target) else {
            return false;
        };
        db_identity.matches_fd_target(&canonical_target)
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
pub fn enumerate_daemon_pids(_binary_name: &str, _db_path: &Path) -> Vec<i32> {
    Vec::new()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonReapPlan {
    pub keeper: Option<i32>,
    pub targets: Vec<i32>,
}

/// Build an idempotent orphan-reap plan from exact daemon argv matches.
///
/// The pidfile daemon wins when it is still among the live candidates. If the
/// pidfile is stale or absent, the lowest PID is elected as the singleton to
/// keep. Every other daemon is a reap target.
pub fn plan_daemon_reap(candidates: &[i32], pidfile_pid: Option<i32>) -> DaemonReapPlan {
    let mut live = candidates.to_vec();
    live.sort_unstable();
    live.dedup();

    let keeper = pidfile_pid
        .filter(|pid| live.contains(pid))
        .or_else(|| live.first().copied());
    let targets = live
        .into_iter()
        .filter(|pid| Some(*pid) != keeper)
        .collect();

    DaemonReapPlan { keeper, targets }
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
        let cases = [
            (vec!["mempal", "daemon", "--foreground"], true),
            (vec!["mempal", "daemon"], true),
            (
                vec!["/usr/local/bin/mempal", "daemon", "start", "--foreground"],
                true,
            ),
            (vec!["target/release/mempal", "daemon", "restart"], true),
            (vec!["mempal", "--verbose", "daemon"], true),
            (
                vec!["mempal", "--config", "/tmp/config.toml", "daemon"],
                true,
            ),
            (vec!["mempal", "--config=/tmp/config.toml", "daemon"], true),
            (vec!["mempal", "serve", "--mcp"], false),
            (vec!["mempal", "reindex"], false),
            (vec!["mempal", "search", "daemon"], false),
            (vec!["mempal", "daemon", "stop"], false),
            (vec!["mempal", "daemon", "status"], false),
            (vec!["mempal", "daemon", "reap"], false),
            (vec!["mempal", "daemon", "restart", "--foreground"], false),
            (vec!["claude", "-p", "mempal daemon --foreground"], false),
            (vec!["/usr/bin/python3", "daemon.py"], false),
            (vec!["mempal"], false),
            (vec!["other-tool", "daemon"], false),
        ];

        for (parts, expected) in cases {
            assert_eq!(
                is_daemon_argv(&argv(&parts), "mempal"),
                expected,
                "argv={parts:?}"
            );
        }
        assert!(!is_daemon_argv::<&str>(&[], "mempal"));
    }

    #[test]
    fn test_first_cli_subcommand_skips_global_value_flags() {
        let parts = ["mempal", "--config", "/tmp/config.toml", "daemon", "status"];
        assert_eq!(first_cli_subcommand(&argv(&parts)), Some((3, "daemon")));

        let eq_parts = ["mempal", "--config=/tmp/config.toml", "daemon", "status"];
        assert_eq!(first_cli_subcommand(&argv(&eq_parts)), Some((2, "daemon")));
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
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        File::create(&db_path).expect("db file");
        assert!(!enumerate_daemon_pids(&name, &db_path).contains(&self_pid));
        // Sanity: a binary name that cannot match anything yields no PIDs.
        assert!(enumerate_daemon_pids("\0no-such-binary", &db_path).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_enumerate_proc_scan_matches_daemon_argv_by_db_identity() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join(".mempal").join("palace.db");
        let other_db_path = tmp.path().join("other").join("palace.db");
        let proc_root = tmp.path().join("proc");
        std::fs::create_dir_all(db_path.parent().expect("db parent")).expect("mempal home");
        std::fs::create_dir_all(other_db_path.parent().expect("other db parent"))
            .expect("other db parent");
        std::fs::create_dir_all(&proc_root).expect("proc root");
        File::create(&db_path).expect("db file");
        File::create(&other_db_path).expect("other db file");

        write_fake_proc(
            &proc_root,
            101,
            b"mempal\0daemon\0--foreground\0",
            Some(&db_path),
            Some(&db_path),
        );
        write_fake_proc(
            &proc_root,
            202,
            b"mempal\0serve\0--mcp\0",
            Some(&db_path),
            Some(&db_path),
        );
        write_fake_proc(
            &proc_root,
            303,
            b"mempal\0daemon\0stop\0",
            Some(&db_path),
            Some(&db_path),
        );
        write_fake_proc(
            &proc_root,
            404,
            b"other\0daemon\0",
            Some(&db_path),
            Some(&db_path),
        );
        write_fake_proc(
            &proc_root,
            505,
            b"mempal\0daemon\0--foreground\0",
            Some(&other_db_path),
            Some(&other_db_path),
        );

        let pids = enumerate_daemon_pids_in_proc("mempal", &db_path, &proc_root, 0);

        assert_eq!(pids, vec![101]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_enumerate_proc_scan_matches_custom_db_outside_home_by_fd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let custom_db = tmp.path().join("var-lib-mempal").join("palace.db");
        let proc_root = tmp.path().join("proc");
        std::fs::create_dir_all(&home).expect("home");
        std::fs::create_dir_all(custom_db.parent().expect("custom db parent"))
            .expect("custom db parent");
        std::fs::create_dir_all(&proc_root).expect("proc root");
        File::create(&custom_db).expect("custom db");

        write_fake_proc(
            &proc_root,
            707,
            b"mempal\0daemon\0--foreground\0",
            Some(&custom_db),
            None,
        );

        let pids = enumerate_daemon_pids_in_proc("mempal", &custom_db, &proc_root, 0);

        assert_eq!(pids, vec![707]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_enumerate_proc_scan_rejects_daemon_argv_for_different_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("ours").join("palace.db");
        let other_db_path = tmp.path().join("other").join("palace.db");
        let proc_root = tmp.path().join("proc");
        std::fs::create_dir_all(db_path.parent().expect("db parent")).expect("db parent");
        std::fs::create_dir_all(other_db_path.parent().expect("other db parent"))
            .expect("other db parent");
        std::fs::create_dir_all(&proc_root).expect("proc root");
        File::create(&db_path).expect("db file");
        File::create(&other_db_path).expect("other db file");

        write_fake_proc(
            &proc_root,
            808,
            b"mempal\0daemon\0--foreground\0",
            Some(&other_db_path),
            Some(&other_db_path),
        );

        let pids = enumerate_daemon_pids_in_proc("mempal", &db_path, &proc_root, 0);

        assert!(pids.is_empty());
    }

    #[cfg(target_os = "linux")]
    fn write_fake_proc(
        proc_root: &Path,
        pid: i32,
        cmdline: &[u8],
        fd_db_path: Option<&Path>,
        env_db_path: Option<&Path>,
    ) {
        use std::os::unix::ffi::OsStrExt;

        let pid_dir = proc_root.join(pid.to_string());
        std::fs::create_dir_all(pid_dir.join("fd")).expect("pid fd dir");
        std::fs::write(pid_dir.join("cmdline"), cmdline).expect("cmdline");

        let mut environ = Vec::new();
        if let Some(path) = env_db_path {
            environ.extend_from_slice(MEMPAL_DB_PATH_ENV);
            environ.extend_from_slice(path.as_os_str().as_bytes());
            environ.push(0);
        }
        std::fs::write(pid_dir.join("environ"), environ).expect("environ");

        if let Some(path) = fd_db_path {
            std::os::unix::fs::symlink(path, pid_dir.join("fd").join("3")).expect("fd symlink");
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_enumerate_proc_scan_matches_non_utf8_db_path() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let proc_root = temp.path().join("proc");
        let db_root = temp.path().join("db");
        std::fs::create_dir_all(&db_root).expect("db dir");

        // Create a non-UTF8 path: 0xFF is invalid UTF-8.
        let non_utf8_name = Vec::from_iter([b'm', b'y', 0xff, b'.', b'd', b'b']);
        let db_name = OsString::from_vec(non_utf8_name);
        let db_path = db_root.join(db_name);
        std::fs::File::create(&db_path).expect("db file");
        let db_path = std::fs::canonicalize(db_path).expect("canonicalize");

        let other_non_utf8_name =
            Vec::from_iter([b'o', b't', b'h', b'e', b'r', 0xfe, b'.', b'd', b'b']);
        let other_db_name = OsString::from_vec(other_non_utf8_name);
        let other_db_path = db_root.join(other_db_name);
        std::fs::File::create(&other_db_path).expect("other db file");
        let other_db_path = std::fs::canonicalize(other_db_path).expect("canonicalize other db");

        write_fake_proc(
            &proc_root,
            101,
            b"mempal\0daemon\0--foreground\0",
            None,
            Some(&db_path),
        );
        write_fake_proc(
            &proc_root,
            202,
            b"mempal\0daemon\0--foreground\0",
            None,
            Some(&other_db_path),
        );

        let pids = enumerate_daemon_pids_in_proc("mempal", &db_path, &proc_root, 999);
        assert_eq!(pids, vec![101]);
    }

    #[test]
    fn test_plan_daemon_reap_keeps_pidfile_daemon_when_live() {
        let plan = plan_daemon_reap(&[303, 101, 202], Some(202));

        assert_eq!(
            plan,
            DaemonReapPlan {
                keeper: Some(202),
                targets: vec![101, 303],
            }
        );
    }

    #[test]
    fn test_plan_daemon_reap_elects_lowest_pid_when_pidfile_stale() {
        let plan = plan_daemon_reap(&[303, 101, 202], Some(999));

        assert_eq!(
            plan,
            DaemonReapPlan {
                keeper: Some(101),
                targets: vec![202, 303],
            }
        );
    }
}

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

use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use crate::db_path_identity::DbPathIdentity;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Legacy lock file name used by pre-#496 daemon builds.
pub const DAEMON_LOCK_FILE: &str = "daemon.lock";
pub const MEMPAL_RUNTIME_DIR_ENV: &str = "MEMPAL_RUNTIME_DIR";
pub const DAEMON_LOCK_SUFFIX: &str = ".daemon.lock";
pub const DAEMON_METADATA_SUFFIX: &str = ".daemon.json";
const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";
const MEMPAL_DB_PATH_ENV: &[u8] = b"MEMPAL_DB_PATH=";

/// Sentinel error meaning a healthy daemon already holds the singleton lock.
///
/// Returned (wrapped in `anyhow::Error`) by daemon bootstrap so the race loser
/// can be turned into a clean success exit at the single production call site
/// without disturbing the `Result<DaemonContext>` signature the integration
/// tests depend on. Downcast with [`anyhow::Error::is`].
#[derive(Debug)]
pub struct DaemonAlreadyRunning {
    pub owner: Option<DaemonLockMetadata>,
    pub lock_path: Option<PathBuf>,
}

impl DaemonAlreadyRunning {
    pub fn new(owner: Option<DaemonLockMetadata>, lock_path: Option<PathBuf>) -> Self {
        Self { owner, lock_path }
    }
}

impl std::fmt::Display for DaemonAlreadyRunning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "daemon already running (singleton lock held")?;
        if let Some(lock_path) = &self.lock_path {
            write!(formatter, ", lock={}", lock_path.display())?;
        }
        if let Some(owner) = &self.owner {
            write!(
                formatter,
                ", owner_pid={}, boot_id={}, version={}, db_fingerprint={}, started_at_unix_ms={}",
                owner.pid,
                owner.boot_id.as_deref().unwrap_or("unknown"),
                owner.binary_version,
                owner.db_fingerprint,
                owner.started_at_unix_ms
            )?;
            if let Some(path) = &owner.executable_path {
                write!(formatter, ", executable={path}")?;
            }
            if owner.executable_deleted {
                write!(formatter, ", executable_deleted=true")?;
            }
        } else {
            write!(formatter, ", owner_metadata=unavailable")?;
        }
        write!(formatter, ")")
    }
}

impl std::error::Error for DaemonAlreadyRunning {}

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
    AlreadyHeld {
        owner: Option<DaemonLockMetadata>,
        lock_path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonLockMetadata {
    pub pid: u32,
    pub boot_id: Option<String>,
    pub binary_version: String,
    pub db_path: String,
    pub db_fingerprint: String,
    pub profile: Option<String>,
    pub started_at_unix_ms: u64,
    pub executable_path: Option<String>,
    pub executable_deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonLockPaths {
    pub runtime_dir: PathBuf,
    pub lock_path: PathBuf,
    pub metadata_path: PathBuf,
    pub db_path: PathBuf,
    pub db_fingerprint: String,
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
    metadata_path: PathBuf,
    metadata: DaemonLockMetadata,
}

impl DaemonLockGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata_path(&self) -> &Path {
        &self.metadata_path
    }

    pub fn metadata(&self) -> &DaemonLockMetadata {
        &self.metadata
    }

    pub fn refresh_metadata(&mut self) -> Result<(), DaemonLockError> {
        let metadata = build_metadata(
            &self.metadata.db_path,
            self.metadata.db_fingerprint.clone(),
            self.metadata.profile.clone(),
        );
        write_metadata_file(&self.metadata_path, &metadata)?;
        self.metadata = metadata;
        Ok(())
    }
}

impl Drop for DaemonLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.metadata_path);
    }
}

/// Try to acquire the daemon singleton lock for a canonical database path.
///
/// Non-blocking: on contention returns [`DaemonLockAcquisition::AlreadyHeld`]
/// (no retry) so the race loser can exit instead of spinning. The open file
/// description backing the returned guard survives `fork`/daemonize, so the
/// lock persists into the final daemon process as long as the guard is held.
pub fn try_acquire(db_path: &Path) -> Result<DaemonLockAcquisition, DaemonLockError> {
    try_acquire_with_runtime_root(db_path, None, None)
}

#[doc(hidden)]
pub fn try_acquire_for_test(
    db_path: &Path,
    runtime_root: &Path,
) -> Result<DaemonLockAcquisition, DaemonLockError> {
    try_acquire_with_runtime_root(db_path, None, Some(runtime_root))
}

fn try_acquire_with_runtime_root(
    db_path: &Path,
    profile: Option<&str>,
    runtime_root: Option<&Path>,
) -> Result<DaemonLockAcquisition, DaemonLockError> {
    let paths = lock_paths_with_writable_runtime(db_path, profile, runtime_root)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(&paths.lock_path)
        .map_err(|source| DaemonLockError::Io {
            path: paths.lock_path.clone(),
            source,
        })?;

    match imp::try_lock_exclusive(&file) {
        Ok(true) => {
            let metadata = build_metadata(
                &paths.db_path.to_string_lossy(),
                paths.db_fingerprint.clone(),
                profile.map(ToOwned::to_owned),
            );
            write_metadata_file(&paths.metadata_path, &metadata)?;
            Ok(DaemonLockAcquisition::Acquired(DaemonLockGuard {
                _file: file,
                path: paths.lock_path,
                metadata_path: paths.metadata_path,
                metadata,
            }))
        }
        Ok(false) => Ok(DaemonLockAcquisition::AlreadyHeld {
            owner: read_metadata_file(&paths.metadata_path),
            lock_path: paths.lock_path,
        }),
        Err(source) => Err(DaemonLockError::Io {
            path: paths.lock_path,
            source,
        }),
    }
}

fn lock_paths_with_writable_runtime(
    db_path: &Path,
    profile: Option<&str>,
    runtime_root: Option<&Path>,
) -> Result<DaemonLockPaths, DaemonLockError> {
    let paths = daemon_lock_paths(db_path, profile, runtime_root)?;
    match std::fs::create_dir_all(&paths.runtime_dir) {
        Ok(()) => Ok(paths),
        Err(source)
            if runtime_root.is_none()
                && env::var_os(MEMPAL_RUNTIME_DIR_ENV).is_none()
                && env::var_os(XDG_RUNTIME_DIR_ENV).is_some() =>
        {
            let fallback_root = env::temp_dir().join("mempal-runtime");
            let fallback_paths = daemon_lock_paths(db_path, profile, Some(&fallback_root))?;
            std::fs::create_dir_all(&fallback_paths.runtime_dir).map_err(|fallback_source| {
                DaemonLockError::Io {
                    path: fallback_paths.runtime_dir.clone(),
                    source: fallback_source,
                }
            })?;
            tracing::warn!(
                runtime_dir = %paths.runtime_dir.display(),
                fallback_runtime_dir = %fallback_paths.runtime_dir.display(),
                error = %source,
                "daemon runtime directory is unavailable; using temp runtime directory"
            );
            Ok(fallback_paths)
        }
        Err(source) => Err(DaemonLockError::Io {
            path: paths.runtime_dir,
            source,
        }),
    }
}

#[doc(hidden)]
pub fn daemon_lock_paths_for_test(
    db_path: &Path,
    runtime_root: &Path,
) -> Result<DaemonLockPaths, DaemonLockError> {
    daemon_lock_paths(db_path, None, Some(runtime_root))
}

fn daemon_lock_paths(
    db_path: &Path,
    profile: Option<&str>,
    runtime_root: Option<&Path>,
) -> Result<DaemonLockPaths, DaemonLockError> {
    let db_path = canonical_db_scope_path(db_path).map_err(|source| DaemonLockError::Io {
        path: db_path.to_path_buf(),
        source,
    })?;
    let db_fingerprint = db_fingerprint(&db_path, profile);
    let runtime_dir = runtime_root
        .map(Path::to_path_buf)
        .unwrap_or_else(default_runtime_dir)
        .join("mempal");
    let lock_path = runtime_dir.join(format!("{db_fingerprint}{DAEMON_LOCK_SUFFIX}"));
    let metadata_path = runtime_dir.join(format!("{db_fingerprint}{DAEMON_METADATA_SUFFIX}"));
    Ok(DaemonLockPaths {
        runtime_dir,
        lock_path,
        metadata_path,
        db_path,
        db_fingerprint,
    })
}

fn default_runtime_dir() -> PathBuf {
    env::var_os(MEMPAL_RUNTIME_DIR_ENV)
        .or_else(|| env::var_os(XDG_RUNTIME_DIR_ENV))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("mempal-runtime"))
}

fn canonical_db_scope_path(path: &Path) -> io::Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let Some(file_name) = absolute.file_name() else {
        return Ok(absolute);
    };
    let parent = absolute
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(std::fs::canonicalize(parent)?.join(file_name))
}

fn db_fingerprint(path: &Path, profile: Option<&str>) -> String {
    let mut hasher = blake3::Hasher::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(path.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(path.to_string_lossy().as_bytes());
    }
    if let Some(profile) = profile {
        hasher.update(b"\0profile\0");
        hasher.update(profile.as_bytes());
    }
    hasher.finalize().to_hex()[..16].to_string()
}

fn build_metadata(
    db_path: &str,
    db_fingerprint: String,
    profile: Option<String>,
) -> DaemonLockMetadata {
    let executable_path = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().into_owned());
    let executable_deleted = executable_path
        .as_deref()
        .is_some_and(|path| path.ends_with(" (deleted)"));
    DaemonLockMetadata {
        pid: std::process::id(),
        boot_id: boot_id(),
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        db_path: db_path.to_string(),
        db_fingerprint,
        profile,
        started_at_unix_ms: unix_ms(),
        executable_path,
        executable_deleted,
    }
}

fn write_metadata_file(
    metadata_path: &Path,
    metadata: &DaemonLockMetadata,
) -> Result<(), DaemonLockError> {
    let tmp_path = metadata_path.with_extension(format!("json.tmp.{}", std::process::id()));
    let payload = serde_json::to_vec_pretty(metadata).map_err(|source| DaemonLockError::Io {
        path: metadata_path.to_path_buf(),
        source: io::Error::other(source),
    })?;
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)
            .map_err(|source| DaemonLockError::Io {
                path: tmp_path.clone(),
                source,
            })?;
        file.write_all(&payload)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| DaemonLockError::Io {
                path: tmp_path.clone(),
                source,
            })?;
    }
    std::fs::rename(&tmp_path, metadata_path).map_err(|source| DaemonLockError::Io {
        path: metadata_path.to_path_buf(),
        source,
    })
}

fn read_metadata_file(metadata_path: &Path) -> Option<DaemonLockMetadata> {
    let payload = std::fs::read(metadata_path).ok()?;
    serde_json::from_slice(&payload).ok()
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn boot_id() -> Option<String> {
    None
}

fn unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    u64::try_from(millis).unwrap_or(u64::MAX)
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

pub(crate) fn first_cli_subcommand<S: AsRef<str>>(argv: &[S]) -> Option<(usize, &str)> {
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
    let Some(db_identity) = DbPathIdentity::from_existing_db_path(db_path) else {
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
        let db_path = tmp.path().join("palace.db");
        File::create(&db_path).expect("db file");
        let runtime = tmp.path().join("runtime");

        let guard1 = match try_acquire_for_test(&db_path, &runtime).expect("first acquire") {
            DaemonLockAcquisition::Acquired(guard) => guard,
            DaemonLockAcquisition::AlreadyHeld { .. } => panic!("first acquire should win"),
        };
        assert!(
            guard1
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(DAEMON_LOCK_SUFFIX))
        );
        assert_eq!(guard1.metadata().pid, std::process::id());
        assert!(guard1.metadata_path().exists());

        // A second acquisition while the first guard is held must observe the
        // lock as already held (distinct open file descriptions conflict under
        // flock even within the same process).
        match try_acquire_for_test(&db_path, &runtime).expect("second acquire attempt") {
            DaemonLockAcquisition::AlreadyHeld {
                owner: Some(owner),
                lock_path,
            } => {
                assert_eq!(owner.pid, std::process::id());
                assert_eq!(lock_path, guard1.path());
            }
            DaemonLockAcquisition::AlreadyHeld { owner: None, .. } => {
                panic!("already-held result should include owner metadata")
            }
            DaemonLockAcquisition::Acquired(_) => {
                panic!("second acquire must not win while first guard is held")
            }
        }

        drop(guard1);

        // After release the lock is acquirable again.
        match try_acquire_for_test(&db_path, &runtime).expect("third acquire after drop") {
            DaemonLockAcquisition::Acquired(_) => {}
            DaemonLockAcquisition::AlreadyHeld { .. } => {
                panic!("acquire after drop should win")
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_lock_scope_is_canonical_db_not_shared_home() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime = tmp.path().join("runtime");
        let db_a = tmp.path().join("profile-a").join("palace.db");
        let db_b = tmp.path().join("profile-b").join("palace.db");
        std::fs::create_dir_all(db_a.parent().expect("db a parent")).expect("db a parent");
        std::fs::create_dir_all(db_b.parent().expect("db b parent")).expect("db b parent");
        File::create(&db_a).expect("db a");
        File::create(&db_b).expect("db b");

        let guard_a = match try_acquire_for_test(&db_a, &runtime).expect("acquire a") {
            DaemonLockAcquisition::Acquired(guard) => guard,
            DaemonLockAcquisition::AlreadyHeld { .. } => panic!("db a should acquire"),
        };
        let guard_b = match try_acquire_for_test(&db_b, &runtime).expect("acquire b") {
            DaemonLockAcquisition::Acquired(guard) => guard,
            DaemonLockAcquisition::AlreadyHeld { .. } => panic!("db b should acquire"),
        };

        assert_ne!(guard_a.path(), guard_b.path());
        assert_ne!(
            guard_a.metadata().db_fingerprint,
            guard_b.metadata().db_fingerprint
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_stale_metadata_without_live_lock_is_recovered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let runtime = tmp.path().join("runtime");
        let db_path = tmp.path().join("palace.db");
        File::create(&db_path).expect("db file");
        let paths = daemon_lock_paths_for_test(&db_path, &runtime).expect("lock paths");
        std::fs::create_dir_all(&paths.runtime_dir).expect("runtime dir");
        std::fs::write(
            &paths.metadata_path,
            r#"{"pid":999999,"boot_id":"stale","binary_version":"old","db_path":"/stale","db_fingerprint":"stale","profile":null,"started_at_unix_ms":1,"executable_path":null,"executable_deleted":true}"#,
        )
        .expect("stale metadata");

        let guard = match try_acquire_for_test(&db_path, &runtime).expect("acquire") {
            DaemonLockAcquisition::Acquired(guard) => guard,
            DaemonLockAcquisition::AlreadyHeld { .. } => panic!("stale metadata must not block"),
        };

        assert_eq!(guard.metadata().pid, std::process::id());
        assert_eq!(guard.metadata().db_fingerprint, paths.db_fingerprint);
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
        use std::ffi::OsString;
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

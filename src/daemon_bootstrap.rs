use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    AsyncDb,
    async_db::RESOURCE_BOUNDED_READERS,
    config::{Config, ConfigHandle},
    db::Database,
    queue::AsyncPendingMessageStore,
};
use anyhow::{Context, Result};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

#[cfg(target_os = "linux")]
use crate::process_diagnostics::{
    DbHolderRemediationTarget, DbHolderReport, inspect_db_holders_for_startup_remediation,
};

const DAEMON_STALL_SECONDS: u64 = 5 * 60;
const DAEMON_STALL_LOG_THROTTLE_SECONDS: u64 = 60;
#[cfg(not(test))]
const DAEMONIZE_READY_TIMEOUT: Duration = Duration::from_secs(45);
#[cfg(not(test))]
const DAEMONIZE_READY_POLL: Duration = Duration::from_millis(50);
#[cfg(target_os = "linux")]
const DB_HOLDER_TERM_GRACE: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const DB_HOLDER_TERM_POLL: Duration = Duration::from_millis(100);

pub type SharedDatabase = Arc<AsyncMutex<Database>>;

#[derive(Clone)]
pub struct DaemonWriteObserver {
    inner: Arc<DaemonWriteObserverInner>,
}

struct DaemonWriteObserverInner {
    started_at: Instant,
    last_successful_write_secs: AtomicU64,
    last_stall_log_secs: AtomicU64,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonStallDiagnostic {
    queued_count: u64,
    seconds_since_successful_write: u64,
    last_error: String,
    uptime_secs: u64,
}

pub struct DaemonContext {
    pub runtime: tokio::runtime::Runtime,
    pub db: SharedDatabase,
    pub async_db: AsyncDb,
    pub store: AsyncPendingMessageStore,
    pub write_observer: DaemonWriteObserver,
    pub config: std::sync::Arc<crate::core::config::Config>,
    pub mempal_home: PathBuf,
    pub log_path: PathBuf,
    // Drop order matters: remove the pidfile (pid_guard) BEFORE releasing the
    // singleton lock (lock_guard), so a successor that wins the lock never
    // observes a stale pidfile pointing at the just-stopped daemon.
    _pid_guard: PidFileGuard,
    _lock_guard: crate::daemon_singleton::DaemonLockGuard,
}

impl DaemonWriteObserver {
    fn new() -> Self {
        Self {
            inner: Arc::new(DaemonWriteObserverInner {
                started_at: Instant::now(),
                last_successful_write_secs: AtomicU64::new(unix_secs()),
                last_stall_log_secs: AtomicU64::new(0),
                last_error: Mutex::new(None),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::new()
    }

    pub fn record_successful_write(&self) {
        self.inner
            .last_successful_write_secs
            .store(unix_secs(), Ordering::Relaxed);
    }

    pub fn record_error(&self, error: impl Into<String>) {
        if let Ok(mut last_error) = self.inner.last_error.lock() {
            *last_error = Some(error.into());
        }
    }

    pub async fn maybe_log_stall(&self, store: &AsyncPendingMessageStore) -> bool {
        let _io_burst =
            crate::observability::IoBurstGuard::start(crate::observability::IoOperationPath::Queue);
        let now = unix_secs();
        let Some(diagnostic) = self.stall_diagnostic(store, now).await else {
            return false;
        };
        let last_log = self.inner.last_stall_log_secs.load(Ordering::Relaxed);
        if now.saturating_sub(last_log) < DAEMON_STALL_LOG_THROTTLE_SECONDS {
            return false;
        }
        if self
            .inner
            .last_stall_log_secs
            .compare_exchange(last_log, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }

        tracing::error!(
            queued_count = diagnostic.queued_count,
            seconds_since_successful_write = diagnostic.seconds_since_successful_write,
            last_error = %diagnostic.last_error,
            uptime_secs = diagnostic.uptime_secs,
            "daemon write stall detected: queued messages exist but no successful write has completed for at least 5 minutes"
        );
        true
    }

    async fn stall_diagnostic(
        &self,
        store: &AsyncPendingMessageStore,
        now_secs: u64,
    ) -> Option<DaemonStallDiagnostic> {
        let stats = match store.stats().await {
            Ok(stats) => stats,
            Err(error) => {
                tracing::warn!(?error, "daemon stall detector failed to read queue stats");
                return None;
            }
        };
        let queued_count = stats.pending.saturating_add(stats.claimed);
        if queued_count == 0 {
            return None;
        }

        let last_successful_write = self
            .inner
            .last_successful_write_secs
            .load(Ordering::Relaxed);
        let seconds_since_successful_write = now_secs.saturating_sub(last_successful_write);
        if seconds_since_successful_write < DAEMON_STALL_SECONDS {
            return None;
        }

        let last_error = self
            .inner
            .last_error
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| "none recorded".to_string());

        Some(DaemonStallDiagnostic {
            queued_count,
            seconds_since_successful_write,
            last_error,
            uptime_secs: self.inner.started_at.elapsed().as_secs(),
        })
    }

    #[cfg(test)]
    fn force_last_successful_write_for_test(&self, timestamp_secs: u64) {
        self.inner
            .last_successful_write_secs
            .store(timestamp_secs, Ordering::Relaxed);
    }
}

impl DaemonContext {
    pub fn bootstrap(config_path: PathBuf, foreground: bool) -> Result<Self> {
        bootstrap_inner(config_path, foreground, None, None)
    }

    pub fn bootstrap_with_events(
        config_path: PathBuf,
        foreground: bool,
        bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
    ) -> Result<Self> {
        bootstrap_inner(config_path, foreground, bootstrap_events, None)
    }

    #[doc(hidden)]
    pub fn bootstrap_with_events_for_test(
        config_path: PathBuf,
        foreground: bool,
        bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
        runtime_root: &Path,
    ) -> Result<Self> {
        bootstrap_inner(
            config_path,
            foreground,
            bootstrap_events,
            Some(runtime_root),
        )
    }
}

fn bootstrap_inner(
    config_path: PathBuf,
    foreground: bool,
    bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
    runtime_root: Option<&Path>,
) -> Result<DaemonContext> {
    let bootstrap_config =
        Config::load_from(&config_path).context("failed to load daemon config")?;
    let db_path = expand_home_path(&bootstrap_config.db_path);
    let mempal_home = mempal_home_from_db(&db_path);
    fs::create_dir_all(&mempal_home)
        .with_context(|| format!("failed to create {}", mempal_home.display()))?;
    let daemon_db_env_path = daemon_db_env_path(&db_path)?;
    // SAFETY: daemon bootstrap is still single-threaded here; the Tokio
    // runtime, config watcher, database connection, and worker tasks have not
    // been created yet, so no concurrent environment access is introduced.
    unsafe {
        env::set_var("MEMPAL_DB_PATH", &daemon_db_env_path);
    }
    let log_path = expand_home_path(&bootstrap_config.daemon.log_path);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Atomic singleton gate (#496): acquire the DB-scoped exclusive daemon lock BEFORE
    // daemonizing. A race loser that cannot get the lock concludes a healthy
    // daemon already holds it and returns `DaemonAlreadyRunning` so the caller
    // can exit cleanly WITHOUT daemonizing. The lock's open file description
    // survives the double-fork in `perform_daemonize`, keeping the singleton
    // guarantee through the daemon's whole lifetime.
    let lock_acquisition = match runtime_root {
        Some(runtime_root) => crate::daemon_singleton::try_acquire_for_test(&db_path, runtime_root),
        None => crate::daemon_singleton::try_acquire(&db_path),
    }
    .context("failed to acquire daemon singleton lock")?;
    let mut lock_guard = match lock_acquisition {
        crate::daemon_singleton::DaemonLockAcquisition::Acquired(guard) => guard,
        crate::daemon_singleton::DaemonLockAcquisition::AlreadyHeld { owner, lock_path } => {
            return Err(anyhow::Error::new(
                crate::daemon_singleton::DaemonAlreadyRunning::new(owner, Some(lock_path)),
            ));
        }
    };

    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::Daemonize);
    perform_daemonize(foreground, &mempal_home, &log_path)?;
    lock_guard
        .refresh_metadata()
        .context("failed to refresh daemon singleton metadata")?;

    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::RuntimeInit);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build daemon runtime")?;

    // harness-point: PR0
    emit_bootstrap_event(
        bootstrap_events.as_ref(),
        BootstrapEvent::ConfigHandleBootstrap,
    );
    ConfigHandle::bootstrap(&config_path).context("failed to bootstrap config hot reload")?;
    let config = ConfigHandle::current();

    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::DbOpen);
    let (db, async_db, store) = open_daemon_storage_with_remediation(&db_path)?;
    let db = Arc::new(AsyncMutex::new(db));
    let write_observer = DaemonWriteObserver::new();

    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::TracingInit);
    init_tracing_subscriber();

    let pid_guard = PidFileGuard::create(mempal_home.join("daemon.pid"))?;
    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::Ready);

    Ok(DaemonContext {
        runtime,
        db,
        async_db,
        store,
        write_observer,
        config,
        mempal_home,
        log_path,
        _pid_guard: pid_guard,
        _lock_guard: lock_guard,
    })
}

fn open_daemon_storage_with_remediation(
    db_path: &Path,
) -> Result<(Database, AsyncDb, AsyncPendingMessageStore)> {
    match open_daemon_storage_once(db_path) {
        Ok(handles) => Ok(handles),
        Err(error) if is_sqlite_lock_error(&error) => {
            #[cfg(target_os = "linux")]
            {
                open_daemon_storage_after_stale_holder_cleanup(db_path, error)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn open_daemon_storage_once(
    db_path: &Path,
) -> Result<(Database, AsyncDb, AsyncPendingMessageStore)> {
    let db = Database::open(db_path).context("failed to open daemon database")?;
    let async_db = AsyncDb::open_for(
        db_path,
        RESOURCE_BOUNDED_READERS,
        crate::core::db_admission::DbHolderClass::Daemon,
    )
    .context("failed to open daemon async database")?;
    let store = AsyncPendingMessageStore::new(db.path()).context("failed to open pending queue")?;
    Ok((db, async_db, store))
}

fn is_sqlite_lock_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("database is locked")
            || message.contains("database file is locked")
            || message.contains("database is busy")
    })
}

#[cfg(target_os = "linux")]
fn open_daemon_storage_after_stale_holder_cleanup(
    db_path: &Path,
    first_error: anyhow::Error,
) -> Result<(Database, AsyncDb, AsyncPendingMessageStore)> {
    let before = inspect_db_holders_for_startup_remediation(db_path);
    let plan = crate::process_diagnostics::plan_stale_db_holder_remediation(&before);
    let mut remediation_errors = Vec::new();

    let mut termination_outcome = DbHolderTerminationOutcome::default();
    if !plan.terminate_targets.is_empty() {
        termination_outcome = terminate_db_holder_targets(
            db_path,
            &plan.terminate_targets,
            DB_HOLDER_TERM_GRACE,
            DB_HOLDER_TERM_POLL,
        );
        remediation_errors.extend(termination_outcome.errors.clone());
    }

    match open_daemon_storage_once(db_path) {
        Ok(handles) => Ok(handles),
        Err(error) if is_sqlite_lock_error(&error) => {
            let after = inspect_db_holders_for_startup_remediation(db_path);
            let terminated_pids = termination_outcome.signaled;
            Err(anyhow::anyhow!(
                "{}",
                crate::process_diagnostics::format_db_lock_remediation_hint(
                    db_path,
                    &format!(
                        "{}; initial error: {}",
                        format_error_chain(&error),
                        format_error_chain(&first_error)
                    ),
                    &after,
                    &terminated_pids,
                    &remediation_errors,
                )
            ))
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn format_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DbHolderTerminationOutcome {
    signaled: Vec<i32>,
    killed: Vec<i32>,
    errors: Vec<String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum DbHolderSignalAttempt {
    Signaled,
    IdentityMismatch(String),
    SignalFailed(String),
}

#[cfg(target_os = "linux")]
trait DbHolderProcessOps {
    fn inspect_holders(&mut self) -> DbHolderReport;
    fn signal(&mut self, pid: i32, signal: i32) -> Result<()>;
    fn is_running(&mut self, pid: i32) -> Result<bool>;
    fn sleep(&mut self, duration: Duration);
}

#[cfg(target_os = "linux")]
struct RealDbHolderProcessOps<'a> {
    db_path: &'a Path,
}

#[cfg(target_os = "linux")]
impl DbHolderProcessOps for RealDbHolderProcessOps<'_> {
    fn inspect_holders(&mut self) -> DbHolderReport {
        inspect_db_holders_for_startup_remediation(self.db_path)
    }

    fn signal(&mut self, pid: i32, signal: i32) -> Result<()> {
        signal_pid(pid, signal)
    }

    fn is_running(&mut self, pid: i32) -> Result<bool> {
        process_is_running(pid)
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(target_os = "linux")]
fn terminate_db_holder_targets(
    db_path: &Path,
    targets: &[DbHolderRemediationTarget],
    grace: Duration,
    poll: Duration,
) -> DbHolderTerminationOutcome {
    let mut ops = RealDbHolderProcessOps { db_path };
    terminate_db_holder_targets_with_ops(targets, &mut ops, grace, poll)
}

#[cfg(target_os = "linux")]
fn terminate_db_holder_targets_with_ops(
    targets: &[DbHolderRemediationTarget],
    ops: &mut impl DbHolderProcessOps,
    grace: Duration,
    poll: Duration,
) -> DbHolderTerminationOutcome {
    let mut outcome = DbHolderTerminationOutcome::default();
    let mut remaining = Vec::new();

    for target in targets {
        match signal_matching_db_holder(target, libc::SIGTERM, ops, "SIGTERM") {
            DbHolderSignalAttempt::Signaled => {
                outcome.signaled.push(target.pid);
                remaining.push(target.clone());
            }
            DbHolderSignalAttempt::IdentityMismatch(error) => {
                outcome.errors.push(error);
            }
            DbHolderSignalAttempt::SignalFailed(error) => {
                outcome.errors.push(error);
                remaining.push(target.clone());
            }
        }
    }

    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        let mut live = Vec::new();
        for target in std::mem::take(&mut remaining) {
            match ops.is_running(target.pid) {
                Ok(true) => live.push(target),
                Ok(false) => {}
                Err(error) => {
                    outcome
                        .errors
                        .push(format!("status pid {} failed: {error}", target.pid));
                }
            }
        }
        if live.is_empty() {
            break;
        }
        remaining = live;
        ops.sleep(poll);
    }

    for target in remaining {
        match ops.is_running(target.pid) {
            Ok(true) => match signal_matching_db_holder(&target, libc::SIGKILL, ops, "SIGKILL") {
                DbHolderSignalAttempt::Signaled => outcome.killed.push(target.pid),
                DbHolderSignalAttempt::IdentityMismatch(error)
                | DbHolderSignalAttempt::SignalFailed(error) => outcome.errors.push(error),
            },
            Ok(false) => {}
            Err(error) => outcome
                .errors
                .push(format!("status pid {} failed: {error}", target.pid)),
        }
    }

    outcome
}

#[cfg(target_os = "linux")]
fn signal_matching_db_holder(
    target: &DbHolderRemediationTarget,
    signal: i32,
    ops: &mut impl DbHolderProcessOps,
    stage: &str,
) -> DbHolderSignalAttempt {
    let report = ops.inspect_holders();
    if !report
        .holders
        .iter()
        .any(|holder| target.matches_holder(holder))
    {
        return DbHolderSignalAttempt::IdentityMismatch(format_identity_mismatch_error(
            target, &report, stage,
        ));
    }

    match ops.signal(target.pid, signal) {
        Ok(()) => DbHolderSignalAttempt::Signaled,
        Err(error) => DbHolderSignalAttempt::SignalFailed(format!(
            "{stage} pid {} failed: {error}",
            target.pid
        )),
    }
}

#[cfg(target_os = "linux")]
fn format_identity_mismatch_error(
    target: &DbHolderRemediationTarget,
    report: &DbHolderReport,
    stage: &str,
) -> String {
    let current = report
        .holders
        .iter()
        .find(|holder| holder.pid == target.pid)
        .map(describe_current_holder)
        .unwrap_or_else(|| "no current holder for this DB".to_string());
    format!(
        "{stage} pid {} skipped: planned DB holder identity no longer matches; expected {}; current {}; retry daemon startup or inspect with `mempal status --full` before manual cleanup",
        target.pid,
        target.describe(),
        current
    )
}

#[cfg(target_os = "linux")]
fn describe_current_holder(holder: &crate::process_diagnostics::DbHolderProcess) -> String {
    let started = holder
        .started_at_unix_secs
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "pid={} role={} classification={} started_at={} files={}",
        holder.pid,
        holder.role,
        holder.classification,
        started,
        holder.opened_files.join(",")
    )
}

#[cfg(target_os = "linux")]
fn signal_pid(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: the PID list comes from exact DB-holder classification for this
    // database. ESRCH means the process exited between planning and signal.
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error).with_context(|| format!("failed to signal pid {pid}"))
}

#[cfg(target_os = "linux")]
fn process_is_running(pid: i32) -> Result<bool> {
    // SAFETY: kill(pid, 0) performs a kernel liveness/permission check without
    // delivering a signal or dereferencing Rust memory.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(error).with_context(|| format!("failed to inspect pid {pid}")),
    }
}

fn unix_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(_) => 0,
    }
}

fn init_tracing_subscriber() {
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(not(test))]
fn perform_daemonize(foreground: bool, mempal_home: &Path, log_path: &Path) -> Result<()> {
    if foreground {
        return Ok(());
    }

    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    let daemonize = daemonize::Daemonize::new()
        .working_directory(mempal_home)
        .umask(0o027)
        .stdout(stdout)
        .stderr(stderr);

    match daemonize.execute() {
        daemonize::Outcome::Child(Ok(_)) => {}
        daemonize::Outcome::Child(Err(error)) => {
            return Err(error).context("failed to daemonize process");
        }
        daemonize::Outcome::Parent(Ok(parent)) => {
            if parent.first_child_exit_code == 0 {
                let pid_path = mempal_home.join("daemon.pid");
                if wait_for_daemon_pid_file(&pid_path, DAEMONIZE_READY_TIMEOUT) {
                    std::process::exit(0);
                }
                eprintln!(
                    "daemon did not report ready: {} was not written within {}s",
                    pid_path.display(),
                    DAEMONIZE_READY_TIMEOUT.as_secs()
                );
                std::process::exit(1);
            }
            std::process::exit(parent.first_child_exit_code);
        }
        daemonize::Outcome::Parent(Err(error)) => {
            return Err(error).context("failed to daemonize process");
        }
    }
    redirect_stdin_to_dev_null()?;
    Ok(())
}

#[cfg(not(test))]
fn wait_for_daemon_pid_file(pid_path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fs::read_to_string(pid_path)
            .ok()
            .and_then(|content| content.trim().parse::<i32>().ok())
            .is_some_and(|pid| pid > 0)
        {
            return true;
        }
        std::thread::sleep(DAEMONIZE_READY_POLL);
    }
    false
}

#[cfg(test)]
fn perform_daemonize(foreground: bool, _mempal_home: &Path, log_path: &Path) -> Result<()> {
    if foreground {
        return Ok(());
    }

    let stdin = OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .context("failed to open /dev/null for daemon stdin")?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    redirect_fd(stdin.as_raw_fd(), libc::STDIN_FILENO)?;
    redirect_fd(stdout.as_raw_fd(), libc::STDOUT_FILENO)?;
    redirect_fd(stderr.as_raw_fd(), libc::STDERR_FILENO)?;
    Ok(())
}

#[cfg(test)]
fn redirect_fd(source_fd: std::os::fd::RawFd, dest_fd: i32) -> Result<()> {
    // SAFETY: dup2 is called with valid file descriptors opened above.
    let rc = unsafe { libc::dup2(source_fd, dest_fd) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to dup2 fd {source_fd} -> {dest_fd}"));
    }
    Ok(())
}

fn mempal_home_from_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

fn daemon_db_env_path(db_path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = fs::canonicalize(db_path) {
        return Ok(canonical);
    }

    if db_path.is_absolute() {
        return Ok(db_path.to_path_buf());
    }

    let cwd = env::current_dir().context("failed to read current working directory")?;
    Ok(cwd.join(db_path))
}

fn emit_bootstrap_event(
    bootstrap_events: Option<&mpsc::Sender<BootstrapEvent>>,
    event: BootstrapEvent,
) {
    if let Some(tx) = bootstrap_events {
        let _ = tx.blocking_send(event);
    }
}

struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn create(path: PathBuf) -> Result<Self> {
        fs::write(&path, format!("{}", std::process::id()))
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
use std::os::fd::AsRawFd;

#[cfg(not(test))]
fn redirect_stdin_to_dev_null() -> Result<()> {
    use std::os::fd::AsRawFd;

    let stdin = OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .context("failed to open /dev/null for daemon stdin")?;
    // SAFETY: dup2 is called with valid file descriptors opened above.
    let rc = unsafe { libc::dup2(stdin.as_raw_fd(), libc::STDIN_FILENO) };
    if rc == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to redirect daemon stdin");
    }
    Ok(())
}

#[cfg(test)]
#[path = "daemon_bootstrap_tests.rs"]
mod tests;

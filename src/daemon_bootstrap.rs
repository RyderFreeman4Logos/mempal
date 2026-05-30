use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    config::{Config, ConfigHandle},
    db::Database,
    queue::PendingMessageStore,
};
use anyhow::{Context, Result};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

const DAEMON_STALL_SECONDS: u64 = 5 * 60;
const DAEMON_STALL_LOG_THROTTLE_SECONDS: u64 = 60;

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
    pub store: PendingMessageStore,
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

    pub fn maybe_log_stall(&self, store: &PendingMessageStore) {
        let now = unix_secs();
        let Some(diagnostic) = self.stall_diagnostic(store, now) else {
            return;
        };
        let last_log = self.inner.last_stall_log_secs.load(Ordering::Relaxed);
        if now.saturating_sub(last_log) < DAEMON_STALL_LOG_THROTTLE_SECONDS {
            return;
        }
        if self
            .inner
            .last_stall_log_secs
            .compare_exchange(last_log, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        tracing::error!(
            queued_count = diagnostic.queued_count,
            seconds_since_successful_write = diagnostic.seconds_since_successful_write,
            last_error = %diagnostic.last_error,
            uptime_secs = diagnostic.uptime_secs,
            "daemon write stall detected: queued messages exist but no successful write has completed for at least 5 minutes"
        );
    }

    fn stall_diagnostic(
        &self,
        store: &PendingMessageStore,
        now_secs: u64,
    ) -> Option<DaemonStallDiagnostic> {
        let stats = match store.stats() {
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
        bootstrap_inner(config_path, foreground, None)
    }

    pub fn bootstrap_with_events(
        config_path: PathBuf,
        foreground: bool,
        bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
    ) -> Result<Self> {
        bootstrap_inner(config_path, foreground, bootstrap_events)
    }
}

fn bootstrap_inner(
    config_path: PathBuf,
    foreground: bool,
    bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
) -> Result<DaemonContext> {
    let bootstrap_config =
        Config::load_from(&config_path).context("failed to load daemon config")?;
    let db_path = expand_home_path(&bootstrap_config.db_path);
    let mempal_home = mempal_home_from_db(&db_path);
    fs::create_dir_all(&mempal_home)
        .with_context(|| format!("failed to create {}", mempal_home.display()))?;
    let log_path = expand_home_path(&bootstrap_config.daemon.log_path);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    // Atomic singleton gate (#257): acquire the exclusive daemon lock BEFORE
    // daemonizing. A race loser that cannot get the lock concludes a healthy
    // daemon already holds it and returns `DaemonAlreadyRunning` so the caller
    // can exit cleanly WITHOUT daemonizing. The lock's open file description
    // survives the double-fork in `perform_daemonize`, keeping the singleton
    // guarantee through the daemon's whole lifetime.
    let lock_guard = match crate::daemon_singleton::try_acquire(&mempal_home)
        .context("failed to acquire daemon singleton lock")?
    {
        crate::daemon_singleton::DaemonLockAcquisition::Acquired(guard) => guard,
        crate::daemon_singleton::DaemonLockAcquisition::AlreadyHeld => {
            return Err(anyhow::Error::new(
                crate::daemon_singleton::DaemonAlreadyRunning,
            ));
        }
    };

    // harness-point: PR0
    emit_bootstrap_event(bootstrap_events.as_ref(), BootstrapEvent::Daemonize);
    perform_daemonize(foreground, &mempal_home, &log_path)?;

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
    let db = Database::open(&db_path).context("failed to open daemon database")?;
    let store = PendingMessageStore::new(db.path()).context("failed to open pending queue")?;
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
        store,
        write_observer,
        config,
        mempal_home,
        log_path,
        _pid_guard: pid_guard,
        _lock_guard: lock_guard,
    })
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
    daemonize.start().context("failed to daemonize process")?;
    redirect_stdin_to_dev_null()?;
    Ok(())
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
mod tests {
    use super::*;

    #[test]
    fn write_observer_reports_stall_when_queue_has_work_and_no_recent_writes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        store
            .enqueue("hook:user-prompt-submit", "{}")
            .expect("enqueue pending message");

        let observer = DaemonWriteObserver::new();
        let now = unix_secs();
        observer.force_last_successful_write_for_test(now.saturating_sub(DAEMON_STALL_SECONDS));
        observer.record_error("failed to merge drawer");

        let diagnostic = observer
            .stall_diagnostic(&store, now)
            .expect("stall diagnostic");
        assert_eq!(diagnostic.queued_count, 1);
        assert!(diagnostic.seconds_since_successful_write >= DAEMON_STALL_SECONDS);
        assert_eq!(diagnostic.last_error, "failed to merge drawer");
    }

    #[test]
    fn write_observer_ignores_empty_queue() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");

        let observer = DaemonWriteObserver::new();
        let now = unix_secs();
        observer.force_last_successful_write_for_test(now.saturating_sub(DAEMON_STALL_SECONDS));

        assert_eq!(observer.stall_diagnostic(&store, now), None);
    }
}

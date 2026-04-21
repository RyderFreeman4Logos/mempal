use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    config::{Config, ConfigHandle},
    db::Database,
    queue::PendingMessageStore,
};
use anyhow::{Context, Result};
use tokio::sync::mpsc;

pub struct DaemonContext {
    pub runtime: tokio::runtime::Runtime,
    pub db: Database,
    pub store: PendingMessageStore,
    pub config: std::sync::Arc<crate::core::config::Config>,
    pub mempal_home: PathBuf,
    pub log_path: PathBuf,
    _pid_guard: PidFileGuard,
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
        config,
        mempal_home,
        log_path,
        _pid_guard: pid_guard,
    })
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

//! Synchronous child-process supervision for integration tests.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DIAGNOSTIC_TAIL_BYTES: u64 = 16 * 1024;

/// Owns a test child until it has exited and been reaped.
///
/// Standard output and error are captured in files so long-running children
/// cannot block on full pipes. [`Drop`] kills and reaps a still-running child,
/// then reports the captured tails when cleanup was needed or the test panics.
pub struct CapturedChild {
    child: Child,
    label: String,
    pid: u32,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    daemon_log_path: Option<PathBuf>,
    started_at: Instant,
}

impl CapturedChild {
    /// Spawns `command` with stdout and stderr redirected to diagnostic files.
    ///
    /// `label` should be unique within `diagnostics_dir` while children overlap.
    pub fn spawn(
        command: &mut Command,
        diagnostics_dir: &Path,
        label: &str,
        daemon_log_path: Option<PathBuf>,
    ) -> io::Result<Self> {
        fs::create_dir_all(diagnostics_dir)?;
        let file_stem = diagnostic_file_stem(label);
        let stdout_path = diagnostics_dir.join(format!("{file_stem}.stdout.log"));
        let stderr_path = diagnostics_dir.join(format!("{file_stem}.stderr.log"));
        let stdout = File::create(&stdout_path)?;
        let stderr = File::create(&stderr_path)?;
        command.stdout(Stdio::from(stdout));
        command.stderr(Stdio::from(stderr));

        let child = command.spawn()?;
        let pid = child.id();
        Ok(Self {
            child,
            label: label.to_owned(),
            pid,
            stdout_path,
            stderr_path,
            daemon_log_path,
            started_at: Instant::now(),
        })
    }

    /// Returns the PID assigned to the owned child.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Polls and reaps the child if it has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Waits until captured stderr contains `event` or panics with diagnostics.
    pub fn wait_for_stderr_event(&mut self, event: &str, timeout: Duration) {
        assert!(!event.is_empty(), "stderr event must not be empty");
        let deadline = Instant::now() + timeout;
        loop {
            let stderr = fs::read(&self.stderr_path).unwrap_or_else(|error| {
                panic!(
                    "failed to read stderr while waiting for `{event}`: {error}\n{}",
                    self.diagnostics()
                )
            });
            if stderr
                .windows(event.len())
                .any(|window| window == event.as_bytes())
            {
                return;
            }
            match self.try_wait() {
                Ok(Some(status)) => panic!(
                    "child exited before stderr event `{event}`: {status}\n{}",
                    self.diagnostics()
                ),
                Ok(None) => {}
                Err(error) => panic!(
                    "failed to poll child while waiting for stderr event `{event}`: {error}\n{}",
                    self.diagnostics()
                ),
            }
            assert!(
                Instant::now() < deadline,
                "child did not emit stderr event `{event}` within {timeout:?}\n{}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits until the child exits and reaps it.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Waits for the child or panics with captured diagnostics.
    pub fn wait_or_panic(&mut self, action: &str) -> ExitStatus {
        match self.wait() {
            Ok(status) => status,
            Err(error) => panic!("{action}: {error}\n{}", self.diagnostics()),
        }
    }

    /// Polls for the child until `timeout`, killing and reaping on expiry.
    pub fn wait_or_panic_with_timeout(&mut self, action: &str, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() >= deadline => {
                    let diagnostics = self.diagnostics();
                    let kill = self.kill();
                    let wait = self.wait();
                    panic!(
                        "{action}: child did not exit within {timeout:?}; kill={kill:?}, wait={wait:?}\n{diagnostics}"
                    );
                }
                Ok(None) => {}
                Err(error) => panic!(
                    "{action}: failed to poll child: {error}\n{}",
                    self.diagnostics()
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Forces the child to exit with the platform's kill operation.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    #[cfg(unix)]
    /// Sends a Unix signal to the owned, unreaped child PID.
    pub fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: the PID comes from the child handle owned by this guard.
        if unsafe { libc::kill(self.pid as i32, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    /// Sends a Unix signal or panics with captured diagnostics.
    pub fn signal_or_panic(&self, signal: libc::c_int, action: &str) {
        self.signal(signal)
            .unwrap_or_else(|error| panic!("{action}: {error}\n{}", self.diagnostics()));
    }

    /// Formats the PID, elapsed time, captured streams, and daemon log tail.
    pub fn diagnostics(&self) -> String {
        let daemon_log = self.daemon_log_path.as_ref().map_or_else(
            || "<not configured>".to_owned(),
            |path| read_tail_for_diagnostics(path),
        );
        format!(
            "child={}, pid={}, elapsed={:?}\nstdout tail:\n{}\nstderr tail:\n{}\ndaemon.log tail:\n{}",
            self.label,
            self.pid,
            self.started_at.elapsed(),
            read_tail_for_diagnostics(&self.stdout_path),
            read_tail_for_diagnostics(&self.stderr_path),
            daemon_log,
        )
    }
}

impl Drop for CapturedChild {
    fn drop(&mut self) {
        let cleanup = match self.child.try_wait() {
            Ok(Some(_)) => None,
            Ok(None) => {
                let kill = self.child.kill();
                let wait = self.child.wait();
                Some(format!("drop cleanup: kill={kill:?}, wait={wait:?}"))
            }
            Err(poll_error) => {
                let kill = self.child.kill();
                let wait = self.child.wait();
                Some(format!(
                    "drop cleanup: poll={poll_error:?}, kill={kill:?}, wait={wait:?}"
                ))
            }
        };

        if std::thread::panicking() || cleanup.is_some() {
            let cleanup = cleanup
                .as_deref()
                .unwrap_or("drop cleanup: child already reaped");
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "{cleanup}\n{}", self.diagnostics());
        }
    }
}

/// Holds an exclusive SQLite lock for a deterministic startup delay.
pub fn hold_sqlite_lock_for(db_path: &Path, duration: Duration) -> std::thread::JoinHandle<()> {
    let connection = rusqlite::Connection::open(db_path).expect("open startup delay lock");
    connection
        .execute_batch("BEGIN EXCLUSIVE;")
        .expect("acquire startup delay lock");
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        drop(connection);
    })
}

/// Reads at most the trailing 16 KiB from `path` for failure reporting.
pub fn read_tail_for_diagnostics(path: &Path) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return format!("<{} unavailable: {error}>", path.display()),
    };
    let len = file.metadata().map_or(0, |metadata| metadata.len());
    let start = len.saturating_sub(DIAGNOSTIC_TAIL_BYTES);
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return format!("<{} seek failed: {error}>", path.display());
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return format!("<{} read failed: {error}>", path.display());
    }
    let tail = String::from_utf8_lossy(&bytes);
    if start == 0 {
        tail.into_owned()
    } else {
        format!("<truncated first {start} bytes>\n{tail}")
    }
}

/// Reports detached daemon PIDs and log output owned by a temporary test home.
pub fn write_daemon_home_diagnostics(db_path: &Path) {
    let mempal_home = db_path.parent().unwrap_or(db_path);
    let pid_path = mempal_home.join("daemon.pid");
    let pidfile =
        fs::read_to_string(&pid_path).unwrap_or_else(|error| format!("<unavailable: {error}>"));
    let discovered_pids = mempal::daemon_singleton::enumerate_daemon_pids("mempal", db_path);
    let daemon_log = read_tail_for_diagnostics(&mempal_home.join("daemon.log"));
    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "daemon home diagnostics: pidfile={pidfile:?}, discovered_pids={discovered_pids:?}\ndaemon.log tail:\n{daemon_log}"
    );
}

fn diagnostic_file_stem(label: &str) -> String {
    label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

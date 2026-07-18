use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

use filetime::{FileTime, set_file_mtime};
use mempal::core::{config::Config, db::Database, queue::PendingMessageStore};
use mempal::hook::HOOK_SPOOL_DIR;
use mempal::hook_payload::prune_hook_payloads;
use serde_json::json;

const DAEMON_STARTUP_DEADLINE: Duration = Duration::from_secs(10);
const DAEMON_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(25);
const DIAGNOSTIC_TAIL_BYTES: u64 = 8 * 1024;

struct OwnedForegroundDaemon {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    daemon_log_path: PathBuf,
    started_at: Instant,
}

impl OwnedForegroundDaemon {
    fn spawn(home: &Path, mempal_home: &Path, daemon_log_path: PathBuf) -> std::io::Result<Self> {
        let stdout_path = mempal_home.join("payload-retention-daemon.stdout.log");
        let stderr_path = mempal_home.join("payload-retention-daemon.stderr.log");
        let stdout = File::create(&stdout_path)?;
        let stderr = File::create(&stderr_path)?;
        let child = Command::new(env!("CARGO_BIN_EXE_mempal"))
            .args(["daemon", "--foreground"])
            .env("HOME", home)
            .env(
                mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
                mempal_home.join("runtime"),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()?;

        Ok(Self {
            child,
            stdout_path,
            stderr_path,
            daemon_log_path,
            started_at: Instant::now(),
        })
    }

    fn wait_for_retention(&mut self, expired: &Path, within_budget: &Path) -> Result<(), String> {
        let phase_started = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    return Err(format!(
                        "foreground daemon exited before startup retention was observed: {status}\n{}",
                        self.diagnostics(
                            "hook payload retention startup",
                            phase_started,
                            DAEMON_STARTUP_DEADLINE,
                        )
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(format!(
                        "failed to poll foreground daemon during startup: {error}\n{}",
                        self.diagnostics(
                            "hook payload retention startup",
                            phase_started,
                            DAEMON_STARTUP_DEADLINE,
                        )
                    ));
                }
            }

            let output = self.output_text();
            let expired_removed = !expired.exists();
            let within_budget_preserved = within_budget.exists();
            let retention_diagnostics = output.contains("hook payload retention")
                && output.contains("scanned_files=2")
                && output.contains("deleted_files=1");
            // This message is emitted only after the production signal handlers
            // are installed, so SIGTERM below exercises the real shutdown path.
            let signal_handler_ready = output.contains("daemon log path");
            if expired_removed
                && within_budget_preserved
                && retention_diagnostics
                && signal_handler_ready
            {
                return Ok(());
            }

            if phase_started.elapsed() >= DAEMON_STARTUP_DEADLINE {
                return Err(format!(
                    "startup retention deadline expired: expired_removed={expired_removed}, within_budget_preserved={within_budget_preserved}, retention_diagnostics={retention_diagnostics}, signal_handler_ready={signal_handler_ready}\n{}",
                    self.diagnostics(
                        "hook payload retention startup",
                        phase_started,
                        DAEMON_STARTUP_DEADLINE,
                    )
                ));
            }
            std::thread::sleep(
                DAEMON_POLL_INTERVAL
                    .min(DAEMON_STARTUP_DEADLINE.saturating_sub(phase_started.elapsed())),
            );
        }
    }

    fn shutdown(&mut self) -> Result<ExitStatus, String> {
        let phase_started = Instant::now();
        match self.child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "foreground daemon exited before SIGTERM: {status}\n{}",
                    self.diagnostics(
                        "hook payload retention shutdown",
                        phase_started,
                        DAEMON_SHUTDOWN_DEADLINE,
                    )
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let diagnostics = self.diagnostics(
                    "hook payload retention shutdown",
                    phase_started,
                    DAEMON_SHUTDOWN_DEADLINE,
                );
                let cleanup = self.kill_and_reap();
                return Err(format!(
                    "failed to poll foreground daemon before SIGTERM: {error}; {cleanup}\n{diagnostics}"
                ));
            }
        }

        let pid = i32::try_from(self.child.id()).map_err(|error| {
            let diagnostics = self.diagnostics(
                "hook payload retention shutdown",
                phase_started,
                DAEMON_SHUTDOWN_DEADLINE,
            );
            let cleanup = self.kill_and_reap();
            format!("foreground daemon PID conversion failed: {error}; {cleanup}\n{diagnostics}")
        })?;
        // SAFETY: the unreaped direct child is still owned and `try_wait` above
        // proved it running, so its PID identity cannot be reused before SIGTERM.
        if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
            let error = std::io::Error::last_os_error();
            let diagnostics = self.diagnostics(
                "hook payload retention shutdown",
                phase_started,
                DAEMON_SHUTDOWN_DEADLINE,
            );
            let cleanup = self.kill_and_reap();
            return Err(format!(
                "failed to send SIGTERM to foreground daemon: {error}; {cleanup}\n{diagnostics}"
            ));
        }

        loop {
            match self.child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(status),
                Ok(Some(status)) => {
                    return Err(format!(
                        "foreground daemon exited unsuccessfully after SIGTERM: {status}\n{}",
                        self.diagnostics(
                            "hook payload retention shutdown",
                            phase_started,
                            DAEMON_SHUTDOWN_DEADLINE,
                        )
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    let diagnostics = self.diagnostics(
                        "hook payload retention shutdown",
                        phase_started,
                        DAEMON_SHUTDOWN_DEADLINE,
                    );
                    let cleanup = self.kill_and_reap();
                    return Err(format!(
                        "failed to poll foreground daemon after SIGTERM: {error}; {cleanup}\n{diagnostics}"
                    ));
                }
            }

            if phase_started.elapsed() >= DAEMON_SHUTDOWN_DEADLINE {
                let diagnostics = self.diagnostics(
                    "hook payload retention shutdown",
                    phase_started,
                    DAEMON_SHUTDOWN_DEADLINE,
                );
                let cleanup = self.kill_and_reap();
                return Err(format!(
                    "foreground daemon exceeded its SIGTERM deadline; {cleanup}\n{diagnostics}"
                ));
            }
            std::thread::sleep(
                DAEMON_POLL_INTERVAL
                    .min(DAEMON_SHUTDOWN_DEADLINE.saturating_sub(phase_started.elapsed())),
            );
        }
    }

    fn diagnostics(&self, role: &str, phase_started: Instant, deadline: Duration) -> String {
        format!(
            "command_role={role}\nphase_elapsed={:?}\nphase_deadline={deadline:?}\nprocess_elapsed={:?}\n{}",
            phase_started.elapsed(),
            self.started_at.elapsed(),
            self.output_text(),
        )
    }

    fn output_text(&self) -> String {
        format!(
            "stdout (last {DIAGNOSTIC_TAIL_BYTES} bytes):\n{}\nstderr (last {DIAGNOSTIC_TAIL_BYTES} bytes):\n{}\ndaemon log (last {DIAGNOSTIC_TAIL_BYTES} bytes):\n{}",
            read_bounded_tail(&self.stdout_path),
            read_bounded_tail(&self.stderr_path),
            read_bounded_tail(&self.daemon_log_path),
        )
    }

    fn kill_and_reap(&mut self) -> String {
        let kill = self.child.kill().map_or_else(
            |error| format!("kill failed: {error}"),
            |()| "kill sent".into(),
        );
        let reap = self.child.wait().map_or_else(
            |error| format!("final reap failed: {error}"),
            |status| format!("final reap status: {status}"),
        );
        format!("{kill}; {reap}")
    }
}

impl Drop for OwnedForegroundDaemon {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_bounded_tail(path: &Path) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return format!("<unavailable: {error}>"),
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let start = length.saturating_sub(DIAGNOSTIC_TAIL_BYTES);
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return format!("<seek failed: {error}>");
    }
    let mut bytes = Vec::with_capacity(DIAGNOSTIC_TAIL_BYTES as usize);
    if let Err(error) = file.take(DIAGNOSTIC_TAIL_BYTES).read_to_end(&mut bytes) {
        return format!("<read failed: {error}>");
    }
    let text = String::from_utf8_lossy(&bytes);
    if start == 0 {
        text.into_owned()
    } else {
        format!("<truncated first {start} bytes>\n{text}")
    }
}

fn write_old_payload(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write spool payload");
    set_file_mtime(path, FileTime::from_unix_time(0, 0)).expect("age spool payload");
}

fn enqueue_payload_handle(store: &PendingMessageStore, path: &Path) {
    let envelope = json!({
        "event": "PostToolUse",
        "kind": "hook_post_tool",
        "agent": "claude",
        "captured_at": "2026-07-15T00:00:00Z",
        "claude_cwd": "/tmp/project",
        "payload": null,
        "payload_path": path.display().to_string(),
        "payload_preview": null,
        "original_size_bytes": 70_000,
        "truncated": false
    });
    store
        .enqueue("hook_post_tool", &envelope.to_string())
        .expect("enqueue payload handle");
}

#[test]
fn hook_payload_retention_prunes_only_old_unreferenced_files() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("initialize database");
    let store = PendingMessageStore::new(&db_path).expect("open queue");
    let spool = mempal_home.join(HOOK_SPOOL_DIR);
    std::fs::create_dir_all(&spool).expect("create spool");

    let claimed = spool.join("claimed.json");
    let pending = spool.join("pending.json");
    let orphan = spool.join("orphan.json");
    let young = spool.join("young.json");
    for path in [&claimed, &pending, &orphan] {
        write_old_payload(path, "old raw payload");
    }
    std::fs::write(&young, "young raw payload").expect("write young payload");

    enqueue_payload_handle(&store, &claimed);
    enqueue_payload_handle(&store, &pending);
    store
        .claim_next("retention-test", 120)
        .expect("claim queue row")
        .expect("claimed queue row");

    let outcome = prune_hook_payloads(&mempal_home, &db_path, 7).expect("prune hook payloads");

    assert_eq!(outcome.scanned_files, 4);
    assert_eq!(outcome.deleted_files, 1);
    assert_eq!(outcome.referenced_files, 2);
    assert_eq!(outcome.young_files, 1);
    assert!(claimed.exists(), "claimed queue payload must be retained");
    assert!(pending.exists(), "pending queue payload must be retained");
    assert!(!orphan.exists(), "old orphan payload should be pruned");
    assert!(young.exists(), "young orphan payload must be retained");
}

#[test]
fn hooks_payload_retention_days_defaults_to_seven_and_is_configurable() {
    let defaults = Config::parse("[hooks]\nenabled = true\n").expect("parse default retention");
    assert_eq!(defaults.hooks.payload_retention_days, 7);

    let configured = Config::parse("[hooks]\npayload_retention_days = 30\n")
        .expect("parse configured retention");
    assert_eq!(configured.hooks.payload_retention_days, 30);

    let error = Config::parse("[hooks]\npayload_retention_days = 0\n")
        .expect_err("zero-day retention must be rejected");
    assert!(error.to_string().contains("payload_retention_days"));
}

#[test]
fn daemon_startup_runs_hook_payload_retention_with_configured_age_budget() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    let spool = mempal_home.join(HOOK_SPOOL_DIR);
    let log_path = mempal_home.join("daemon.log");
    std::fs::create_dir_all(&spool).expect("create spool");
    Database::open(&db_path).expect("initialize database");

    let expired = spool.join("expired.json");
    let within_budget = spool.join("within-budget.json");
    write_old_payload(&expired, "expired raw payload");
    std::fs::write(&within_budget, "retained raw payload").expect("write retained payload");
    set_file_mtime(
        &within_budget,
        FileTime::from_system_time(SystemTime::now() - Duration::from_secs(8 * 86_400)),
    )
    .expect("age retained payload");

    std::fs::write(
        mempal_home.join("config.toml"),
        format!(
            "db_path = \"{}\"\n\n[hooks]\nenabled = false\npayload_retention_days = 30\n\n[daemon]\nlog_path = \"{}\"\n",
            db_path.display(),
            log_path.display()
        ),
    )
    .expect("write daemon config");

    let mut daemon = OwnedForegroundDaemon::spawn(tmp.path(), &mempal_home, log_path.clone())
        .expect("spawn foreground daemon");
    let startup_result = daemon.wait_for_retention(&expired, &within_budget);
    let shutdown_result = daemon.shutdown();
    let diagnostics = daemon.output_text();

    assert!(
        startup_result.is_ok() && shutdown_result.is_ok(),
        "foreground daemon lifecycle failed:\nstartup={startup_result:?}\nshutdown={shutdown_result:?}\n{diagnostics}"
    );
    assert!(
        shutdown_result
            .expect("shutdown result checked above")
            .success(),
        "foreground daemon must shut down successfully after SIGTERM\n{diagnostics}"
    );

    assert!(
        !expired.exists(),
        "daemon startup must prune expired payloads"
    );
    assert!(
        within_budget.exists(),
        "daemon startup must honor configured retention days"
    );
    assert!(diagnostics.contains("hook payload retention"));
    assert!(diagnostics.contains("scanned_files=2"));
    assert!(diagnostics.contains("deleted_files=1"));
}

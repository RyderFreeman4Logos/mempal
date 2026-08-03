#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use mempal::core::db::Database;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn daemon_runtime_dir(home: &Path) -> PathBuf {
    home.join(".mempal/runtime")
}

trait DaemonCommandExt {
    fn daemon_home(&mut self, home: &Path) -> &mut Self;
}

impl DaemonCommandExt for Command {
    fn daemon_home(&mut self, home: &Path) -> &mut Self {
        self.env("HOME", home).env(
            mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
            daemon_runtime_dir(home),
        )
    }
}

fn command_output_with_timeout(command: &mut Command, timeout: Duration, label: &str) -> Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));
    wait_child_output_with_timeout(child, timeout, label)
}

fn wait_child_output_with_timeout(mut child: Child, timeout: Duration, label: &str) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collect {label} output: {error}"));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collect timed-out {label} output: {error}"));
                panic!(
                    "{label} did not exit within {timeout:?}; stdout={}, stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("poll {label}: {error}");
            }
        }
    }
}

fn setup_daemon_home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new_in("/tmp").expect("short tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[embedder]
backend = "stub"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            mempal_home.join("daemon.log").display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn registered_process_birth(home: &Path) -> (i32, u64) {
    let pid = fs::read_to_string(home.join(".mempal/daemon.pid"))
        .expect("read registered daemon pid")
        .trim()
        .parse::<i32>()
        .expect("parse registered daemon pid");
    let stat = fs::read(format!("/proc/{pid}/stat")).expect("read daemon process stat");
    let command_end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .expect("daemon process stat command terminator");
    let fields = std::str::from_utf8(&stat[command_end + 2..]).expect("daemon process stat utf8");
    let start_time_ticks = fields
        .split_whitespace()
        .nth(19)
        .expect("daemon process start time")
        .parse::<u64>()
        .expect("parse daemon process start time");
    (pid, start_time_ticks)
}

fn wait_for_daemon(home: &Path) -> Output {
    let mut command = Command::new(mempal_bin());
    command
        .args(["daemon", "wait", "--timeout-secs", "10"])
        .daemon_home(home)
        .stdin(Stdio::null());
    command_output_with_timeout(&mut command, Duration::from_secs(15), "daemon readiness")
}

struct DaemonCleanup {
    db_path: PathBuf,
}

impl Drop for DaemonCleanup {
    fn drop(&mut self) {
        let binary = "mempal".to_string();
        for daemon in mempal::daemon_singleton::enumerate_daemon_processes(&binary, &self.db_path) {
            if daemon.is_current() {
                // SAFETY: discovery is scoped to this test's temporary database,
                // and the process birth is revalidated immediately before signal.
                let _ = unsafe { libc::kill(daemon.pid, libc::SIGKILL) };
            }
        }
    }
}

#[test]
fn test_daemon_wait_after_restart_requires_status_and_write_transport_readiness() {
    let (tmp, db_path) = setup_daemon_home();
    let _cleanup = DaemonCleanup {
        db_path: db_path.clone(),
    };

    let mut start_command = Command::new(mempal_bin());
    start_command
        .args(["daemon", "start"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null());
    let start =
        command_output_with_timeout(&mut start_command, Duration::from_secs(15), "daemon start");
    assert!(
        start.status.success(),
        "daemon start failed: status={:?}, stdout={}, stderr={}",
        start.status,
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    let initial_wait = wait_for_daemon(tmp.path());
    assert!(
        initial_wait.status.success(),
        "initial daemon wait failed: status={:?}, stdout={}, stderr={}",
        initial_wait.status,
        String::from_utf8_lossy(&initial_wait.stdout),
        String::from_utf8_lossy(&initial_wait.stderr)
    );
    let initial_process_birth = registered_process_birth(tmp.path());

    let mut restart_command = Command::new(mempal_bin());
    restart_command
        .args(["daemon", "restart"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null());
    let restart = command_output_with_timeout(
        &mut restart_command,
        Duration::from_secs(15),
        "daemon restart",
    );
    assert!(
        restart.status.success(),
        "daemon restart failed: status={:?}, stdout={}, stderr={}",
        restart.status,
        String::from_utf8_lossy(&restart.stdout),
        String::from_utf8_lossy(&restart.stderr)
    );

    let wait = wait_for_daemon(tmp.path());
    assert!(
        wait.status.success(),
        "daemon wait failed: status={:?}, stdout={}, stderr={}",
        wait.status,
        String::from_utf8_lossy(&wait.stdout),
        String::from_utf8_lossy(&wait.stderr)
    );
    let restarted_process_birth = registered_process_birth(tmp.path());
    assert_ne!(
        restarted_process_birth, initial_process_birth,
        "restart must replace the registered daemon process birth"
    );
    let stdout = String::from_utf8_lossy(&wait.stdout);
    assert!(stdout.contains("daemon ready"), "{stdout}");
    assert!(
        tmp.path().join(".mempal/daemon-hook.sock").exists(),
        "readiness must include the daemon write transport"
    );

    let mut status_command = Command::new(mempal_bin());
    status_command
        .args(["daemon", "status"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null());
    let status = command_output_with_timeout(
        &mut status_command,
        Duration::from_secs(10),
        "daemon status",
    );
    assert!(status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("status: running"),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[test]
fn test_daemon_wait_timeout_is_explicit_and_redacted() {
    let (tmp, db_path) = setup_daemon_home();

    let mut wait_command = Command::new(mempal_bin());
    wait_command
        .args(["daemon", "wait", "--timeout-secs", "1"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null());
    let output = command_output_with_timeout(
        &mut wait_command,
        Duration::from_secs(5),
        "daemon readiness timeout",
    );

    assert!(!output.status.success(), "daemon wait should time out");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon readiness timed out after 1s"),
        "{stderr}"
    );
    assert!(
        stderr.contains("singleton daemon not registered"),
        "{stderr}"
    );
    assert!(!stderr.contains(&db_path.display().to_string()), "{stderr}");
    assert!(
        !stderr.contains(&tmp.path().display().to_string()),
        "{stderr}"
    );
}

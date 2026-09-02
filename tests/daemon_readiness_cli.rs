#![cfg(target_os = "linux")]

#[path = "common/harness/cli_deadline.rs"]
mod cli_deadline;

const _: fn() = cli_deadline::reference_shared_cli_deadline_api;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Mutex;
use std::time::Duration;

use cli_deadline::{push_args, run_cli_output, with_home};
use mempal::core::db::Database;
use tempfile::TempDir;

// ponytail: one daemon-readiness test-binary lock; split by fixture family if throughput matters.
static DAEMON_READINESS_TEST_LOCK: Mutex<()> = Mutex::new(());

fn daemon_runtime_dir(home: &Path) -> PathBuf {
    home.join(".mempal/runtime")
}

fn run_daemon(home: &Path, args: &[&str], role: &'static str, timeout: Duration) -> Output {
    run_cli_output(
        role,
        |spec| {
            with_home(spec, home);
            spec.env(
                mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
                daemon_runtime_dir(home).into_os_string(),
            );
            push_args(spec, args.iter().copied());
        },
        timeout,
    )
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

[api]
addr = "127.0.0.1:0"

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
    run_daemon(
        home,
        &["daemon", "wait", "--timeout-secs", "10"],
        "daemon readiness",
        Duration::from_secs(15),
    )
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
    let _test_lock = DAEMON_READINESS_TEST_LOCK
        .lock()
        .expect("daemon readiness test lock");
    let (tmp, db_path) = setup_daemon_home();
    let _cleanup = DaemonCleanup {
        db_path: db_path.clone(),
    };

    let start = run_daemon(
        tmp.path(),
        &["daemon", "start"],
        "daemon start",
        Duration::from_secs(15),
    );
    assert!(
        start.status.success(),
        "daemon start failed: status={:?}",
        start.status
    );
    let initial_wait = wait_for_daemon(tmp.path());
    assert!(
        initial_wait.status.success(),
        "initial daemon wait failed: status={:?}",
        initial_wait.status
    );
    let initial_process_birth = registered_process_birth(tmp.path());

    let restart = run_daemon(
        tmp.path(),
        &["daemon", "restart"],
        "daemon restart",
        Duration::from_secs(15),
    );
    assert!(
        restart.status.success(),
        "daemon restart failed: status={:?}",
        restart.status
    );

    let wait = wait_for_daemon(tmp.path());
    assert!(
        wait.status.success(),
        "daemon wait failed: status={:?}",
        wait.status
    );
    let restarted_process_birth = registered_process_birth(tmp.path());
    assert_ne!(
        restarted_process_birth, initial_process_birth,
        "restart must replace the registered daemon process birth"
    );
    assert!(String::from_utf8_lossy(&wait.stdout).contains("daemon ready"));
    assert!(
        tmp.path().join(".mempal/daemon-hook.sock").exists(),
        "readiness must include the daemon write transport"
    );

    let status = run_daemon(
        tmp.path(),
        &["daemon", "status"],
        "daemon status",
        Duration::from_secs(10),
    );
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("status: running"));
}

#[test]
fn test_daemon_wait_timeout_is_explicit_and_redacted() {
    let _test_lock = DAEMON_READINESS_TEST_LOCK
        .lock()
        .expect("daemon readiness test lock");
    let (tmp, db_path) = setup_daemon_home();

    let output = run_daemon(
        tmp.path(),
        &["daemon", "wait", "--timeout-secs", "1"],
        "daemon readiness timeout",
        Duration::from_secs(5),
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

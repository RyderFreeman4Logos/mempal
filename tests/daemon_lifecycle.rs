use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mempal::bootstrap_events::BootstrapEvent;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::daemon_bootstrap::DaemonContext;
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mockito::Server;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_daemon_home() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    let config_path = mempal_home.join("config.toml");
    fs::write(
        &config_path,
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
    (tmp, db_path, config_path)
}

#[cfg(unix)]
struct DaemonHomeCleanup {
    db_path: PathBuf,
}

#[cfg(unix)]
impl Drop for DaemonHomeCleanup {
    fn drop(&mut self) {
        let pid_path = self
            .db_path
            .parent()
            .unwrap_or(self.db_path.as_path())
            .join("daemon.pid");
        if let Ok(content) = fs::read_to_string(&pid_path)
            && let Ok(pid) = content.trim().parse::<i32>()
        {
            // SAFETY: this test owns the temporary mempal_home being cleaned;
            // the signal is restricted to a process tracked by the test's pidfile.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        for pid in mempal::daemon_singleton::enumerate_daemon_pids("mempal", &self.db_path) {
            // SAFETY: this test owns the temporary mempal_home being enumerated;
            // the signal is restricted to processes attributed to that exact db.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[cfg(unix)]
fn process_is_running_for_test(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 probes liveness without delivering a
    // signal. EPERM still means a process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn wait_for_pid_file(pid_path: &std::path::Path, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(content) = fs::read_to_string(pid_path)
            && let Ok(pid) = content.trim().parse::<i32>()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[cfg(unix)]
fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait().expect("poll child exit").is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn spawn_db_backed_orphan_daemon(home: &std::path::Path, db_path: &std::path::Path) -> Child {
    let mempal_home = db_path.parent().expect("db parent");
    let pid_path = mempal_home.join("daemon.pid");
    let mut child = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn foreground daemon");
    let pid = child.id() as i32;
    let pidfile_pid =
        wait_for_pid_file(&pid_path, Duration::from_secs(10)).expect("daemon pidfile");
    assert_eq!(pidfile_pid, pid);
    fs::remove_file(&pid_path).expect("remove pidfile to make orphan");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let pids = mempal::daemon_singleton::enumerate_daemon_pids("mempal", db_path);
        if pids.contains(&pid) {
            return child;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("db-backed orphan daemon pid {pid} was not enumerated");
}

#[test]
fn test_daemon_context_bootstrap_ordering() {
    let (_tmp, _db_path, config_path) = setup_daemon_home();
    let runtime = tokio::runtime::Runtime::new().expect("bootstrap runtime");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);

    let context = DaemonContext::bootstrap_with_events(config_path.clone(), true, Some(tx))
        .expect("bootstrap");
    let mut stages = Vec::new();
    runtime.block_on(async {
        while let Ok(Some(stage)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
        {
            stages.push(stage);
            if matches!(stages.last(), Some(BootstrapEvent::Ready)) {
                break;
            }
        }
    });
    let pid_path = context.mempal_home.join("daemon.pid");

    assert_eq!(
        stages,
        vec![
            BootstrapEvent::Daemonize,
            BootstrapEvent::RuntimeInit,
            BootstrapEvent::ConfigHandleBootstrap,
            BootstrapEvent::DbOpen,
            BootstrapEvent::TracingInit,
            BootstrapEvent::Ready,
        ]
    );
    assert!(
        pid_path.exists(),
        "pid file must exist during daemon lifetime"
    );
    drop(context);
    assert!(!pid_path.exists(), "pid file must be removed on drop");
}

#[cfg(unix)]
#[test]
fn test_daemon_restart_reaps_orphan_without_pidfile() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut orphan = spawn_db_backed_orphan_daemon(tmp.path(), &db_path);
    let orphan_pid = orphan.id() as i32;
    assert!(
        process_is_running_for_test(orphan_pid),
        "fake orphan daemon pid {orphan_pid} should be running"
    );
    assert!(
        !pid_path.exists(),
        "fake orphan must be discoverable by argv without a pidfile"
    );

    let output = Command::new(mempal_bin())
        .args(["daemon", "restart"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon restart");
    assert!(
        output.status.success(),
        "daemon restart failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wait_for_child_exit(&mut orphan, Duration::from_secs(10)),
        "orphan daemon pid {orphan_pid} should be reaped by restart"
    );

    let restarted_pid =
        wait_for_pid_file(&pid_path, Duration::from_secs(10)).expect("restarted daemon pidfile");
    assert_ne!(restarted_pid, orphan_pid);
    assert!(
        process_is_running_for_test(restarted_pid),
        "restarted daemon pid {restarted_pid} should be running"
    );
}

#[cfg(unix)]
#[test]
fn test_daemon_start_repairs_orphan_pidfile_before_reporting_running() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut orphan = spawn_db_backed_orphan_daemon(tmp.path(), &db_path);
    let orphan_pid = orphan.id() as i32;
    assert!(!pid_path.exists());

    let output = Command::new(mempal_bin())
        .args(["daemon", "start"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon start");
    assert!(
        !output.status.success(),
        "daemon start should report the singleton is already running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon already running"), "{stderr}");

    let repaired_pid = wait_for_pid_file(&pid_path, Duration::from_secs(2))
        .expect("daemon start should repair singleton orphan pidfile before returning");
    assert_eq!(repaired_pid, orphan_pid);
    assert!(process_is_running_for_test(repaired_pid));

    orphan.kill().expect("kill orphan daemon");
    orphan.wait().expect("wait orphan daemon");
}

#[cfg(unix)]
#[test]
fn test_daemon_stop_terminates_pidfile_only_process() -> Result<()> {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = Command::new("sleep")
        .args(["120"]) // no-op placeholder process to exercise pidfile-only flow
        .spawn()
        .context("spawn placeholder")?;
    fs::write(&pid_path, child.id().to_string()).context("write pidfile")?;

    let output = Command::new(mempal_bin())
        .args(["daemon", "stop"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .context("run daemon stop")?;
    assert!(
        output.status.success(),
        "daemon stop failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(wait_for_child_exit(&mut child, Duration::from_secs(5)));
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_daemon_restart_terminates_pidfile_only_process_and_restarts() -> Result<()> {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = Command::new("sleep")
        .args(["120"])
        .spawn()
        .context("spawn placeholder")?;
    let old_pid = child.id() as i32;
    fs::write(&pid_path, old_pid.to_string()).context("write pidfile")?;

    let output = Command::new(mempal_bin())
        .args(["daemon", "restart"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .context("run daemon restart")?;
    assert!(
        output.status.success(),
        "daemon restart failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let restarted_pid = wait_for_pid_file(&pid_path, Duration::from_secs(10))
        .context("daemon restart should write pidfile")?;
    assert_ne!(restarted_pid, old_pid);
    assert!(
        process_is_running_for_test(restarted_pid),
        "restarted daemon pid {restarted_pid} should be running"
    );
    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(5)),
        "old daemon placeholder pid {old_pid} should be terminated by restart"
    );

    child.kill()?;
    child.wait()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_daemon_reap_repairs_single_orphan_pidfile() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut orphan = spawn_db_backed_orphan_daemon(tmp.path(), &db_path);
    let orphan_pid = orphan.id() as i32;
    assert!(
        !pid_path.exists(),
        "test setup should leave a live singleton without a pidfile"
    );

    let output = Command::new(mempal_bin())
        .args(["daemon", "reap"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon reap");
    assert!(
        output.status.success(),
        "daemon reap failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let repaired_pid = wait_for_pid_file(&pid_path, Duration::from_secs(2))
        .expect("daemon reap should repair singleton orphan pidfile");
    assert_eq!(repaired_pid, orphan_pid);
    assert!(process_is_running_for_test(repaired_pid));
    orphan.kill().expect("kill orphan daemon");
    orphan.wait().expect("wait orphan daemon");
}

#[cfg(unix)]
#[test]
fn test_status_full_treats_single_orphan_as_running() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut orphan = spawn_db_backed_orphan_daemon(tmp.path(), &db_path);
    let orphan_pid = orphan.id() as i32;
    assert!(!pid_path.exists());

    let output = Command::new(mempal_bin())
        .args(["status", "--full"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run status --full");
    assert!(
        output.status.success(),
        "status --full failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("  running: true"), "{stdout}");
    assert!(stdout.contains(&format!("  pid: {orphan_pid}")), "{stdout}");

    orphan.kill().expect("kill orphan daemon");
    orphan.wait().expect("wait orphan daemon");
}

#[cfg(unix)]
#[test]
fn test_daemon_reap_keeps_single_pidfile_process() -> Result<()> {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = Command::new("sleep")
        .args(["120"])
        .spawn()
        .context("spawn placeholder")?;
    fs::write(&pid_path, child.id().to_string()).context("write pidfile")?;

    let output = Command::new(mempal_bin())
        .args(["daemon", "reap"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .output()
        .context("run daemon reap")?;
    assert!(
        output.status.success(),
        "daemon reap failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(process_is_running_for_test(child.id() as i32));
    child.kill()?;
    child.wait()?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_daemon_sigterm_graceful() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let mut server = Server::new();
    let _mock = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#)
        .create();
    fs::write(
        tmp.path().join(".mempal/config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 3
request_timeout_secs = 5

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            server.url(),
            tmp.path().join(".mempal/daemon.log").display()
        ),
    )
    .expect("rewrite config");
    let store = PendingMessageStore::new(&db_path).expect("store");
    let envelope = CapturedHookEnvelope {
        event: HookEvent::SessionStart.display_name().to_string(),
        kind: HookEvent::SessionStart.queue_kind().to_string(),
        agent: "claude".to_string(),
        captured_at: "123".to_string(),
        claude_cwd: "/tmp/project".to_string(),
        payload: Some(r#"{"session_id":"abc","cwd":"/tmp/project"}"#.to_string()),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: 32,
        truncated: false,
    };
    let payload = serde_json::to_string(&envelope).expect("serialize envelope");
    store
        .enqueue(HookEvent::SessionStart.queue_kind(), &payload)
        .expect("enqueue");

    let mut child = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let db = Database::open(&db_path).expect("open db");
        if db.drawer_count().expect("drawer count") > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    assert_eq!(rc, 0, "failed to send SIGTERM");
    let status = child.wait().expect("wait child");
    assert!(
        status.success(),
        "daemon must exit cleanly after SIGTERM: {status:?}"
    );

    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let claimed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'claimed'",
            [],
            |row| row.get(0),
        )
        .expect("claimed count");
    assert_eq!(claimed, 0, "no message may remain claimed after SIGTERM");
    let pid_path = tmp.path().join(".mempal/daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && pid_path.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_path.exists(), "daemon pid file must be removed");
}

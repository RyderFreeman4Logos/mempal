mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::harness::{
    CapturedChild, embed_mock::start as start_embed_mock, write_daemon_home_diagnostics,
};
use mempal::bootstrap_events::BootstrapEvent;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::daemon_bootstrap::DaemonContext;
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mockito::Server;
use serde_json::json;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_daemon_home() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new_in("/tmp").expect("short tempdir");
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

fn daemon_runtime_dir(home: &Path) -> PathBuf {
    home.join(".mempal").join("runtime")
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

#[cfg(unix)]
#[test]
fn status_commands_exit_successfully_when_stdout_pipe_is_closed() {
    let (home, _db_path, _config_path) = setup_daemon_home();

    for args in [&["daemon", "status"][..], &["status"][..]] {
        let command = args.join(" ");
        let (reader, writer) = std::os::unix::net::UnixStream::pair().expect("create stdout pipe");
        drop(reader);
        let writer: std::os::fd::OwnedFd = writer.into();

        let child = Command::new(mempal_bin())
            .args(args)
            .daemon_home(home.path())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {command}: {error}"));
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("wait for {command}: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "{command} should treat a closed stdout pipe as success: status={:?}, stderr={stderr}",
            output.status
        );
        assert!(!stderr.contains("panicked"), "unexpected panic: {stderr}");
    }
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set_path(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        // SAFETY: daemon lifecycle tests serialize process-wide environment
        // mutation with ENV_LOCK and restore the previous value on drop.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: daemon lifecycle tests serialize process-wide environment
        // mutation with ENV_LOCK and this restores the captured previous value.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}

fn prepared_ingest_payload(content: &str) -> String {
    serde_json::to_string(&json!({
        "request": {
            "content": content,
            "wing": "daemon",
            "room": "ingest-async",
            "source": "daemon-lifecycle-test",
            "source_type": "user_explicit",
            "confidence": 0.9,
            "project_id": null,
            "supersedes": null,
            "replace_text": null,
            "valid_from": null,
            "valid_until": null,
            "dry_run": false,
            "wait": false,
            "wait_timeout_secs": null,
            "diary_rollup": false,
            "importance": 0,
            "memory_kind": "evidence",
            "domain": "project",
            "field": "general",
            "is_pinned": false,
            "provenance": null,
            "statement": null,
            "tier": null,
            "status": null,
            "supporting_refs": null,
            "counterexample_refs": null,
            "teaching_refs": null,
            "verification_refs": null,
            "scope_constraints": null,
            "trigger_hints": null,
            "anchor_kind": null,
            "anchor_id": null,
            "parent_anchor_id": null,
            "cwd": null
        },
        "controls": {
            "no_gate": false,
            "bypass_novelty": false
        },
        "project_id": null,
        "scrubbed_content": content,
        "source_type": "user_explicit",
        "confidence": 0.9,
        "metadata": {
            "memory_kind": "evidence",
            "domain": "project",
            "field": "general",
            "is_pinned": false,
            "anchor_kind": "global",
            "anchor_id": "general",
            "parent_anchor_id": null,
            "provenance": null,
            "statement": null,
            "tier": null,
            "status": null,
            "supporting_refs": [],
            "counterexample_refs": [],
            "teaching_refs": [],
            "verification_refs": [],
            "scope_constraints": null,
            "trigger_hints": null
        },
        "superseded_drawer_id": null,
        "raw_turn": false,
        "drawer_importance": 0
    }))
    .expect("serialize prepared ingest payload")
}

fn write_openai_daemon_config(
    config_path: &std::path::Path,
    db_path: &std::path::Path,
    log_path: &std::path::Path,
    base_url: &str,
) {
    fs::write(
        config_path,
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"
api_model = "test-embed"
base_url = "{base_url}"
dim = 4

[embed.openai_compat]
base_url = "{base_url}"
model = "test-embed"
dim = 4
request_timeout_secs = 5

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            log_path.display()
        ),
    )
    .expect("write openai daemon config");
}

#[cfg(unix)]
struct DaemonHomeCleanup {
    db_path: PathBuf,
}

#[cfg(unix)]
impl Drop for DaemonHomeCleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            write_daemon_home_diagnostics(&self.db_path);
        }
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
fn wait_for_child_exit(child: &mut CapturedChild, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(error) => panic!(
                "failed to poll child exit: {error}\n{}",
                child.diagnostics()
            ),
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn spawn_foreground_daemon(home: &Path, label: &str) -> CapturedChild {
    let mut command = Command::new(mempal_bin());
    command
        .args(["daemon", "--foreground"])
        .daemon_home(home)
        .stdin(Stdio::null());
    CapturedChild::spawn(
        &mut command,
        &daemon_runtime_dir(home),
        label,
        Some(home.join(".mempal/daemon.log")),
    )
    .unwrap_or_else(|error| panic!("spawn foreground daemon {label}: {error}"))
}

fn spawn_placeholder_daemon(home: &Path, label: &str) -> std::io::Result<CapturedChild> {
    let mut command = Command::new("sleep");
    command.arg("120").stdin(Stdio::null());
    CapturedChild::spawn(&mut command, &daemon_runtime_dir(home), label, None)
}

fn spawn_db_backed_orphan_daemon(
    home: &std::path::Path,
    db_path: &std::path::Path,
) -> CapturedChild {
    let mempal_home = db_path.parent().expect("db parent");
    let pid_path = mempal_home.join("daemon.pid");
    let child = spawn_foreground_daemon(home, "db-backed-orphan");
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
    panic!(
        "db-backed orphan daemon pid {pid} was not enumerated\n{}",
        child.diagnostics()
    );
}

#[test]
fn test_daemon_context_bootstrap_ordering() {
    let (_tmp, _db_path, config_path) = setup_daemon_home();
    let _env_lock = ENV_LOCK.lock().expect("env lock");
    let _runtime_guard = EnvVarGuard::set_path(
        mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
        &daemon_runtime_dir(_tmp.path()),
    );
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

#[test]
fn test_duplicate_daemon_bootstrap_is_rejected_before_work_begins() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let runtime_dir = tmp.path().join("runtime");
    let _guard = match mempal::daemon_singleton::try_acquire_for_test(&db_path, &runtime_dir)
        .expect("first acquire")
    {
        mempal::daemon_singleton::DaemonLockAcquisition::Acquired(guard) => guard,
        mempal::daemon_singleton::DaemonLockAcquisition::AlreadyHeld { .. } => {
            panic!("test lock should be free")
        }
    };

    let output = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", tmp.path())
        .env(
            mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
            &runtime_dir,
        )
        .stdin(Stdio::null())
        .output()
        .expect("run duplicate daemon");

    assert!(
        !output.status.success(),
        "duplicate daemon must fail, stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("daemon already running"),
        "duplicate error should include owner metadata, stderr={stderr}"
    );
    assert!(
        !tmp.path().join(".mempal/daemon.pid").exists(),
        "duplicate daemon must fail before writing pidfile"
    );
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
        .daemon_home(tmp.path())
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
        .daemon_home(tmp.path())
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
fn test_daemon_stop_terminates_pidfile_only_process() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = spawn_placeholder_daemon(tmp.path(), "pidfile-only-stop-placeholder")
        .expect("spawn placeholder");
    fs::write(&pid_path, child.id().to_string()).expect("write pidfile");

    let output = Command::new(mempal_bin())
        .args(["daemon", "stop"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon stop");
    assert!(
        output.status.success(),
        "daemon stop failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(5)),
        "pidfile-only placeholder was not terminated\n{}",
        child.diagnostics()
    );
}

#[cfg(unix)]
#[test]
fn test_daemon_restart_terminates_pidfile_only_process_and_restarts() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = spawn_placeholder_daemon(tmp.path(), "pidfile-only-restart-placeholder")
        .expect("spawn placeholder");
    let old_pid = child.id() as i32;
    fs::write(&pid_path, old_pid.to_string()).expect("write pidfile");

    let output = Command::new(mempal_bin())
        .args(["daemon", "restart"])
        .daemon_home(tmp.path())
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

    let restarted_pid = wait_for_pid_file(&pid_path, Duration::from_secs(10))
        .expect("daemon restart should write pidfile");
    assert_ne!(restarted_pid, old_pid);
    assert!(
        process_is_running_for_test(restarted_pid),
        "restarted daemon pid {restarted_pid} should be running"
    );
    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(5)),
        "old daemon placeholder pid {old_pid} should be terminated by restart"
    );
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
        .daemon_home(tmp.path())
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
        .daemon_home(tmp.path())
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
fn test_daemon_reap_keeps_single_pidfile_process() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = spawn_placeholder_daemon(tmp.path(), "pidfile-only-reap-placeholder")
        .expect("spawn placeholder");
    fs::write(&pid_path, child.id().to_string()).expect("write pidfile");

    let output = Command::new(mempal_bin())
        .args(["daemon", "reap"])
        .daemon_home(tmp.path())
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

    assert!(
        process_is_running_for_test(child.id() as i32),
        "daemon reap should keep the pidfile-only placeholder alive\n{}",
        child.diagnostics()
    );
    child.kill().expect("kill placeholder");
    child.wait().expect("wait placeholder");
}

#[cfg(unix)]
#[test]
fn test_daemon_processes_ingest_async_queue_rows() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let store = PendingMessageStore::new(&db_path).expect("store");
    let operation_id = store
        .enqueue(
            "ingest_async",
            &prepared_ingest_payload("daemon consumes queued ingest_async row"),
        )
        .expect("enqueue async ingest");

    let mut child = spawn_foreground_daemon(tmp.path(), "process-ingest-async");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut completed = false;
    while Instant::now() < deadline {
        let record = PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(&operation_id)
            .expect("load operation status")
            .expect("operation record exists");
        if record.op_state == "completed" {
            completed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    child.signal_or_panic(libc::SIGTERM, "failed to send SIGTERM");
    let status = child.wait_or_panic("failed to wait for daemon");
    assert!(
        status.success(),
        "daemon must exit cleanly after SIGTERM: {status:?}\n{}",
        child.diagnostics()
    );

    assert!(
        completed,
        "daemon must claim and complete ingest_async operations"
    );
    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_sigterm_drains_running_ingest_async_before_reclaim() {
    let (tmp, db_path, config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    handle.pause();
    write_openai_daemon_config(
        &config_path,
        &db_path,
        &tmp.path().join(".mempal/daemon.log"),
        &format!("http://{addr}/v1"),
    );

    let store = PendingMessageStore::new(&db_path).expect("store");
    let operation_id = store
        .enqueue(
            "ingest_async",
            &prepared_ingest_payload("daemon drains active ingest_async row"),
        )
        .expect("enqueue async ingest");

    let mut child = spawn_foreground_daemon(tmp.path(), "drain-running-ingest-async");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let record = PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(&operation_id)
            .expect("load operation status")
            .expect("operation record exists");
        if record.op_state == "running" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let running = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load running status")
        .expect("operation record exists");
    assert_eq!(running.op_state, "running");

    child.signal_or_panic(libc::SIGTERM, "failed to send SIGTERM");
    assert!(
        !wait_for_child_exit(&mut child, Duration::from_millis(300)),
        "daemon must not exit while the active ingest_async claim is mid-ingest\n{}",
        child.diagnostics()
    );

    handle.resume();
    let status = child.wait_or_panic("failed to wait for daemon");
    handle.shutdown().await;
    assert!(
        status.success(),
        "daemon must exit cleanly after draining active ingest: {status:?}\n{}",
        child.diagnostics()
    );

    let completed = PendingMessageStore::new_without_reclaim(&db_path)
        .operation_status(&operation_id)
        .expect("load completed status")
        .expect("operation record exists");
    assert_eq!(completed.op_state, "completed");
    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);

    let mut restarted = spawn_foreground_daemon(tmp.path(), "restart-after-drain");
    std::thread::sleep(Duration::from_millis(300));
    restarted.signal_or_panic(libc::SIGTERM, "failed to stop restarted daemon");
    let restart_status = restarted.wait_or_panic("failed to wait for restarted daemon");
    assert!(
        restart_status.success(),
        "restarted daemon must exit cleanly: {restart_status:?}\n{}",
        restarted.diagnostics()
    );
    let db = Database::open(&db_path).expect("open db after restart");
    assert_eq!(
        db.drawer_count().expect("drawer count after restart"),
        1,
        "completed async ingest must not be reprocessed after restart"
    );
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

    let mut child = spawn_foreground_daemon(tmp.path(), "sigterm-graceful");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let db = Database::open(&db_path).expect("open db");
        if db.drawer_count().expect("drawer count") > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    child.signal_or_panic(libc::SIGTERM, "failed to send SIGTERM");
    let status = child.wait_or_panic("failed to wait for daemon");
    assert!(
        status.success(),
        "daemon must exit cleanly after SIGTERM: {status:?}\n{}",
        child.diagnostics()
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

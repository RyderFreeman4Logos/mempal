mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::harness::{
    CapturedChild, embed_mock::start as start_embed_mock, hold_sqlite_lock_for,
    write_daemon_home_diagnostics,
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
        // Isolate daemon children from ambient embedder overrides.
        self.env("HOME", home)
            .env(
                mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
                daemon_runtime_dir(home),
            )
            .env_remove("MEMPAL_EMBED_BACKEND")
            .env_remove("MEMPAL_EMBED_BASE_URL")
            .env_remove("MEMPAL_EMBED_MODEL")
            .env_remove("MEMPAL_EMBED_DIM")
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
        // SAFETY: ENV_LOCK serializes this mutation; the guard restores it.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: ENV_LOCK serializes this restore.
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
request_timeout_secs = 15

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
fn read_pid_file(pid_path: &Path) -> i32 {
    fs::read_to_string(pid_path)
        .unwrap_or_else(|error| panic!("read daemon pidfile {}: {error}", pid_path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|error| panic!("parse daemon pidfile {}: {error}", pid_path.display()))
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

fn spawn_db_backed_orphan_daemon(home: &Path, db_path: &Path) -> CapturedChild {
    let pid_path = db_path.parent().expect("parent").join("daemon.pid");
    let mut child = spawn_foreground_daemon(home, "db-backed-orphan");
    let pid = child.id() as i32;
    child.wait_for_stderr_event("daemon hook workers started", Duration::from_secs(30));
    let pidfile_pid = read_pid_file(&pid_path);
    assert_eq!(pidfile_pid, pid);
    assert!(
        Command::new(mempal_bin())
            .args(["daemon", "wait"])
            .daemon_home(home)
            .status()
            .expect("wait")
            .success()
    );
    fs::remove_file(&pid_path).expect("remove pidfile");
    assert!(
        mempal::daemon_singleton::enumerate_daemon_pids("mempal", db_path).contains(&pid),
        "db-backed orphan daemon pid {pid} was not enumerated\n{}",
        child.diagnostics()
    );
    child
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
            BootstrapEvent::RecoveryAdmitted,
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

    orphan.wait_or_panic("orphan daemon should be reaped by restart");

    let restarted_pid = read_pid_file(&pid_path);
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

    let repaired_pid = read_pid_file(&pid_path);
    assert_eq!(repaired_pid, orphan_pid);
    assert!(process_is_running_for_test(repaired_pid));

    orphan.kill().expect("kill orphan daemon");
    orphan.wait().expect("wait orphan daemon");
}
#[cfg(unix)]
#[test]
fn test_daemon_stop_does_not_terminate_pidfile_only_process() {
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
        !output.status.success(),
        "pidfile-only process is not a daemon"
    );

    assert!(
        process_is_running_for_test(child.id() as i32),
        "pidfile-only placeholder was terminated\n{}",
        child.diagnostics()
    );
    child.kill().expect("kill placeholder");
    child.wait().expect("wait placeholder");
}
#[cfg(unix)]
#[test]
fn test_daemon_status_does_not_report_pidfile_only_process_as_running() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let _cleanup = DaemonHomeCleanup {
        db_path: db_path.clone(),
    };
    let pid_path = tmp.path().join(".mempal/daemon.pid");

    let mut child = spawn_placeholder_daemon(tmp.path(), "pidfile-only-status-placeholder")
        .expect("spawn placeholder");
    fs::write(&pid_path, child.id().to_string()).expect("write pidfile");

    let output = Command::new(mempal_bin())
        .args(["daemon", "status"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("run daemon status");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("status: stopped"), "{stdout}");
    assert!(process_is_running_for_test(child.id() as i32));

    child.kill().expect("kill placeholder");
    child.wait().expect("wait placeholder");
}
#[cfg(unix)]
#[test]
fn test_daemon_restart_does_not_terminate_pidfile_only_process_and_restarts() {
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

    let restarted_pid = read_pid_file(&pid_path);
    assert_ne!(restarted_pid, old_pid);
    assert!(
        process_is_running_for_test(restarted_pid),
        "restarted daemon pid {restarted_pid} should be running"
    );
    assert!(process_is_running_for_test(old_pid));
    child.kill().expect("kill placeholder");
    child.wait().expect("wait placeholder");
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

    let repaired_pid = read_pid_file(&pid_path);
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
fn test_daemon_reap_does_not_keep_pidfile_only_process() {
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
        "daemon reap should not signal the pidfile-only placeholder\n{}",
        child.diagnostics()
    );
    assert!(
        !pid_path.exists(),
        "pidfile-only process is not a reap keeper"
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

    let wait = Command::new(mempal_bin())
        .args(["operation", "wait", &operation_id, "--timeout-secs", "30"])
        .daemon_home(tmp.path())
        .stdin(Stdio::null())
        .output()
        .expect("wait for ingest operation");
    assert!(
        wait.status.success(),
        "operation wait failed: status={:?}, stdout={}, stderr={}",
        wait.status,
        String::from_utf8_lossy(&wait.stdout),
        String::from_utf8_lossy(&wait.stderr)
    );

    child.signal_or_panic(libc::SIGTERM, "failed to send SIGTERM");
    let status = child.wait_or_panic("failed to wait for daemon");
    assert!(
        status.success(),
        "daemon must exit cleanly after SIGTERM: {status:?}\n{}",
        child.diagnostics()
    );

    assert!(
        store
            .operation_status(&operation_id)
            .expect("load completed operation")
            .is_some_and(|record| record.op_state == "completed"),
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
    // Full-suite / local-gates load can delay bootstrap past 10s; product drain
    // budget stays 30s. Widen only this readiness probe before SIGTERM.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            panic!(
                "daemon exited before paused embed request\n{}",
                child.diagnostics()
            );
        }
        let record = store
            .operation_status(&operation_id)
            .expect("load operation status")
            .expect("operation record exists");
        if record.op_state == "running" && handle.request_count() > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon did not reach the paused embed request before SIGTERM: op_state={}, request_count={}, failure_detail={:.160}, rejected_reason={:.160}\n{}",
            record.op_state,
            handle.request_count(),
            record.failure_detail.as_deref().unwrap_or("none"),
            record.rejected_reason.as_deref().unwrap_or("none"),
            child.diagnostics()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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

    let completed = store
        .operation_status(&operation_id)
        .expect("load completed status")
        .expect("operation record exists");
    assert_eq!(
        completed.op_state,
        "completed",
        "drained ingest must complete: op_state={}, failure_detail={:.160}, rejected_reason={:.160}",
        completed.op_state,
        completed.failure_detail.as_deref().unwrap_or("none"),
        completed.rejected_reason.as_deref().unwrap_or("none")
    );
    let db = Database::open(&db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);

    let release_startup = hold_sqlite_lock_for(&db_path, Duration::from_secs(2));
    let mut restarted = spawn_foreground_daemon(tmp.path(), "restart-after-drain");
    restarted.wait_for_stderr_event("daemon hook workers started", Duration::from_secs(30));
    release_startup.join().expect("release restart delay lock");
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
#[path = "daemon_lifecycle/sigterm_graceful.rs"]
mod sigterm_graceful;

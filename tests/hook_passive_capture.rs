mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::harness::{BootstrapObserver, DaemonSupervisor, start as start_embed_mock};
use mempal::bootstrap_events::BootstrapEvent;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::daemon_bootstrap::DaemonContext;
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

async fn test_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn setup_home() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    let config_path = mempal_home.join("config.toml");
    (tmp, mempal_home, db_path, config_path)
}

fn write_config(
    config_path: &Path,
    db_path: &Path,
    enabled: bool,
    poll_ms: u64,
    claim_ttl_secs: u64,
    base_url: Option<&str>,
) {
    let embed = match base_url {
        Some(base_url) => format!(
            r#"
[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{base_url}"
model = "test-embed"
dim = 4
request_timeout_secs = 2
"#
        ),
        None => r#"
[embed]
backend = "model2vec"
"#
        .to_string(),
    };
    fs::write(
        config_path,
        format!(
            r#"
db_path = "{}"
{embed}
[hooks]
enabled = {enabled}
daemon_poll_interval_ms = {poll_ms}
daemon_claim_ttl_secs = {claim_ttl_secs}

[privacy]
enabled = true

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            db_path
                .parent()
                .expect("mempal home")
                .join("daemon.log")
                .display()
        ),
    )
    .expect("write config");
}

fn queue_store(db_path: &Path) -> PendingMessageStore {
    PendingMessageStore::new(db_path).expect("pending store")
}

fn enqueue_envelope(db_path: &Path, envelope: &CapturedHookEnvelope) -> String {
    let payload = serde_json::to_string(envelope).expect("serialize envelope");
    queue_store(db_path)
        .enqueue(&envelope.kind, &payload)
        .expect("enqueue envelope")
}

async fn wait_for_condition<F>(timeout: Duration, mut check: F)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("condition not satisfied within {timeout:?}");
}

fn drawer_count(db_path: &Path) -> i64 {
    Database::open(db_path)
        .expect("open db")
        .drawer_count()
        .expect("drawer count")
}

fn latest_drawer_row(db_path: &Path) -> (String, String, String, String) {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT wing, room, content, source_file FROM drawers ORDER BY added_at DESC, id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("latest drawer row")
}

fn message_status(db_path: &Path, id: &str) -> Option<(String, i64)> {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT status, retry_count FROM pending_messages WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_crash_reclaim_stale() {
    let _guard = test_guard().await;
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    handle.pause();
    write_config(
        &config_path,
        &db_path,
        true,
        50,
        1,
        Some(&format!("http://{addr}/v1")),
    );

    let message_id = enqueue_envelope(
        &db_path,
        &CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "123".to_string(),
            claude_cwd: tmp.path().display().to_string(),
            payload: Some(
                r#"{"tool_name":"Bash","input":"ls","output":"ok","exit_code":0}"#.to_string(),
            ),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: 64,
            truncated: false,
        },
    );

    let mut daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn daemon");
    daemon
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait ready");
    wait_for_condition(Duration::from_secs(5), || handle.request_count() > 0).await;
    wait_for_condition(Duration::from_secs(5), || {
        matches!(message_status(&db_path, &message_id), Some((ref status, _)) if status == "claimed")
    })
    .await;

    daemon.sigkill();
    let _ = daemon.wait().await.expect("wait killed daemon");
    tokio::time::sleep(Duration::from_secs(2)).await;

    handle.resume();
    let mut restarted = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn restarted daemon");
    restarted
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait restarted ready");
    wait_for_condition(Duration::from_secs(10), || drawer_count(&db_path) > 0).await;

    assert!(
        message_status(&db_path, &message_id).is_none(),
        "message should be confirmed after reclaim"
    );
    restarted.sigterm();
    let status = restarted.wait().await.expect("wait restarted daemon");
    assert!(status.success(), "restarted daemon exited with {status:?}");
    handle.shutdown().await;
}

#[test]
fn test_daemon_exits_when_disabled() {
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    write_config(&config_path, &db_path, false, 50, 60, None);

    let output = Command::new(mempal_bin())
        .args(["daemon", "--foreground"])
        .env("HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run daemon");

    assert!(
        output.status.success(),
        "daemon should exit 0 when disabled"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("hooks not enabled"),
        "missing no-op message: {stderr}"
    );
    assert!(
        !tmp.path().join(".mempal/daemon.pid").exists(),
        "pid file must not be created when hooks are disabled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_handles_truncated_envelope_without_retry() {
    let _guard = test_guard().await;
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config(
        &config_path,
        &db_path,
        true,
        50,
        60,
        Some(&format!("http://{addr}/v1")),
    );

    let message_id = enqueue_envelope(
        &db_path,
        &CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "123".to_string(),
            claude_cwd: tmp.path().display().to_string(),
            payload: None,
            payload_path: Some("/tmp/truncated.json".to_string()),
            payload_preview: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCD".to_string()),
            original_size_bytes: 10_000_001,
            truncated: true,
        },
    );

    let mut daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn daemon");
    daemon
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait ready");
    wait_for_condition(Duration::from_secs(10), || drawer_count(&db_path) > 0).await;

    let (wing, room, content, _) = latest_drawer_row(&db_path);
    assert_eq!(wing, "hooks-raw");
    assert_eq!(room, "truncated");
    assert!(content.contains("\"_truncated\":true"), "{content}");
    assert!(
        message_status(&db_path, &message_id).is_none(),
        "truncated envelope must be confirmed, not retried"
    );
    let failed_count: i64 = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'failed'",
            [],
            |row| row.get(0),
        )
        .expect("failed count");
    assert_eq!(failed_count, 0);

    daemon.sigterm();
    let status = daemon.wait().await.expect("wait daemon");
    assert!(status.success(), "daemon exited with {status:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_processes_hook_post_tool_to_drawer() {
    let _guard = test_guard().await;
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config(
        &config_path,
        &db_path,
        true,
        50,
        60,
        Some(&format!("http://{addr}/v1")),
    );

    let raw_secret = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCD";
    enqueue_envelope(
        &db_path,
        &CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "123".to_string(),
            claude_cwd: tmp.path().display().to_string(),
            payload: Some(
                serde_json::json!({
                    "tool_name": "Bash",
                    "input": "printf secret",
                    "output": raw_secret,
                    "exit_code": 0
                })
                .to_string(),
            ),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: 128,
            truncated: false,
        },
    );

    let mut daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn daemon");
    daemon
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait ready");
    wait_for_condition(Duration::from_secs(10), || drawer_count(&db_path) > 0).await;

    let (wing, room, content, source_file) = latest_drawer_row(&db_path);
    assert_eq!(wing, "hooks-raw");
    assert_eq!(room, "Bash");
    assert!(
        Path::new(&source_file).exists(),
        "payload path should exist"
    );
    assert!(content.contains("\"preview\""), "{content}");
    assert!(
        content.contains("[REDACTED:openai_key]"),
        "privacy scrub must affect stored preview: {content}"
    );
    assert!(
        !content.contains(raw_secret),
        "raw secret leaked into drawer"
    );

    daemon.sigterm();
    let status = daemon.wait().await.expect("wait daemon");
    assert!(status.success(), "daemon exited with {status:?}");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_sigterm_graceful_shutdown() {
    let _guard = test_guard().await;
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config(
        &config_path,
        &db_path,
        true,
        50,
        60,
        Some(&format!("http://{addr}/v1")),
    );

    enqueue_envelope(
        &db_path,
        &CapturedHookEnvelope {
            event: HookEvent::SessionStart.display_name().to_string(),
            kind: HookEvent::SessionStart.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "123".to_string(),
            claude_cwd: tmp.path().display().to_string(),
            payload: Some(r#"{"session_id":"abc","cwd":"/tmp/project"}"#.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: 32,
            truncated: false,
        },
    );

    let mut daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn daemon");
    daemon
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait ready");
    wait_for_condition(Duration::from_secs(10), || drawer_count(&db_path) > 0).await;

    daemon.sigterm();
    let status = tokio::time::timeout(Duration::from_secs(3), daemon.wait())
        .await
        .expect("daemon did not stop within deadline")
        .expect("wait daemon");
    assert!(status.success(), "daemon exited with {status:?}");

    let claimed: i64 = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'claimed'",
            [],
            |row| row.get(0),
        )
        .expect("claimed count");
    assert_eq!(claimed, 0, "no message may remain claimed after SIGTERM");
    assert!(
        !tmp.path().join(".mempal/daemon.pid").exists(),
        "daemon pid file must be removed"
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_status_reports_state_and_queue() {
    let _guard = test_guard().await;
    let (tmp, mempal_home, db_path, config_path) = setup_home();
    write_config(&config_path, &db_path, true, 50, 60, None);

    let store = queue_store(&db_path);
    store
        .enqueue("hook_post_tool", r#"{"tool_name":"Bash"}"#)
        .expect("enqueue pending");
    store
        .enqueue("hook_post_tool", r#"{"tool_name":"Edit"}"#)
        .expect("enqueue claimed");
    let claimed = store
        .claim_next("status-worker", 60)
        .expect("claim")
        .expect("message");
    store
        .refresh_heartbeat(&claimed.id, "status-worker")
        .expect("refresh heartbeat");
    fs::write(
        mempal_home.join("daemon.pid"),
        std::process::id().to_string(),
    )
    .expect("write daemon pid");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", tmp.path())
        .output()
        .expect("run status");
    assert!(output.status.success(), "status must succeed");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Daemon:"), "{stdout}");
    assert!(stdout.contains("running: true"), "{stdout}");
    assert!(stdout.contains("Queue:"), "{stdout}");
    assert!(stdout.contains("pending: 1"), "{stdout}");
    assert!(stdout.contains("claimed: 1"), "{stdout}");
    assert!(stdout.contains("last_heartbeat_unix_secs:"), "{stdout}");
}

#[test]
fn test_no_sqlite_before_daemonize() {
    let (_tmp, _mempal_home, db_path, config_path) = setup_home();
    write_config(&config_path, &db_path, true, 50, 60, None);

    let runtime = tokio::runtime::Runtime::new().expect("bootstrap runtime");
    let (tx, mut observer): (_, BootstrapObserver) = common::harness::channel();
    let context = DaemonContext::bootstrap_with_events(config_path, true, Some(tx))
        .expect("bootstrap context");
    let seen = runtime
        .block_on(async {
            observer
                .recv_until(BootstrapEvent::Ready, Duration::from_secs(1))
                .await
        })
        .expect("observe bootstrap");

    assert_eq!(
        seen,
        vec![
            BootstrapEvent::Daemonize,
            BootstrapEvent::RuntimeInit,
            BootstrapEvent::ConfigHandleBootstrap,
            BootstrapEvent::DbOpen,
            BootstrapEvent::TracingInit,
            BootstrapEvent::Ready,
        ]
    );
    drop(context);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_truncated_envelope_preview_is_scrubbed() {
    let _guard = test_guard().await;
    let (tmp, _mempal_home, db_path, config_path) = setup_home();
    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config(
        &config_path,
        &db_path,
        true,
        50,
        60,
        Some(&format!("http://{addr}/v1")),
    );

    enqueue_envelope(
        &db_path,
        &CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "123".to_string(),
            claude_cwd: tmp.path().display().to_string(),
            payload: None,
            payload_path: Some("/tmp/truncated.json".to_string()),
            payload_preview: Some("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890WXYZ".to_string()),
            original_size_bytes: 10_000_001,
            truncated: true,
        },
    );

    let mut daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn daemon");
    daemon
        .wait_ready(Duration::from_secs(5))
        .await
        .expect("wait ready");
    wait_for_condition(Duration::from_secs(10), || drawer_count(&db_path) > 0).await;

    let stderr = daemon.stderr_lines().await.join("\n");
    assert!(
        stderr.contains("[REDACTED:openai_key]"),
        "scrubbed preview missing from daemon logs: {stderr}"
    );
    assert!(
        !stderr.contains("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890WXYZ"),
        "raw secret leaked into daemon logs: {stderr}"
    );

    daemon.sigterm();
    let status = daemon.wait().await.expect("wait daemon");
    assert!(status.success(), "daemon exited with {status:?}");
    handle.shutdown().await;
}

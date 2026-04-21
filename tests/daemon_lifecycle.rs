use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
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
backend = "model2vec"

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

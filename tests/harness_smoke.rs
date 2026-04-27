mod common;

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use anyhow::Result;
use common::harness::{
    AlwaysFailMigrationHook, BootstrapObserver, CountingMigrationHook, DaemonSupervisor, FailMode,
    McpStdio, NoopMigrationHook, ReloadCounter, dump as dump_vec0, restore as restore_vec0,
};
use mempal::bootstrap_events::BootstrapEvent;
use mempal::core::config::ConfigHandle;
use mempal::core::db::{Database, MigrationHook, apply_fork_ext_migrations_with_hook};
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::{Drawer, SourceType};
use mempal::daemon_bootstrap::DaemonContext;
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use tempfile::TempDir;

#[test]
fn vec0_snapshot_round_trips() {
    let tmp = TempDir::new().expect("tempdir");
    let source = Database::open(&tmp.path().join("source.db")).expect("open source db");
    let target = Database::open(&tmp.path().join("target.db")).expect("open target db");
    let drawer = Drawer {
        id: "drawer-1".to_string(),
        content: "content".to_string(),
        wing: "wing".to_string(),
        room: Some("room".to_string()),
        source_file: Some("source.txt".to_string()),
        source_type: SourceType::Manual,
        added_at: "2026-04-21T00:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 3,
        ..Drawer::default()
    };
    source.insert_drawer(&drawer).expect("insert drawer");
    source
        .insert_vector(&drawer.id, &[0.1, 0.2, 0.3, 0.4])
        .expect("insert vector");

    let snapshot = dump_vec0(source.conn()).expect("dump vec0");
    restore_vec0(target.conn(), &snapshot).expect("restore vec0");

    assert_eq!(
        dump_vec0(target.conn()).expect("dump restored vec0"),
        snapshot
    );
}

#[test]
fn migration_hook_smoke() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let counting = CountingMigrationHook::new();
    apply_fork_ext_migrations_with_hook(db.conn(), Some(&counting))
        .expect("rerun migrations with counting hook");
    assert_eq!(counting.count(), 0);

    let noop = NoopMigrationHook;
    assert!(noop.pre_commit().is_ok());
    let fail = AlwaysFailMigrationHook;
    assert!(fail.pre_commit().is_err());
}

#[tokio::test]
async fn mcp_stdio_initializes() -> Result<()> {
    let tmp = TempDir::new()?;
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home)?;
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path)?;
    let (addr, handle) = common::harness::start(0).await?;

    let mut client = McpStdio::start(
        &db_path,
        HashMap::from([(
            "MEMPAL_TEST_EMBED_BASE_URL".to_string(),
            format!("http://{addr}/v1"),
        )]),
    )
    .await?;
    let server_info = tokio::time::timeout(Duration::from_secs(5), client.initialize()).await??;
    assert!(!server_info.server_info.name.trim().is_empty());
    client.shutdown().await?;
    handle.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_integration_smoke() -> Result<()> {
    let tmp = TempDir::new()?;
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home)?;
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");

    Database::open(&db_path)?;
    let store = PendingMessageStore::new(&db_path)?;
    let (addr, mock_handle) = common::harness::start(0).await?;
    let base_url = format!("http://{addr}/v1");
    let config = format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"
base_url = "{}"
api_model = "test-embed"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2

[hooks]
enabled = true
daemon_poll_interval_ms = 50
daemon_claim_ttl_secs = 30

[daemon]
log_path = "{}"
"#,
        db_path.display(),
        base_url,
        base_url,
        mempal_home.join("daemon.log").display()
    );
    fs::write(&config_path, config)?;
    ConfigHandle::bootstrap(&config_path)?;

    let envelope = CapturedHookEnvelope {
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
    };
    let payload = serde_json::to_string(&envelope)?;
    store.enqueue(HookEvent::SessionStart.queue_kind(), &payload)?;

    let (tx, mut observer): (_, BootstrapObserver) = common::harness::channel();
    let bootstrap_config = config_path.clone();
    let bootstrap_task = tokio::task::spawn_blocking(move || {
        DaemonContext::bootstrap_with_events(bootstrap_config, true, Some(tx))
    });
    let bootstrap_context = tokio::time::timeout(Duration::from_secs(10), bootstrap_task)
        .await?
        .expect("join bootstrap task")?;
    let seen = observer
        .recv_until(BootstrapEvent::Ready, Duration::from_secs(1))
        .await?;
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
    tokio::task::spawn_blocking(move || drop(bootstrap_context))
        .await
        .expect("join context drop task");

    let mut supervisor = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), tmp.path().display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await?;
    supervisor.wait_ready(Duration::from_secs(10)).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if mock_handle.request_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        mock_handle.request_count() > 0,
        "daemon never hit embed mock"
    );

    supervisor.sigterm();
    let status = supervisor.wait().await?;
    assert!(status.success(), "daemon exited with {status:?}");
    mock_handle.shutdown().await;

    let counter = ReloadCounter::from_hot_reload_state();
    assert_eq!(counter.count(), 0);
    counter.reset();
    Ok(())
}

#[tokio::test]
async fn embed_mock_failure_modes() -> Result<()> {
    let (addr, handle) = common::harness::start(0).await?;
    handle.set_dim(8);

    let client = reqwest::Client::new();
    let url = format!("http://{addr}/v1/embeddings");
    let ok = client
        .post(&url)
        .json(&serde_json::json!({ "input": ["hello"] }))
        .send()
        .await?;
    assert!(ok.status().is_success());

    handle.set_fail_mode(FailMode::Http500).await;
    let response = client
        .post(&url)
        .json(&serde_json::json!({ "input": ["hello"] }))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );

    handle.set_fail_mode(FailMode::RateLimit429).await;
    let response = client
        .post(&url)
        .json(&serde_json::json!({ "input": ["hello"] }))
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    handle.pause();
    let paused = client
        .post(&url)
        .json(&serde_json::json!({ "input": ["hello"] }))
        .send();
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.resume();
    let _ = paused.await?;

    handle.shutdown().await;
    Ok(())
}

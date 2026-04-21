use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mempal::core::{db::Database, queue::PendingMessageStore};
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use rusqlite::Connection;
use tempfile::TempDir;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs() as i64
}

struct EventuallyOkEmbedder {
    attempts: Arc<AtomicUsize>,
    fail_before_success: usize,
}

#[async_trait]
impl Embedder for EventuallyOkEmbedder {
    async fn embed(&self, _texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt < self.fail_before_success {
            tokio::time::sleep(Duration::from_millis(25)).await;
            return Err(EmbedError::Runtime(format!("transient failure {attempt}")));
        }
        Ok(vec![vec![0.1, 0.2, 0.3]])
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "eventually-ok"
    }
}

fn hotpatch_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    fs::read_dir(dir)
        .expect("read hotpatch dir")
        .map(|entry| entry.expect("dir entry").path())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_heartbeat_fires_during_embed_retry() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("open db");

    let store = PendingMessageStore::new(&db_path).expect("store");
    let envelope = CapturedHookEnvelope {
        event: HookEvent::PostToolUse.display_name().to_string(),
        kind: HookEvent::PostToolUse.queue_kind().to_string(),
        agent: "claude".to_string(),
        captured_at: "123".to_string(),
        claude_cwd: "/tmp/project".to_string(),
        payload: Some(
            r#"{"tool_name":"Bash","input":"ls","output":"ok","exit_code":0}"#.to_string(),
        ),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: 64,
        truncated: false,
    };
    let payload = serde_json::to_string(&envelope).expect("serialize envelope");
    let id = store
        .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
        .expect("enqueue");
    let claimed = store
        .claim_next("worker-heartbeat", 120)
        .expect("claim")
        .expect("message");
    assert_eq!(claimed.id, id);

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET claimed_at = ?2, heartbeat_at = ?2 WHERE id = ?1",
        rusqlite::params![id, now_secs() - 30],
    )
    .expect("age heartbeat");
    drop(conn);

    let attempts = Arc::new(AtomicUsize::new(0));
    let embedder = EventuallyOkEmbedder {
        attempts: Arc::clone(&attempts),
        fail_before_success: 3,
    };

    let config = mempal::core::config::Config::default();
    process_claimed_message_with_embedder(
        &Database::open(&db_path).expect("reopen db"),
        &store,
        "worker-heartbeat",
        &claimed,
        &embedder,
        DaemonIngestContext {
            prototype_classifier: None,
            config: &config,
            mempal_home: &mempal_home,
        },
    )
    .await
    .expect("process message");

    let conn = Connection::open(&db_path).expect("reopen sqlite");
    let (heartbeat_at, claimed_at): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT heartbeat_at, claimed_at FROM pending_messages WHERE id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query heartbeat");

    assert!(
        attempts.load(Ordering::SeqCst) >= 4,
        "expected retries before success"
    );
    assert!(
        heartbeat_at.unwrap_or_default() > claimed_at.unwrap_or_default(),
        "heartbeat must be refreshed during retry loop"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_daemon_hotpatch_preserves_project_scope_for_post_tool_use() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    let project_dir = tmp.path().join("workspace/project-alpha");
    fs::create_dir_all(project_dir.join("src")).expect("create project src");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    fs::write(project_dir.join("CLAUDE.md"), "# Project\n").expect("write claude");
    fs::write(project_dir.join("src/lib.rs"), "pub fn demo() {}\n").expect("write source");
    Database::open(&db_path).expect("open db");

    let store = PendingMessageStore::new(&db_path).expect("store");
    let hook_payload = serde_json::json!({
        "tool_name": "Edit",
        "input": "edit src/lib.rs",
        "tool_input": {
            "file_path": project_dir.join("src/lib.rs").display().to_string()
        },
        "output": "ok",
        "exit_code": 0
    });
    let envelope = CapturedHookEnvelope {
        event: HookEvent::PostToolUse.display_name().to_string(),
        kind: HookEvent::PostToolUse.queue_kind().to_string(),
        agent: "claude".to_string(),
        captured_at: "123".to_string(),
        claude_cwd: project_dir.display().to_string(),
        payload: Some(hook_payload.to_string()),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: hook_payload.to_string().len(),
        truncated: false,
    };
    let payload = serde_json::to_string(&envelope).expect("serialize envelope");
    let id = store
        .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
        .expect("enqueue");
    let claimed = store
        .claim_next("worker-hotpatch-project", 120)
        .expect("claim")
        .expect("message");
    assert_eq!(claimed.id, id);

    let config = mempal::core::config::Config::parse(&format!(
        r#"
db_path = "{}"

[project]
id = "project-alpha"

[search]
strict_project_isolation = true

[hotpatch]
enabled = true
min_importance_stars = 0
watch_files = ["CLAUDE.md"]
max_suggestion_length = 80
allowed_target_prefixes = ["{}"]

[hooks]
enabled = true
"#,
        db_path.display(),
        project_dir.parent().expect("workspace root").display(),
    ))
    .expect("parse config");
    let embedder = EventuallyOkEmbedder {
        attempts: Arc::new(AtomicUsize::new(0)),
        fail_before_success: 0,
    };

    let drawer_id = process_claimed_message_with_embedder(
        &Database::open(&db_path).expect("reopen db"),
        &store,
        "worker-hotpatch-project",
        &claimed,
        &embedder,
        DaemonIngestContext {
            prototype_classifier: None,
            config: &config,
            mempal_home: &mempal_home,
        },
    )
    .await
    .expect("process message");

    let conn = Connection::open(&db_path).expect("open sqlite");
    let project_id: Option<String> = conn
        .query_row(
            "SELECT project_id FROM drawers WHERE id = ?1",
            [&drawer_id],
            |row| row.get(0),
        )
        .expect("query drawer project");
    assert_eq!(project_id.as_deref(), Some("project-alpha"));

    let files = hotpatch_files(&mempal_home.join("hotpatch"));
    assert_eq!(files.len(), 1, "expected one hotpatch suggestion file");
    let content = fs::read_to_string(&files[0]).expect("read hotpatch file");
    assert!(content.contains("tool=Edit"));
}

#![cfg(feature = "integration")]

use async_trait::async_trait;
use mempal::core::AsyncDb;
use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mempal::xurl::model::Tool;
use mempal::xurl::store::{self, TurnFilter};
use rusqlite::{Connection, params};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

#[derive(Clone)]
struct DeterministicEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "deterministic"
    }
}

struct AutoIngestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    mempal_home: PathBuf,
    project_dir: PathBuf,
    config: Config,
    store: PendingMessageStore,
}

impl AutoIngestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        let hermes_home = tmp.path().join(".hermes");
        let project_dir = tmp.path().join("workspace/project-alpha");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        fs::create_dir_all(&hermes_home).expect("create hermes home");
        fs::create_dir_all(&project_dir).expect("create project dir");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open mempal db");
        write_hermes_state_db(&hermes_home.join("state.db"), &project_dir);

        let config_text = format!(
            r#"
db_path = "{}"

[project]
id = "project-alpha"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[hooks.session_end]
auto_ingest_conversation = true
hermes_profile = "default"
hermes_home = "{}"

[privacy]
enabled = false

[ingest_gating]
enabled = true

[[ingest_gating.rules]]
action = "reject"
content_contains = "drop-noise"

[search]
strict_project_isolation = true
progressive_disclosure = true
preview_chars = 48
"#,
            db_path.display(),
            hermes_home.display(),
        );
        let config = Config::parse(&config_text).expect("parse config");
        let store = PendingMessageStore::new(&db_path).expect("open pending store");
        Self {
            _tmp: tmp,
            db_path,
            mempal_home,
            project_dir,
            config,
            store,
        }
    }

    fn enqueue_session_end(&self) {
        let payload = serde_json::json!({
            "session_id": "hermes-auto-session",
            "agent": "hermes"
        });
        let envelope = CapturedHookEnvelope {
            event: HookEvent::SessionEnd.display_name().to_string(),
            kind: HookEvent::SessionEnd.queue_kind().to_string(),
            agent: "hermes".to_string(),
            captured_at: "1713000000".to_string(),
            claude_cwd: self.project_dir.display().to_string(),
            payload: Some(payload.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: payload.to_string().len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::SessionEnd.queue_kind(), &serialized)
            .expect("enqueue session end");
    }

    async fn process_once(&self) {
        let claimed = self
            .store
            .claim_next("worker-hermes-auto", 120)
            .expect("claim next")
            .expect("claimed message");
        let db = AsyncDb::open(&self.db_path, 4).expect("open async db");
        let async_store = AsyncPendingMessageStore::from_store(self.store.clone());
        let embedder = DeterministicEmbedder {
            vector: vec![0.2, 0.8, 0.4],
        };
        process_claimed_message_with_embedder(
            &db,
            &async_store,
            "worker-hermes-auto",
            &claimed,
            &embedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: None,
                config: &self.config,
                mempal_home: &self.mempal_home,
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect("process hook message");
    }
}

fn write_hermes_state_db(path: &Path, project_dir: &Path) {
    let conn = Connection::open(path).expect("open hermes state db");
    conn.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            source TEXT,
            cwd TEXT
        );
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp REAL NOT NULL,
            cwd TEXT
        );",
    )
    .expect("create hermes schema");
    conn.execute(
        "INSERT INTO sessions (id, title, source, cwd) VALUES (?1, ?2, ?3, ?4)",
        params![
            "hermes-auto-session",
            "Auto ingest regression",
            "hermes-test",
            project_dir.display().to_string()
        ],
    )
    .expect("insert session");
    let messages = [
        (
            "msg-keep-1",
            "user",
            "keep durable Hermes decision about automatic gated session import",
            1.0,
        ),
        (
            "msg-drop-1",
            "assistant",
            "drop-noise transient spinner output that must not be stored",
            2.0,
        ),
        (
            "msg-keep-2",
            "assistant",
            "keep follow-up implementation note for automatic Hermes ingest",
            3.0,
        ),
    ];
    for (id, role, content, timestamp) in messages {
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, timestamp, cwd) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                "hermes-auto-session",
                role,
                content,
                timestamp,
                project_dir.display().to_string(),
            ],
        )
        .expect("insert message");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_end_auto_ingests_hermes_turns_after_gate() {
    let env = AutoIngestEnv::new();
    env.enqueue_session_end();
    env.process_once().await;

    let db = Database::open(&env.db_path).expect("open db");
    let turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            session_id: Some("hermes-auto-session".to_string()),
            hermes_profile: Some("default".to_string()),
            limit: 10,
            ..Default::default()
        },
        true,
        true,
    )
    .expect("query hermes turns");
    let contents = turns
        .iter()
        .map(|turn| turn.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(contents.len(), 2);
    assert!(
        contents
            .iter()
            .any(|content| content.contains("durable Hermes decision"))
    );
    assert!(
        contents
            .iter()
            .any(|content| content.contains("implementation note"))
    );
    assert!(
        contents
            .iter()
            .all(|content| !content.contains("drop-noise"))
    );

    let vector_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id IN (
                SELECT id FROM conversation_turns WHERE tool = 'hermes' AND session_id = 'hermes-auto-session'
            )",
            [],
            |row| row.get(0),
        )
        .expect("count vectors");
    assert_eq!(vector_count, 2);

    let skipped_audit_rows: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM gating_audit WHERE decision = 'skip'",
            [],
            |row| row.get(0),
        )
        .expect("count skip audit rows");
    assert_eq!(skipped_audit_rows, 1);

    let skipped_previews_with_content: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM gating_audit WHERE decision = 'skip' AND content_preview IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count skip audit previews");
    assert_eq!(skipped_previews_with_content, 0);
}

//! Integration tests for Issue #235 Task 2: SessionEnd hook auto-ingestion.
//! Verifies that the daemon triggers conversation ingestion when a SessionEnd
//! event fires with `auto_ingest_conversation = true`.

#![cfg(feature = "integration")]

use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use tempfile::TempDir;

// ---- Minimal stub embedder ----

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "stub"
    }
}

// ---- Session JSONL fixture ----

fn session_jsonl(session_id: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"user","sessionId":"{sid}","uuid":"u1","message":{{"id":"m1","role":"user","content":[{{"type":"text","text":"Hello, help me debug this code."}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","uuid":"u2","message":{{"id":"m2","role":"assistant","content":[{{"type":"text","text":"Sure! Please share the code."}}]}}}}"#,
            "\n",
            r#"{{"type":"user","sessionId":"{sid}","uuid":"u3","message":{{"id":"m3","role":"user","content":[{{"type":"text","text":"Here is the function."}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","uuid":"u4","message":{{"id":"m4","role":"assistant","content":[{{"type":"text","text":"I see the issue. Missing return value."}}]}}}}"#,
        ),
        sid = session_id
    )
}

// ---- Helpers ----

fn count_conversation_drawers(db: &Database, session_id: &str) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE wing = 'conversation' AND room = ?1 AND deleted_at IS NULL",
            rusqlite::params![session_id],
            |row| row.get::<_, i64>(0),
        )
        .expect("count conversation drawers")
}

// ---- Test environment ----

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    mempal_home: PathBuf,
    config: Config,
    store: PendingMessageStore,
}

impl TestEnv {
    fn new(auto_ingest_conversation: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");

        let config_text = format!(
            r#"
db_path = "{db}"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[hooks.session_end]
extract_self_review = false
auto_ingest_conversation = {auto_ingest}

[privacy]
enabled = false

[ingest_gating]
enabled = false
"#,
            db = db_path.display(),
            auto_ingest = auto_ingest_conversation,
        );

        let config = Config::parse(&config_text).expect("parse config");
        let store = PendingMessageStore::new(&db_path).expect("open store");

        Self {
            _tmp: tmp,
            db_path,
            mempal_home,
            config,
            store,
        }
    }

    /// Enqueue a SessionEnd event whose payload includes `transcript_path`.
    fn enqueue_session_end_with_path(&self, session_id: &str, transcript_path: &str) -> String {
        let payload = serde_json::json!({
            "session_id": session_id,
            "transcript_path": transcript_path,
            "messages": [],
            "tool_calls": []
        });
        let envelope = CapturedHookEnvelope {
            event: HookEvent::SessionEnd.display_name().to_string(),
            kind: HookEvent::SessionEnd.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "1713000000".to_string(),
            claude_cwd: "/tmp/test-project".to_string(),
            payload: Some(payload.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: payload.to_string().len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::SessionEnd.queue_kind(), &serialized)
            .expect("enqueue session end")
    }

    /// Enqueue a SessionEnd event without a transcript path (no file found scenario).
    fn enqueue_session_end_no_path(&self, session_id: &str) -> String {
        let payload = serde_json::json!({
            "session_id": session_id,
            "messages": [],
            "tool_calls": []
        });
        let envelope = CapturedHookEnvelope {
            event: HookEvent::SessionEnd.display_name().to_string(),
            kind: HookEvent::SessionEnd.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "1713000000".to_string(),
            claude_cwd: "/nonexistent/project/path/xyz".to_string(),
            payload: Some(payload.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: payload.to_string().len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::SessionEnd.queue_kind(), &serialized)
            .expect("enqueue session end")
    }

    async fn process_once(&self) -> anyhow::Result<String> {
        let claimed = self
            .store
            .claim_next("worker-conv-ingest", 120)?
            .expect("claimed message");
        let db = Database::open(&self.db_path)?;
        process_claimed_message_with_embedder(
            &db,
            &self.store,
            "worker-conv-ingest",
            &claimed,
            &StubEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &self.config,
                mempal_home: &self.mempal_home,
            },
        )
        .await
    }
}

// ---- Tests ----

#[tokio::test]
async fn test_session_end_triggers_conversation_ingestion() {
    let env = TestEnv::new(true);
    let session_id = "auto-ingest-session";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = env._tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    env.enqueue_session_end_with_path(session_id, &jsonl_path.to_string_lossy());
    env.process_once().await.expect("process session end");

    let db = Database::open(&env.db_path).expect("open db");
    let count = count_conversation_drawers(&db, session_id);
    assert!(
        count > 0,
        "expected conversation drawers to be created, got 0"
    );
}

#[tokio::test]
async fn test_session_end_conversation_drawers_have_correct_wing_room() {
    let env = TestEnv::new(true);
    let session_id = "wing-room-check-session";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = env._tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    env.enqueue_session_end_with_path(session_id, &jsonl_path.to_string_lossy());
    env.process_once().await.expect("process");

    let db = Database::open(&env.db_path).expect("open db");
    let wrong_wing: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE room = ?1 AND wing != 'conversation' AND deleted_at IS NULL",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .expect("query");
    assert_eq!(
        wrong_wing, 0,
        "all session drawers must have wing='conversation'"
    );

    let correct: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE wing = 'conversation' AND room = ?1 AND deleted_at IS NULL",
            rusqlite::params![session_id],
            |row| row.get(0),
        )
        .expect("query");
    assert!(
        correct > 0,
        "must have drawers with wing='conversation' room=session_id"
    );
}

#[tokio::test]
async fn test_session_end_dedup_guard_skips_already_ingested() {
    let env = TestEnv::new(true);
    let session_id = "dedup-auto-ingest";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = env._tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    // Pre-ingest manually to simulate prior CLI ingestion.
    {
        let db = Database::open(&env.db_path).expect("open db");
        let opts = IngestOptions {
            room: Some(session_id),
            source_root: jsonl_path.parent(),
            dry_run: false,
            ..IngestOptions::default()
        };
        ingest_file_with_options(&db, &StubEmbedder, &jsonl_path, "conversation", opts)
            .await
            .expect("pre-ingest");
    }

    let db = Database::open(&env.db_path).expect("open db");
    let count_before = count_conversation_drawers(&db, session_id);
    drop(db);

    // Now trigger SessionEnd - dedup guard should skip re-ingestion.
    env.enqueue_session_end_with_path(session_id, &jsonl_path.to_string_lossy());
    env.process_once().await.expect("process session end");

    let db = Database::open(&env.db_path).expect("open db");
    let count_after = count_conversation_drawers(&db, session_id);
    assert_eq!(
        count_before, count_after,
        "drawer count must not grow when session was already ingested (dedup guard)"
    );
}

#[tokio::test]
async fn test_session_end_missing_file_is_nonfatal() {
    let env = TestEnv::new(true);
    let session_id = "missing-file-session";

    // Enqueue without any valid transcript path — file doesn't exist.
    env.enqueue_session_end_no_path(session_id);
    // Must not return an error even though the JSONL file is missing.
    let result = env.process_once().await;
    assert!(
        result.is_ok(),
        "missing JSONL file must not cause daemon message processing to fail: {result:?}"
    );

    let db = Database::open(&env.db_path).expect("open db");
    let count = count_conversation_drawers(&db, session_id);
    assert_eq!(count, 0, "no conversation drawers when file is missing");
}

#[tokio::test]
async fn test_session_end_disabled_by_default() {
    // auto_ingest_conversation defaults to false.
    let env = TestEnv::new(false);
    let session_id = "disabled-feature-session";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = env._tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    env.enqueue_session_end_with_path(session_id, &jsonl_path.to_string_lossy());
    env.process_once().await.expect("process session end");

    let db = Database::open(&env.db_path).expect("open db");
    let count = count_conversation_drawers(&db, session_id);
    assert_eq!(
        count, 0,
        "no conversation drawers when auto_ingest_conversation=false"
    );
}

#[tokio::test]
async fn test_session_end_invalid_jsonl_is_nonfatal() {
    let env = TestEnv::new(true);
    let session_id = "invalid-jsonl-session";

    // Write garbage that isn't valid CC session JSONL.
    let bad_jsonl_path = env._tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&bad_jsonl_path, b"not valid jsonl at all\x00\x01\x02").expect("write bad file");

    env.enqueue_session_end_with_path(session_id, &bad_jsonl_path.to_string_lossy());
    // Even if the file fails to ingest, the daemon message must not fail.
    let result = env.process_once().await;
    assert!(
        result.is_ok(),
        "invalid JSONL must not cause daemon message processing to fail: {result:?}"
    );
}

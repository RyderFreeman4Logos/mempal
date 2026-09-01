use std::sync::atomic::{AtomicUsize, Ordering};

use mempal::core::{
    AsyncDb,
    config::{Config, LlmJudgeConfig},
    db::Database,
    queue::{AsyncPendingMessageStore, PendingMessageStore},
};
use mempal::daemon::{
    DaemonIngestContext, HookLlmGateRuntime, process_claimed_message_with_embedder,
};
use mempal::embed::Embedder;
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use rusqlite::Connection;

struct CountingEmbedder {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "stale-pre-model-reclaim"
    }
}

#[tokio::test]
async fn stale_pre_model_reclaim_replays_gating_without_reingesting_completed_drawer() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
    let store = PendingMessageStore::new(&db_path).expect("open queue");
    let async_store = AsyncPendingMessageStore::from_store(store.clone());
    let hook_payload = serde_json::json!({
        "tool_name": "DesignCapture",
        "input": "record stale pre-model replay",
        "output": "This automatic capture must replay its model gates after a stale reclaim.",
        "exit_code": 0
    })
    .to_string();
    let envelope = CapturedHookEnvelope {
        event: HookEvent::PostToolUse.display_name().to_string(),
        kind: HookEvent::PostToolUse.queue_kind().to_string(),
        agent: "codex".to_string(),
        captured_at: "2026-05-01T12:34:56Z".to_string(),
        claude_cwd: tmp.path().to_string_lossy().to_string(),
        payload: Some(hook_payload.clone()),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: hook_payload.len(),
        truncated: false,
    };
    let message_id = store
        .enqueue(
            HookEvent::PostToolUse.queue_kind(),
            &serde_json::to_string(&envelope).expect("serialize envelope"),
        )
        .expect("enqueue hook envelope");
    let first_claim = store
        .claim_next("first-worker", 60)
        .expect("claim first worker")
        .expect("first claim");

    let mut config = Config::default();
    config.llm.enabled = true;
    config.llm.model = Some("test-llm".to_string());
    config.llm.enabled_for = vec!["gating".to_string()];
    config.ingest_gating.enabled = true;
    config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
        enabled: true,
        ..LlmJudgeConfig::default()
    });
    let embedder = CountingEmbedder {
        calls: AtomicUsize::new(0),
    };

    process_claimed_message_with_embedder(
        &async_db,
        &async_store,
        "first-worker",
        &first_claim,
        &embedder,
        DaemonIngestContext {
            prototype_classifier: None,
            llm_gate: None,
            config: &config,
            mempal_home: tmp.path(),
            runtime_writer_lease: None,
            heartbeat_trigger: None,
        },
    )
    .await
    .expect_err("missing gate must leave a persist-before-model admission");
    let drawer_id: String = db
        .conn()
        .query_row("SELECT id FROM drawers", [], |row| row.get(0))
        .expect("query admitted drawer");
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 0);

    Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET claimed_at = 0, heartbeat_at = 0 WHERE id = ?1",
            [message_id.as_str()],
        )
        .expect("age first claim");
    assert_eq!(store.reclaim_stale(0).expect("reclaim stale claim"), 1);
    let reclaimed = store
        .claim_next("replay-worker", 60)
        .expect("claim reclaimed message")
        .expect("reclaimed message");
    assert_eq!(reclaimed.retry_count, 0);

    let mut llm_server = mockito::Server::new_async().await;
    let llm_mock = llm_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(
            serde_json::json!({
                "model": "test-llm",
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": serde_json::json!({"verdict": "keep", "score": 0.95}).to_string()
                    }
                }]
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
    let llm_gate = HookLlmGateRuntime::new(&config.llm);

    process_claimed_message_with_embedder(
        &async_db,
        &async_store,
        "replay-worker",
        &reclaimed,
        &embedder,
        DaemonIngestContext {
            prototype_classifier: None,
            llm_gate: Some(&llm_gate),
            config: &config,
            mempal_home: tmp.path(),
            runtime_writer_lease: None,
            heartbeat_trigger: None,
        },
    )
    .await
    .expect("stale pre-model reclaim must replay the LLM and embed gates");
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        db.conn()
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row
                .get::<_, i64>(0))
            .expect("count drawers"),
        1
    );
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                [drawer_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count drawer vectors"),
        1
    );

    process_claimed_message_with_embedder(
        &async_db,
        &async_store,
        "replay-worker",
        &reclaimed,
        &embedder,
        DaemonIngestContext {
            prototype_classifier: None,
            llm_gate: Some(&llm_gate),
            config: &config,
            mempal_home: tmp.path(),
            runtime_writer_lease: None,
            heartbeat_trigger: None,
        },
    )
    .await
    .expect("completed retry-zero ingest must remain idempotent");
    assert_eq!(embedder.calls.load(Ordering::SeqCst), 1);
    llm_mock.assert_async().await;
}

//! Tests for fix/llm-worker-hot-reload (issue #176):
//! LLM workers stuck after config hot-reload model change.

use std::sync::Mutex;
use std::time::Duration;

use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;

// Tests in this file share the global HotReloadState (OnceLock singleton).
// Serialize them to prevent cross-test config snapshot contamination.
static TEST_LOCK: Mutex<()> = Mutex::new(());

// ── helpers ─────────────────────────────────────────────────────────────────

fn make_store(db: &Database) -> PendingMessageStore {
    PendingMessageStore::new(db.path()).expect("open queue")
}

// ── release_claim ────────────────────────────────────────────────────────────

#[test]
fn test_release_claim_returns_task_to_pending() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let store = make_store(&db);

    let id = store
        .enqueue("llm_task", r#"{"type":"test"}"#)
        .expect("enqueue");

    // Claim the task.
    let msg = store
        .claim_next_by_kind("test-worker", 300, "llm_task")
        .expect("claim_next_by_kind")
        .expect("claimed message");
    assert_eq!(msg.id, id);
    let op_state: String = db
        .conn()
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("read claimed op_state");
    assert_eq!(op_state, "running");

    let stats_claimed = store.stats().expect("stats");
    assert_eq!(stats_claimed.claimed, 1);
    assert_eq!(stats_claimed.pending, 0);

    // Release back to pending — simulates worker cancellation due to config change.
    store.release_claim(&id).expect("release_claim");
    let released_op_state: String = db
        .conn()
        .query_row(
            "SELECT op_state FROM pending_messages WHERE id = ?1",
            [&id],
            |row| row.get::<_, String>(0),
        )
        .expect("read released op_state");
    assert_eq!(released_op_state, "queued");

    let stats_after = store.stats().expect("stats after release");
    assert_eq!(stats_after.claimed, 0, "task must be pending after release");
    assert_eq!(stats_after.pending, 1, "task must be pending after release");
    assert_eq!(
        stats_after.failed, 0,
        "release_claim must not increment retry count or mark as failed"
    );
}

#[test]
fn test_release_claim_does_not_increment_retry_count() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let store = make_store(&db);

    let id = store.enqueue("llm_task", "{}").expect("enqueue");
    let msg = store
        .claim_next_by_kind("worker", 300, "llm_task")
        .expect("claim_next_by_kind")
        .expect("message");
    assert_eq!(msg.retry_count, 0);

    store.release_claim(&id).expect("release_claim");

    // Claim again — retry_count must still be 0.
    let msg2 = store
        .claim_next_by_kind("worker", 300, "llm_task")
        .expect("claim_next_by_kind")
        .expect("message after release");
    assert_eq!(
        msg2.retry_count, 0,
        "release_claim must not increment retry_count"
    );
}

#[test]
fn test_release_claim_returns_error_for_unknown_id() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let store = make_store(&db);

    let result = store.release_claim("nonexistent-id");
    assert!(
        result.is_err(),
        "release_claim on unknown id must return MessageNotFound"
    );
}

// ── LLM generation watch channel ─────────────────────────────────────────────

#[test]
fn test_llm_gen_increments_on_model_change() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config_path = tmp.path().join("config.toml");

    // Bootstrap with model-a so the global snapshot is set.
    std::fs::write(
        &config_path,
        r#"
[llm]
enabled = true
base_url = "http://127.0.0.1:19999/v1"
model = "model-a"
"#,
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap");

    let mut rx = ConfigHandle::subscribe_llm_gen();
    let gen_before = *rx.borrow_and_update();

    // Write model-b and trigger reload_from_disk directly (bypasses watcher).
    std::fs::write(
        &config_path,
        r#"
[llm]
enabled = true
base_url = "http://127.0.0.1:19999/v1"
model = "model-b"
"#,
    )
    .expect("write config");
    ConfigHandle::harness_reload_from_path(&config_path);

    let gen_after = *rx.borrow_and_update();
    assert!(
        gen_after > gen_before,
        "generation must increment when llm.model changes (before={gen_before}, after={gen_after})"
    );
}

#[test]
fn test_llm_gen_does_not_increment_on_unrelated_change() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let config_path = tmp.path().join("config.toml");

    std::fs::write(
        &config_path,
        r#"
[llm]
enabled = true
base_url = "http://127.0.0.1:19999/v1"
model = "model-stable"
"#,
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap");

    let mut rx = ConfigHandle::subscribe_llm_gen();
    let gen_before = *rx.borrow_and_update();

    // Change a non-LLM field (search.preview_chars) only.
    std::fs::write(
        &config_path,
        r#"
[llm]
enabled = true
base_url = "http://127.0.0.1:19999/v1"
model = "model-stable"

[search]
preview_chars = 200
"#,
    )
    .expect("write config");
    ConfigHandle::harness_reload_from_path(&config_path);

    let gen_after = *rx.borrow_and_update();
    assert_eq!(
        gen_after, gen_before,
        "generation must NOT change for non-LLM config changes"
    );
}

// ── tokio::select! cancellation ───────────────────────────────────────────────

/// Simulates the worker's tokio::select! pattern: a slow async operation races
/// against an LLM generation change signal and is cancelled when the signal fires.
#[tokio::test]
async fn test_worker_select_cancels_on_gen_change() {
    let (gen_tx, mut gen_rx) = tokio::sync::watch::channel(0u64);

    // Mark current value as seen so `changed()` only fires for NEW values.
    let _ = gen_rx.borrow_and_update();

    // Spawn a task that bumps the generation after a short delay.
    let handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = gen_tx.send(1u64);
    });

    let slow_op = async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok::<(), anyhow::Error>(())
    };

    // The slow_op should be cancelled by gen_rx.changed() well before 60s.
    let result: Option<Result<(), anyhow::Error>> = tokio::select! {
        r = slow_op => Some(r),
        _ = gen_rx.changed() => None,
    };

    assert!(
        result.is_none(),
        "slow operation must be cancelled when generation changes"
    );
    handle.await.expect("spawned task");
}

/// Verify that the per-request timeout on LlmClient is set from config.
#[test]
fn test_llm_client_has_request_timeout_from_config() {
    use mempal::core::config::LlmConfig;
    use mempal::llm::LlmClient;

    let config = LlmConfig {
        enabled: true,
        base_url: Some("http://127.0.0.1:19999/v1".to_string()),
        model: Some("test-model".to_string()),
        request_timeout_secs: 42,
        ..Default::default()
    };

    // LlmClient::from_config must succeed (it builds the reqwest client with
    // the timeout). The timeout value is baked into the reqwest::Client and
    // cannot be read back directly, so we just verify construction succeeds
    // with a non-default timeout.
    let client = LlmClient::from_config(&config)
        .expect("LlmClient must build with custom request_timeout_secs");

    // Sanity-check that the client was created (indirectly validates the
    // timeout parameter was accepted by reqwest without panicking).
    assert_eq!(
        client.current_max_concurrent(),
        config.max_concurrent.max(1)
    );
}

// ── queue release idempotency ────────────────────────────────────────────────

#[test]
fn test_release_claim_on_already_pending_is_error() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let store = make_store(&db);

    let id = store.enqueue("llm_task", "{}").expect("enqueue");
    // Task is pending (not claimed) — release_claim should return error.
    let result = store.release_claim(&id);
    assert!(
        result.is_err(),
        "release_claim on a pending (unclaimed) task must fail"
    );
}

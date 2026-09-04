use super::*;
use crate::core::config::{Config, ConfigHandle, IngestGatingConfig, LlmJudgeConfig};
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use crate::daemon_bootstrap::DaemonWriteObserver;
use crate::ingest::gating::GatingDecision;
use rusqlite::params;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Notify;

#[path = "worker_contention_tests.rs"]
mod contention_tests;

pub(crate) fn shared_llm_client_runtime_with_worker_test_lock(
    config: &LlmConfig,
    worker_test_lock: tokio::sync::OwnedMutexGuard<()>,
) -> SharedLlmClientRuntime {
    SharedLlmClientRuntime {
        inner: Arc::new(std::sync::Mutex::new(LlmClientRuntime::new(config))),
        _worker_test_lock: Arc::new(worker_test_lock),
    }
}

fn spawn_runtime_ticker() -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let ticks = Arc::new(AtomicU64::new(0));
    let ticks_bg = Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ticks_bg.fetch_add(1, Ordering::SeqCst);
        }
    });
    (ticks, ticker)
}

fn assert_runtime_ticked(ticks: &AtomicU64, label: &str) {
    let observed = ticks.load(Ordering::SeqCst);
    assert!(
        observed >= 5,
        "{label} advanced ticker {observed} times; LLM DB verdict work must not block Tokio worker"
    );
}

#[test]
fn llm_idle_poll_interval_backs_off_exponentially_and_caps() {
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 0),
        Duration::from_millis(500)
    );
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 1),
        Duration::from_secs(1)
    );
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 2),
        Duration::from_secs(2)
    );
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 3),
        Duration::from_secs(4)
    );
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 4),
        Duration::from_secs(5)
    );
    assert_eq!(
        super::llm_idle_poll_interval(LLM_POLL_INTERVAL, 20),
        Duration::from_secs(5)
    );
}

fn insert_drawer(db: &Database, id: &str) {
    db.insert_drawer(&Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: "LLM verdict runtime liveness drawer".to_string(),
        wing: "llm".to_string(),
        room: Some("runtime".to_string()),
        source_file: Some("llm-runtime.md".to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 3,
    }))
    .expect("insert drawer");
}

fn drawer_is_deleted(db: &Database, id: &str) -> bool {
    db.conn()
        .query_row(
            "SELECT deleted_at IS NOT NULL FROM drawers WHERE id = ?1",
            params![id],
            |row| row.get::<_, bool>(0),
        )
        .expect("read drawer deletion state")
}

fn record_pending_llm_audit(db: &Database, id: &str) {
    let decision = GatingDecision::accepted(0, Some("llm_pending".to_string()), None);
    db.record_gating_audit(id, &decision, None, Some("judge me"))
        .expect("record pending LLM audit row");
}

fn llm_judge_config(threshold: f64) -> Config {
    Config {
        ingest_gating: IngestGatingConfig {
            llm_judge: Some(LlmJudgeConfig {
                enabled: true,
                threshold,
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn worker_test_config(base_url: &str) -> String {
    format!(
        r#"
[config_hot_reload]
enabled = false

[llm]
enabled = true
base_url = "{base_url}"
model = "test-model"
enabled_for = ["gating"]
max_concurrent = 1
retry_interval_secs = 1
request_timeout_secs = 5

[ingest_gating.llm_judge]
enabled = true
threshold = 0.5
"#
    )
}

fn worker_endpoint_pool_config(primary_base_url: &str, secondary_base_url: &str) -> String {
    format!(
        r#"
[config_hot_reload]
enabled = false

[llm]
enabled = true
enabled_for = ["gating"]
max_concurrent = 1
retry_interval_secs = 1
request_timeout_secs = 5

[[llm.endpoints]]
id = "primary"
base_url = "{primary_base_url}"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "{secondary_base_url}"
model = "secondary-model"

[ingest_gating.llm_judge]
enabled = true
threshold = 0.5
"#
    )
}

async fn spawn_counting_llm_server(
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_counting_llm_server_with_expected_content(count, notify, None).await
}

async fn spawn_counting_llm_server_with_expected_content(
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
    expected_content: Option<String>,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::post};

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(request): Json<LlmRequest>| {
            let count = Arc::clone(&count);
            let notify = Arc::clone(&notify);
            let expected_content = expected_content.clone();
            async move {
                if expected_content.as_deref().is_none_or(|expected| {
                    request
                        .messages
                        .iter()
                        .any(|message| message.role == "user" && message.content.contains(expected))
                }) {
                    count.fetch_add(1, Ordering::SeqCst);
                    notify.notify_one();
                }
                Json(serde_json::json!({
                    "id": "test",
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": "{\"verdict\":\"keep\",\"score\":0.9}"
                        },
                        "finish_reason": "stop"
                    }],
                    "model": "test-model",
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2
                    }
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test LLM server");
    let addr = listener.local_addr().expect("test LLM server address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve test LLM server");
    });
    (format!("http://{addr}/v1"), handle)
}

async fn spawn_failing_llm_server(
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Router, http::StatusCode, routing::post};

    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let count = Arc::clone(&count);
            let notify = Arc::clone(&notify);
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                notify.notify_one();
                (StatusCode::INTERNAL_SERVER_ERROR, "server error")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind failing test LLM server");
    let addr = listener
        .local_addr()
        .expect("failing test LLM server address");
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve failing test LLM server");
    });
    (format!("http://{addr}/v1"), handle)
}

fn gating_task(id: &str) -> LlmTaskPayload {
    LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: id.to_string(),
        drawer_ids: vec![id.to_string()],
        content: "judge me".to_string(),
        system_prompt: None,
    }
}

#[test]
fn gating_task_constructor_bounds_utf8_content_and_records_original_size() {
    let content = "界".repeat((MAX_LLM_GATE_CONTENT_BYTES / 3) + 100);
    let task = LlmTaskPayload::for_gating(vec!["drawer-bounded".to_string()], &content, None);

    assert!(task.content.len() <= MAX_LLM_GATE_CONTENT_BYTES);
    assert!(task.content.is_char_boundary(task.content.len()));
    assert!(
        task.content
            .contains(&format!("original_content_bytes={}", content.len()))
    );
    assert!(
        task.content
            .contains(&format!("limit_bytes={MAX_LLM_GATE_CONTENT_BYTES}"))
    );
}

#[test]
fn gating_request_bounds_legacy_unbounded_task_content() {
    let secret = "LEGACY_LLM_GATE_SECRET_DO_NOT_COPY";
    let mut content = "x".repeat(MAX_LLM_GATE_CONTENT_BYTES);
    content.push_str(secret);
    let task = LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: "legacy-drawer".to_string(),
        drawer_ids: vec!["legacy-drawer".to_string()],
        content,
        system_prompt: None,
    };

    let request = gating_request(&task);
    let user_content = &request.messages[1].content;
    assert!(user_content.len() <= MAX_LLM_GATE_CONTENT_BYTES);
    assert!(!user_content.contains(secret));
}

fn llm_audit_verdict(db: &Database, id: &str) -> (String, f64) {
    db.conn()
        .query_row(
            "SELECT llm_verdict, llm_score FROM gating_audit WHERE drawer_id = ?1",
            params![id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )
        .expect("read LLM audit verdict")
}

#[test]
fn test_strict_gating_parser_accepts_default_score_reason_keep_shape() {
    let (verdict, score) =
        parse_strict_gating_verdict(r#"{"score":0.95,"reason":"important design note"}"#)
            .expect("default score/reason response should parse");
    let outcome = effective_gating_outcome(&llm_judge_config(0.7), &verdict, score);

    assert_eq!(verdict, LLM_VERDICT_KEEP);
    assert!((score - 0.95).abs() < f64::EPSILON);
    assert_eq!(outcome.verdict, GatingRetentionVerdict::Keep);
}

#[test]
fn test_strict_gating_parser_accepts_default_score_reason_reject_shape() {
    let (verdict, score) =
        parse_strict_gating_verdict(r#"{"score":0.12,"reason":"routine tool output"}"#)
            .expect("default score/reason response should parse");
    let outcome = effective_gating_outcome(&llm_judge_config(0.7), &verdict, score);

    assert_eq!(verdict, LLM_VERDICT_KEEP);
    assert!((score - 0.12).abs() < f64::EPSILON);
    assert_eq!(outcome.verdict, GatingRetentionVerdict::Reject);
}

#[test]
fn test_strict_gating_parser_rejects_ambiguous_score_without_reason_or_verdict() {
    let error = parse_strict_gating_verdict(r#"{"score":0.95}"#)
        .expect_err("score-only response is ambiguous under the default prompt contract");

    assert!(
        error.to_string().contains("reason") || error.to_string().contains("verdict"),
        "unexpected parser error: {error:#}"
    );
}

#[test]
fn test_strict_gating_parser_does_not_echo_unsupported_verdict_value() {
    let raw_verdict = "private echoed model fragment";
    let error =
        parse_strict_gating_verdict(&format!(r#"{{"score":0.95,"verdict":"{raw_verdict}"}}"#))
            .expect_err("unsupported verdict should fail strict parsing");
    let error_text = error.to_string();

    assert!(
        error_text.contains("verdict"),
        "unexpected parser error: {error:#}"
    );
    assert!(
        !error_text.contains(raw_verdict),
        "strict parser error must not echo model-provided verdict text: {error:#}"
    );
}
#[test]
fn test_llm_task_failure_disposition_uses_retry_after_as_queue_delay() {
    let error = Err::<(), _>(LlmError::TemporarilyUnavailable {
        retry_after: Duration::from_secs(7),
        reason: "model_cooldown".to_string(),
        http_status: None,
    })
    .context("LLM gating request failed")
    .expect_err("synthetic error");

    assert_eq!(
        llm_task_failure_disposition(&error),
        QueueFailureDisposition::RetryableAfter { delay_ms: 7_000 }
    );
}

#[test]
fn test_llm_task_failure_disposition_terminals_non_retryable_errors() {
    let error = Err::<(), _>(LlmError::ClientError {
        status: reqwest::StatusCode::BAD_REQUEST,
        body: "invalid model".to_string(),
        retry_after: None,
    })
    .context("LLM gating request failed")
    .expect_err("synthetic error");

    assert_eq!(
        llm_task_failure_disposition(&error),
        QueueFailureDisposition::Terminal
    );
}

struct ConfigHarnessResetGuard;

impl Drop for ConfigHarnessResetGuard {
    fn drop(&mut self) {
        ConfigHandle::harness_reset();
    }
}

fn with_isolated_llm_worker_runtime(test: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("llm worker test runtime")
        .block_on(test);
}

#[test]
fn test_worker_observes_client_preparation_release_lock() {
    with_isolated_llm_worker_runtime(async {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let _guard = crate::core::config::global_config_test_lock()
            .lock_owned()
            .await;
        let _shutdown_guard = crate::daemon::global_shutdown_test_lock()
            .lock_owned()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _config_reset = ConfigHarnessResetGuard;
        let config_path = tmp.path().join("config.toml");
        let db_path = tmp.path().join("palace.db");
        std::fs::write(
        &config_path,
        "[config_hot_reload]\nenabled = false\n[llm]\nenabled = true\nenabled_for = [\"gating\"]\nbase_url = \"https://example.com/v1\"\nmodel = \"test-model\"\nretry_interval_secs = 1\n[privacy.remote_calls]\nfail_closed = true\n",
    )
    .expect("write unavailable LLM config");
        ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap unavailable LLM config");

        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        store
            .enqueue(LLM_TASK_KIND, "{}")
            .expect("enqueue LLM task");
        let async_store =
            AsyncPendingMessageStore::from_store(store).with_release_lock_failures_for_test(1);
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = shared_llm_client_runtime_with_worker_test_lock(
            &ConfigHandle::current().llm,
            worker_test_lock,
        );
        let test_lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-worker-test", 300, None)
            .expect("acquire test lease")
            .expect("test lease available");
        let observer = DaemonWriteObserver::for_test();
        let worker = tokio::spawn(run_llm_worker(
            Arc::new(async_store),
            client_runtime,
            Arc::new(LlmStatus::new(5)),
            async_db,
            observer.clone(),
            test_lease,
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if observer
                    .last_error_for_test()
                    .is_some_and(|(error, is_lock)| {
                        is_lock && error.contains("client preparation failure")
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("client-preparation release lock should reach the typed observer");
        worker.abort();
        let _ = worker.await;
    });
}

#[test]
fn test_worker_survives_confirm_lock_contention() {
    with_isolated_llm_worker_runtime(async {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let _guard = crate::core::config::global_config_test_lock()
            .lock_owned()
            .await;
        let _shutdown_guard = crate::daemon::global_shutdown_test_lock()
            .lock_owned()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _config_reset = ConfigHarnessResetGuard;
        let config_path = tmp.path().join("config.toml");
        let db_path = tmp.path().join("palace.db");
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_notify = Arc::new(Notify::new());
        let (base_url, server) =
            spawn_counting_llm_server(Arc::clone(&request_count), request_notify).await;
        std::fs::write(&config_path, worker_test_config(&base_url)).expect("write worker config");
        ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap worker config");

        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        for drawer_id in ["confirm-lock-first", "confirm-lock-second"] {
            insert_drawer(&db, drawer_id);
            record_pending_llm_audit(&db, drawer_id);
            store
                .enqueue(
                    LLM_TASK_KIND,
                    &serde_json::to_string(&gating_task(drawer_id)).expect("serialize task"),
                )
                .expect("enqueue LLM task");
        }
        let async_store = AsyncPendingMessageStore::from_store(store.clone())
            .with_complete_lock_failures_for_test(1);
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = shared_llm_client_runtime_with_worker_test_lock(
            &ConfigHandle::current().llm,
            worker_test_lock,
        );
        let test_lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-worker-test", 300, None)
            .expect("acquire test lease")
            .expect("test lease available");
        let (worker_completed, completion) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(run_llm_worker_inner(
            Arc::new(async_store),
            client_runtime,
            Arc::new(LlmStatus::new(5)),
            async_db,
            DaemonWriteObserver::for_test(),
            test_lease,
            Some(worker_completed),
        ));

        tokio::time::timeout(Duration::from_secs(20), completion)
            .await
            .expect("worker should retain capacity after confirm lock")
            .expect("worker completion observer should remain connected");
        worker.abort();
        let _ = worker.await;
        server.abort();
        let _ = server.await;

        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        let stats = store.stats().expect("queue stats");
        assert_eq!((stats.pending, stats.claimed), (0, 1));
    });
}

#[test]
fn test_worker_uses_reloaded_client_when_generation_changes_before_claim_returns() {
    with_isolated_llm_worker_runtime(async {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let _guard = crate::core::config::global_config_test_lock()
            .lock_owned()
            .await;
        let _shutdown_guard = crate::daemon::global_shutdown_test_lock()
            .lock_owned()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // Declared after `tmp` so Drop resets before TempDir deletion.
        let _config_reset = ConfigHarnessResetGuard;
        let config_path = tmp.path().join("config.toml");
        let db_path = tmp.path().join("palace.db");

        let old_count = Arc::new(AtomicUsize::new(0));
        let new_count = Arc::new(AtomicUsize::new(0));
        let old_notify = Arc::new(Notify::new());
        let new_notify = Arc::new(Notify::new());
        let (old_base_url, old_server) =
            spawn_counting_llm_server(Arc::clone(&old_count), Arc::clone(&old_notify)).await;
        let (new_base_url, new_server) =
            spawn_counting_llm_server(Arc::clone(&new_count), Arc::clone(&new_notify)).await;

        std::fs::write(&config_path, worker_test_config(&old_base_url)).expect("write old config");
        ConfigHandle::bootstrap_quiet(&config_path).expect("bootstrap old config");

        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let task = LlmTaskPayload {
            task_type: "gating".to_string(),
            drawer_id: "claim-after-reload-drawer".to_string(),
            drawer_ids: vec![],
            content: "claim-after-reload content".to_string(),
            system_prompt: None,
        };
        store
            .enqueue(
                LLM_TASK_KIND,
                &serde_json::to_string(&task).expect("serialize task"),
            )
            .expect("enqueue LLM task");

        let async_store = AsyncPendingMessageStore::from_store(store)
            .with_blocking_delay(Duration::from_millis(500));
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let client_runtime = shared_llm_client_runtime_with_worker_test_lock(
            &ConfigHandle::current().llm,
            worker_test_lock,
        );
        let test_lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-worker-test", 300, None)
            .expect("acquire test lease")
            .expect("test lease available");
        let worker = tokio::spawn(run_llm_worker(
            Arc::new(async_store),
            client_runtime,
            Arc::new(LlmStatus::new(5)),
            async_db,
            DaemonWriteObserver::for_test(),
            test_lease,
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        std::fs::write(&config_path, worker_test_config(&new_base_url)).expect("write new config");
        ConfigHandle::harness_reload_from_path(&config_path);

        let observed_endpoint = tokio::select! {
            _ = new_notify.notified() => "new",
            _ = old_notify.notified() => "old",
            _ = tokio::time::sleep(Duration::from_secs(5)) => "timeout",
        };

        worker.abort();
        let _ = worker.await;
        old_server.abort();
        new_server.abort();
        let _ = old_server.await;
        let _ = new_server.await;

        assert_eq!(
            observed_endpoint, "new",
            "task claimed after LLM generation reload must use the fresh client"
        );
        assert_eq!(
            old_count.load(Ordering::SeqCst),
            0,
            "stale pre-reload client must not process a claim returned after reload"
        );
        assert_eq!(new_count.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn test_below_threshold_keep_verdict_becomes_reject_and_soft_deletes() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    insert_drawer(&db, "llm-low-score-keep");
    record_pending_llm_audit(&db, "llm-low-score-keep");
    let task = gating_task("llm-low-score-keep");
    let config = llm_judge_config(0.6);

    apply_gating_verdict(&db, &task, &config, "keep", 0.2).expect("apply verdict");

    assert!(drawer_is_deleted(&db, "llm-low-score-keep"));
    assert_eq!(
        llm_audit_verdict(&db, "llm-low-score-keep"),
        ("reject".to_string(), 0.2)
    );
    assert_eq!(effective_retention_verdict("keep", 0.2, 0.6), "reject");
}

#[test]
fn test_above_threshold_keep_verdict_stays_keep_without_soft_delete() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    insert_drawer(&db, "llm-high-score-keep");
    record_pending_llm_audit(&db, "llm-high-score-keep");
    let task = gating_task("llm-high-score-keep");
    let config = llm_judge_config(0.6);

    apply_gating_verdict(&db, &task, &config, "keep", 0.9).expect("apply verdict");

    assert!(!drawer_is_deleted(&db, "llm-high-score-keep"));
    assert_eq!(
        llm_audit_verdict(&db, "llm-high-score-keep"),
        ("keep".to_string(), 0.9)
    );
    assert_eq!(effective_retention_verdict("keep", 0.9, 0.6), "keep");
}

#[tokio::test(flavor = "current_thread")]
async fn test_llm_verdict_db_work_runs_off_runtime() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    insert_drawer(&db, "llm-verdict-offruntime");
    let async_db = AsyncDb::open(&db_path, 4)
        .expect("open async db")
        .with_write_delay(Duration::from_millis(300));
    let task = LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: "llm-verdict-offruntime".to_string(),
        drawer_ids: vec!["llm-verdict-offruntime".to_string()],
        content: "judge me".to_string(),
        system_prompt: None,
    };
    let (ticks, ticker) = spawn_runtime_ticker();

    let test_lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "test", "llm-verdict-test", 300, None)
        .expect("acquire test lease")
        .expect("test lease available");
    apply_gating_verdict_async(
        &async_db,
        task,
        Config::default(),
        "reject".to_string(),
        0.1,
        &test_lease,
    )
    .await
    .expect("apply verdict");
    ticker.abort();

    assert_runtime_ticked(&ticks, "LLM verdict");
    assert!(drawer_is_deleted(&db, "llm-verdict-offruntime"));
}

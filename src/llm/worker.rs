use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::AsyncDb;
use crate::core::config::{Config, ConfigHandle, LlmConfig};
use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore};
use crate::daemon_bootstrap::DaemonWriteObserver;

use super::client::{LlmClient, LlmError, LlmMessage, LlmRequest};
use super::retry::{self, HeartbeatCallback};
use super::status::LlmStatus;

const LLM_TASK_KIND: &str = "llm_task";
const LLM_CLAIM_TTL_SECS: i64 = 300;
const LLM_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LLM_VERDICT_KEEP: &str = "keep";
const LLM_VERDICT_REJECT: &str = "reject";

pub const DEFAULT_GATING_JUDGE_PROMPT: &str = "You are a memory importance judge for a software engineering project memory system.\n\nGiven a piece of content captured from a coding session, determine if it contains IMPORTANT information worth storing long-term. Score from 0.0 to 1.0.\n\nIMPORTANT (score >= 0.7):\n- Architecture or design decisions and their rationale\n- Bug root cause analysis and fix strategies\n- User preferences, workflow choices, or explicit feedback\n- Configuration decisions and why they were made\n- Trade-off evaluations between approaches\n- Security concerns or mitigation strategies\n- Project milestones, status changes, or completion records\n- Integration decisions (which tools, which APIs, why)\n\nNOT IMPORTANT (score < 0.4):\n- Raw tool output (file listings, grep results, git status)\n- Routine code edits without design rationale\n- Build/test output logs\n- Boilerplate file content\n- Simple command execution without decision context\n- Repetitive status checks\n\nRespond with ONLY a JSON object: {\"score\": 0.0-1.0, \"reason\": \"brief explanation\"}";

#[derive(Debug, Clone, PartialEq)]
struct LlmClientConfigSignature {
    backend: String,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    extra_body: Option<Value>,
    request_timeout_secs: u64,
}

impl From<&LlmConfig> for LlmClientConfigSignature {
    fn from(config: &LlmConfig) -> Self {
        Self {
            backend: config.backend.clone(),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            api_key_env: config.api_key_env.clone(),
            extra_body: config.extra_body.clone(),
            request_timeout_secs: config.request_timeout_secs,
        }
    }
}

#[doc(hidden)]
pub struct LlmClientRuntime {
    client: Option<Arc<LlmClient>>,
    signature: LlmClientConfigSignature,
}

impl LlmClientRuntime {
    pub fn new(config: &LlmConfig) -> Self {
        let client = match LlmClient::from_config(config) {
            Ok(client) => Some(Arc::new(client)),
            Err(error) => {
                tracing::warn!(%error, "LLM client unavailable at startup; worker will retry");
                None
            }
        };
        Self {
            client,
            signature: LlmClientConfigSignature::from(config),
        }
    }

    #[doc(hidden)]
    pub fn client_for_config(&mut self, config: &LlmConfig) -> Result<Arc<LlmClient>, LlmError> {
        let signature = LlmClientConfigSignature::from(config);
        if self.client.is_none() || self.signature != signature {
            self.client = Some(Arc::new(LlmClient::from_config(config)?));
            self.signature = signature;
            tracing::info!("LLM client rebuilt after config hot-reload");
        }
        self.client.as_ref().map(Arc::clone).ok_or_else(|| {
            LlmError::MissingConfiguration("LLM client unavailable after rebuild".to_string())
        })
    }
}

pub type SharedLlmClientRuntime = Arc<Mutex<LlmClientRuntime>>;

async fn client_for_claimed_generation(
    client_runtime: &SharedLlmClientRuntime,
    llm_gen_rx: &mut tokio::sync::watch::Receiver<u64>,
) -> Result<(Arc<LlmClient>, Arc<Config>), LlmError> {
    loop {
        let snapshot = ConfigHandle::current_llm_runtime_snapshot();
        if !crate::daemon::llm_worker_claim_enabled(snapshot.config.as_ref()) {
            return Err(LlmError::MissingConfiguration(
                "LLM worker claim disabled by current config".to_string(),
            ));
        }

        let client = {
            let mut runtime = client_runtime
                .lock()
                .expect("LLM client runtime mutex poisoned");
            runtime.client_for_config(&snapshot.config.llm)
        }?;

        let new_max = snapshot.config.llm.max_concurrent.max(1);
        if client.current_max_concurrent() != new_max {
            client.update_concurrency(new_max).await;
            tracing::info!("LLM worker: concurrency updated to {new_max}");
        }

        let observed_generation = *llm_gen_rx.borrow_and_update();
        if observed_generation == snapshot.generation {
            return Ok((client, snapshot.config));
        }

        tracing::info!(
            from_generation = snapshot.generation,
            to_generation = observed_generation,
            "LLM config changed while preparing claimed task; rebuilding client"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTaskPayload {
    pub task_type: String,
    /// Primary drawer ID; kept for backward compat with tasks already in the queue.
    pub drawer_id: String,
    /// All drawer IDs for multi-chunk ingests. When non-empty, takes precedence
    /// over `drawer_id` so every chunk is acted on (e.g. rejected together).
    #[serde(default)]
    pub drawer_ids: Vec<String>,
    pub content: String,
    pub system_prompt: Option<String>,
}

pub async fn run_llm_worker(
    store: Arc<AsyncPendingMessageStore>,
    client_runtime: SharedLlmClientRuntime,
    status: Arc<LlmStatus>,
    db: AsyncDb,
    write_observer: DaemonWriteObserver,
) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static WORKER_INDEX: AtomicUsize = AtomicUsize::new(0);
    let idx = WORKER_INDEX.fetch_add(1, Ordering::SeqCst);
    let worker_id = format!("llm-worker-{}-{idx}", std::process::id());
    tracing::info!("LLM worker started: {worker_id}");

    if idx == 0 {
        let reclaimed = store
            .reclaim_stale(LLM_CLAIM_TTL_SECS)
            .await
            .context("LLM worker failed to reclaim stale claims")?;
        if reclaimed > 0 {
            tracing::info!("LLM worker reclaimed {reclaimed} stale tasks");
        }
    }

    // Subscribe to LLM config generation changes. When a hot-reloadable LLM
    // field changes (endpoint, credentials, model, etc.), the receiver value
    // is bumped and any in-flight task is cancelled so the worker restarts
    // with the new config.
    let mut llm_gen_rx = ConfigHandle::subscribe_llm_gen();

    loop {
        if crate::daemon::shutdown_requested() {
            tracing::info!("LLM worker: shutdown requested");
            break;
        }

        // Re-read config at the start of each claim cycle so runtime-disabled
        // subsystems stop claiming promptly. The LLM client itself is prepared
        // only after a task is claimed, using the then-current LLM generation.
        let config = ConfigHandle::current();
        if !crate::daemon::llm_worker_claim_enabled(config.as_ref()) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let message = match store
            .claim_next_by_kind(
                worker_id.clone(),
                LLM_CLAIM_TTL_SECS,
                LLM_TASK_KIND.to_string(),
            )
            .await
        {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                tokio::time::sleep(LLM_POLL_INTERVAL).await;
                continue;
            }
            Err(error) => {
                tracing::warn!(?error, "LLM worker claim_next failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let (client, config) =
            match client_for_claimed_generation(&client_runtime, &mut llm_gen_rx).await {
                Ok(prepared) => prepared,
                Err(error) => {
                    status.record_failure(&error);
                    let message_id = message.id.clone();
                    tracing::warn!(
                        %error,
                        message_id,
                        "LLM client unavailable after claim; releasing task for retry"
                    );
                    if let Err(release_error) = store.release_claim(message).await {
                        tracing::warn!(
                            ?release_error,
                            message_id,
                            "failed to release claimed LLM task after client preparation failure; \
                             task will be reclaimed by TTL or next startup"
                        );
                    }
                    let retry_interval = ConfigHandle::current().llm.retry_interval_secs.max(1);
                    tokio::time::sleep(Duration::from_secs(retry_interval)).await;
                    continue;
                }
            };

        let message_id = message.id.clone();
        tracing::info!("LLM worker claimed task {message_id}");
        let start = Instant::now();

        let heartbeat_store = store.clone();
        let heartbeat_message_id = message.id.clone();
        let heartbeat_worker_id = worker_id.clone();
        let heartbeat: Box<HeartbeatCallback> = Box::new(move || {
            let store = heartbeat_store.clone();
            let message_id = heartbeat_message_id.clone();
            let worker_id = heartbeat_worker_id.clone();
            tokio::spawn(async move {
                if let Err(error) = store.refresh_heartbeat(message_id.clone(), worker_id).await {
                    tracing::warn!(?error, message_id, "failed to refresh LLM task heartbeat");
                }
            });
            Ok(())
        });

        // Race the LLM task against a config-change signal. If the LLM config
        // changes while a request is in-flight, the task future is dropped
        // (reqwest futures are cancel-safe), the task is released back to
        // pending (no retry count increment), and the worker restarts the loop
        // with the fresh config snapshot.
        let task_result = tokio::select! {
            result = process_llm_task_shared(
                client.as_ref(),
                &status,
                &db,
                &message.payload,
                config.as_ref(),
                Some(heartbeat.as_ref()),
            ) => Some(result),
            _ = llm_gen_rx.changed() => None,
        };

        let latency_ms = start.elapsed().as_millis();
        match task_result {
            Some(Ok(())) => {
                tracing::info!("LLM task {message_id} completed in {latency_ms}ms");
                confirm_llm_task_async(&store, message.clone()).await?;
                write_observer.record_successful_write();
            }
            Some(Err(error)) => {
                tracing::error!("LLM task {message_id} failed after {latency_ms}ms: {error}");
                write_observer.record_error(error.to_string());
                store
                    .mark_failed(message.clone(), error.to_string())
                    .await
                    .with_context(|| format!("failed to mark_failed LLM task {message_id}"))?;
            }
            None => {
                tracing::info!(
                    worker_id,
                    message_id,
                    "LLM worker restarting due to config change; releasing task back to pending"
                );
                if let Err(error) = store.release_claim(message.clone()).await {
                    tracing::warn!(
                        ?error,
                        message_id,
                        "failed to release claimed LLM task on config change; \
                         task will be reclaimed by TTL or next startup"
                    );
                }
                // Continue loop — next iteration picks up the updated config.
            }
        }
    }

    Ok(())
}

/// Confirm a completed LLM task in the pending-message store.
async fn confirm_llm_task_async(
    store: &AsyncPendingMessageStore,
    claim: ClaimedMessage,
) -> Result<()> {
    let message_id = claim.id.clone();
    match store.confirm(claim).await {
        Ok(()) => {}
        Err(crate::core::queue::QueueError::MessageNotFound(_)) => {
            tracing::warn!(
                message_id,
                "LLM task already removed before confirm; continuing"
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to confirm LLM task {message_id}"));
        }
    }
    Ok(())
}

/// Confirm a completed LLM task for synchronous callers and legacy tests.
///
/// Async daemon workers use `confirm_llm_task_async` so queue SQLite does not
/// run on Tokio worker threads.
pub fn confirm_llm_task(store: &PendingMessageStore, claim: &ClaimedMessage) -> Result<()> {
    let message_id = claim.id.clone();
    match store.confirm(claim) {
        Ok(()) => {}
        Err(crate::core::queue::QueueError::MessageNotFound(_)) => {
            tracing::warn!(
                message_id,
                "LLM task already removed before confirm; continuing"
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to confirm LLM task {message_id}"));
        }
    }
    Ok(())
}

async fn process_llm_task_shared(
    client: &LlmClient,
    status: &LlmStatus,
    db: &AsyncDb,
    payload: &str,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let task: LlmTaskPayload =
        serde_json::from_str(payload).context("failed to decode LLM task payload")?;

    match task.task_type.as_str() {
        "gating" => process_gating_task(client, status, db, &task, config, heartbeat).await,
        other => anyhow::bail!("unknown LLM task type: {other}"),
    }
}

async fn process_gating_task(
    client: &LlmClient,
    status: &LlmStatus,
    db: &AsyncDb,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let (verdict, score) = request_gating_verdict(client, status, task, config, heartbeat).await?;
    apply_gating_verdict_async(db, task.clone(), config.clone(), verdict, score).await
}

async fn apply_gating_verdict_async(
    db: &AsyncDb,
    task: LlmTaskPayload,
    config: crate::core::config::Config,
    verdict: String,
    score: f64,
) -> Result<()> {
    db.run_write_anyhow(move |db| apply_gating_verdict(db, &task, &config, &verdict, score))
        .await
}

pub async fn process_llm_task(
    client: &LlmClient,
    status: &LlmStatus,
    db: &Database,
    payload: &str,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let task: LlmTaskPayload =
        serde_json::from_str(payload).context("failed to decode LLM task payload")?;

    match task.task_type.as_str() {
        "gating" => {
            let (verdict, score) =
                request_gating_verdict(client, status, &task, config, heartbeat).await?;
            apply_gating_verdict(db, &task, config, &verdict, score)
        }
        other => anyhow::bail!("unknown LLM task type: {other}"),
    }
}

async fn request_gating_verdict(
    client: &LlmClient,
    status: &LlmStatus,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<(String, f64)> {
    let system_prompt = task
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_GATING_JUDGE_PROMPT);

    let request = LlmRequest {
        messages: vec![
            LlmMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: task.content.clone(),
            },
        ],
        model: None,
        temperature: Some(0.0),
        max_tokens: Some(1024),
    };

    let retry_interval = config.llm.retry_interval_secs;
    let response = retry::retry_llm_operation(retry_interval, heartbeat, || {
        client.chat_completion(&request)
    })
    .await;

    match response {
        Ok(response) => {
            status.record_success();
            Ok(parse_gating_verdict(&response.content))
        }
        Err(error) => {
            status.record_failure(&error);
            Err(error).context("LLM gating request failed")
        }
    }
}

fn apply_gating_verdict(
    db: &Database,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    verdict: &str,
    score: f64,
) -> Result<()> {
    let threshold = config
        .ingest_gating
        .llm_judge
        .as_ref()
        .map(|judge| judge.threshold)
        .unwrap_or(0.3);

    let effective_verdict = effective_retention_verdict(verdict, score, threshold);
    // The gating_audit row exists only for the primary drawer_id (recorded during ingest
    // before chunking). Record the effective retention verdict there; the remaining
    // chunk IDs have no audit row and updating them would violate the NOT NULL
    // explain_json constraint.
    db.upsert_llm_verdict(&task.drawer_id, effective_verdict, Some(score))
        .context("failed to upsert LLM verdict")?;

    if effective_verdict == LLM_VERDICT_REJECT {
        // Resolve all drawer IDs: prefer the multi-chunk list when present,
        // fall back to the single drawer_id for backward-compat queue tasks.
        let all_ids: Vec<&str> = if task.drawer_ids.is_empty() {
            vec![task.drawer_id.as_str()]
        } else {
            task.drawer_ids.iter().map(String::as_str).collect()
        };
        tracing::info!(
            drawer_ids = ?all_ids,
            verdict = effective_verdict,
            raw_verdict = verdict,
            score,
            threshold,
            "LLM verdict triggered retroactive soft-delete for drawers"
        );
        for id in &all_ids {
            let deleted = db
                .soft_delete_drawer(id)
                .with_context(|| format!("failed to soft-delete rejected drawer {id}"))?;
            if deleted {
                tracing::info!(
                    drawer_id = %id,
                    verdict = effective_verdict,
                    raw_verdict = verdict,
                    score,
                    threshold,
                    "LLM verdict retroactively soft-deleted drawer"
                );
            }
        }
    }

    Ok(())
}

fn effective_retention_verdict(verdict: &str, score: f64, threshold: f64) -> &'static str {
    if is_reject_verdict(verdict) || score < threshold {
        LLM_VERDICT_REJECT
    } else {
        LLM_VERDICT_KEEP
    }
}

fn is_reject_verdict(verdict: &str) -> bool {
    matches!(
        verdict.trim().to_ascii_lowercase().as_str(),
        "reject"
            | "rejected"
            | "skip"
            | "skipped"
            | "delete"
            | "deleted"
            | "drop"
            | "dropped"
            | "quarantine"
            | "quarantined"
    )
}

fn parse_gating_verdict(content: &str) -> (String, f64) {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) {
        let verdict = parsed
            .get("verdict")
            .and_then(|v| v.as_str())
            .unwrap_or("keep")
            .to_string();
        let score = parsed.get("score").and_then(|v| v.as_f64()).unwrap_or(0.5);
        return (verdict, score);
    }
    let content_lower = content.to_lowercase();
    if content_lower.contains("reject") {
        ("reject".to_string(), 0.1)
    } else {
        ("keep".to_string(), 0.8)
    }
}

#[cfg(test)]
mod tests {
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

    async fn spawn_counting_llm_server(
        count: Arc<AtomicUsize>,
        notify: Arc<Notify>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{Json, Router, routing::post};

        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let count = Arc::clone(&count);
                let notify = Arc::clone(&notify);
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    notify.notify_waiters();
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

    fn gating_task(id: &str) -> LlmTaskPayload {
        LlmTaskPayload {
            task_type: "gating".to_string(),
            drawer_id: id.to_string(),
            drawer_ids: vec![id.to_string()],
            content: "judge me".to_string(),
            system_prompt: None,
        }
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_worker_uses_reloaded_client_when_generation_changes_before_claim_returns() {
        let _guard = crate::core::config::global_config_test_lock()
            .lock_owned()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
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
        let client_runtime = Arc::new(Mutex::new(LlmClientRuntime::new(
            &ConfigHandle::current().llm,
        )));
        let worker = tokio::spawn(run_llm_worker(
            Arc::new(async_store),
            client_runtime,
            Arc::new(LlmStatus::new(5)),
            async_db,
            DaemonWriteObserver::for_test(),
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

        apply_gating_verdict_async(
            &async_db,
            task,
            Config::default(),
            "reject".to_string(),
            0.1,
        )
        .await
        .expect("apply verdict");
        ticker.abort();

        assert_runtime_ticked(&ticks, "LLM verdict");
        assert!(drawer_is_deleted(&db, "llm-verdict-offruntime"));
    }
}

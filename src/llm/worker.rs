use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::AsyncDb;
use crate::core::config::{Config, ConfigHandle, LlmConfig, RemoteCallPolicyConfig};
use crate::core::db::Database;
use crate::core::queue::{
    AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore, QueueFailureDisposition,
};
use crate::core::types::RuntimeWriterLease;
use crate::daemon_bootstrap::DaemonWriteObserver;

use super::client::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse};
use super::retry::{self, HeartbeatCallback};
use super::router::LlmRouter;
use super::status::LlmStatus;

const LLM_TASK_KIND: &str = "llm_task";
const LLM_CLAIM_TTL_SECS: i64 = 300;
const LLM_POLL_INTERVAL: Duration = Duration::from_millis(500);
const LLM_MAX_IDLE_POLL_INTERVAL: Duration = Duration::from_secs(5);
const LLM_VERDICT_KEEP: &str = "keep";
const LLM_VERDICT_REJECT: &str = "reject";
/// Maximum UTF-8 bytes copied into one LLM gating task or request.
pub const MAX_LLM_GATE_CONTENT_BYTES: usize = 64 * 1024;

pub const DEFAULT_GATING_JUDGE_PROMPT: &str = "You are a memory importance judge for a software engineering project memory system.\n\nGiven a piece of content captured from a coding session, determine if it contains IMPORTANT information worth storing long-term. Score from 0.0 to 1.0.\n\nIMPORTANT (score >= 0.7):\n- Architecture or design decisions and their rationale\n- Bug root cause analysis and fix strategies\n- User preferences, workflow choices, or explicit feedback\n- Configuration decisions and why they were made\n- Trade-off evaluations between approaches\n- Security concerns or mitigation strategies\n- Project milestones, status changes, or completion records\n- Integration decisions (which tools, which APIs, why)\n\nNOT IMPORTANT (score < 0.4):\n- Raw tool output (file listings, grep results, git status)\n- Routine code edits without design rationale\n- Build/test output logs\n- Boilerplate file content\n- Simple command execution without decision context\n- Repetitive status checks\n\nRespond with ONLY a JSON object: {\"score\": 0.0-1.0, \"reason\": \"brief explanation\"}";

#[derive(Debug, Clone, PartialEq)]
struct LlmClientConfigSignature {
    backend: String,
    base_url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    extra_body: Option<Value>,
    endpoints: Vec<crate::core::config::LlmEndpointConfig>,
    request_timeout_secs: u64,
    retry_interval_secs: u64,
    max_concurrent: usize,
    remote_call_policy: RemoteCallPolicyConfig,
}

impl LlmClientConfigSignature {
    fn new(config: &LlmConfig, remote_call_policy: &RemoteCallPolicyConfig) -> Self {
        Self {
            backend: config.backend.clone(),
            base_url: config.base_url.clone(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            api_key_env: config.api_key_env.clone(),
            extra_body: config.extra_body.clone(),
            endpoints: config.endpoints.clone(),
            request_timeout_secs: config.request_timeout_secs,
            retry_interval_secs: config.retry_interval_secs,
            max_concurrent: config.max_concurrent,
            remote_call_policy: remote_call_policy.clone(),
        }
    }
}

#[doc(hidden)]
pub struct LlmClientRuntime {
    router: Option<Arc<LlmRouter>>,
    signature: LlmClientConfigSignature,
}

impl LlmClientRuntime {
    pub fn new(config: &LlmConfig) -> Self {
        let router = match LlmRouter::from_config(config) {
            Ok(router) => Some(Arc::new(router)),
            Err(error) => {
                tracing::warn!(%error, "LLM router unavailable at startup; worker will retry");
                None
            }
        };
        Self {
            router,
            signature: LlmClientConfigSignature::new(config, &RemoteCallPolicyConfig::default()),
        }
    }

    #[doc(hidden)]
    pub fn router_for_config(
        &mut self,
        config: &LlmConfig,
        remote_call_policy: &RemoteCallPolicyConfig,
    ) -> Result<Arc<LlmRouter>, LlmError> {
        let signature = LlmClientConfigSignature::new(config, remote_call_policy);
        if self.router.is_none() || self.signature != signature {
            self.router = Some(Arc::new(LlmRouter::from_config_with_policy(
                config,
                remote_call_policy,
            )?));
            self.signature = signature;
            tracing::info!("LLM router rebuilt after config hot-reload");
        }
        self.router.as_ref().map(Arc::clone).ok_or_else(|| {
            LlmError::MissingConfiguration("LLM router unavailable after rebuild".to_string())
        })
    }
}

#[derive(Clone)]
pub struct SharedLlmClientRuntime {
    inner: Arc<Mutex<LlmClientRuntime>>,
    #[cfg(test)]
    _worker_test_lock: Arc<tokio::sync::OwnedMutexGuard<()>>,
}

impl SharedLlmClientRuntime {
    pub fn new(config: &LlmConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LlmClientRuntime::new(config))),
            #[cfg(test)]
            // ponytail: global lock; per-fixture locks if throughput matters.
            _worker_test_lock: Arc::new(super::acquire_llm_worker_test_lock()),
        }
    }
}

impl std::ops::Deref for SharedLlmClientRuntime {
    type Target = Mutex<LlmClientRuntime>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

async fn client_for_claimed_generation(
    client_runtime: &SharedLlmClientRuntime,
    llm_gen_rx: &mut tokio::sync::watch::Receiver<u64>,
) -> Result<(Arc<LlmRouter>, Arc<Config>), LlmError> {
    loop {
        let snapshot = ConfigHandle::current_llm_runtime_snapshot();
        if !crate::daemon::llm_worker_claim_enabled(snapshot.config.as_ref()) {
            return Err(LlmError::MissingConfiguration(
                "LLM worker claim disabled by current config".to_string(),
            ));
        }

        let router = {
            let mut runtime = client_runtime
                .lock()
                .expect("LLM client runtime mutex poisoned");
            runtime.router_for_config(&snapshot.config.llm, &snapshot.config.privacy.remote_calls)
        }?;

        let observed_generation = *llm_gen_rx.borrow_and_update();
        if observed_generation == snapshot.generation {
            return Ok((router, snapshot.config));
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

impl LlmTaskPayload {
    pub(crate) fn for_gating(
        drawer_ids: Vec<String>,
        content: &str,
        system_prompt: Option<String>,
    ) -> Self {
        let drawer_id = drawer_ids.first().cloned().unwrap_or_default();
        Self {
            task_type: "gating".to_string(),
            drawer_id,
            drawer_ids,
            content: bounded_llm_gate_content(content),
            system_prompt,
        }
    }
}

fn bounded_llm_gate_content(content: &str) -> String {
    if content.len() <= MAX_LLM_GATE_CONTENT_BYTES {
        return content.to_string();
    }
    let marker = format!(
        "\n\n[LLM gate content truncated; original_content_bytes={} limit_bytes={MAX_LLM_GATE_CONTENT_BYTES}]",
        content.len()
    );
    let prefix_limit = MAX_LLM_GATE_CONTENT_BYTES.saturating_sub(marker.len());
    let mut prefix_end = prefix_limit.min(content.len());
    while prefix_end > 0 && !content.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    let mut bounded = String::with_capacity(prefix_end.saturating_add(marker.len()));
    bounded.push_str(&content[..prefix_end]);
    bounded.push_str(&marker);
    bounded
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatingRetentionVerdict {
    Keep,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GatingJudgeOutcome {
    pub verdict: GatingRetentionVerdict,
    pub score: f64,
}

impl GatingRetentionVerdict {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Keep => LLM_VERDICT_KEEP,
            Self::Reject => LLM_VERDICT_REJECT,
        }
    }

    pub(crate) fn is_keep(self) -> bool {
        self == Self::Keep
    }
}

pub async fn run_llm_worker(
    store: Arc<AsyncPendingMessageStore>,
    client_runtime: SharedLlmClientRuntime,
    status: Arc<LlmStatus>,
    db: AsyncDb,
    write_observer: DaemonWriteObserver,
    lease: RuntimeWriterLease,
) -> Result<()> {
    run_llm_worker_inner(
        store,
        client_runtime,
        status,
        db,
        write_observer,
        lease,
        None,
    )
    .await
}

async fn run_llm_worker_inner(
    store: Arc<AsyncPendingMessageStore>,
    client_runtime: SharedLlmClientRuntime,
    status: Arc<LlmStatus>,
    db: AsyncDb,
    write_observer: DaemonWriteObserver,
    lease: RuntimeWriterLease,
    mut completion_observer: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static WORKER_INDEX: AtomicUsize = AtomicUsize::new(0);
    let idx = WORKER_INDEX.fetch_add(1, Ordering::SeqCst);
    let worker_id = format!("llm-worker-{}-{idx}", std::process::id());
    tracing::info!("LLM worker started: {worker_id}");

    // Restart with fresh config whenever hot-reloadable LLM settings change,
    // cancelling any in-flight task through the generation receiver.
    let mut llm_gen_rx = ConfigHandle::subscribe_llm_gen();
    let mut idle_count = 0_u32;

    loop {
        if crate::daemon::shutdown_requested() {
            tracing::info!("LLM worker: shutdown requested");
            break;
        }

        // Re-read config each claim cycle so runtime-disabled subsystems stop promptly;
        // prepare the client only after a claim using that generation.
        let config = ConfigHandle::current();
        if !crate::daemon::llm_worker_claim_enabled(config.as_ref()) {
            idle_count = 0;
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
            Ok(Some(msg)) => {
                write_observer.record_queue_maintenance_success();
                msg
            }
            Ok(None) => {
                let effective_interval = llm_idle_poll_interval(LLM_POLL_INTERVAL, idle_count);
                idle_count = idle_count.saturating_add(1);
                tokio::time::sleep(effective_interval).await;
                continue;
            }
            Err(error) => {
                idle_count = 0;
                tracing::warn!(?error, "LLM worker claim_next failed");
                write_observer.record_claim_error(&error);
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        idle_count = 0;

        let (router, config) = match client_for_claimed_generation(&client_runtime, &mut llm_gen_rx)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                status.record_failure(&error);
                let message_id = message.id.clone();
                tracing::warn!(
                    %error,
                    message_id,
                    "LLM client unavailable after claim; releasing task for retry"
                );
                let release_result = store.release_claim(message).await;
                write_observer.observe_maintenance_queue_result(
                        format!(
                            "failed to release claimed LLM task {message_id} after client preparation failure"
                        ),
                        &release_result,
                    );
                if let Err(release_error) = release_result {
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

        let (heartbeat_tx, mut heartbeat_rx) = tokio::sync::mpsc::channel(1);
        let heartbeat_store = store.clone();
        let heartbeat_message_id = message.id.clone();
        let heartbeat_worker_id = worker_id.clone();
        let heartbeat_write_observer = write_observer.clone();
        let heartbeat_handle = tokio::spawn(async move {
            while heartbeat_rx.recv().await.is_some() {
                let result = heartbeat_store
                    .refresh_heartbeat(heartbeat_message_id.clone(), heartbeat_worker_id.clone())
                    .await;
                heartbeat_write_observer.observe_maintenance_queue_result(
                    format!("failed to refresh LLM task heartbeat {heartbeat_message_id}"),
                    &result,
                );
                if let Err(error) = result {
                    tracing::warn!(
                        ?error,
                        message_id = heartbeat_message_id,
                        "failed to refresh LLM task heartbeat"
                    );
                }
            }
        });
        let heartbeat: Box<HeartbeatCallback> = Box::new(move || {
            let _ = heartbeat_tx.try_send(());
            Ok(())
        });

        // Race the LLM task against a config-change signal. If the LLM config
        // changes while a request is in-flight, the task future is dropped
        // (reqwest futures are cancel-safe), the task is released back to
        // pending (no retry count increment), and the worker restarts the loop
        // with the fresh config snapshot.
        let task_result = tokio::select! {
            result = process_llm_task_shared(
                router.as_ref(),
                &status,
                &db,
                &message.payload,
                config.as_ref(),
                Some(heartbeat.as_ref()),
                &lease,
            ) => Some(result),
            _ = llm_gen_rx.changed() => None,
        };
        drop(heartbeat);
        let _ = heartbeat_handle.await;

        let latency_ms = start.elapsed().as_millis();
        match task_result {
            Some(Ok(())) => {
                tracing::info!("LLM task {message_id} completed in {latency_ms}ms");
                let confirm_result = confirm_llm_task_async(&store, message.clone()).await;
                write_observer.observe_semantic_queue_result(
                    format!("failed to confirm LLM task {message_id}"),
                    &confirm_result,
                );
                if let Err(error) = confirm_result {
                    if error.is_sqlite_lock() {
                        tracing::warn!(
                            ?error,
                            message_id,
                            "LLM task confirm hit SQLite contention; keeping worker capacity alive"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return Err(error)
                        .with_context(|| format!("failed to confirm LLM task {message_id}"));
                }
                if let Some(observer) = completion_observer.take() {
                    let _ = observer.send(());
                }
            }
            Some(Err(error)) => {
                tracing::error!("LLM task {message_id} failed after {latency_ms}ms: {error}");
                write_observer.record_error(error.to_string());
                let disposition = llm_task_failure_disposition(&error);
                let mark_result = store
                    .mark_failed_with_disposition(message.clone(), error.to_string(), disposition)
                    .await;
                write_observer.observe_maintenance_queue_result(
                    format!("failed to mark_failed LLM task {message_id}"),
                    &mark_result,
                );
                if let Err(mark_error) = mark_result {
                    if mark_error.is_sqlite_lock() {
                        tracing::warn!(
                            ?mark_error,
                            message_id,
                            "LLM task mark_failed hit SQLite contention; keeping worker capacity alive"
                        );
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    return Err(mark_error)
                        .with_context(|| format!("failed to mark_failed LLM task {message_id}"));
                }
            }
            None => {
                tracing::info!(
                    worker_id,
                    message_id,
                    "LLM worker restarting due to config change; releasing task back to pending"
                );
                let release_result = store.release_claim(message.clone()).await;
                write_observer.observe_maintenance_queue_result(
                    format!("failed to release claimed LLM task {message_id}"),
                    &release_result,
                );
                if let Err(error) = release_result {
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
fn llm_idle_poll_interval(base_interval: Duration, idle_count: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(idle_count.min(u32::BITS - 1))
        .unwrap_or(u32::MAX);
    base_interval
        .checked_mul(multiplier)
        .unwrap_or(LLM_MAX_IDLE_POLL_INTERVAL)
        .min(LLM_MAX_IDLE_POLL_INTERVAL)
}

/// Confirm a completed LLM task in the pending-message store.
async fn confirm_llm_task_async(
    store: &AsyncPendingMessageStore,
    claim: ClaimedMessage,
) -> crate::core::queue::Result<()> {
    let message_id = claim.id.clone();
    match store.confirm(claim).await {
        Ok(()) => Ok(()),
        Err(crate::core::queue::QueueError::MessageNotFound(_)) => {
            tracing::warn!(
                message_id,
                "LLM task already removed before confirm; continuing"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
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
    router: &LlmRouter,
    status: &LlmStatus,
    db: &AsyncDb,
    payload: &str,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
    lease: &RuntimeWriterLease,
) -> Result<()> {
    let task: LlmTaskPayload =
        serde_json::from_str(payload).context("failed to decode LLM task payload")?;

    match task.task_type.as_str() {
        "gating" => process_gating_task(router, status, db, &task, config, heartbeat, lease).await,
        other => anyhow::bail!("unknown LLM task type: {other}"),
    }
}

async fn process_gating_task(
    router: &LlmRouter,
    status: &LlmStatus,
    db: &AsyncDb,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
    lease: &RuntimeWriterLease,
) -> Result<()> {
    let outcome = request_effective_gating_verdict(router, status, task, config, heartbeat).await?;
    apply_gating_verdict_async(
        db,
        task.clone(),
        config.clone(),
        outcome.verdict.as_str().to_string(),
        outcome.score,
        lease,
    )
    .await
}

async fn apply_gating_verdict_async(
    db: &AsyncDb,
    task: LlmTaskPayload,
    config: crate::core::config::Config,
    verdict: String,
    score: f64,
    lease: &RuntimeWriterLease,
) -> Result<()> {
    let lease = lease.clone();
    db.run_write_anyhow(move |db| {
        db.with_runtime_writer_lease_write(Some(&lease), "llm gating verdict", || {
            apply_gating_verdict(db, &task, &config, &verdict, score)
        })
    })
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
                request_gating_verdict_with_client(client, status, &task, config, heartbeat)
                    .await?;
            apply_gating_verdict(db, &task, config, &verdict, score)
        }
        other => anyhow::bail!("unknown LLM task type: {other}"),
    }
}

async fn request_gating_verdict(
    router: &LlmRouter,
    status: &LlmStatus,
    task: &LlmTaskPayload,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<(String, f64)> {
    let request = gating_request(task);
    let response = router
        .chat_completion(&request, heartbeat)
        .await
        .map(|routed| routed.response);

    record_gating_response(status, response)
}

pub(crate) async fn request_effective_gating_verdict(
    router: &LlmRouter,
    status: &LlmStatus,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<GatingJudgeOutcome> {
    let (verdict, score) = request_gating_verdict(router, status, task, heartbeat).await?;
    Ok(effective_gating_outcome(config, &verdict, score))
}

pub(crate) async fn request_strict_effective_gating_verdict(
    router: &LlmRouter,
    status: &LlmStatus,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<GatingJudgeOutcome> {
    let request = gating_request(task);
    let response = router
        .chat_completion(&request, heartbeat)
        .await
        .map(|routed| routed.response);
    let (verdict, score) = record_strict_gating_response(status, response)?;
    Ok(effective_gating_outcome(config, &verdict, score))
}

async fn request_gating_verdict_with_client(
    client: &LlmClient,
    status: &LlmStatus,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<(String, f64)> {
    let request = gating_request(task);
    let retry_interval = config.llm.retry_interval_secs;
    let response = retry::retry_llm_operation(retry_interval, heartbeat, || {
        client.chat_completion(&request)
    })
    .await;

    record_gating_response(status, response)
}

fn gating_request(task: &LlmTaskPayload) -> LlmRequest {
    let system_prompt = task
        .system_prompt
        .as_deref()
        .unwrap_or(DEFAULT_GATING_JUDGE_PROMPT);

    LlmRequest {
        messages: vec![
            LlmMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: bounded_llm_gate_content(&task.content),
            },
        ],
        model: None,
        temperature: Some(0.0),
        max_tokens: Some(1024),
    }
}

fn record_gating_response(
    status: &LlmStatus,
    response: Result<LlmResponse, LlmError>,
) -> Result<(String, f64)> {
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

fn record_strict_gating_response(
    status: &LlmStatus,
    response: Result<LlmResponse, LlmError>,
) -> Result<(String, f64)> {
    match response {
        Ok(response) => {
            let verdict = parse_strict_gating_verdict(&response.content)?;
            status.record_success();
            Ok(verdict)
        }
        Err(error) => {
            status.record_failure(&error);
            Err(error).context("LLM gating request failed")
        }
    }
}

fn llm_task_failure_disposition(error: &anyhow::Error) -> QueueFailureDisposition {
    if error.chain().any(|cause| {
        cause
            .to_string()
            .contains("failed to decode LLM task payload")
    }) || error
        .chain()
        .any(|cause| cause.to_string().contains("unknown LLM task type"))
    {
        return QueueFailureDisposition::Terminal;
    }
    let Some(llm_error) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LlmError>())
    else {
        return QueueFailureDisposition::Retryable;
    };
    if !llm_error.is_retryable() {
        return QueueFailureDisposition::Terminal;
    }
    llm_error
        .retry_after()
        .map(duration_to_retry_delay)
        .unwrap_or(QueueFailureDisposition::Retryable)
}

fn duration_to_retry_delay(duration: Duration) -> QueueFailureDisposition {
    let millis = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
    QueueFailureDisposition::RetryableAfter { delay_ms: millis }
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

fn effective_gating_outcome(
    config: &crate::core::config::Config,
    verdict: &str,
    score: f64,
) -> GatingJudgeOutcome {
    let threshold = config
        .ingest_gating
        .llm_judge
        .as_ref()
        .map(|judge| judge.threshold)
        .unwrap_or(0.3);
    let verdict = match effective_retention_verdict(verdict, score, threshold) {
        LLM_VERDICT_REJECT => GatingRetentionVerdict::Reject,
        _ => GatingRetentionVerdict::Keep,
    };
    GatingJudgeOutcome { verdict, score }
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

fn is_keep_verdict(verdict: &str) -> bool {
    matches!(
        verdict.trim().to_ascii_lowercase().as_str(),
        "keep" | "kept" | "accept" | "accepted" | "retain" | "retained"
    )
}

#[derive(Debug, thiserror::Error)]
#[error("LLM gating verdict is malformed: {0}")]
struct StrictGatingVerdictError(String);

fn parse_strict_gating_verdict(content: &str) -> Result<(String, f64)> {
    let parsed = serde_json::from_str::<serde_json::Value>(content).map_err(|error| {
        StrictGatingVerdictError(format!("verdict must be valid JSON: {error}"))
    })?;
    let score = parsed
        .get("score")
        .and_then(|value| value.as_f64())
        .context("LLM gating verdict JSON must include numeric field 'score'")?;
    if !(0.0..=1.0).contains(&score) {
        anyhow::bail!("LLM gating verdict score must be between 0.0 and 1.0");
    }
    if let Some(verdict) = parsed.get("verdict").and_then(|value| value.as_str()) {
        if !is_keep_verdict(verdict) && !is_reject_verdict(verdict) {
            return Err(StrictGatingVerdictError(
                "verdict field must be keep or reject".to_string(),
            )
            .into());
        }
        return Ok((verdict.to_string(), score));
    }
    let reason = parsed
        .get("reason")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .context("LLM gating verdict JSON without 'verdict' must include non-empty string field 'reason'")?;
    let _ = reason;
    Ok((LLM_VERDICT_KEEP.to_string(), score))
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
pub(crate) mod tests {
    include!("worker_tests.rs");
}

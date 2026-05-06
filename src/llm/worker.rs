use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::config::ConfigHandle;
use crate::core::db::Database;
use crate::core::queue::PendingMessageStore;
use crate::daemon_bootstrap::{DaemonWriteObserver, SharedDatabase};

use super::client::{LlmClient, LlmError, LlmMessage, LlmRequest};
use super::retry::{self, HeartbeatCallback};
use super::status::LlmStatus;

const LLM_TASK_KIND: &str = "llm_task";
const LLM_CLAIM_TTL_SECS: i64 = 300;
const LLM_POLL_INTERVAL: Duration = Duration::from_millis(500);

pub const DEFAULT_GATING_JUDGE_PROMPT: &str = "You are a memory importance judge for a software engineering project memory system.\n\nGiven a piece of content captured from a coding session, determine if it contains IMPORTANT information worth storing long-term. Score from 0.0 to 1.0.\n\nIMPORTANT (score >= 0.7):\n- Architecture or design decisions and their rationale\n- Bug root cause analysis and fix strategies\n- User preferences, workflow choices, or explicit feedback\n- Configuration decisions and why they were made\n- Trade-off evaluations between approaches\n- Security concerns or mitigation strategies\n- Project milestones, status changes, or completion records\n- Integration decisions (which tools, which APIs, why)\n\nNOT IMPORTANT (score < 0.4):\n- Raw tool output (file listings, grep results, git status)\n- Routine code edits without design rationale\n- Build/test output logs\n- Boilerplate file content\n- Simple command execution without decision context\n- Repetitive status checks\n\nRespond with ONLY a JSON object: {\"score\": 0.0-1.0, \"reason\": \"brief explanation\"}";

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
    store: Arc<PendingMessageStore>,
    client: Arc<LlmClient>,
    status: Arc<LlmStatus>,
    db: SharedDatabase,
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
            .context("LLM worker failed to reclaim stale claims")?;
        if reclaimed > 0 {
            tracing::info!("LLM worker reclaimed {reclaimed} stale tasks");
        }
    }

    loop {
        if crate::daemon::shutdown_requested() {
            tracing::info!("LLM worker: shutdown requested");
            break;
        }

        let config = ConfigHandle::current();
        if !config.llm.enabled {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let new_max = config.llm.max_concurrent.max(1);
        if client.current_max_concurrent() != new_max {
            client.update_concurrency(new_max).await;
            tracing::info!("LLM worker: concurrency updated to {new_max}");
        }

        let message = match store.claim_next_by_kind(&worker_id, LLM_CLAIM_TTL_SECS, LLM_TASK_KIND)
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

        let message_id = message.id.clone();
        tracing::info!("LLM worker claimed task {message_id}");
        let start = Instant::now();

        let heartbeat_store = store.clone();
        let heartbeat_message_id = message.id.clone();
        let heartbeat_worker_id = worker_id.clone();
        let heartbeat: Box<HeartbeatCallback> = Box::new(move || {
            heartbeat_store
                .refresh_heartbeat(&heartbeat_message_id, &heartbeat_worker_id)
                .map_err(|error| {
                    LlmError::MissingConfiguration(format!("heartbeat failed: {error}"))
                })?;
            Ok(())
        });

        let result = process_llm_task_shared(
            &client,
            &status,
            &db,
            &message.payload,
            &config,
            Some(heartbeat.as_ref()),
        )
        .await;

        let latency_ms = start.elapsed().as_millis();
        match result {
            Ok(()) => {
                tracing::info!("LLM task {message_id} completed in {latency_ms}ms");
                store
                    .confirm(&message_id)
                    .with_context(|| format!("failed to confirm LLM task {message_id}"))?;
                write_observer.record_successful_write();
            }
            Err(error) => {
                tracing::error!("LLM task {message_id} failed after {latency_ms}ms: {error}");
                write_observer.record_error(error.to_string());
                store
                    .mark_failed(&message_id, &error.to_string())
                    .with_context(|| format!("failed to mark_failed LLM task {message_id}"))?;
            }
        }
    }

    Ok(())
}

async fn process_llm_task_shared(
    client: &LlmClient,
    status: &LlmStatus,
    db: &SharedDatabase,
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
    db: &SharedDatabase,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let (verdict, score) = request_gating_verdict(client, status, task, config, heartbeat).await?;
    let db = db.lock().await;
    apply_gating_verdict(&db, task, config, &verdict, score)
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
    // The gating_audit row exists only for the primary drawer_id (recorded during ingest
    // before chunking). Record the verdict there; the remaining chunk IDs have no audit
    // row and updating them would violate the NOT NULL explain_json constraint.
    db.upsert_llm_verdict(&task.drawer_id, verdict, Some(score))
        .context("failed to upsert LLM verdict")?;

    let threshold = config
        .ingest_gating
        .llm_judge
        .as_ref()
        .map(|judge| judge.threshold)
        .unwrap_or(0.3);

    let rejected_by_verdict = is_reject_verdict(verdict);
    if rejected_by_verdict || score < threshold {
        // Resolve all drawer IDs: prefer the multi-chunk list when present,
        // fall back to the single drawer_id for backward-compat queue tasks.
        let all_ids: Vec<&str> = if task.drawer_ids.is_empty() {
            vec![task.drawer_id.as_str()]
        } else {
            task.drawer_ids.iter().map(String::as_str).collect()
        };
        tracing::info!(
            drawer_ids = ?all_ids,
            verdict,
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
                    verdict,
                    score,
                    threshold,
                    "LLM verdict retroactively soft-deleted drawer"
                );
            }
        }
    }

    Ok(())
}

fn is_reject_verdict(verdict: &str) -> bool {
    matches!(
        verdict.trim().to_ascii_lowercase().as_str(),
        "reject" | "rejected" | "skip" | "skipped"
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

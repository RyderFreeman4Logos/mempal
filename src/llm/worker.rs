use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::config::ConfigHandle;
use crate::core::db::Database;
use crate::core::queue::PendingMessageStore;

use super::client::{LlmClient, LlmError, LlmMessage, LlmRequest};
use super::retry::{self, HeartbeatCallback};
use super::status::LlmStatus;

const LLM_TASK_KIND: &str = "llm_task";
const LLM_CLAIM_TTL_SECS: i64 = 300;
const LLM_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmTaskPayload {
    pub task_type: String,
    pub drawer_id: String,
    pub content: String,
    pub system_prompt: Option<String>,
}

pub async fn run_llm_worker(
    store: Arc<PendingMessageStore>,
    client: Arc<LlmClient>,
    status: Arc<LlmStatus>,
    db_path: std::path::PathBuf,
) -> Result<()> {
    let worker_id = format!("llm-worker-{}", std::process::id());
    tracing::info!("LLM worker started: {worker_id}");

    let reclaimed = store
        .reclaim_stale(LLM_CLAIM_TTL_SECS)
        .context("LLM worker failed to reclaim stale claims")?;
    if reclaimed > 0 {
        tracing::info!("LLM worker reclaimed {reclaimed} stale tasks");
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

        let result = process_llm_task(
            &client,
            &status,
            &db_path,
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
            }
            Err(error) => {
                tracing::error!("LLM task {message_id} failed after {latency_ms}ms: {error}");
                store
                    .mark_failed(&message_id, &error.to_string())
                    .with_context(|| format!("failed to mark_failed LLM task {message_id}"))?;
            }
        }
    }

    Ok(())
}

async fn process_llm_task(
    client: &LlmClient,
    status: &LlmStatus,
    db_path: &std::path::Path,
    payload: &str,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let task: LlmTaskPayload =
        serde_json::from_str(payload).context("failed to decode LLM task payload")?;

    match task.task_type.as_str() {
        "gating" => process_gating_task(client, status, db_path, &task, config, heartbeat).await,
        other => anyhow::bail!("unknown LLM task type: {other}"),
    }
}

async fn process_gating_task(
    client: &LlmClient,
    status: &LlmStatus,
    db_path: &std::path::Path,
    task: &LlmTaskPayload,
    config: &crate::core::config::Config,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<()> {
    let system_prompt = task
        .system_prompt
        .as_deref()
        .unwrap_or("You are a memory quality judge. Evaluate the following content and respond with a JSON object containing 'verdict' (keep/reject) and 'score' (0.0-1.0).");

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
        max_tokens: Some(256),
    };

    let retry_interval = config.llm.retry_interval_secs;
    let response = retry::retry_llm_operation(retry_interval, heartbeat, || {
        client.chat_completion(&request)
    })
    .await;

    match response {
        Ok(response) => {
            status.record_success();
            let (verdict, score) = parse_gating_verdict(&response.content);
            let db = Database::open(db_path).context("failed to open database for LLM verdict")?;
            db.upsert_llm_verdict(&task.drawer_id, &verdict, Some(score))
                .context("failed to upsert LLM verdict")?;

            let threshold = config
                .ingest_gating
                .llm_judge
                .as_ref()
                .map(|judge| judge.threshold)
                .unwrap_or(0.3);

            if score < threshold {
                tracing::info!(
                    drawer_id = task.drawer_id,
                    score,
                    threshold,
                    "LLM rejected drawer, executing soft-delete"
                );
                db.soft_delete_drawer(&task.drawer_id)
                    .context("failed to soft-delete rejected drawer")?;
            }

            Ok(())
        }
        Err(error) => {
            status.record_failure(&error);
            Err(error).context("LLM gating request failed")
        }
    }
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

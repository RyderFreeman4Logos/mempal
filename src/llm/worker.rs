use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::AsyncDb;
use crate::core::config::ConfigHandle;
use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::daemon_bootstrap::DaemonWriteObserver;

use super::client::{LlmClient, LlmMessage, LlmRequest};
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
    store: Arc<AsyncPendingMessageStore>,
    client: Arc<LlmClient>,
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
    // field changes (e.g. model), the receiver value is bumped and any
    // in-flight task is cancelled so the worker restarts with the new config.
    let mut llm_gen_rx = ConfigHandle::subscribe_llm_gen();

    loop {
        if crate::daemon::shutdown_requested() {
            tracing::info!("LLM worker: shutdown requested");
            break;
        }

        // Re-read config at the start of each claim cycle so model/timeout
        // changes are picked up without a full daemon restart.
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

        // Mark the current generation as seen AFTER claiming so that `changed()`
        // only fires for changes that happen while THIS task is in-flight.
        let _ = llm_gen_rx.borrow_and_update();

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
                &client,
                &status,
                &db,
                &message.payload,
                &config,
                Some(heartbeat.as_ref()),
            ) => Some(result),
            _ = llm_gen_rx.changed() => None,
        };

        let latency_ms = start.elapsed().as_millis();
        match task_result {
            Some(Ok(())) => {
                tracing::info!("LLM task {message_id} completed in {latency_ms}ms");
                confirm_llm_task_async(&store, &message_id).await?;
                write_observer.record_successful_write();
            }
            Some(Err(error)) => {
                tracing::error!("LLM task {message_id} failed after {latency_ms}ms: {error}");
                write_observer.record_error(error.to_string());
                store
                    .mark_failed(message_id.clone(), error.to_string())
                    .await
                    .with_context(|| format!("failed to mark_failed LLM task {message_id}"))?;
            }
            None => {
                tracing::info!(
                    worker_id,
                    message_id,
                    "LLM worker restarting due to config change; releasing task back to pending"
                );
                if let Err(error) = store.release_claim(message_id.clone()).await {
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
async fn confirm_llm_task_async(store: &AsyncPendingMessageStore, message_id: &str) -> Result<()> {
    match store.confirm(message_id.to_string()).await {
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
pub fn confirm_llm_task(store: &PendingMessageStore, message_id: &str) -> Result<()> {
    match store.confirm(message_id) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
    use rusqlite::params;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

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

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{future::Future, pin::Pin};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    AsyncDb,
    db::{Database, DbError},
    project::resolve_project_id,
    queue::{AsyncPendingMessageStore, ClaimedMessage, QueueFailureDisposition},
    strata::{is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{BootstrapEvidenceArgs, Drawer, SourceType},
    utils::{current_timestamp, route_room_from_taxonomy, synthetic_source_file},
};
use crate::embed::{
    EmbedError, Embedder, build_backend_from_name, global_embed_status,
    retry::{HeartbeatCallback, retry_embed_operation},
};
use crate::ingest::gating::{
    GatingDecision, IngestCandidate, PrototypeClassifier, compile_classifier_from_embedder,
    evaluate_tier1, tier2_enabled,
};
use crate::ingest::novelty::{NoveltyAction, NoveltyCandidate, evaluate as evaluate_novelty};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::daemon_bootstrap::DaemonContext;
use crate::hook::CapturedHookEnvelope;
use crate::hotpatch::generator::{GenerationOptions, suggest_for_drawer};
use crate::session_review::{
    SessionReviewOutcome, analysis_content, append_hooks_raw_metadata, extract_session_review,
    split_session_metadata, validate_linked_drawer_ids,
};

const SESSION_REVIEW_REJECTED_TOTAL_KEY: &str = "session_review.rejected.total";

/// Budget given to LLM workers to finish in-flight tasks during graceful shutdown.
/// Coupled to the orphan reaper grace period in `src/main.rs`.
pub const DAEMON_DRAIN_BUDGET: Duration = Duration::from_secs(30);
const DAEMON_HOOK_WORKER_LIMIT: usize = 4;

pub fn run_command(config_path: PathBuf, foreground: bool) -> Result<()> {
    run_command_with_bootstrap_events(config_path, foreground, None)
}

pub fn run_command_with_bootstrap_events(
    config_path: PathBuf,
    foreground: bool,
    bootstrap_events: Option<mpsc::Sender<BootstrapEvent>>,
) -> Result<()> {
    // harness-point: PR0
    let context =
        match DaemonContext::bootstrap_with_events(config_path, foreground, bootstrap_events) {
            Ok(context) => context,
            // A concurrent daemon already holds the singleton lock (#257): this is
            // a clean no-op, not a failure. Exit success without daemonizing.
            Err(error) if error.is::<crate::daemon_singleton::DaemonAlreadyRunning>() => {
                eprintln!("daemon already running; exiting");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
    context.runtime.block_on(run_loop(&context))
}

async fn run_loop(context: &DaemonContext) -> Result<()> {
    let db_path = {
        let db = context.db.lock().await;
        db.path().to_path_buf()
    };
    global_embed_status().set_audit_db_path(Some(db_path));
    {
        let db = context.db.lock().await;
        db.prune_expired_audit_logs()
            .context("failed to prune expired audit logs")?;
    }

    install_shutdown_handlers()?;
    tracing::info!("daemon log path: {}", context.log_path.display());

    if context.config.hooks.session_end.auto_ingest_conversation {
        tracing::warn!(
            "config.hooks.session_end.auto_ingest_conversation is set to true but was \
             removed in P16. Use `mempal xurl ingest` instead. \
             Set auto_ingest_conversation = false to suppress this warning."
        );
    }

    // Start REST API before the hooks check so the API remains available
    // even when hooks are disabled.
    #[cfg(feature = "rest")]
    let _rest_task: Option<tokio::task::JoinHandle<_>> = if context.config.api.enabled {
        use crate::api::{ApiState, serve_with_shutdown as serve_rest_api};
        use std::sync::Arc;

        let addr = context.config.api.addr.clone();
        let db_path_rest = {
            let db = context.db.lock().await;
            db.path().to_path_buf()
        };
        let config_for_rest = context.config.as_ref().clone();
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                let local_addr = listener
                    .local_addr()
                    .context("failed to resolve REST server address")?;
                tracing::info!("daemon REST listening on http://{local_addr}");
                eprintln!("daemon REST listening on http://{local_addr}");
                let factory = crate::embed::ConfiguredEmbedderFactory::new(config_for_rest);
                let state = ApiState::new(db_path_rest, Arc::new(factory));
                Some(tokio::spawn(async move {
                    if let Err(error) = serve_rest_api(listener, state, async {
                        while !shutdown_requested() {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                        }
                    })
                    .await
                    {
                        tracing::error!("daemon REST server error: {error}");
                    }
                }))
            }
            Err(error) => {
                tracing::warn!("daemon REST server failed to bind {addr}: {error}");
                eprintln!("warning: daemon REST server failed to bind {addr}: {error}");
                None
            }
        }
    } else {
        None
    };

    if !context.config.hooks.enabled {
        eprintln!("hooks not enabled; daemon running REST API only (no worker loop)");
        // Keep the process alive for the REST server.
        #[cfg(feature = "rest")]
        if let Some(task) = _rest_task {
            let _ = task.await;
        }
        return Ok(());
    }

    let embedder = Arc::new(
        DaemonEmbedder::from_config(context.config.as_ref())
            .await
            .context("failed to build daemon embedder")?,
    );
    let prototype_classifier = Arc::new(
        compile_classifier_from_embedder(embedder.as_ref(), &context.config.ingest_gating)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .context("gating prototype init failed")?,
    );
    let worker_id = format!("mempal-daemon-{}", std::process::id());
    let claim_ttl_secs = context.config.hooks.daemon_claim_ttl_secs as i64;
    let poll_interval = Duration::from_millis(context.config.hooks.daemon_poll_interval_ms);
    let reclaimed = context
        .store
        .reclaim_stale(claim_ttl_secs)
        .await
        .context("failed to reclaim stale daemon claims")?;
    tracing::info!("daemon startup reclaim_stale reclaimed={reclaimed}");
    let stall_watchdog_handle = spawn_stall_watchdog(
        context.write_observer.clone(),
        context.store.clone(),
        Duration::from_secs(60),
    );

    let llm_worker_handles: Vec<tokio::task::JoinHandle<_>> = if context.config.llm.enabled {
        match crate::llm::LlmClient::from_config(&context.config.llm) {
            Ok(llm_client) => {
                let num_workers = context.config.llm.max_concurrent.max(1);
                let llm_status = std::sync::Arc::new(crate::llm::LlmStatus::new(10));
                let llm_store = std::sync::Arc::new(context.store.clone());
                let llm_client = std::sync::Arc::new(llm_client);
                let async_db = context.async_db.clone();
                let write_observer = context.write_observer.clone();
                tracing::info!("spawning {num_workers} LLM worker tasks");
                (0..num_workers)
                    .map(|i| {
                        let store = llm_store.clone();
                        let client = llm_client.clone();
                        let status = llm_status.clone();
                        let db = async_db.clone();
                        let observer = write_observer.clone();
                        tokio::spawn(async move {
                            if let Err(e) = crate::llm::worker::run_llm_worker(
                                store, client, status, db, observer,
                            )
                            .await
                            {
                                tracing::error!("LLM worker {i} fatal error: {e:#}");
                            }
                            Ok::<(), anyhow::Error>(())
                        })
                    })
                    .collect()
            }
            Err(error) => {
                tracing::warn!("LLM client init failed, skipping LLM worker: {error}");
                vec![]
            }
        }
    } else {
        vec![]
    };

    let mut hook_workers = JoinSet::new();
    loop {
        context.write_observer.maybe_log_stall(&context.store).await;
        drain_finished_hook_workers(&mut hook_workers);

        if shutdown_requested() {
            tracing::info!("shutdown requested; stopping daemon loop");
            break;
        }

        if hook_workers.len() >= DAEMON_HOOK_WORKER_LIMIT {
            wait_for_hook_worker_or_tick(&mut hook_workers, poll_interval).await;
            continue;
        }

        match poll_claim_next(&context.store, &worker_id, claim_ttl_secs, |duration| {
            Box::pin(tokio::time::sleep(duration))
        })
        .await
        {
            ClaimPollResult::Claimed(message) => {
                spawn_hook_worker(
                    &mut hook_workers,
                    HookWorkerState {
                        async_db: context.async_db.clone(),
                        store: context.store.clone(),
                        worker_id: worker_id.clone(),
                        embedder: Arc::clone(&embedder),
                        prototype_classifier: Arc::clone(&prototype_classifier),
                        config: Arc::clone(&context.config),
                        mempal_home: context.mempal_home.clone(),
                        write_observer: context.write_observer.clone(),
                    },
                    message,
                );
            }
            ClaimPollResult::Idle => {
                wait_for_hook_worker_or_tick(&mut hook_workers, poll_interval).await;
            }
            ClaimPollResult::RetryAfterError => continue,
        }
    }

    drain_hook_workers_with_budget(&mut hook_workers, DAEMON_DRAIN_BUDGET).await;

    // Give LLM workers a window to finish their current tasks, then abort.
    let drain_start = tokio::time::Instant::now();
    for handle in llm_worker_handles {
        let elapsed = drain_start.elapsed();
        let remaining = DAEMON_DRAIN_BUDGET.saturating_sub(elapsed);
        if remaining.is_zero() {
            handle.abort();
            let _ = handle.await;
        } else {
            match tokio::time::timeout(remaining, handle).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!(
                        "LLM worker did not exit within drain deadline; task claim will be released on next startup"
                    );
                }
            }
        }
    }
    tracing::info!("LLM workers stopped");
    stall_watchdog_handle.abort();
    let _ = stall_watchdog_handle.await;

    // Release any tasks still claimed by workers that were aborted or did not finish.
    let released = context
        .store
        .reclaim_stale(0)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(?error, "failed to release claimed messages on shutdown");
            0
        });
    if released > 0 {
        tracing::info!("released {released} claimed messages back to pending on shutdown");
    }

    Ok(())
}

struct HookWorkerState {
    async_db: AsyncDb,
    store: AsyncPendingMessageStore,
    worker_id: String,
    embedder: Arc<DaemonEmbedder>,
    prototype_classifier: Arc<Option<PrototypeClassifier>>,
    config: Arc<crate::core::config::Config>,
    mempal_home: PathBuf,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
}

fn spawn_hook_worker(
    hook_workers: &mut JoinSet<()>,
    state: HookWorkerState,
    message: ClaimedMessage,
) {
    hook_workers.spawn(async move {
        process_hook_worker_message(state, message).await;
    });
}

async fn process_hook_worker_message(state: HookWorkerState, message: ClaimedMessage) {
    let message_id = message.id.clone();
    let result = process_claimed_message_with_embedder(
        &state.async_db,
        &state.store,
        &state.worker_id,
        &message,
        state.embedder.as_ref(),
        DaemonIngestContext {
            prototype_classifier: state.prototype_classifier.as_ref().as_ref(),
            config: state.config.as_ref(),
            mempal_home: &state.mempal_home,
        },
    )
    .await;

    match result {
        Ok(_) => {
            if let Err(error) = state.store.confirm(message_id.clone()).await {
                tracing::error!(?error, "failed to confirm {message_id}");
                state
                    .write_observer
                    .record_error(format!("failed to confirm {message_id}: {error}"));
            } else {
                state.write_observer.record_successful_write();
            }
        }
        Err(error) => {
            tracing::error!("daemon message {message_id} failed: {error}");
            state.write_observer.record_error(error.to_string());
            let disposition = queue_failure_disposition(&error);
            if let Err(mark_error) = state
                .store
                .mark_failed_with_disposition(message_id.clone(), error.to_string(), disposition)
                .await
            {
                tracing::error!(?mark_error, "failed to mark_failed {message_id}");
                state
                    .write_observer
                    .record_error(format!("failed to mark_failed {message_id}: {mark_error}"));
            }
        }
    }
}

fn drain_finished_hook_workers(hook_workers: &mut JoinSet<()>) {
    while let Some(result) = hook_workers.try_join_next() {
        handle_hook_worker_join(result);
    }
}

async fn wait_for_hook_worker_or_tick(hook_workers: &mut JoinSet<()>, tick: Duration) {
    if hook_workers.is_empty() {
        tokio::time::sleep(tick).await;
        return;
    }

    tokio::select! {
        result = hook_workers.join_next() => {
            if let Some(result) = result {
                handle_hook_worker_join(result);
            }
        }
        () = tokio::time::sleep(tick) => {}
    }
}

async fn drain_hook_workers_with_budget(hook_workers: &mut JoinSet<()>, budget: Duration) {
    let drain_start = tokio::time::Instant::now();
    while !hook_workers.is_empty() {
        let elapsed = drain_start.elapsed();
        let remaining = budget.saturating_sub(elapsed);
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, hook_workers.join_next()).await {
            Ok(Some(result)) => handle_hook_worker_join(result),
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if !hook_workers.is_empty() {
        tracing::warn!(
            remaining = hook_workers.len(),
            "hook workers did not exit within drain deadline; task claims will be released on shutdown"
        );
        hook_workers.abort_all();
        while let Some(result) = hook_workers.join_next().await {
            handle_hook_worker_join(result);
        }
    }
}

fn handle_hook_worker_join(result: std::result::Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!(?error, "hook worker task failed");
    }
}

fn spawn_stall_watchdog(
    observer: crate::daemon_bootstrap::DaemonWriteObserver,
    store: AsyncPendingMessageStore,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if shutdown_requested() {
                break;
            }
            observer.maybe_log_stall(&store).await;
        }
    })
}

trait ClaimNextSource {
    fn claim_next<'a>(
        &'a self,
        worker_id: &'a str,
        claim_ttl_secs: i64,
    ) -> Pin<Box<dyn Future<Output = crate::core::queue::Result<Option<ClaimedMessage>>> + Send + 'a>>;
}

impl ClaimNextSource for AsyncPendingMessageStore {
    fn claim_next<'a>(
        &'a self,
        worker_id: &'a str,
        claim_ttl_secs: i64,
    ) -> Pin<Box<dyn Future<Output = crate::core::queue::Result<Option<ClaimedMessage>>> + Send + 'a>>
    {
        Box::pin(async move { self.claim_next(worker_id.to_string(), claim_ttl_secs).await })
    }
}

enum ClaimPollResult {
    Claimed(ClaimedMessage),
    Idle,
    RetryAfterError,
}

type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub struct DaemonIngestContext<'a> {
    pub prototype_classifier: Option<&'a PrototypeClassifier>,
    pub config: &'a crate::core::config::Config,
    pub mempal_home: &'a Path,
}

struct DrawerIngestContext<'a, E: Embedder + ?Sized> {
    db: &'a AsyncDb,
    store: &'a AsyncPendingMessageStore,
    worker_id: &'a str,
    message: &'a ClaimedMessage,
    embedder: &'a E,
    daemon: &'a DaemonIngestContext<'a>,
    envelope: &'a CapturedHookEnvelope,
}

async fn poll_claim_next<'a, S>(
    store: &impl ClaimNextSource,
    worker_id: &str,
    claim_ttl_secs: i64,
    sleep_on_error: S,
) -> ClaimPollResult
where
    S: Fn(Duration) -> SleepFuture<'a>,
{
    match store.claim_next(worker_id, claim_ttl_secs).await {
        Ok(Some(message)) => ClaimPollResult::Claimed(message),
        Ok(None) => ClaimPollResult::Idle,
        Err(error) => {
            tracing::warn!(?error, "claim_next failed");
            sleep_on_error(Duration::from_secs(1)).await;
            ClaimPollResult::RetryAfterError
        }
    }
}

pub async fn process_claimed_message_with_embedder<E: Embedder + ?Sized>(
    db: &AsyncDb,
    store: &AsyncPendingMessageStore,
    worker_id: &str,
    message: &ClaimedMessage,
    embedder: &E,
    context: DaemonIngestContext<'_>,
) -> Result<String> {
    let envelope: CapturedHookEnvelope =
        serde_json::from_str(&message.payload).context("failed to decode queued hook envelope")?;
    let records = {
        let envelope = envelope.clone();
        let config = (*context.config).clone();
        let mempal_home = context.mempal_home.to_path_buf();
        db.run_write_anyhow(move |db| build_drawer_records(db, &envelope, &config, &mempal_home))
            .await?
    };
    let drawer_context = DrawerIngestContext {
        db,
        store,
        worker_id,
        message,
        embedder,
        daemon: &context,
        envelope: &envelope,
    };
    let mut last_drawer_id = None;
    for record in records {
        let drawer_id = ingest_drawer_record(&drawer_context, record).await?;
        let suggest_result = {
            let config = (*context.config).clone();
            let mempal_home = context.mempal_home.to_path_buf();
            let drawer_id_for_suggest = drawer_id.clone();
            db.run_read_anyhow(move |db| {
                suggest_for_drawer(
                    db,
                    &config,
                    &mempal_home,
                    &drawer_id_for_suggest,
                    GenerationOptions::default(),
                )
            })
            .await
        };
        if let Err(error) = suggest_result {
            tracing::warn!(?error, drawer_id, "hotpatch suggestion generation failed");
        }
        last_drawer_id = Some(drawer_id);
    }

    Ok(last_drawer_id.unwrap_or_else(|| message.id.clone()))
}

fn build_gating_candidate(
    envelope: &CapturedHookEnvelope,
    record: &DrawerRecord,
) -> IngestCandidate {
    let mut tool_name = None;
    let mut exit_code = None;

    if envelope.event == crate::hook::HookEvent::PostToolUse.display_name()
        && let Some(payload) = envelope.payload.as_deref()
        && let Ok(value) = serde_json::from_str::<Value>(payload)
    {
        tool_name = value
            .get("tool_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        exit_code = value
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok());
    }

    IngestCandidate {
        content: analysis_content(&record.content).to_string(),
        event: Some(envelope.event.clone()),
        tool_name,
        exit_code,
    }
}

async fn embed_text_with_heartbeat<E: Embedder + ?Sized>(
    embedder: &E,
    content: &str,
    heartbeat: Option<&HeartbeatCallback>,
) -> crate::embed::Result<Vec<f32>> {
    let status = global_embed_status();
    let texts = [content];
    let vectors =
        retry_embed_operation(status, heartbeat, || async { embedder.embed(&texts).await }).await?;
    status.record_primary_success();
    vectors
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError::Runtime("embedder returned no vectors".to_string()))
}

fn queue_failure_disposition(error: &anyhow::Error) -> QueueFailureDisposition {
    for cause in error.chain() {
        if let Some(embed_error) = cause.downcast_ref::<EmbedError>() {
            return if embed_error.is_retryable() {
                QueueFailureDisposition::Retryable
            } else {
                QueueFailureDisposition::Terminal
            };
        }
        if cause.downcast_ref::<serde_json::Error>().is_some() {
            return QueueFailureDisposition::Terminal;
        }
    }
    QueueFailureDisposition::Retryable
}

#[derive(Debug, Clone)]
struct DrawerRecord {
    wing: String,
    room: String,
    source_file: String,
    content: String,
    added_at: String,
    importance: i32,
    bypass_novelty: bool,
    project_id: Option<String>,
}

fn build_drawer_records(
    db: &Database,
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
    mempal_home: &Path,
) -> Result<Vec<DrawerRecord>> {
    let mut audit_record = build_audit_drawer_record(envelope, config, mempal_home)?;
    apply_turn_strata(&mut audit_record, config);
    let mut records = Vec::new();
    if should_keep_drawer_record(&audit_record, config) {
        records.push(audit_record.clone());
    }
    if let Some(record) = build_user_prompt_project_record(db, envelope, config, &audit_record)? {
        let mut record = record;
        apply_turn_strata(&mut record, config);
        if should_keep_drawer_record(&record, config) {
            records.push(record);
        }
    }
    if envelope.event == crate::hook::HookEvent::SessionEnd.display_name() {
        let session_review_payload = if config.hooks.session_end.extract_self_review {
            load_session_review_payload(envelope)?
        } else {
            None
        };
        let review_record = (|| -> Result<Option<DrawerRecord>> {
            match extract_session_review(
                session_review_payload.as_deref(),
                &envelope.agent,
                &config.hooks.session_end,
            )? {
                SessionReviewOutcome::Review(review) => {
                    let project_id = resolve_hook_project_id(envelope, config)?;
                    let (_, metadata) = split_session_metadata(&review.content);
                    if let Some(session_id) = metadata.session_id.as_deref() {
                        validate_linked_drawer_ids(
                            db.conn(),
                            session_id,
                            project_id.as_deref(),
                            &metadata.linked_drawer_ids,
                        )
                        .with_context(|| {
                            format!(
                                "session-review linked_drawer_ids rejected for source_file={}",
                                review.source_file
                            )
                        })?;
                    }
                    Ok(Some(DrawerRecord {
                        wing: review.wing,
                        room: review.room,
                        source_file: review.source_file,
                        content: config.scrub_content(&review.content),
                        added_at: envelope.captured_at.clone(),
                        importance: review.importance,
                        bypass_novelty: true,
                        project_id,
                    }))
                }
                SessionReviewOutcome::Skipped(reason) => {
                    tracing::info!(?reason, "session self-review skipped");
                    Ok(None)
                }
            }
        })();

        match review_record {
            Ok(Some(mut record)) => {
                apply_turn_strata(&mut record, config);
                if should_keep_drawer_record(&record, config) {
                    records.push(record);
                }
            }
            Ok(None) => {}
            Err(error) => {
                record_session_review_rejection(db);
                tracing::warn!(
                    ?error,
                    event = %envelope.event,
                    agent = %envelope.agent,
                    claude_cwd = %envelope.claude_cwd,
                    "session self-review rejected; hooks-raw audit will still persist"
                );
            }
        }
    }

    Ok(records)
}

fn apply_turn_strata(record: &mut DrawerRecord, config: &crate::core::config::Config) {
    if let Some(importance) =
        raw_turn_importance(&record.wing, Some(record.room.as_str()), &config.turns)
    {
        record.importance = importance;
    }
}

fn should_keep_drawer_record(record: &DrawerRecord, config: &crate::core::config::Config) -> bool {
    !raw_turn_storage_disabled(&record.wing, &record.room, config)
}

fn raw_turn_storage_disabled(wing: &str, room: &str, config: &crate::core::config::Config) -> bool {
    is_raw_turn(wing, Some(room), &config.turns)
        && !should_store_raw_turns(&config.turns.storage_mode)
}

fn wing_from_cwd(cwd: &str) -> Option<String> {
    let name = Path::new(cwd).file_name()?.to_str()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

fn build_user_prompt_project_record(
    db: &Database,
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
    audit_record: &DrawerRecord,
) -> Result<Option<DrawerRecord>> {
    if envelope.event != crate::hook::HookEvent::UserPromptSubmit.display_name() {
        return Ok(None);
    }
    if envelope.truncated {
        return Ok(None);
    }

    let raw_payload = envelope.payload.as_deref().unwrap_or_default();
    let content = config.scrub_content(&user_prompt_content(raw_payload));
    if content.trim().is_empty() {
        return Ok(None);
    }

    let wing = wing_from_cwd(&envelope.claude_cwd).unwrap_or_else(|| config.hooks.wing.clone());
    let wing = wing.trim().to_string();
    if wing.is_empty() || wing == "hooks-raw" {
        return Ok(None);
    }

    let taxonomy = db
        .taxonomy_entries()
        .context("failed to load taxonomy for hook user-prompt promotion")?;
    let room = route_room_from_taxonomy(&content, &wing, &taxonomy);
    let project_id = resolve_hook_project_id(envelope, config)?;

    Ok(Some(DrawerRecord {
        wing,
        room,
        source_file: audit_record.source_file.clone(),
        content,
        added_at: audit_record.added_at.clone(),
        importance: 0,
        bypass_novelty: false,
        project_id,
    }))
}

fn record_session_review_rejection(db: &Database) {
    if let Err(error) = db.conn().execute(
        r#"
        INSERT INTO fork_ext_meta (key, value)
        VALUES (?1, '1')
        ON CONFLICT(key) DO UPDATE
        SET value = CAST(CAST(COALESCE(fork_ext_meta.value, '0') AS INTEGER) + 1 AS TEXT)
        "#,
        [SESSION_REVIEW_REJECTED_TOTAL_KEY],
    ) {
        tracing::warn!(
            ?error,
            key = SESSION_REVIEW_REJECTED_TOTAL_KEY,
            "failed to record session-review rejection counter"
        );
    }
}

fn load_session_review_payload(envelope: &CapturedHookEnvelope) -> Result<Option<String>> {
    if !envelope.truncated {
        return Ok(envelope.payload.clone());
    }

    let payload_path = envelope
        .payload_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("truncated session_end missing payload_path"))?;
    fs::read_to_string(payload_path).map(Some).with_context(|| {
        format!(
            "failed to read truncated session_end payload {}",
            payload_path
        )
    })
}

fn build_audit_drawer_record(
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
    mempal_home: &Path,
) -> Result<DrawerRecord> {
    let project_id = resolve_hook_project_id(envelope, config)?;
    if envelope.truncated {
        let preview = config.scrub_content(envelope.payload_preview.as_deref().unwrap_or_default());
        tracing::warn!(
            event = %envelope.event,
            original_size_bytes = envelope.original_size_bytes,
            payload_preview = %preview,
            "processing truncated hook envelope"
        );
        let content = serde_json::to_string(&json!({
            "_truncated": true,
            "event": envelope.event,
            "agent": envelope.agent,
            "captured_at": envelope.captured_at,
            "claude_cwd": envelope.claude_cwd,
            "original_size_bytes": envelope.original_size_bytes,
            "payload_preview": preview,
            "payload_path": envelope.payload_path,
        }))
        .context("failed to serialize truncated hook marker")?;
        let source_file = envelope
            .payload_path
            .clone()
            .unwrap_or_else(|| synthetic_source_file("hook-truncated"));
        return Ok(DrawerRecord {
            wing: "hooks-raw".to_string(),
            room: "truncated".to_string(),
            source_file,
            content,
            added_at: envelope.captured_at.clone(),
            importance: 0,
            bypass_novelty: false,
            project_id,
        });
    }

    let raw_payload = envelope.payload.as_deref().unwrap_or_default();
    let (wing, room) = audit_target_for_event(&envelope.event, raw_payload, config);
    let preview = config.scrub_content(&preview_for_event(&envelope.event, raw_payload));
    let payload_path = if raw_turn_storage_disabled(&wing, &room, config) {
        synthetic_source_file("hook-payload-skipped")
    } else {
        persist_raw_payload(raw_payload, mempal_home)?
    };
    let content = serde_json::to_string(&json!({
        "event": envelope.event,
        "agent": envelope.agent,
        "captured_at": envelope.captured_at,
        "claude_cwd": envelope.claude_cwd,
        "preview": preview,
        "meta": {
            "hook_payload_path": payload_path,
            "original_size_bytes": envelope.original_size_bytes,
        }
    }))
    .context("failed to serialize hook diary drawer")?;
    let content = append_hooks_raw_metadata(
        &content,
        hook_payload_session_id(raw_payload).as_deref(),
        Some(envelope.captured_at.as_str()),
    );

    Ok(DrawerRecord {
        wing,
        room,
        source_file: payload_path,
        content,
        added_at: envelope.captured_at.clone(),
        importance: 0,
        bypass_novelty: false,
        project_id,
    })
}

fn resolve_hook_project_id(
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
) -> Result<Option<String>> {
    resolve_project_id(None, config, Some(Path::new(&envelope.claude_cwd)))
        .map_err(anyhow::Error::from)
}

fn audit_target_for_event(
    event: &str,
    raw_payload: &str,
    config: &crate::core::config::Config,
) -> (String, String) {
    match event {
        "PostToolUse" => (
            "hooks-raw".to_string(),
            serde_json::from_str::<Value>(raw_payload)
                .ok()
                .and_then(|value| {
                    value
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| "unknown-tool".to_string()),
        ),
        "UserPromptSubmit" => ("hooks-raw".to_string(), "user-prompt".to_string()),
        "SessionStart" | "SessionEnd" => ("hooks-raw".to_string(), "session-lifecycle".to_string()),
        _ => (
            config.hooks.wing.clone(),
            envelope_agent_fallback(raw_payload),
        ),
    }
}

fn envelope_agent_fallback(raw_payload: &str) -> String {
    serde_json::from_str::<Value>(raw_payload)
        .ok()
        .and_then(|value| {
            value
                .get("agent")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown-agent".to_string())
}

async fn ingest_drawer_record<E: Embedder + ?Sized>(
    context: &DrawerIngestContext<'_, E>,
    record: DrawerRecord,
) -> Result<String> {
    let (drawer_id, exists) = {
        let record = record.clone();
        context
            .db
            .run_read_anyhow(move |db| {
                db.resolve_ingest_drawer_id(
                    &record.wing,
                    Some(record.room.as_str()),
                    &record.content,
                    record.project_id.as_deref(),
                )
                .with_context(|| {
                    format!(
                        "failed to resolve drawer identity for {}/{}",
                        record.wing, record.room
                    )
                })
            })
            .await?
    };
    if exists {
        return Ok(drawer_id);
    }

    let candidate = build_gating_candidate(context.envelope, &record);
    let mut gating_decision = evaluate_tier1(&candidate, &context.daemon.config.ingest_gating);
    if gating_decision.is_none()
        && context.daemon.config.ingest_gating.enabled
        && !tier2_enabled(&context.daemon.config.ingest_gating)
    {
        gating_decision = Some(GatingDecision::accepted(
            0,
            Some("tier2_disabled".to_string()),
            None,
        ));
    }
    if let Some(decision) = gating_decision.as_ref()
        && decision.is_rejected()
    {
        record_gating_audit_async(
            context.db,
            &drawer_id,
            decision,
            record.project_id.clone(),
            &candidate.content,
        )
        .await?;
        return Ok(drawer_id);
    }

    let heartbeat_store = context.store.clone();
    let heartbeat_message_id = context.message.id.clone();
    let heartbeat_worker_id = context.worker_id.to_string();
    let heartbeat = move || -> crate::embed::Result<()> {
        let store = heartbeat_store.clone();
        let message_id = heartbeat_message_id.clone();
        let worker_id = heartbeat_worker_id.clone();
        tokio::spawn(async move {
            if let Err(error) = store.refresh_heartbeat(message_id.clone(), worker_id).await {
                tracing::warn!(
                    ?error,
                    message_id,
                    "failed to refresh daemon ingest heartbeat"
                );
            }
        });
        Ok(())
    };

    let mut vector = None;
    let mut gating_audit_recorded = false;
    if gating_decision.is_none()
        && let Some(classifier) = context.daemon.prototype_classifier
    {
        let candidate_vector = embed_text_with_heartbeat(
            context.embedder,
            analysis_content(&record.content),
            Some(&heartbeat),
        )
        .await?;
        let decision = classifier.decide(
            &candidate_vector,
            context
                .daemon
                .config
                .ingest_gating
                .embedding_classifier
                .threshold,
        );
        record_gating_audit_async(
            context.db,
            &drawer_id,
            &decision,
            record.project_id.clone(),
            &candidate.content,
        )
        .await?;
        gating_audit_recorded = true;
        if decision.is_rejected() {
            return Ok(drawer_id);
        }
        gating_decision = Some(decision);
        vector = Some(candidate_vector);
    }
    if !gating_audit_recorded && let Some(decision) = gating_decision.as_ref() {
        record_gating_audit_async(
            context.db,
            &drawer_id,
            decision,
            record.project_id.clone(),
            &candidate.content,
        )
        .await?;
    }

    let vector = match vector {
        Some(vector) => vector,
        None => {
            embed_text_with_heartbeat(
                context.embedder,
                analysis_content(&record.content),
                Some(&heartbeat),
            )
            .await?
        }
    };
    if record.bypass_novelty {
        insert_drawer_with_vector_async(context.db, &drawer_id, record.clone(), vector.clone())
            .await?;
        enqueue_llm_gating_after_durable_insert(
            context.db,
            context.store,
            context.daemon.config,
            &gating_decision,
            &drawer_id,
            &candidate.content,
        )
        .await?;
        return Ok(drawer_id);
    }

    let novelty = {
        let candidate = NoveltyCandidate {
            wing: record.wing.clone(),
            room: Some(record.room.clone()),
            project_id: record.project_id.clone(),
        };
        let vector = vector.clone();
        let config = context.daemon.config.ingest_gating.novelty.clone();
        context
            .db
            .run_read_anyhow(move |db| Ok(evaluate_novelty(db, &candidate, &vector, &config)))
            .await?
    };
    match novelty.action {
        NoveltyAction::Insert => {
            if novelty.should_audit {
                record_novelty_audit_async(
                    context.db,
                    &drawer_id,
                    NoveltyAction::Insert,
                    novelty.near_drawer_id.clone(),
                    novelty.cosine,
                    novelty.audit_decision.map(ToOwned::to_owned),
                    record.project_id.clone(),
                )
                .await?;
            }
            insert_drawer_with_vector_async(context.db, &drawer_id, record.clone(), vector.clone())
                .await?;
            enqueue_llm_gating_after_durable_insert(
                context.db,
                context.store,
                context.daemon.config,
                &gating_decision,
                &drawer_id,
                &candidate.content,
            )
            .await?;
            Ok(drawer_id)
        }
        NoveltyAction::Drop => {
            if novelty.should_audit {
                record_novelty_audit_async(
                    context.db,
                    &drawer_id,
                    NoveltyAction::Drop,
                    novelty.near_drawer_id.clone(),
                    novelty.cosine,
                    novelty.audit_decision.map(ToOwned::to_owned),
                    record.project_id.clone(),
                )
                .await?;
            }
            Ok(novelty.near_drawer_id.unwrap_or(drawer_id))
        }
        NoveltyAction::Merge => {
            let target_id = novelty
                .near_drawer_id
                .clone()
                .unwrap_or_else(|| drawer_id.clone());
            let _target_lock = if target_id == drawer_id {
                None
            } else {
                Some(
                    crate::ingest::lock::acquire_source_lock(
                        context.daemon.mempal_home,
                        &target_id,
                        Duration::from_secs(5),
                    )
                    .with_context(|| format!("failed to lock merge target {}", target_id))?,
                )
            };
            let (existing_content, merge_count) = {
                let target_id = target_id.clone();
                context
                    .db
                    .run_read_anyhow(move |db| {
                        db.drawer_merge_state(&target_id)
                            .with_context(|| format!("failed to load merge target {}", target_id))?
                            .ok_or_else(|| {
                                anyhow::anyhow!("novelty merge target missing: {}", target_id)
                            })
                    })
                    .await?
            };
            let merged_at = current_timestamp();
            let merged_content = format!(
                "{existing_content}\n---\nSUPPLEMENTARY ({merged_at}):\n{}",
                record.content
            );
            let capped = merge_count
                >= context
                    .daemon
                    .config
                    .ingest_gating
                    .novelty
                    .max_merges_per_drawer
                || merged_content.len()
                    > context
                        .daemon
                        .config
                        .ingest_gating
                        .novelty
                        .max_content_bytes_per_drawer;
            if capped {
                record_novelty_audit_async(
                    context.db,
                    &drawer_id,
                    NoveltyAction::Insert,
                    Some(target_id),
                    novelty.cosine,
                    Some("insert_due_to_merge_cap".to_string()),
                    record.project_id.clone(),
                )
                .await?;
                insert_drawer_with_vector_async(
                    context.db,
                    &drawer_id,
                    record.clone(),
                    vector.clone(),
                )
                .await?;
                enqueue_llm_gating_after_durable_insert(
                    context.db,
                    context.store,
                    context.daemon.config,
                    &gating_decision,
                    &drawer_id,
                    &candidate.content,
                )
                .await?;
                Ok(drawer_id)
            } else {
                let merged_vector = match embed_text_with_heartbeat(
                    context.embedder,
                    &merged_content,
                    Some(&heartbeat),
                )
                .await
                {
                    Ok(vector) => vector,
                    Err(_error) => {
                        tracing::warn!(
                            target_id = %target_id,
                            candidate_drawer_id = %drawer_id,
                            merged_content_bytes = merged_content.len(),
                            "novelty merge re-embed failed; fail-open insert"
                        );
                        record_novelty_audit_async(
                            context.db,
                            &drawer_id,
                            NoveltyAction::Insert,
                            Some(target_id),
                            novelty.cosine,
                            Some("insert_due_to_embed_error".to_string()),
                            record.project_id.clone(),
                        )
                        .await?;
                        insert_drawer_with_vector_async(
                            context.db,
                            &drawer_id,
                            record.clone(),
                            vector.clone(),
                        )
                        .await?;
                        enqueue_llm_gating_after_durable_insert(
                            context.db,
                            context.store,
                            context.daemon.config,
                            &gating_decision,
                            &drawer_id,
                            &candidate.content,
                        )
                        .await?;
                        return Ok(drawer_id);
                    }
                };
                let drawer_id_for_merge = drawer_id.clone();
                let target_id_for_merge = target_id.clone();
                let audit_decision = novelty.audit_decision.map(ToOwned::to_owned);
                let project_id = record.project_id.clone();
                context
                    .db
                    .run_write_anyhow(move |db| {
                        db.record_novelty_audit(
                            &drawer_id_for_merge,
                            NoveltyAction::Merge,
                            Some(target_id_for_merge.as_str()),
                            novelty.cosine,
                            audit_decision.as_deref(),
                            project_id.as_deref(),
                        )
                        .with_context(|| {
                            format!("failed to record novelty audit {}", drawer_id_for_merge)
                        })?;
                        db.update_drawer_after_merge(
                            &target_id_for_merge,
                            &merged_content,
                            &merged_at,
                            &merged_vector,
                            merge_count,
                        )
                        .map_err(|error| match error {
                            DbError::DrawerMergeConflict { .. } => anyhow::Error::new(error),
                            error => anyhow::Error::new(error).context(format!(
                                "failed to merge hook drawer {}",
                                target_id_for_merge
                            )),
                        })?;
                        Ok(())
                    })
                    .await?;
                Ok(target_id)
            }
        }
    }
}

async fn enqueue_llm_gating_after_durable_insert(
    _db: &AsyncDb,
    store: &AsyncPendingMessageStore,
    config: &crate::core::config::Config,
    gating_decision: &Option<GatingDecision>,
    drawer_id: &str,
    content: &str,
) -> Result<()> {
    if !should_enqueue_llm_gating(config, gating_decision) {
        return Ok(());
    }

    let system_prompt = config
        .ingest_gating
        .llm_judge
        .as_ref()
        .and_then(|j| j.system_prompt.clone());
    let payload = serde_json::to_string(&crate::llm::LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: drawer_id.to_string(),
        drawer_ids: vec![drawer_id.to_string()],
        content: content.to_string(),
        system_prompt,
    })
    .context("failed to serialize LLM gating payload")?;
    let drawer_id = drawer_id.to_string();
    if let Err(error) = store.enqueue("llm_task".to_string(), payload).await {
        tracing::warn!(
            ?error,
            "failed to enqueue LLM gating task for {}",
            drawer_id
        );
    } else {
        tracing::info!("enqueued LLM gating task for {}", drawer_id);
    }
    Ok(())
}

async fn record_gating_audit_async(
    db: &AsyncDb,
    drawer_id: &str,
    decision: &GatingDecision,
    project_id: Option<String>,
    content: &str,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let decision = decision.clone();
    let content = content.to_string();
    db.run_write_anyhow(move |db| {
        db.record_gating_audit(&drawer_id, &decision, project_id.as_deref(), Some(&content))
            .with_context(|| format!("failed to record gating audit {}", drawer_id))?;
        Ok(())
    })
    .await
}

async fn record_novelty_audit_async(
    db: &AsyncDb,
    drawer_id: &str,
    action: NoveltyAction,
    near_drawer_id: Option<String>,
    cosine: Option<f32>,
    audit_decision: Option<String>,
    project_id: Option<String>,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    db.run_write_anyhow(move |db| {
        db.record_novelty_audit(
            &drawer_id,
            action,
            near_drawer_id.as_deref(),
            cosine,
            audit_decision.as_deref(),
            project_id.as_deref(),
        )
        .with_context(|| format!("failed to record novelty audit {}", drawer_id))?;
        Ok(())
    })
    .await
}

async fn insert_drawer_with_vector_async(
    db: &AsyncDb,
    drawer_id: &str,
    record: DrawerRecord,
    vector: Vec<f32>,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    db.run_write_anyhow(move |db| insert_drawer_with_vector(db, &drawer_id, &record, &vector))
        .await
}

fn insert_drawer_with_vector(
    db: &Database,
    drawer_id: &str,
    record: &DrawerRecord,
    vector: &[f32],
) -> Result<()> {
    if db
        .drawer_exists(drawer_id)
        .with_context(|| format!("failed to re-check existing drawer {}", drawer_id))?
    {
        return Ok(());
    }

    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: drawer_id.to_string(),
        content: record.content.clone(),
        wing: record.wing.clone(),
        room: Some(record.room.clone()),
        source_file: Some(record.source_file.clone()),
        source_type: SourceType::SystemGenerated,
        added_at: record.added_at.clone(),
        chunk_index: Some(0),
        importance: record.importance,
    });
    db.insert_drawer_with_project(&drawer, record.project_id.as_deref())
        .with_context(|| format!("failed to insert hook drawer {}", drawer.id))?;
    db.insert_vector_with_project(&drawer.id, vector, record.project_id.as_deref())
        .with_context(|| format!("failed to insert hook vector {}", drawer.id))?;
    Ok(())
}

fn preview_for_event(event: &str, raw_payload: &str) -> String {
    let parsed = serde_json::from_str::<Value>(raw_payload).ok();
    match event {
        "UserPromptSubmit" => parsed
            .as_ref()
            .and_then(|value| value.get("prompt"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| raw_payload.to_string()),
        "PostToolUse" => {
            let tool_name = parsed
                .as_ref()
                .and_then(|value| value.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown-tool");
            let input = parsed
                .as_ref()
                .and_then(|value| value.get("input"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let output = parsed
                .as_ref()
                .and_then(|value| value.get("output"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let exit_code = parsed
                .as_ref()
                .and_then(|value| value.get("exit_code"))
                .and_then(Value::as_i64)
                .unwrap_or_default();
            format!("tool={tool_name}\nexit_code={exit_code}\ninput={input}\noutput={output}")
        }
        "SessionStart" | "SessionEnd" => parsed
            .map(|value| {
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw_payload.to_string())
            })
            .unwrap_or_else(|| raw_payload.to_string()),
        _ => raw_payload.to_string(),
    }
}

fn user_prompt_content(raw_payload: &str) -> String {
    serde_json::from_str::<Value>(raw_payload)
        .ok()
        .and_then(|value| {
            value
                .get("prompt")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| raw_payload.to_string())
}

fn hook_payload_session_id(raw_payload: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw_payload)
        .ok()
        .and_then(|value| {
            value
                .get("session_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn persist_raw_payload(raw_payload: &str, mempal_home: &Path) -> Result<String> {
    let payload_dir = mempal_home.join("hook-payloads");
    fs::create_dir_all(&payload_dir)
        .with_context(|| format!("failed to create {}", payload_dir.display()))?;
    let digest = blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
    let path = payload_dir.join(format!("{digest}.json"));
    if !path.exists() {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(raw_payload.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    Ok(path.to_string_lossy().to_string())
}

fn should_enqueue_llm_gating(
    config: &crate::core::config::Config,
    gating_decision: &Option<GatingDecision>,
) -> bool {
    if !config.llm.enabled {
        return false;
    }
    if !config.llm.enabled_for.iter().any(|s| s == "gating") {
        return false;
    }
    let Some(judge) = config.ingest_gating.llm_judge.as_ref() else {
        return false;
    };
    if !judge.enabled {
        return false;
    }
    match gating_decision {
        Some(decision) if decision.is_rejected() => false,
        Some(decision) if decision.tier <= 1 && decision.label.is_some() => false,
        _ => true,
    }
}

struct DaemonEmbedder {
    primary: Box<dyn Embedder>,
    fallback: Option<Box<dyn Embedder>>,
}

#[cfg(unix)]
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn daemon_signal_handler(_signal: i32) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_shutdown_handlers() -> Result<()> {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    // SAFETY: installs a process signal handler that only writes an AtomicBool,
    // which is signal-safe.
    unsafe {
        let handler = daemon_signal_handler as *const () as usize;
        if libc::signal(libc::SIGTERM, handler) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error())
                .context("failed to install SIGTERM handler");
        }
        if libc::signal(libc::SIGINT, handler) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error())
                .context("failed to install SIGINT handler");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_shutdown_handlers() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
pub fn shutdown_requested() -> bool {
    false
}

impl DaemonEmbedder {
    async fn from_config(config: &crate::core::config::Config) -> crate::embed::Result<Self> {
        let primary = build_backend_from_name(config, config.embed.backend.as_str()).await?;
        let fallback = match config.embed.fallback.as_deref() {
            Some(name) if name.eq_ignore_ascii_case(config.embed.backend.as_str()) => None,
            Some(name) => Some(build_backend_from_name(config, name).await?),
            None => None,
        };
        Ok(Self { primary, fallback })
    }
}

#[async_trait::async_trait]
impl Embedder for DaemonEmbedder {
    async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
        let status = global_embed_status();
        if let Some(fallback) = &self.fallback {
            match self.primary.embed(texts).await {
                Ok(vectors) => {
                    status.record_primary_success();
                    Ok(vectors)
                }
                Err(primary_error) => {
                    status.record_failure(&primary_error);
                    let message = format!(
                        "embedder fallback active: {} failed, using {}",
                        self.primary.name(),
                        fallback.name()
                    );
                    let vectors = fallback.embed(texts).await?;
                    status.record_fallback_success(message);
                    Ok(vectors)
                }
            }
        } else {
            self.primary.embed(texts).await
        }
    }

    fn dimensions(&self) -> usize {
        self.primary.dimensions()
    }

    fn name(&self) -> &str {
        self.primary.name()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::core::{
        AsyncDb,
        config::{Config, LlmJudgeConfig, TurnStorageMode},
        db::Database,
        queue::{AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore, QueueError},
        types::{Drawer, SourceType},
    };
    use crate::embed::{EmbedError, Embedder};
    use crate::hook::{CapturedHookEnvelope, HookEvent};
    use std::pin::Pin;

    use super::{
        ClaimNextSource, ClaimPollResult, DaemonEmbedder, DaemonIngestContext, HookWorkerState,
        build_drawer_records, poll_claim_next, process_claimed_message_with_embedder,
        wing_from_cwd,
    };

    struct StubClaimSource {
        responses: Mutex<VecDeque<Result<Option<ClaimedMessage>, QueueError>>>,
    }

    impl StubClaimSource {
        fn new(responses: Vec<Result<Option<ClaimedMessage>, QueueError>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl ClaimNextSource for StubClaimSource {
        fn claim_next<'a>(
            &'a self,
            _worker_id: &'a str,
            _claim_ttl_secs: i64,
        ) -> Pin<
            Box<
                dyn Future<Output = crate::core::queue::Result<Option<ClaimedMessage>>> + Send + 'a,
            >,
        > {
            Box::pin(std::future::ready(
                self.responses
                    .lock()
                    .expect("responses mutex")
                    .pop_front()
                    .expect("stub response"),
            ))
        }
    }

    fn claimed_message(id: &str) -> ClaimedMessage {
        ClaimedMessage {
            id: id.to_string(),
            kind: "hook_user_prompt".to_string(),
            payload: "{}".to_string(),
            retry_count: 0,
            claim_token: "worker:claim".to_string(),
            source_hash: "hash".to_string(),
            created_at: 0,
            claimed_at: 0,
        }
    }

    fn unix_now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_secs() as i64
    }

    #[tokio::test]
    async fn test_daemon_survives_transient_claim_error() {
        let store = StubClaimSource::new(vec![
            Err(QueueError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::DatabaseBusy,
                    extended_code: rusqlite::ffi::SQLITE_BUSY,
                },
                Some("database is locked".to_string()),
            ))),
            Ok(Some(claimed_message("msg-1"))),
        ]);
        let slept = AtomicUsize::new(0);

        let first = poll_claim_next(&store, "worker-a", 60, |_| {
            slept.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        })
        .await;
        let second =
            poll_claim_next(&store, "worker-a", 60, |_| Box::pin(std::future::ready(()))).await;

        assert!(matches!(first, ClaimPollResult::RetryAfterError));
        assert_eq!(slept.load(Ordering::SeqCst), 1);
        match second {
            ClaimPollResult::Claimed(message) => assert_eq!(message.id, "msg-1"),
            ClaimPollResult::Idle | ClaimPollResult::RetryAfterError => {
                panic!("expected claimed message on retry")
            }
        }
    }

    struct StaticEmbedder;

    #[async_trait::async_trait]
    impl Embedder for StaticEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "static-test"
        }
    }

    struct HeartbeatProbeEmbedder {
        db_path: PathBuf,
        message_id: String,
        stale_heartbeat_at: i64,
        attempts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Embedder for HeartbeatProbeEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(EmbedError::Runtime("transient test failure".to_string()));
            }

            let heartbeat_at: Option<i64> = rusqlite::Connection::open(&self.db_path)
                .expect("open heartbeat probe db")
                .query_row(
                    "SELECT heartbeat_at FROM pending_messages WHERE id = ?1",
                    [self.message_id.as_str()],
                    |row| row.get(0),
                )
                .expect("read heartbeat");
            assert!(
                heartbeat_at.unwrap_or_default() > self.stale_heartbeat_at,
                "bounded hook worker must heartbeat using the same worker id that claimed the row"
            );

            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "heartbeat-probe"
        }
    }

    struct MergeConflictProbeEmbedder {
        db_path: PathBuf,
        injected: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Embedder for MergeConflictProbeEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            if texts.iter().any(|text| text.contains("SUPPLEMENTARY"))
                && self
                    .injected
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                let db = Database::open(&self.db_path)
                    .map_err(|error| EmbedError::Runtime(format!("open db failed: {error}")))?;
                db.update_drawer_after_merge(
                    "existing-target",
                    "original target\n---\nSUPPLEMENTARY (other):\nother supplement",
                    "1713000001",
                    &[0.8, 0.2, 0.0],
                    0,
                )
                .map_err(|error| EmbedError::Runtime(format!("inject merge failed: {error}")))?;
            }
            Ok(texts.iter().map(|_| vec![0.9, 0.435_889_9, 0.0]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "merge-conflict-probe"
        }
    }

    #[tokio::test]
    async fn test_bounded_hook_worker_heartbeats_with_claim_worker_id() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let mempal_home = tmp.path().join(".mempal");
        std::fs::create_dir_all(&mempal_home).expect("create mempal home");
        Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(&db_path).expect("store");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());

        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": "printf heartbeat",
            "output": "ok",
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
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        let queued_id = store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let worker_id = "bounded-hook-worker";
        let message = store
            .claim_next(worker_id, 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let stale_heartbeat_at = unix_now_secs() - 30;
        rusqlite::Connection::open(&db_path)
            .expect("open sqlite")
            .execute(
                "UPDATE pending_messages SET claimed_at = ?2, heartbeat_at = ?2 WHERE id = ?1",
                rusqlite::params![queued_id, stale_heartbeat_at],
            )
            .expect("age heartbeat");

        let config = Config::default();
        assert!(
            !config.llm.enabled,
            "test runtime config must keep LLM disabled"
        );
        super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                store: async_store,
                worker_id: worker_id.to_string(),
                embedder: std::sync::Arc::new(DaemonEmbedder {
                    primary: Box::new(HeartbeatProbeEmbedder {
                        db_path,
                        message_id: message.id.clone(),
                        stale_heartbeat_at,
                        attempts: AtomicUsize::new(0),
                    }),
                    fallback: None,
                }),
                prototype_classifier: std::sync::Arc::new(None),
                config: std::sync::Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            message,
        )
        .await;
    }

    #[tokio::test]
    async fn test_hook_worker_retries_stale_novelty_merge_without_overwrite() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let mempal_home = tmp.path().join(".mempal");
        std::fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db = Database::open(&db_path).expect("open db");
        let project_id = Some("merge-conflict-project");
        db.insert_drawer_with_project(
            &Drawer {
                id: "existing-target".to_string(),
                content: "original target".to_string(),
                wing: "hooks-raw".to_string(),
                room: Some("Bash".to_string()),
                source_file: Some("existing-target.md".to_string()),
                source_type: SourceType::SystemGenerated,
                added_at: "1713000000".to_string(),
                chunk_index: Some(0),
                ..Drawer::default()
            },
            project_id,
        )
        .expect("insert target drawer");
        db.insert_vector_with_project("existing-target", &[1.0, 0.0, 0.0], project_id)
            .expect("insert target vector");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(&db_path).expect("store");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());

        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": "printf candidate",
            "output": "candidate supplement",
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
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        let queued_id = store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("merge-conflict-worker", 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let mut config = Config::default();
        config.project.id = project_id.map(ToOwned::to_owned);
        config.ingest_gating.novelty.enabled = true;
        config.ingest_gating.novelty.merge_threshold = 0.85;
        config.ingest_gating.novelty.duplicate_threshold = 0.95;
        assert!(
            !config.llm.enabled,
            "test runtime config must keep LLM disabled"
        );
        let injected = Arc::new(AtomicBool::new(false));
        super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                store: async_store,
                worker_id: "merge-conflict-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder {
                    primary: Box::new(MergeConflictProbeEmbedder {
                        db_path: db_path.clone(),
                        injected: Arc::clone(&injected),
                    }),
                    fallback: None,
                }),
                prototype_classifier: Arc::new(None),
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            message,
        )
        .await;
        assert!(
            injected.load(Ordering::SeqCst),
            "test embedder must inject a concurrent merge during re-embed"
        );

        let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
        let (status, retry_count, last_error): (String, i64, Option<String>) = conn
            .query_row(
                "SELECT status, retry_count, last_error FROM pending_messages WHERE id = ?1",
                [queued_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query queue row");
        assert_eq!(status, "pending");
        assert_eq!(retry_count, 1);
        assert!(
            last_error
                .as_deref()
                .is_some_and(|error| error.contains("changed during novelty merge")),
            "last_error={last_error:?}"
        );

        let (content, merge_count): (String, i64) = conn
            .query_row(
                "SELECT content, merge_count FROM drawers WHERE id = 'existing-target'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query target drawer");
        assert!(content.contains("other supplement"));
        assert!(!content.contains("candidate supplement"));
        assert_eq!(merge_count, 1);
    }

    struct LlmClaimRaceProbeEmbedder {
        store: PendingMessageStore,
        embed_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Embedder for LlmClaimRaceProbeEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            self.embed_calls.fetch_add(1, Ordering::SeqCst);
            let claim = self
                .store
                .claim_next_by_kind("llm-race-probe", 60, "llm_task")
                .map_err(|error| EmbedError::Runtime(format!("claim llm task failed: {error}")))?;
            assert!(
                claim.is_none(),
                "LLM gating task must not be claimable before drawer/vector insert completes"
            );
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "llm-race-probe"
        }
    }

    #[tokio::test]
    async fn test_daemon_uses_envelope_captured_at_for_drawer_added_at() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let captured_at = "2026-05-01T12:34:56Z";
        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": "date",
            "output": "Fri May  1 12:34:56 UTC 2026",
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: captured_at.to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(hook_payload.clone()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: hook_payload.len(),
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        let queued_id = store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("timestamp-test-worker", 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let mut config = Config::default();
        config.project.id = Some("timestamp-test".to_string());
        process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "timestamp-test-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &config,
                mempal_home: tmp.path(),
            },
        )
        .await
        .expect("process hook envelope");

        let (added_at, source_type, confidence): (String, String, f64) = db
            .conn()
            .query_row(
                "SELECT added_at, source_type, confidence FROM drawers WHERE room = 'Bash' AND deleted_at IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query drawer metadata");
        assert_eq!(added_at, captured_at);
        assert_eq!(source_type, "system_generated");
        assert!((confidence - 0.3).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_daemon_enqueues_llm_gating_after_drawer_vector_is_durable() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": "printf race",
            "output": "durable before llm",
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
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        let queued_id = store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("llm-race-worker", 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let mut config = Config::default();
        config.llm.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let embedder = LlmClaimRaceProbeEmbedder {
            store: store.clone(),
            embed_calls: AtomicUsize::new(0),
        };
        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "llm-race-worker",
            &message,
            &embedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &config,
                mempal_home: tmp.path(),
            },
        )
        .await
        .expect("process hook envelope");

        assert_eq!(embedder.embed_calls.load(Ordering::SeqCst), 1);
        assert!(
            db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "drawer must exist before LLM task is claimable"
        );
        let vector_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                [drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("query vector");
        assert_eq!(
            vector_count, 1,
            "vector must exist before LLM task is claimable"
        );

        let llm_task = store
            .claim_next_by_kind("llm-race-final", 60, "llm_task")
            .expect("claim llm task")
            .expect("llm task should be queued after durable insert");
        let task: crate::llm::LlmTaskPayload =
            serde_json::from_str(&llm_task.payload).expect("decode llm task");
        assert_eq!(task.drawer_id, drawer_id);
    }

    #[test]
    fn test_storage_mode_off_skips_hook_payload_file_for_raw_audit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let raw_payload = serde_json::json!({
            "session_id": "sess-raw-off",
            "tool_name": "Bash",
            "input": "printf secret",
            "output": "raw payload must not be written",
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-05-01T12:34:56Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(raw_payload.clone()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: raw_payload.len(),
            truncated: false,
        };
        let mut config = Config::default();
        config.turns.storage_mode = TurnStorageMode::Off;
        config.turns.raw_turn_wings = vec!["hooks-raw".to_string()];
        config.turns.raw_turn_rooms = Vec::new();

        let records =
            build_drawer_records(&db, &envelope, &config, &mempal_home).expect("drawer records");

        assert!(records.is_empty());
        assert!(
            !mempal_home.join("hook-payloads").exists(),
            "raw hook payload directory must not be created when turn storage is off"
        );
    }

    #[test]
    fn test_wing_from_cwd_returns_basename() {
        assert_eq!(
            wing_from_cwd("/home/obj/project/github/RyderFreeman4Logos/mempal"),
            Some("mempal".to_string())
        );
        assert_eq!(
            wing_from_cwd("/home/user/projects/warifu-ce"),
            Some("warifu-ce".to_string())
        );
        assert_eq!(
            wing_from_cwd("/home/user/my-project/"),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn test_wing_from_cwd_returns_none_for_root_and_empty() {
        assert_eq!(wing_from_cwd("/"), None);
        assert_eq!(wing_from_cwd(""), None);
    }
}

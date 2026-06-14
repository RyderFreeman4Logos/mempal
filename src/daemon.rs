use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{future::Future, pin::Pin};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    AsyncDb,
    db::{Database, DbError, NoveltyAuditInsert},
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
use arc_swap::ArcSwap;
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
const ENDPOINT_RECOVERY_REQUEUE_INTERVAL: Duration = Duration::from_secs(30);

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

    #[cfg(unix)]
    let hook_ipc_service = spawn_hook_ipc_service(
        &context.mempal_home,
        context.store.clone(),
        context.write_observer.clone(),
    )
    .await
    .context("failed to start daemon hook IPC service")?;

    let embedder = Arc::new(
        DaemonEmbedder::from_config(context.config.as_ref())
            .await
            .context("failed to build daemon embedder")?,
    );
    let prototype_classifier: Arc<ArcSwap<Option<PrototypeClassifier>>> =
        Arc::new(ArcSwap::from_pointee(None));
    {
        let classifier_slot = Arc::clone(&prototype_classifier);
        let embedder_for_init = Arc::clone(&embedder);
        let gating_config = context.config.ingest_gating.clone();
        tokio::spawn(async move {
            loop {
                match compile_classifier_from_embedder(embedder_for_init.as_ref(), &gating_config)
                    .await
                {
                    Ok(classifier) => {
                        classifier_slot.store(Arc::new(classifier));
                        tracing::info!("gating prototype classifier initialized");
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "gating prototype init failed; retrying in 2s"
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });
    }
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
    let endpoint_requeue_handle = spawn_endpoint_recovery_requeue_worker(
        context.store.clone(),
        Arc::new(crate::core::config::ConfigHandle::current),
        ENDPOINT_RECOVERY_REQUEUE_INTERVAL,
    );

    let llm_worker_handles: Vec<tokio::task::JoinHandle<_>> = if context.config.llm.enabled {
        let llm_client_runtime = crate::llm::worker::LlmClientRuntime::new(&context.config.llm);
        let num_workers = context.config.llm.pool_capacity();
        let llm_status = Arc::new(crate::llm::LlmStatus::new(10));
        let llm_store = Arc::new(context.store.clone());
        let llm_client_runtime = Arc::new(Mutex::new(llm_client_runtime));
        let async_db = context.async_db.clone();
        let write_observer = context.write_observer.clone();
        tracing::info!("spawning {num_workers} LLM worker tasks");
        (0..num_workers)
            .map(|i| {
                let store = llm_store.clone();
                let client_runtime = llm_client_runtime.clone();
                let status = llm_status.clone();
                let db = async_db.clone();
                let observer = write_observer.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::llm::worker::run_llm_worker(
                        store,
                        client_runtime,
                        status,
                        db,
                        observer,
                    )
                    .await
                    {
                        tracing::error!("LLM worker {i} fatal error: {e:#}");
                    }
                    Ok::<(), anyhow::Error>(())
                })
            })
            .collect()
    } else {
        vec![]
    };

    let hook_worker_state = HookWorkerState {
        async_db: context.async_db.clone(),
        store: context.store.clone(),
        worker_id: worker_id.clone(),
        embedder: Arc::clone(&embedder),
        prototype_classifier: Arc::clone(&prototype_classifier),
        config: Arc::clone(&context.config),
        mempal_home: context.mempal_home.clone(),
        write_observer: context.write_observer.clone(),
    };
    let mut hook_workers = JoinSet::new();
    let mut next_hook_worker_index = 0;
    spawn_hook_workers_until_limit(
        &mut hook_workers,
        &hook_worker_state,
        &mut next_hook_worker_index,
        claim_ttl_secs,
        poll_interval,
    );
    loop {
        context.write_observer.maybe_log_stall(&context.store).await;

        if shutdown_requested() {
            tracing::info!("shutdown requested; stopping daemon loop");
            break;
        }

        spawn_hook_workers_until_limit(
            &mut hook_workers,
            &hook_worker_state,
            &mut next_hook_worker_index,
            claim_ttl_secs,
            poll_interval,
        );
        wait_for_hook_worker_or_tick(&mut hook_workers, poll_interval).await;
    }

    #[cfg(unix)]
    hook_ipc_service.shutdown().await;

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
    endpoint_requeue_handle.abort();
    let _ = endpoint_requeue_handle.await;

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

#[derive(Clone)]
struct HookWorkerState {
    async_db: AsyncDb,
    store: AsyncPendingMessageStore,
    worker_id: String,
    embedder: Arc<DaemonEmbedder>,
    prototype_classifier: Arc<ArcSwap<Option<PrototypeClassifier>>>,
    config: Arc<crate::core::config::Config>,
    mempal_home: PathBuf,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
}

fn spawn_hook_workers_until_limit(
    hook_workers: &mut JoinSet<()>,
    state: &HookWorkerState,
    next_worker_index: &mut usize,
    claim_ttl_secs: i64,
    poll_interval: Duration,
) {
    while hook_workers.len() < DAEMON_HOOK_WORKER_LIMIT {
        let worker_index = *next_worker_index;
        *next_worker_index = (*next_worker_index).saturating_add(1);
        spawn_hook_worker(
            hook_workers,
            state.clone(),
            worker_index,
            claim_ttl_secs,
            poll_interval,
        );
    }
}

fn spawn_hook_worker(
    hook_workers: &mut JoinSet<()>,
    mut state: HookWorkerState,
    worker_index: usize,
    claim_ttl_secs: i64,
    poll_interval: Duration,
) {
    state.worker_id = format!("{}-hook-{worker_index}", state.worker_id);
    hook_workers.spawn(async move {
        run_hook_worker(state, claim_ttl_secs, poll_interval).await;
    });
}

async fn run_hook_worker(state: HookWorkerState, claim_ttl_secs: i64, poll_interval: Duration) {
    loop {
        if shutdown_requested() {
            break;
        }

        match poll_claim_next(&state.store, &state.worker_id, claim_ttl_secs, |duration| {
            Box::pin(tokio::time::sleep(duration))
        })
        .await
        {
            ClaimPollResult::Claimed(message) => {
                process_hook_worker_message(state.clone(), message, claim_ttl_secs).await;
            }
            ClaimPollResult::Idle => tokio::time::sleep(poll_interval).await,
            ClaimPollResult::RetryAfterError => continue,
        }
    }
}

async fn process_hook_worker_message(
    state: HookWorkerState,
    message: ClaimedMessage,
    claim_ttl_secs: i64,
) {
    let message_id = message.id.clone();
    refresh_hook_message_heartbeat(&state.store, &message_id, &state.worker_id).await;
    let heartbeat_handle = spawn_hook_message_heartbeat(
        state.store.clone(),
        message_id.clone(),
        state.worker_id.clone(),
        hook_message_heartbeat_interval(claim_ttl_secs),
    );
    let classifier_arc = state.prototype_classifier.load_full();
    let classifier_ref = classifier_arc.as_ref().as_ref();
    let result = process_claimed_message_with_embedder(
        &state.async_db,
        &state.store,
        &state.worker_id,
        &message,
        state.embedder.as_ref(),
        DaemonIngestContext {
            prototype_classifier: classifier_ref,
            config: state.config.as_ref(),
            mempal_home: &state.mempal_home,
        },
    )
    .await;

    match result {
        Ok(_) => {
            if let Err(error) = state.store.confirm(message.clone()).await {
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
                .mark_failed_with_disposition(message.clone(), error.to_string(), disposition)
                .await
            {
                tracing::error!(?mark_error, "failed to mark_failed {message_id}");
                state
                    .write_observer
                    .record_error(format!("failed to mark_failed {message_id}: {mark_error}"));
            }
        }
    }
    heartbeat_handle.abort();
    let _ = heartbeat_handle.await;
}

fn spawn_hook_message_heartbeat(
    store: AsyncPendingMessageStore,
    message_id: String,
    worker_id: String,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if shutdown_requested() {
                break;
            }
            refresh_hook_message_heartbeat(&store, &message_id, &worker_id).await;
        }
    })
}

async fn refresh_hook_message_heartbeat(
    store: &AsyncPendingMessageStore,
    message_id: &str,
    worker_id: &str,
) {
    if let Err(error) = store
        .refresh_heartbeat(message_id.to_string(), worker_id.to_string())
        .await
    {
        tracing::warn!(
            ?error,
            message_id,
            "failed to refresh hook message heartbeat"
        );
    }
}

fn hook_message_heartbeat_interval(claim_ttl_secs: i64) -> Duration {
    let ttl_secs = claim_ttl_secs.max(1) as u64;
    Duration::from_secs((ttl_secs / 3).max(1))
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EndpointRecoveryRequeuePlan {
    embedding: bool,
    llm: bool,
}

impl EndpointRecoveryRequeuePlan {
    fn is_empty(self) -> bool {
        !self.embedding && !self.llm
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct EndpointRecoveryRequeueState {
    embedding_reachable: bool,
    llm_generation_reachable: bool,
}

impl EndpointRecoveryRequeueState {
    fn plan(
        &self,
        health: &crate::endpoint_health::EndpointHealthSnapshot,
    ) -> EndpointRecoveryRequeuePlan {
        EndpointRecoveryRequeuePlan {
            embedding: health.embedding.reachable && !self.embedding_reachable,
            llm: health.llm_generation.reachable && !self.llm_generation_reachable,
        }
    }

    fn commit_successes(
        &mut self,
        health: &crate::endpoint_health::EndpointHealthSnapshot,
        completed: EndpointRecoveryRequeuePlan,
    ) {
        if health.embedding.reachable {
            self.embedding_reachable = self.embedding_reachable || completed.embedding;
        } else {
            self.embedding_reachable = false;
        }
        if health.llm_generation.reachable {
            self.llm_generation_reachable = self.llm_generation_reachable || completed.llm;
        } else {
            self.llm_generation_reachable = false;
        }
    }
}

type EndpointRecoveryConfigProvider =
    Arc<dyn Fn() -> Arc<crate::core::config::Config> + Send + Sync>;

fn endpoint_recovery_probe_config(
    config_provider: &EndpointRecoveryConfigProvider,
) -> Arc<crate::core::config::Config> {
    config_provider()
}

fn spawn_endpoint_recovery_requeue_worker(
    store: AsyncPendingMessageStore,
    config_provider: EndpointRecoveryConfigProvider,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut state = EndpointRecoveryRequeueState::default();
        loop {
            ticker.tick().await;
            if shutdown_requested() {
                break;
            }
            let config = endpoint_recovery_probe_config(&config_provider);
            let (health, daemon_llm_generation) = tokio::join!(
                crate::endpoint_health::probe_endpoints(config.as_ref()),
                crate::endpoint_health::probe_daemon_llm_generation(config.as_ref())
            );
            let health = crate::endpoint_health::EndpointHealthSnapshot {
                llm: daemon_llm_generation.clone(),
                llm_generation: daemon_llm_generation,
                ..health
            };
            if let Err(error) =
                requeue_failed_model_tasks_after_recovery(&store, &mut state, &health).await
            {
                tracing::warn!(
                    ?error,
                    "failed to auto-requeue model tasks after endpoint recovery"
                );
            }
        }
    })
}

async fn requeue_failed_model_tasks_after_recovery(
    store: &AsyncPendingMessageStore,
    state: &mut EndpointRecoveryRequeueState,
    health: &crate::endpoint_health::EndpointHealthSnapshot,
) -> crate::core::queue::Result<EndpointRecoveryRequeuePlan> {
    let plan = state.plan(health);
    if plan.is_empty() {
        state.commit_successes(health, EndpointRecoveryRequeuePlan::default());
        return Ok(plan);
    }
    let mut completed = EndpointRecoveryRequeuePlan::default();
    let mut error = None;
    if plan.embedding {
        match store
            .auto_requeue_failed_model_tasks("embedding".to_string())
            .await
        {
            Ok(requeued) => {
                completed.embedding = true;
                if requeued > 0 {
                    tracing::info!(
                        requeued,
                        "auto-requeued failed embedding tasks after endpoint recovery"
                    );
                }
            }
            Err(requeue_error) => {
                error.get_or_insert(requeue_error);
            }
        }
    }
    if plan.llm {
        match store
            .auto_requeue_failed_model_tasks("llm".to_string())
            .await
        {
            Ok(requeued) => {
                completed.llm = true;
                if requeued > 0 {
                    tracing::info!(
                        requeued,
                        "auto-requeued failed LLM tasks after endpoint recovery"
                    );
                }
            }
            Err(requeue_error) => {
                error.get_or_insert(requeue_error);
            }
        }
    }
    state.commit_successes(health, completed);
    if let Some(error) = error {
        return Err(error);
    }
    Ok(plan)
}

#[cfg(unix)]
struct HookIpcServiceHandle {
    listener_task: Option<tokio::task::JoinHandle<()>>,
    socket_guard: Option<crate::hook_ipc::SocketFileGuard>,
}

#[cfg(unix)]
impl HookIpcServiceHandle {
    async fn shutdown(mut self) {
        if let Some(listener_task) = self.listener_task.take() {
            listener_task.abort();
            let _ = listener_task.await;
        }

        self.socket_guard.take();
    }
}

#[cfg(unix)]
impl Drop for HookIpcServiceHandle {
    fn drop(&mut self) {
        if let Some(listener_task) = &self.listener_task {
            listener_task.abort();
        }
        self.socket_guard.take();
    }
}

#[cfg(unix)]
async fn spawn_hook_ipc_service(
    mempal_home: &Path,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
) -> Result<HookIpcServiceHandle> {
    let (listener, socket_guard) = crate::hook_ipc::bind_listener(mempal_home)?;
    let socket_path = socket_guard.path().to_path_buf();
    let listener_task = tokio::spawn(run_hook_ipc_listener(listener, store, write_observer));
    tracing::info!("daemon hook IPC listening on {}", socket_path.display());
    Ok(HookIpcServiceHandle {
        listener_task: Some(listener_task),
        socket_guard: Some(socket_guard),
    })
}

#[cfg(unix)]
async fn run_hook_ipc_listener(
    listener: tokio::net::UnixListener,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
) {
    loop {
        if shutdown_requested() {
            break;
        }

        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let store = store.clone();
                        let write_observer = write_observer.clone();
                        tokio::spawn(async move {
                            handle_hook_ipc_connection(stream, store, write_observer).await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(?error, "hook IPC accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
}

#[cfg(unix)]
async fn handle_hook_ipc_connection(
    mut stream: tokio::net::UnixStream,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
) {
    let request_result = tokio::time::timeout(
        crate::hook_ipc::HOOK_IPC_READ_TIMEOUT,
        crate::hook_ipc::read_enqueue_request(&mut stream),
    )
    .await;
    let response = match request_result {
        Ok(Ok(request)) => persist_hook_ipc_request(&store, &write_observer, request).await,
        Ok(Err(error)) => crate::hook_ipc::HookIpcEnqueueResponse::Error {
            message: format!("invalid hook IPC request: {error}"),
        },
        Err(_) => crate::hook_ipc::HookIpcEnqueueResponse::Error {
            message: "invalid hook IPC request: timed out reading frame".to_string(),
        },
    };

    if let Err(error) = crate::hook_ipc::write_enqueue_response(&mut stream, &response).await {
        tracing::warn!(?error, "failed to write hook IPC response");
    }
}

#[cfg(unix)]
async fn persist_hook_ipc_request(
    store: &AsyncPendingMessageStore,
    write_observer: &crate::daemon_bootstrap::DaemonWriteObserver,
    request: crate::hook_ipc::HookIpcEnqueueRequest,
) -> crate::hook_ipc::HookIpcEnqueueResponse {
    match store
        .enqueue_idempotent_with_key(
            request.kind.clone(),
            request.payload.clone(),
            request.idempotency_key.clone(),
        )
        .await
    {
        Ok(message_id) => {
            tracing::debug!(message_id, kind = %request.kind, "persisted hook IPC capture");
            write_observer.record_successful_write();
            crate::hook_ipc::HookIpcEnqueueResponse::Accepted
        }
        Err(error) => {
            let message = format!("failed to persist hook IPC capture: {error}");
            write_observer.record_error(message.clone());
            tracing::warn!(?error, kind = %request.kind, "failed to persist hook IPC capture");
            crate::hook_ipc::HookIpcEnqueueResponse::Error { message }
        }
    }
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
        && !should_enqueue_llm_gating(context.daemon.config, &gating_decision)
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
        if should_enqueue_llm_gating(context.daemon.config, &Some(decision.clone())) {
            let llm_decision = GatingDecision::accepted(0, Some("llm_pending".to_string()), None);
            record_gating_audit_async(
                context.db,
                &drawer_id,
                &llm_decision,
                record.project_id.clone(),
                &candidate.content,
            )
            .await?;
            gating_audit_recorded = true;
            gating_decision = Some(llm_decision);
        } else {
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
        }
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
                        db.update_drawer_after_merge_and_record_novelty_audit(
                            &target_id_for_merge,
                            &merged_content,
                            &merged_at,
                            &merged_vector,
                            merge_count,
                            NoveltyAuditInsert {
                                candidate_hash: &drawer_id_for_merge,
                                action: NoveltyAction::Merge,
                                near_drawer_id: Some(target_id_for_merge.as_str()),
                                cosine: novelty.cosine,
                                audit_decision: audit_decision.as_deref(),
                                project_id: project_id.as_deref(),
                            },
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
    crate::ingest::gating::should_route_to_llm_judge(config, gating_decision)
}

pub(crate) fn llm_worker_claim_enabled(config: &crate::core::config::Config) -> bool {
    config.llm.enabled && !config.llm.enabled_for.is_empty()
}

struct DaemonEmbedder {
    name: String,
    runtime: Mutex<DaemonEmbedderRuntime>,
}

struct DaemonEmbedderRuntime {
    generation: u64,
    primary: Arc<dyn Embedder>,
    fallback: Option<Arc<dyn Embedder>>,
}

#[derive(Clone)]
struct DaemonEmbedderRuntimeSnapshot {
    primary: Arc<dyn Embedder>,
    fallback: Option<Arc<dyn Embedder>>,
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
        let generation = crate::core::config::ConfigHandle::current_embed_generation();
        let runtime = DaemonEmbedderRuntime::from_config(config, generation).await?;
        let name = runtime.primary.name().to_string();
        Ok(Self {
            name,
            runtime: Mutex::new(runtime),
        })
    }

    async fn runtime_snapshot(&self) -> crate::embed::Result<DaemonEmbedderRuntimeSnapshot> {
        let generation = crate::core::config::ConfigHandle::current_embed_generation();
        if let Some(snapshot) = self.snapshot_if_generation(generation) {
            return Ok(snapshot);
        }

        let config = crate::core::config::ConfigHandle::current();
        let replacement = DaemonEmbedderRuntime::from_config(config.as_ref(), generation).await?;
        let mut guard = self
            .runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned");
        if guard.generation != generation {
            *guard = replacement;
        }
        Ok(guard.snapshot())
    }

    fn snapshot_if_generation(&self, generation: u64) -> Option<DaemonEmbedderRuntimeSnapshot> {
        let guard = self
            .runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned");
        (guard.generation == generation).then(|| guard.snapshot())
    }

    #[cfg(test)]
    fn from_primary_for_test(primary: Box<dyn Embedder>) -> Self {
        let name = primary.name().to_string();
        let runtime = DaemonEmbedderRuntime {
            generation: crate::core::config::ConfigHandle::current_embed_generation(),
            primary: Arc::from(primary),
            fallback: None,
        };
        Self {
            name,
            runtime: Mutex::new(runtime),
        }
    }
}

#[async_trait::async_trait]
impl Embedder for DaemonEmbedder {
    async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
        let runtime = self.runtime_snapshot().await?;
        let status = global_embed_status();
        if let Some(fallback) = &runtime.fallback {
            match runtime.primary.embed(texts).await {
                Ok(vectors) => {
                    status.record_primary_success();
                    Ok(vectors)
                }
                Err(primary_error) => {
                    status.record_failure(&primary_error);
                    let message = format!(
                        "embedder fallback active: {} failed, using {}",
                        runtime.primary.name(),
                        fallback.name()
                    );
                    let vectors = fallback.embed(texts).await?;
                    status.record_fallback_success(message);
                    Ok(vectors)
                }
            }
        } else {
            runtime.primary.embed(texts).await
        }
    }

    fn dimensions(&self) -> usize {
        self.runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned")
            .primary
            .dimensions()
    }

    fn name(&self) -> &str {
        &self.name
    }
}

impl DaemonEmbedderRuntime {
    async fn from_config(
        config: &crate::core::config::Config,
        generation: u64,
    ) -> crate::embed::Result<Self> {
        let primary: Arc<dyn Embedder> =
            Arc::from(build_backend_from_name(config, config.embed.backend.as_str()).await?);
        let fallback = match config.embed.fallback.as_deref() {
            Some(name) if name.eq_ignore_ascii_case(config.embed.backend.as_str()) => None,
            Some(name) => Some(Arc::from(build_backend_from_name(config, name).await?)),
            None => None,
        };
        Ok(Self {
            generation,
            primary,
            fallback,
        })
    }

    fn snapshot(&self) -> DaemonEmbedderRuntimeSnapshot {
        DaemonEmbedderRuntimeSnapshot {
            primary: Arc::clone(&self.primary),
            fallback: self.fallback.as_ref().map(Arc::clone),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::core::{
        AsyncDb,
        config::{Config, LlmJudgeConfig, TurnStorageMode},
        db::Database,
        queue::{
            AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore, QueueError,
            QueueFailureDisposition,
        },
        types::{Drawer, SourceType},
    };
    use crate::embed::{EmbedError, Embedder};
    use crate::endpoint_health::{EndpointHealthSnapshot, ProbeStatus};
    use crate::hook::{CapturedHookEnvelope, HookEvent};
    use arc_swap::ArcSwap;
    use std::pin::Pin;

    use super::{
        ClaimNextSource, ClaimPollResult, DaemonEmbedder, DaemonIngestContext,
        EndpointRecoveryConfigProvider, EndpointRecoveryRequeuePlan, EndpointRecoveryRequeueState,
        HookWorkerState, build_drawer_records, compile_classifier_from_embedder,
        llm_worker_claim_enabled, poll_claim_next, process_claimed_message_with_embedder,
        run_hook_worker, wing_from_cwd,
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

    fn endpoint_health_snapshot(
        embedding_reachable: bool,
        llm_generation_reachable: bool,
    ) -> EndpointHealthSnapshot {
        let embedding = ProbeStatus {
            reachable: embedding_reachable,
            latency_ms: None,
            detail: "test embedding".to_string(),
        };
        let llm_generation = ProbeStatus {
            reachable: llm_generation_reachable,
            latency_ms: None,
            detail: "test generation".to_string(),
        };
        EndpointHealthSnapshot {
            embedding,
            llm: llm_generation.clone(),
            llm_control_plane: ProbeStatus {
                reachable: llm_generation_reachable,
                latency_ms: None,
                detail: "test control plane".to_string(),
            },
            llm_generation,
        }
    }

    #[test]
    fn test_endpoint_recovery_probe_config_uses_current_provider_value() {
        let mut old_config = crate::core::config::Config::default();
        old_config.llm.model = Some("old-model".to_string());
        let mut new_config = crate::core::config::Config::default();
        new_config.llm.model = Some("new-model".to_string());

        let current = Arc::new(ArcSwap::from_pointee(old_config));
        let provider_current = Arc::clone(&current);
        let provider: EndpointRecoveryConfigProvider =
            Arc::new(move || provider_current.load_full());

        assert_eq!(
            super::endpoint_recovery_probe_config(&provider)
                .llm
                .model
                .as_deref(),
            Some("old-model")
        );
        current.store(Arc::new(new_config));
        assert_eq!(
            super::endpoint_recovery_probe_config(&provider)
                .llm
                .model
                .as_deref(),
            Some("new-model")
        );
    }

    #[test]
    fn test_endpoint_recovery_requeue_state_triggers_only_on_reachable_edges() {
        let mut state = EndpointRecoveryRequeueState::default();

        let down = endpoint_health_snapshot(false, false);
        assert_eq!(
            state.plan(&down),
            EndpointRecoveryRequeuePlan {
                embedding: false,
                llm: false,
            }
        );
        state.commit_successes(&down, EndpointRecoveryRequeuePlan::default());

        let embedding_up = endpoint_health_snapshot(true, false);
        let embedding_plan = EndpointRecoveryRequeuePlan {
            embedding: true,
            llm: false,
        };
        assert_eq!(state.plan(&embedding_up), embedding_plan);
        state.commit_successes(&embedding_up, embedding_plan);
        assert_eq!(
            state.plan(&embedding_up),
            EndpointRecoveryRequeuePlan {
                embedding: false,
                llm: false,
            }
        );

        state.commit_successes(&down, EndpointRecoveryRequeuePlan::default());
        let llm_up = endpoint_health_snapshot(false, true);
        assert_eq!(
            state.plan(&llm_up),
            EndpointRecoveryRequeuePlan {
                embedding: false,
                llm: true,
            }
        );
    }

    #[test]
    fn test_endpoint_recovery_requeue_state_commits_edges_only_after_success() {
        let health = endpoint_health_snapshot(true, true);
        let mut state = EndpointRecoveryRequeueState::default();

        let first_plan = state.plan(&health);
        assert_eq!(
            first_plan,
            EndpointRecoveryRequeuePlan {
                embedding: true,
                llm: true,
            }
        );
        assert_eq!(state.plan(&health), first_plan);

        state.commit_successes(
            &health,
            EndpointRecoveryRequeuePlan {
                embedding: true,
                llm: false,
            },
        );
        assert_eq!(
            state.plan(&health),
            EndpointRecoveryRequeuePlan {
                embedding: false,
                llm: true,
            }
        );

        state.commit_successes(
            &health,
            EndpointRecoveryRequeuePlan {
                embedding: false,
                llm: true,
            },
        );
        assert!(state.plan(&health).is_empty());

        let down = endpoint_health_snapshot(false, false);
        state.commit_successes(&down, EndpointRecoveryRequeuePlan::default());
        assert_eq!(state.plan(&health), first_plan);
    }

    #[tokio::test]
    async fn test_endpoint_recovery_requeues_only_retryable_failed_model_tasks() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let sync_store = PendingMessageStore::new_without_reclaim(&db_path);
        let async_store = AsyncPendingMessageStore::new_without_reclaim(&db_path);

        let retryable_embedding_id = sync_store
            .enqueue("hook_event", r#"{"n":1}"#)
            .expect("enqueue retryable embedding");
        let terminal_embedding_id = sync_store
            .enqueue("hook_event", r#"{"n":2}"#)
            .expect("enqueue terminal embedding");
        let retryable_llm_id = sync_store
            .enqueue("llm_task", r#"{"task_type":"gating","drawer_id":"d1"}"#)
            .expect("enqueue retryable llm");

        sync_store
            .mark_model_task_failed_retryable(&retryable_embedding_id, "timeout")
            .expect("mark retryable embedding failed");
        let terminal_claim = sync_store
            .claim_next("terminal-worker", 60)
            .expect("claim terminal embedding")
            .expect("terminal embedding row");
        assert_eq!(terminal_claim.id, terminal_embedding_id);
        sync_store
            .mark_failed_with_disposition(
                &terminal_claim,
                "invalid payload",
                QueueFailureDisposition::Terminal,
            )
            .expect("mark terminal embedding failed");
        sync_store
            .mark_model_task_failed_retryable(&retryable_llm_id, "429 Too Many Requests")
            .expect("mark retryable llm failed");

        let mut state = EndpointRecoveryRequeueState::default();
        let plan = super::requeue_failed_model_tasks_after_recovery(
            &async_store,
            &mut state,
            &endpoint_health_snapshot(true, true),
        )
        .await
        .expect("requeue after recovery");
        assert_eq!(
            plan,
            EndpointRecoveryRequeuePlan {
                embedding: true,
                llm: true,
            }
        );

        let stats = sync_store.stats().expect("queue stats");
        assert_eq!(stats.pending, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.failed_retryable, 0);
        assert_eq!(stats.failed_terminal, 1);
        assert_eq!(stats.failed_retryable_embed, 0);
        assert_eq!(stats.failed_retryable_llm, 0);
        assert!(stats.last_auto_requeue_at_unix_ms.is_some());

        let repeated_plan = super::requeue_failed_model_tasks_after_recovery(
            &async_store,
            &mut state,
            &endpoint_health_snapshot(true, true),
        )
        .await
        .expect("second recovery check");
        assert!(repeated_plan.is_empty());
        let terminal_status = sync_store
            .operation_status(&terminal_embedding_id)
            .expect("terminal status")
            .expect("terminal record");
        assert_eq!(terminal_status.op_state, "failed");
    }

    #[test]
    fn test_llm_worker_spawn_and_claim_gates_are_separate() {
        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some("http://127.0.0.1:9/v1".to_string());
        config.llm.enabled_for.clear();

        assert!(config.llm.enabled);
        assert!(!llm_worker_claim_enabled(&config));

        config.llm.enabled_for.push("gating".to_string());
        assert!(llm_worker_claim_enabled(&config));
    }

    #[test]
    fn test_daemon_llm_enqueue_requires_a_gating_decision() {
        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });

        assert!(
            !super::should_enqueue_llm_gating(&config, &None),
            "daemon must not enqueue LLM gating tasks before Tier 2 produced an auditable decision"
        );

        let llm_pending = crate::ingest::gating::GatingDecision::accepted(
            0,
            Some("llm_pending".to_string()),
            None,
        );
        assert!(super::should_enqueue_llm_gating(
            &config,
            &Some(llm_pending)
        ));
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

    #[cfg(unix)]
    async fn send_hook_ipc_request(
        store: AsyncPendingMessageStore,
        observer: crate::daemon_bootstrap::DaemonWriteObserver,
        request: crate::hook_ipc::HookIpcEnqueueRequest,
    ) -> crate::hook_ipc::HookIpcEnqueueResponse {
        let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
        let handler = tokio::spawn(super::handle_hook_ipc_connection(server, store, observer));
        let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
        frame.push(b'\n');
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .expect("write request");
        tokio::io::AsyncWriteExt::flush(&mut client)
            .await
            .expect("flush request");

        let mut reader = tokio::io::BufReader::new(client);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read response");
        handler.await.expect("handler task");
        serde_json::from_str(line.trim()).expect("hook IPC response")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_hook_ipc_ack_requires_sqlite_persistence() {
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
            HookEvent::UserPromptSubmit.queue_kind(),
            r#"{"event":"UserPromptSubmit","payload":"durable before ack"}"#,
        );

        let response = send_hook_ipc_request(store, observer, request).await;
        assert_eq!(response, crate::hook_ipc::HookIpcEnqueueResponse::Accepted);
        let (kind, payload): (String, String) = rusqlite::Connection::open(&db_path)
            .expect("open sqlite")
            .query_row(
                "SELECT kind, payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query persisted IPC message");
        assert_eq!(kind, HookEvent::UserPromptSubmit.queue_kind());
        assert!(payload.contains("durable before ack"), "{payload}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_hook_ipc_waits_for_sqlite_persistence_after_client_timeout() {
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let lock_conn = rusqlite::Connection::open(&db_path).expect("open lock connection");
        lock_conn
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold SQLite write lock");

        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
            HookEvent::UserPromptSubmit.queue_kind(),
            r#"{"event":"UserPromptSubmit","payload":"durable after lock"}"#,
        );

        let mut response_task =
            tokio::spawn(async move { send_hook_ipc_request(store, observer, request).await });
        assert!(
            tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, &mut response_task)
                .await
                .is_err(),
            "locked SQLite persistence must not ACK before durability"
        );
        let count_while_locked: i64 = rusqlite::Connection::open(&db_path)
            .expect("open read connection")
            .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
                row.get(0)
            })
            .expect("count pending while locked");
        assert_eq!(count_while_locked, 0);

        lock_conn.execute_batch("ROLLBACK;").expect("release lock");
        let response = response_task.await.expect("IPC response task");
        assert_eq!(response, crate::hook_ipc::HookIpcEnqueueResponse::Accepted);
        let (count_after_unlock, payload): (i64, String) = rusqlite::Connection::open(&db_path)
            .expect("open read connection")
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(payload), '') FROM pending_messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("query pending after unlock");
        assert_eq!(count_after_unlock, 1);
        assert!(payload.contains("durable after lock"), "{payload}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_hook_ipc_stalled_request_times_out_without_persisting() {
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
        let (client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
        let handler = tokio::spawn(super::handle_hook_ipc_connection(server, store, observer));

        let mut reader = tokio::io::BufReader::new(client);
        let mut line = String::new();
        let bytes_read = tokio::time::timeout(
            crate::hook_ipc::HOOK_IPC_READ_TIMEOUT + Duration::from_secs(1),
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line),
        )
        .await
        .expect("stalled request must receive timeout response")
        .expect("read response");
        assert!(bytes_read > 0, "daemon should write an error response");

        handler.await.expect("handler task");
        match serde_json::from_str::<crate::hook_ipc::HookIpcEnqueueResponse>(line.trim())
            .expect("hook IPC response")
        {
            crate::hook_ipc::HookIpcEnqueueResponse::Accepted => {
                panic!("stalled IPC request must not be accepted")
            }
            crate::hook_ipc::HookIpcEnqueueResponse::Error { message } => {
                assert!(message.contains("timed out reading frame"), "{message}");
            }
        }

        let count: i64 = rusqlite::Connection::open(&db_path)
            .expect("open sqlite")
            .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
                row.get(0)
            })
            .expect("count pending");
        assert_eq!(count, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_hook_ipc_timeout_fallback_is_idempotent_with_slow_daemon_persist() {
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let kind = HookEvent::UserPromptSubmit.queue_kind().to_string();
        let payload =
            r#"{"event":"UserPromptSubmit","payload":"timeout fallback same capture"}"#.to_string();
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new(&kind, &payload);
        let idempotency_key = request.idempotency_key.clone();

        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path)
            .with_blocking_delay(crate::hook_ipc::HOOK_IPC_TIMEOUT + Duration::from_millis(200));
        let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
        let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
        let handler = tokio::spawn(super::handle_hook_ipc_connection(server, store, observer));

        let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
        frame.push(b'\n');
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .expect("write request");
        tokio::io::AsyncWriteExt::flush(&mut client)
            .await
            .expect("flush request");

        let timed_out = tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, async move {
            let mut reader = tokio::io::BufReader::new(client);
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await
        })
        .await;
        assert!(
            timed_out.is_err(),
            "client should time out before daemon persist"
        );

        let fallback_store = PendingMessageStore::new_without_reclaim(&db_path);
        let fallback_id = fallback_store
            .enqueue_idempotent_with_key(&kind, &payload, &idempotency_key)
            .expect("fallback enqueue");

        handler.await.expect("handler task");

        let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
                row.get(0)
            })
            .expect("count pending");
        assert_eq!(
            count, 1,
            "daemon and fallback must collapse the same capture"
        );
        let (stored_id, stored_kind, stored_payload): (String, String, String) = conn
            .query_row(
                "SELECT id, kind, payload FROM pending_messages LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read pending row");
        assert_eq!(stored_id, fallback_id);
        assert_eq!(stored_kind, kind);
        assert_eq!(stored_payload, payload);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_bounded_hook_worker_continues_claiming_after_completed_batch() {
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let mempal_home = tmp.path().join(".mempal");
        std::fs::create_dir_all(&mempal_home).expect("create mempal home");
        Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(&db_path).expect("store");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());

        for label in ["first", "second"] {
            let hook_payload = serde_json::json!({
                "tool_name": "Bash",
                "input": format!("printf {label}"),
                "output": label,
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
            store
                .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
                .expect("enqueue hook envelope");
        }

        let config = Config::default();
        assert!(
            !config.llm.enabled,
            "test runtime config must keep LLM disabled"
        );
        let worker = tokio::spawn(run_hook_worker(
            HookWorkerState {
                async_db,
                store: async_store,
                worker_id: "bounded-continuation-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            60,
            Duration::from_millis(10),
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let completed: i64 = rusqlite::Connection::open(&db_path)
                    .expect("open sqlite")
                    .query_row(
                        "SELECT COUNT(*) FROM pending_message_completions WHERE kind = ?1",
                        [HookEvent::PostToolUse.queue_kind()],
                        |row| row.get(0),
                    )
                    .expect("count completions");
                if completed == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker should continue claiming after first completion");

        let stats = store.stats().expect("queue stats");
        assert_eq!(stats.pending, 0);
        assert_eq!(stats.claimed, 0);

        super::SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker should observe shutdown")
            .expect("worker task should not panic");
        super::SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
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
                embedder: std::sync::Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    HeartbeatProbeEmbedder {
                        db_path,
                        message_id: message.id.clone(),
                        stale_heartbeat_at,
                        attempts: AtomicUsize::new(0),
                    },
                ))),
                prototype_classifier: std::sync::Arc::new(ArcSwap::from_pointee(None)),
                config: std::sync::Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            message,
            60,
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
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    MergeConflictProbeEmbedder {
                        db_path: db_path.clone(),
                        injected: Arc::clone(&injected),
                    },
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            message,
            60,
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

        let merge_audit_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM novelty_audit WHERE decision = 'merge' AND near_drawer_id = 'existing-target'",
                [],
                |row| row.get(0),
            )
            .expect("count merge audit rows");
        assert_eq!(
            merge_audit_count, 0,
            "conflicted daemon merge must not record a successful merge audit row"
        );
    }

    struct LlmClaimRaceProbeEmbedder {
        store: PendingMessageStore,
        embed_calls: AtomicUsize,
    }

    struct SlowEmbedder {
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl Embedder for SlowEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            tokio::time::sleep(self.delay).await;
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "slow-test"
        }
    }

    #[tokio::test]
    async fn test_hook_worker_heartbeats_long_message_processing_until_confirm() {
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
            "input": "printf slow",
            "output": "slow processing",
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
        let worker_id = "long-processing-worker";
        let message = store
            .claim_next(worker_id, 1)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let config = Config::default();
        assert!(
            !config.llm.enabled,
            "test runtime config must keep LLM disabled"
        );
        let worker = tokio::spawn(super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                store: async_store.clone(),
                worker_id: worker_id.to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    SlowEmbedder {
                        delay: Duration::from_secs(3),
                    },
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
            },
            message,
            1,
        ));

        tokio::time::sleep(Duration::from_millis(2_500)).await;
        let duplicate = async_store
            .claim_next("stealing-worker".to_string(), 1)
            .await
            .expect("stealing worker claim_next");
        assert!(
            duplicate.is_none(),
            "long-running hook processing must heartbeat so claim_next cannot reclaim it"
        );

        tokio::time::timeout(Duration::from_secs(3), worker)
            .await
            .expect("worker should finish")
            .expect("worker task should not panic");
        let completed: i64 = rusqlite::Connection::open(&db_path)
            .expect("open sqlite")
            .query_row(
                "SELECT COUNT(*) FROM pending_message_completions WHERE message_id = ?1",
                [queued_id.as_str()],
                |row| row.get(0),
            )
            .expect("count completions");
        assert_eq!(completed, 1);
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
            "tool_name": "DesignCapture",
            "input": "record durable llm enqueue order",
            "output": "The daemon must durably insert the drawer and vector before exposing the local LLM gating task to the worker queue.",
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
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 1.1;
        config.ingest_gating.embedding_classifier.prototypes = vec!["keep".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let classifier_embedder = StaticEmbedder;
        let classifier =
            compile_classifier_from_embedder(&classifier_embedder, &config.ingest_gating)
                .await
                .expect("compile classifier")
                .expect("classifier enabled");
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
                prototype_classifier: Some(&classifier),
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

    #[tokio::test]
    async fn test_daemon_routes_tier2_unclassified_to_llm_before_skipping() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record calibrated gating behavior",
            "output": "Retain this calibrated high-signal design note about local LLM judging so the next agent can continue the forget safety work without losing context.",
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
            .claim_next("llm-unclassified-worker", 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let mut config = Config::default();
        config.llm.enabled = true;
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 1.1;
        config.ingest_gating.embedding_classifier.prototypes = vec!["keep".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let embedder = StaticEmbedder;
        let classifier = compile_classifier_from_embedder(&embedder, &config.ingest_gating)
            .await
            .expect("compile classifier")
            .expect("classifier enabled");

        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "llm-unclassified-worker",
            &message,
            &embedder,
            DaemonIngestContext {
                prototype_classifier: Some(&classifier),
                config: &config,
                mempal_home: tmp.path(),
            },
        )
        .await
        .expect("process hook envelope");

        assert!(
            db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "unclassified Tier 2 candidates should be stored fail-open for local LLM judge"
        );
        let llm_task = store
            .claim_next_by_kind("llm-unclassified-final", 60, "llm_task")
            .expect("claim llm task")
            .expect("unclassified Tier 2 candidate should be queued for LLM judge");
        let task: crate::llm::LlmTaskPayload =
            serde_json::from_str(&llm_task.payload).expect("decode llm task");
        assert_eq!(task.drawer_id, drawer_id);

        let (audit_decision, label, audit_drawer_id, content_preview): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT decision, label, drawer_id, content_preview FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("query daemon gating audit");
        assert_eq!(audit_decision, "keep");
        assert_eq!(label.as_deref(), Some("llm_pending"));
        assert_eq!(audit_drawer_id.as_deref(), Some(drawer_id.as_str()));
        assert!(
            content_preview.is_none(),
            "fail-open LLM-pending audit rows must not retain raw content previews"
        );
        let dropped_total: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM fork_ext_meta WHERE key = 'gating.dropped.total'), 0)",
                [],
                |row| row.get(0),
            )
            .expect("query dropped counter");
        assert_eq!(
            dropped_total, 0,
            "fail-open LLM-pending candidates must not increment drop counters"
        );
        db.upsert_llm_verdict(&drawer_id, "keep", Some(0.9))
            .expect("upsert llm verdict");
        let llm_verdict: Option<String> = db
            .conn()
            .query_row(
                "SELECT llm_verdict FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("query llm verdict");
        assert_eq!(llm_verdict.as_deref(), Some("keep"));
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

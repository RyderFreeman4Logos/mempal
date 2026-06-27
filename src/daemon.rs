use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::OnceLock;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{future::Future, pin::Pin};

use crate::bootstrap_events::BootstrapEvent;
use crate::core::{
    AsyncDb,
    db::{Database, DbError, NoveltyAuditInsert},
    project::resolve_project_id,
    queue::{AsyncPendingMessageStore, ClaimedMessage, QueueFailureDisposition},
    strata::{is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{BootstrapEvidenceArgs, Drawer, RuntimeWriterLease, SourceType},
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
use crate::llm::LlmError;
use crate::observability::{
    OperationTelemetryRecord, OperationTelemetrySource, OperationTelemetrySpan,
};
use anyhow::{Context, Result};
use arc_swap::ArcSwap;
use serde_json::{Value, json};
#[cfg(any(test, unix))]
use tokio::sync::Notify;
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
const AUTOMATIC_HOOK_LLM_GATE_MAX_SECS: u64 = 30;
const SQLITE_WRITER_LEASE_NAME: &str = "sqlite-writer";
const DAEMON_WRITER_LEASE_TTL_SECS: u64 = 120;
const DAEMON_WRITER_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(30);
const DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE: Duration = Duration::from_secs(5);
const DAEMON_WRITER_LEASE_RENEW_RETRY_DELAY: Duration = Duration::from_millis(50);

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
            // A concurrent daemon already holds the singleton lock (#496):
            // fail before daemonizing or starting any workers, with owner
            // metadata in the error for operators and scripts.
            Err(error) if error.is::<crate::daemon_singleton::DaemonAlreadyRunning>() => {
                return Err(error);
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
    global_embed_status().set_audit_db_path(Some(db_path.clone()));
    let writer_lease = acquire_daemon_writer_lease(context, &db_path).await?;
    {
        let db = context.db.lock().await;
        db.prune_expired_audit_logs()
            .context("failed to prune expired audit logs")?;
    }

    install_shutdown_handlers()?;
    tracing::info!("daemon log path: {}", context.log_path.display());
    write_daemon_embedder_status(
        &context.mempal_home,
        &crate::daemon_status::DaemonEmbedderRuntimeStatus::unloaded_from_config(
            context.config.as_ref(),
            "daemon-startup",
        ),
    );

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
                let factory =
                    crate::embed::ConfiguredEmbedderFactory::new_for_daemon(config_for_rest);
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
        DaemonEmbedder::from_config(context.config.as_ref(), &context.mempal_home)
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
    let ingest_drain_worker = spawn_daemon_ingest_drain_worker(context, &db_path, &writer_lease)
        .context("failed to start daemon async ingest worker")?;
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

    let llm_gate = context
        .config
        .llm
        .enabled
        .then(|| HookLlmGateRuntime::new(&context.config.llm));
    let llm_worker_handles: Vec<tokio::task::JoinHandle<_>> = if let Some(llm_gate) = &llm_gate {
        let num_workers = context.config.llm.pool_capacity();
        let llm_store = Arc::new(context.store.clone());
        let llm_client_runtime = llm_gate.client_runtime.clone();
        let llm_status = llm_gate.status.clone();
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
        db_path: db_path.clone(),
        store: context.store.clone(),
        worker_id: worker_id.clone(),
        embedder: Arc::clone(&embedder),
        prototype_classifier: Arc::clone(&prototype_classifier),
        llm_gate,
        config: Arc::clone(&context.config),
        mempal_home: context.mempal_home.clone(),
        write_observer: context.write_observer.clone(),
        runtime_writer_lease: Some(writer_lease.lease().clone()),
        #[cfg(test)]
        idle_observer: None,
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
    tracing::info!(
        limit = DAEMON_HOOK_WORKER_LIMIT,
        "daemon hook workers started"
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
    ingest_drain_worker.request_shutdown();

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
    ingest_drain_worker.shutdown_and_drain().await;

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

struct RuntimeWriterLeaseHandle {
    db_path: PathBuf,
    lease: RuntimeWriterLease,
    heartbeat: tokio::task::JoinHandle<()>,
}

impl RuntimeWriterLeaseHandle {
    fn new(db_path: PathBuf, lease: RuntimeWriterLease) -> Self {
        let heartbeat = spawn_runtime_writer_lease_heartbeat(db_path.clone(), lease.clone());
        Self {
            db_path,
            lease,
            heartbeat,
        }
    }

    fn lease(&self) -> &RuntimeWriterLease {
        &self.lease
    }
}

impl Drop for RuntimeWriterLeaseHandle {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if let Ok(db) = Database::open(&self.db_path) {
            let _ = db.runtime_writer_lease_release(
                &self.lease.name,
                &self.lease.owner,
                &self.lease.session_id,
            );
        }
    }
}

async fn acquire_daemon_writer_lease(
    context: &DaemonContext,
    db_path: &Path,
) -> Result<RuntimeWriterLeaseHandle> {
    let owner = format!("mempal-daemon-{}", std::process::id());
    let metadata = json!({
        "command": "daemon",
        "db_path": db_path.to_string_lossy(),
    })
    .to_string();
    let lease = {
        let db = context.db.lock().await;
        db.runtime_writer_lease_acquire(
            SQLITE_WRITER_LEASE_NAME,
            &owner,
            "daemon",
            DAEMON_WRITER_LEASE_TTL_SECS,
            Some(&metadata),
        )
        .context("failed to acquire daemon writer lease")?
    };
    match lease {
        Some(lease) => Ok(RuntimeWriterLeaseHandle::new(db_path.to_path_buf(), lease)),
        None => {
            let active = {
                let db = context.db.lock().await;
                db.runtime_writer_lease_status(Some(SQLITE_WRITER_LEASE_NAME))
                    .unwrap_or_default()
            };
            Err(anyhow::anyhow!(
                "SQLite writer lease `{}` is already held: {}",
                SQLITE_WRITER_LEASE_NAME,
                format_runtime_writer_leases(&active)
            ))
        }
    }
}

fn spawn_runtime_writer_lease_heartbeat(
    db_path: PathBuf,
    lease: RuntimeWriterLease,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(DAEMON_WRITER_LEASE_RENEW_INTERVAL);
        loop {
            interval.tick().await;
            let db_path = db_path.clone();
            let lease_for_renew = lease.clone();
            let result = tokio::task::spawn_blocking(move || -> Result<bool> {
                renew_daemon_writer_lease_with_retry(&db_path, &lease_for_renew)
            })
            .await;
            match result {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    tracing::error!(
                        lease = %lease.name,
                        owner = %lease.owner,
                        "daemon writer lease was lost; requesting shutdown"
                    );
                    #[cfg(unix)]
                    request_shutdown_and_notify();
                    break;
                }
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "failed to renew daemon writer lease");
                }
                Err(error) => {
                    tracing::warn!(error = %error, "writer lease heartbeat task failed");
                }
            }
        }
    })
}

fn renew_daemon_writer_lease_with_retry(
    db_path: &Path,
    lease: &RuntimeWriterLease,
) -> Result<bool> {
    let started = Instant::now();
    loop {
        match renew_daemon_writer_lease_once(db_path, lease) {
            Ok(renewed) => return Ok(renewed),
            Err(error)
                if anyhow_error_is_sqlite_lock(&error)
                    && started.elapsed() < DAEMON_WRITER_LEASE_RENEW_RETRY_DEADLINE =>
            {
                std::thread::sleep(DAEMON_WRITER_LEASE_RENEW_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn renew_daemon_writer_lease_once(db_path: &Path, lease: &RuntimeWriterLease) -> Result<bool> {
    let db = Database::open_with_busy_timeout(db_path, DAEMON_WRITER_LEASE_RENEW_BUSY_TIMEOUT)
        .context("failed to open DB for writer lease renew")?;
    db.runtime_writer_lease_renew(
        &lease.name,
        &lease.owner,
        &lease.session_id,
        DAEMON_WRITER_LEASE_TTL_SECS,
    )
    .context("failed to renew daemon writer lease")
}

fn anyhow_error_is_sqlite_lock(error: &anyhow::Error) -> bool {
    error.chain().any(|error| {
        error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(crate::core::db::rusqlite_error_is_lock)
            || error
                .downcast_ref::<DbError>()
                .is_some_and(crate::core::db::db_error_is_sqlite_lock)
    })
}

fn format_runtime_writer_leases(leases: &[RuntimeWriterLease]) -> String {
    if leases.is_empty() {
        return "none visible".to_string();
    }
    leases
        .iter()
        .map(|lease| {
            format!(
                "name={} owner={} pid={} mode={} expires_at={} heartbeat_at={}",
                lease.name,
                lease.owner,
                lease.pid,
                lease.mode,
                lease.expires_at,
                lease.heartbeat_at
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn ensure_daemon_runtime_writer_lease_active(
    db: &Database,
    lease: Option<&RuntimeWriterLease>,
    operation: &'static str,
) -> Result<()> {
    let Some(lease) = lease else {
        return Ok(());
    };
    let active = db
        .runtime_writer_lease_is_active(&lease.name, &lease.owner, &lease.session_id)
        .with_context(|| {
            format!(
                "failed to verify daemon writer lease `{}` before {operation}",
                lease.name
            )
        })?;
    if active {
        Ok(())
    } else {
        anyhow::bail!(
            "SQLite writer lease `{}` for {} was lost before {operation}",
            lease.name,
            lease.owner
        )
    }
}

fn spawn_daemon_ingest_drain_worker(
    context: &DaemonContext,
    db_path: &Path,
    writer_lease: &RuntimeWriterLeaseHandle,
) -> Result<crate::mcp::IngestDrainWorkerHandle> {
    let config = context.config.as_ref().clone();
    let server = crate::mcp::MempalMcpServer::new_with_factory_and_config(
        db_path.to_path_buf(),
        config.clone(),
        Arc::new(crate::embed::ConfiguredEmbedderFactory::new_for_daemon(
            config,
        )),
    )?
    .with_external_ingest_writer_lease(writer_lease.lease().clone());
    let handle = server.spawn_scoped_ingest_drain_worker();
    tracing::info!("daemon async ingest worker started");
    Ok(handle)
}

#[derive(Clone)]
struct HookWorkerState {
    async_db: AsyncDb,
    db_path: PathBuf,
    store: AsyncPendingMessageStore,
    worker_id: String,
    embedder: Arc<DaemonEmbedder>,
    prototype_classifier: Arc<ArcSwap<Option<PrototypeClassifier>>>,
    llm_gate: Option<HookLlmGateRuntime>,
    config: Arc<crate::core::config::Config>,
    mempal_home: PathBuf,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
    runtime_writer_lease: Option<RuntimeWriterLease>,
    #[cfg(test)]
    idle_observer: Option<Arc<Notify>>,
}

#[derive(Clone)]
pub struct HookLlmGateRuntime {
    client_runtime: crate::llm::worker::SharedLlmClientRuntime,
    status: Arc<crate::llm::LlmStatus>,
}

impl HookLlmGateRuntime {
    pub fn new(config: &crate::core::config::LlmConfig) -> Self {
        Self {
            client_runtime: Arc::new(Mutex::new(crate::llm::worker::LlmClientRuntime::new(
                config,
            ))),
            status: Arc::new(crate::llm::LlmStatus::new(10)),
        }
    }

    async fn judge(
        &self,
        config: &crate::core::config::Config,
        task: &crate::llm::LlmTaskPayload,
        heartbeat: Option<&crate::llm::retry::HeartbeatCallback>,
    ) -> Result<crate::llm::worker::GatingJudgeOutcome> {
        if !crate::ingest::gating::llm_judge_active(config) {
            anyhow::bail!(
                "LLM gating is required for automatic hook writes but the LLM judge is not active"
            );
        }
        let router = {
            let mut runtime = self
                .client_runtime
                .lock()
                .expect("LLM client runtime mutex poisoned");
            runtime
                .router_for_config(&config.llm, &config.privacy.remote_calls)
                .context("LLM gating router unavailable")?
        };
        crate::llm::worker::request_strict_effective_gating_verdict(
            &router,
            &self.status,
            task,
            config,
            heartbeat,
        )
        .await
    }
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
            Box::pin(wait_for_shutdown_or_sleep(duration))
        })
        .await
        {
            ClaimPollResult::Claimed(message) => {
                process_hook_worker_message(state.clone(), message, claim_ttl_secs).await;
            }
            ClaimPollResult::Idle => {
                #[cfg(test)]
                let idle_observer = state.idle_observer.clone();
                wait_for_shutdown_or_sleep_after(poll_interval, move || {
                    #[cfg(test)]
                    notify_hook_worker_idle(idle_observer);
                })
                .await;
            }
            ClaimPollResult::RetryAfterError => continue,
        }
    }
}

async fn wait_for_shutdown_or_sleep(duration: Duration) {
    wait_for_shutdown_or_sleep_after(duration, || {}).await;
}

async fn wait_for_shutdown_or_sleep_after(duration: Duration, before_wait: impl FnOnce()) {
    #[cfg(unix)]
    {
        let shutdown = shutdown_notify().notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();
        if shutdown_requested() {
            return;
        }
        before_wait();

        tokio::select! {
            () = tokio::time::sleep(duration) => {}
            () = &mut shutdown => {}
        }
    }

    #[cfg(not(unix))]
    {
        before_wait();
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
fn notify_hook_worker_idle(observer: Option<Arc<Notify>>) {
    if let Some(observer) = observer {
        observer.notify_one();
    }
}

async fn process_hook_worker_message(
    state: HookWorkerState,
    message: ClaimedMessage,
    claim_ttl_secs: i64,
) {
    let message_id = message.id.clone();
    let span = OperationTelemetrySpan::start(
        state.db_path.clone(),
        OperationTelemetryRecord::new(
            OperationTelemetrySource::Daemon,
            format!("hook {}", message.kind),
            "daemon.hook_worker.message",
        )
        .with_retry_count(message.retry_count as u64),
    );
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
            llm_gate: state.llm_gate.as_ref(),
            config: state.config.as_ref(),
            mempal_home: &state.mempal_home,
            runtime_writer_lease: state.runtime_writer_lease.as_ref(),
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
                span.finish_error(error);
            } else {
                state.write_observer.record_successful_write();
                span.finish_success();
            }
        }
        Err(error) => {
            let error_text = error.to_string();
            tracing::error!("daemon message {message_id} failed: {error_text}");
            state.write_observer.record_error(error_text.clone());
            let disposition = queue_failure_disposition(&error);
            if let Err(mark_error) = state
                .store
                .mark_failed_with_disposition(message.clone(), error_text.clone(), disposition)
                .await
            {
                tracing::error!(?mark_error, "failed to mark_failed {message_id}");
                state
                    .write_observer
                    .record_error(format!("failed to mark_failed {message_id}: {mark_error}"));
                span.finish_error(mark_error);
            } else {
                span.finish_error(error_text);
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
    if shutdown_requested() {
        return;
    }

    if hook_workers.is_empty() {
        wait_for_shutdown_or_sleep(tick).await;
        return;
    }

    #[cfg(unix)]
    {
        let shutdown = shutdown_notify().notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();
        if shutdown_requested() {
            return;
        }

        tokio::select! {
            result = hook_workers.join_next() => {
                if let Some(result) = result {
                    handle_hook_worker_join(result);
                }
            }
            () = tokio::time::sleep(tick) => {}
            () = &mut shutdown => {}
        }
    }

    #[cfg(not(unix))]
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
    pub llm_gate: Option<&'a HookLlmGateRuntime>,
    pub config: &'a crate::core::config::Config,
    pub mempal_home: &'a Path,
    pub runtime_writer_lease: Option<&'a RuntimeWriterLease>,
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
        let runtime_writer_lease = context.runtime_writer_lease.cloned();
        db.run_write_anyhow(move |db| {
            ensure_daemon_runtime_writer_lease_active(
                db,
                runtime_writer_lease.as_ref(),
                "build daemon hook drawer records",
            )?;
            build_drawer_records(db, &envelope, &config, &mempal_home)
        })
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

    if let Some(stats) =
        auto_ingest_hermes_session_end(db, store, worker_id, message, embedder, &context, &envelope)
            .await?
    {
        tracing::info!(
            turns_parsed = stats.turns_parsed,
            turns_inserted = stats.turns_inserted,
            turns_skipped = stats.turns_skipped,
            turns_updated = stats.turns_updated,
            vectors_created = stats.vectors_created,
            "auto-ingested gated Hermes session turns"
        );
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

async fn auto_ingest_hermes_session_end<E: Embedder + ?Sized>(
    db: &AsyncDb,
    store: &AsyncPendingMessageStore,
    worker_id: &str,
    message: &ClaimedMessage,
    embedder: &E,
    context: &DaemonIngestContext<'_>,
    envelope: &CapturedHookEnvelope,
) -> Result<Option<crate::xurl::ingest::IngestStats>> {
    if envelope.event != crate::hook::HookEvent::SessionEnd.display_name()
        || !context.config.hooks.session_end.auto_ingest_conversation
    {
        return Ok(None);
    }
    if !context.config.ingest_gating.enabled {
        tracing::warn!(
            "SessionEnd Hermes auto-ingest skipped because ingest_gating.enabled is false; \
             automatic conversation import requires a pre-insert gate"
        );
        return Ok(Some(crate::xurl::ingest::IngestStats::default()));
    }

    let Some(session_id) = hermes_session_id_from_envelope(envelope) else {
        tracing::warn!(
            event = %envelope.event,
            agent = %envelope.agent,
            "SessionEnd Hermes auto-ingest skipped because payload has no session_id"
        );
        return Ok(Some(crate::xurl::ingest::IngestStats::default()));
    };
    let profile = context.config.hooks.session_end.hermes_profile.trim();
    let profile = if profile.is_empty() {
        "default"
    } else {
        profile
    };
    let hermes_db = crate::xurl::ingest::default_hermes_db_path(
        profile,
        context.config.hooks.session_end.hermes_home.as_deref(),
    );
    if !hermes_db.exists() {
        tracing::warn!(
            hermes_db = %hermes_db.display(),
            hermes_profile = profile,
            "SessionEnd Hermes auto-ingest skipped because state.db was not found"
        );
        return Ok(Some(crate::xurl::ingest::IngestStats::default()));
    }

    let mut parse_options =
        crate::xurl::parser::hermes::HermesParseOptions::new(&session_id, profile, false);
    parse_options.session_id_filter = Some(session_id.clone());
    parse_options.cwd = Some(envelope.claude_cwd.clone());
    let parse_db = hermes_db.clone();
    let parsed_turns = tokio::task::spawn_blocking(move || {
        crate::xurl::parser::hermes::parse_hermes_db_with_options(&parse_db, &parse_options)
    })
    .await
    .context("Hermes auto-ingest parser task failed")?
    .with_context(|| format!("failed to parse Hermes state.db {}", hermes_db.display()))?;
    let turns_parsed = parsed_turns.len();

    let heartbeat_store = store.clone();
    let heartbeat_message_id = message.id.clone();
    let heartbeat_worker_id = worker_id.to_string();
    let embed_heartbeat = move || -> crate::embed::Result<()> {
        let store = heartbeat_store.clone();
        let message_id = heartbeat_message_id.clone();
        let worker_id = heartbeat_worker_id.clone();
        tokio::spawn(async move {
            if let Err(error) = store.refresh_heartbeat(message_id.clone(), worker_id).await {
                tracing::warn!(
                    ?error,
                    message_id,
                    "failed to refresh Hermes auto-ingest embed heartbeat"
                );
            }
        });
        Ok(())
    };
    let llm_heartbeat_store = store.clone();
    let llm_heartbeat_message_id = message.id.clone();
    let llm_heartbeat_worker_id = worker_id.to_string();
    let llm_heartbeat = move || -> std::result::Result<(), crate::llm::LlmError> {
        let store = llm_heartbeat_store.clone();
        let message_id = llm_heartbeat_message_id.clone();
        let worker_id = llm_heartbeat_worker_id.clone();
        tokio::spawn(async move {
            if let Err(error) = store.refresh_heartbeat(message_id.clone(), worker_id).await {
                tracing::warn!(
                    ?error,
                    message_id,
                    "failed to refresh Hermes auto-ingest LLM heartbeat"
                );
            }
        });
        Ok(())
    };

    let project_id = resolve_hook_project_id(envelope, context.config)?;
    let mut kept_turns = Vec::new();
    for mut turn in parsed_turns {
        turn.content = context.config.scrub_content(&turn.content);
        if turn.content.trim().is_empty() {
            continue;
        }
        let candidate_hash = crate::xurl::store::turn_id_for(&turn);
        if gate_automatic_hermes_turn(
            db,
            embedder,
            context,
            AutomaticHermesTurnGateInput {
                turn: &turn,
                candidate_hash: &candidate_hash,
                project_id: project_id.as_deref(),
                embed_heartbeat: Some(&embed_heartbeat),
                llm_heartbeat: Some(&llm_heartbeat),
            },
        )
        .await?
        {
            kept_turns.push(turn);
        }
    }

    if kept_turns.is_empty() {
        return Ok(Some(crate::xurl::ingest::IngestStats {
            turns_parsed,
            turns_inserted: 0,
            turns_skipped: 0,
            turns_updated: 0,
            vectors_created: 0,
        }));
    }

    let runtime_writer_lease = context.runtime_writer_lease.cloned();
    let insert_stats = db
        .run_write_anyhow(move |db| {
            ensure_daemon_runtime_writer_lease_active(
                db,
                runtime_writer_lease.as_ref(),
                "insert Hermes turns",
            )?;
            crate::xurl::store::insert_turns(db.conn(), &kept_turns)
                .context("failed to insert gated Hermes turns")
        })
        .await?;
    let turn_ids = insert_stats.turn_ids.clone();
    let vectors_created = embed_and_write_hermes_turn_vectors(
        db,
        embedder,
        &turn_ids,
        context.config,
        context.runtime_writer_lease,
        Some(&embed_heartbeat),
    )
    .await
    .context("failed to embed gated Hermes turns")?;

    Ok(Some(crate::xurl::ingest::IngestStats {
        turns_parsed,
        turns_inserted: insert_stats.inserted,
        turns_skipped: insert_stats.skipped,
        turns_updated: insert_stats.updated,
        vectors_created,
    }))
}

async fn embed_and_write_hermes_turn_vectors<E: Embedder + ?Sized>(
    db: &AsyncDb,
    embedder: &E,
    turn_ids: &[String],
    config: &crate::core::config::Config,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    heartbeat: Option<&HeartbeatCallback>,
) -> Result<usize> {
    let mut embedded = 0usize;
    for batch in turn_ids.chunks(500) {
        let batch_ids = batch.to_vec();
        let candidates = db
            .run_read_anyhow(move |db| {
                if batch_ids.is_empty() {
                    return Ok(Vec::<(String, String)>::new());
                }
                let placeholders = (0..batch_ids.len())
                    .map(|index| format!("?{}", index + 1))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT ct.id, ct.content \
                     FROM conversation_turns ct \
                     LEFT JOIN conversation_turn_vectors ctv \
                       ON ctv.turn_id = ct.id AND ctv.chunk_index = 0 \
                     WHERE ct.id IN ({placeholders}) AND ctv.turn_id IS NULL"
                );
                let mut stmt = db.conn().prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(batch_ids.iter()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row?);
                }
                Ok(out)
            })
            .await?;
        if candidates.is_empty() {
            continue;
        }

        let mut vector_rows = Vec::<(String, i64, Vec<u8>, String, i64)>::new();
        for (turn_id, content) in candidates {
            let chunks = crate::ingest::chunk::chunk_text_token_aware(
                &content,
                &config.chunker,
                embedder,
                Some(&format!("xurl:auto-hermes:{turn_id}")),
            );
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let vector = embed_text_with_heartbeat(embedder, chunk, heartbeat).await?;
                let dim = i64::try_from(vector.len()).unwrap_or(i64::MAX);
                let fingerprint = config
                    .embed
                    .current_vector_embedder_fingerprint(vector.len());
                vector_rows.push((
                    turn_id.clone(),
                    chunk_index as i64,
                    serialize_f32_vector(&vector),
                    fingerprint,
                    dim,
                ));
            }
        }
        if vector_rows.is_empty() {
            continue;
        }

        let runtime_writer_lease = runtime_writer_lease.cloned();
        let rows_written = db
            .run_write_anyhow(move |db| {
                ensure_daemon_runtime_writer_lease_active(
                    db,
                    runtime_writer_lease.as_ref(),
                    "insert Hermes turn vectors",
                )?;
                let conn = db.conn();
                conn.execute_batch("BEGIN IMMEDIATE")?;
                let write = (|| -> rusqlite::Result<usize> {
                    let mut rows_written = 0usize;
                    for (turn_id, chunk_index, blob, fingerprint, dim) in &vector_rows {
                        rows_written += conn.execute(
                            "INSERT INTO conversation_turn_vectors \
                             (turn_id, chunk_index, vector, embedder_fingerprint, dim, index_version) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                             ON CONFLICT(turn_id, chunk_index) DO NOTHING",
                            rusqlite::params![
                                turn_id,
                                chunk_index,
                                blob,
                                fingerprint,
                                dim,
                                crate::core::db::CURRENT_VECTOR_INDEX_VERSION,
                            ],
                        )?;
                    }
                    Ok(rows_written)
                })();
                match write {
                    Ok(rows_written) => {
                        conn.execute_batch("COMMIT")?;
                        Ok(rows_written)
                    }
                    Err(error) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        Err(error.into())
                    }
                }
            })
            .await?;
        embedded += rows_written;
    }
    Ok(embedded)
}

fn serialize_f32_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

struct AutomaticHermesTurnGateInput<'a> {
    turn: &'a crate::xurl::model::RawTurn,
    candidate_hash: &'a str,
    project_id: Option<&'a str>,
    embed_heartbeat: Option<&'a HeartbeatCallback>,
    llm_heartbeat: Option<&'a crate::llm::retry::HeartbeatCallback>,
}

async fn gate_automatic_hermes_turn<E: Embedder + ?Sized>(
    db: &AsyncDb,
    embedder: &E,
    context: &DaemonIngestContext<'_>,
    input: AutomaticHermesTurnGateInput<'_>,
) -> Result<bool> {
    let candidate = IngestCandidate {
        content: input.turn.content.clone(),
        event: Some("HermesSessionTurn".to_string()),
        tool_name: input.turn.metadata.tool_name.clone(),
        exit_code: None,
    };
    let mut gating_decision = evaluate_tier1(&candidate, &context.config.ingest_gating);
    if gating_decision.is_none()
        && context.config.ingest_gating.enabled
        && !tier2_enabled(&context.config.ingest_gating)
    {
        gating_decision = Some(GatingDecision::accepted(
            0,
            Some("tier2_disabled".to_string()),
            None,
        ));
    }
    if let Some(decision) = gating_decision.as_ref()
        && decision.is_rejected()
        && should_drop_reject_before_automatic_hook_llm_gate(context.config, decision)
    {
        record_gating_audit_async(
            db,
            context.runtime_writer_lease,
            input.candidate_hash,
            decision,
            input.project_id.map(ToOwned::to_owned),
            None,
        )
        .await?;
        return Ok(false);
    }

    if gating_decision.is_none() && tier2_enabled(&context.config.ingest_gating) {
        let classifier = context.prototype_classifier.ok_or_else(|| {
            anyhow::anyhow!(
                "Hermes auto-ingest requires prototype classifier but it is not available"
            )
        })?;
        let candidate_vector = embed_text_with_heartbeat(
            embedder,
            analysis_content(&candidate.content),
            input.embed_heartbeat,
        )
        .await?;
        let decision = classifier.decide(
            &candidate_vector,
            context.config.ingest_gating.embedding_classifier.threshold,
        );
        if decision.is_rejected()
            && should_drop_reject_before_automatic_hook_llm_gate(context.config, &decision)
        {
            record_gating_audit_async(
                db,
                context.runtime_writer_lease,
                input.candidate_hash,
                &decision,
                input.project_id.map(ToOwned::to_owned),
                None,
            )
            .await?;
            return Ok(false);
        }
        gating_decision = Some(decision);
    }

    if automatic_hook_llm_gate_required(context.config) {
        let classifier_decision = gating_decision.clone();
        let llm_decision = judge_automatic_content_llm_gate(
            context.llm_gate,
            context.config,
            input.candidate_hash,
            &candidate.content,
            input.llm_heartbeat,
        )
        .await?;
        let audit_decision = classifier_decision
            .as_ref()
            .map(|decision| audit_decision_with_llm_outcome(decision, &llm_decision))
            .unwrap_or_else(|| llm_decision.clone());
        record_gating_audit_async(
            db,
            context.runtime_writer_lease,
            input.candidate_hash,
            &audit_decision,
            input.project_id.map(ToOwned::to_owned),
            (!audit_decision.is_rejected()).then_some(candidate.content.as_str()),
        )
        .await?;
        record_llm_verdict_async(
            db,
            context.runtime_writer_lease,
            input.candidate_hash,
            &llm_decision,
        )
        .await?;
        return Ok(!llm_decision.is_rejected());
    }

    let decision = gating_decision.unwrap_or_else(|| {
        GatingDecision::accepted(0, Some("gating_default_keep".to_string()), None)
    });
    let keep = !decision.is_rejected();
    record_gating_audit_async(
        db,
        context.runtime_writer_lease,
        input.candidate_hash,
        &decision,
        input.project_id.map(ToOwned::to_owned),
        keep.then_some(candidate.content.as_str()),
    )
    .await?;
    Ok(keep)
}

fn hermes_session_id_from_envelope(envelope: &CapturedHookEnvelope) -> Option<String> {
    let payload = envelope.payload.as_deref()?;
    let value = serde_json::from_str::<Value>(payload).ok()?;
    json_string_at(&value, &["session_id"])
        .or_else(|| json_string_at(&value, &["sessionId"]))
        .or_else(|| json_string_at(&value, &["session", "id"]))
        .or_else(|| json_string_at(&value, &["session", "session_id"]))
}

fn json_string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
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
        if cause
            .downcast_ref::<AutomaticHookLlmGateTerminalFailure>()
            .is_some()
        {
            return QueueFailureDisposition::Terminal;
        }
        if let Some(llm_error) = cause.downcast_ref::<LlmError>() {
            if !llm_error.is_retryable() {
                return QueueFailureDisposition::Terminal;
            }
            return llm_error
                .retry_after()
                .map(queue_retry_delay_from_duration)
                .unwrap_or(QueueFailureDisposition::Retryable);
        }
        if cause.downcast_ref::<serde_json::Error>().is_some() {
            return QueueFailureDisposition::Terminal;
        }
    }
    QueueFailureDisposition::Retryable
}

fn queue_retry_delay_from_duration(duration: Duration) -> QueueFailureDisposition {
    let delay_ms = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
    QueueFailureDisposition::RetryableAfter { delay_ms }
}

#[derive(Debug, thiserror::Error)]
#[error("automatic hook LLM gate terminal failure: {reason}")]
struct AutomaticHookLlmGateTerminalFailure {
    reason: String,
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
    deferred_raw_payload: Option<String>,
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
                        deferred_raw_payload: None,
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
        deferred_raw_payload: audit_record.deferred_raw_payload.clone(),
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

    let Some(payload_path) = envelope.payload_path.as_deref() else {
        tracing::info!(
            original_size_bytes = envelope.original_size_bytes,
            "truncated session_end omitted raw payload before LLM gate; skipping self-review extraction"
        );
        return Ok(None);
    };
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
            deferred_raw_payload: None,
        });
    }

    let raw_payload = envelope.payload.as_deref().unwrap_or_default();
    let (wing, room) = audit_target_for_event(&envelope.event, raw_payload, config);
    let preview = config.scrub_content(&preview_for_event(&envelope.event, raw_payload));
    let (payload_path, deferred_raw_payload) = if raw_turn_storage_disabled(&wing, &room, config) {
        (synthetic_source_file("hook-payload-skipped"), None)
    } else {
        (
            raw_payload_storage_path(raw_payload, mempal_home)
                .to_string_lossy()
                .to_string(),
            Some(raw_payload.to_string()),
        )
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
        deferred_raw_payload,
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
        && should_drop_reject_before_automatic_hook_llm_gate(context.daemon.config, decision)
    {
        record_gating_audit_async(
            context.db,
            context.daemon.runtime_writer_lease,
            &drawer_id,
            decision,
            record.project_id.clone(),
            None,
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
    let llm_heartbeat_store = context.store.clone();
    let llm_heartbeat_message_id = context.message.id.clone();
    let llm_heartbeat_worker_id = context.worker_id.to_string();
    let llm_heartbeat = move || -> std::result::Result<(), crate::llm::LlmError> {
        let store = llm_heartbeat_store.clone();
        let message_id = llm_heartbeat_message_id.clone();
        let worker_id = llm_heartbeat_worker_id.clone();
        tokio::spawn(async move {
            if let Err(error) = store.refresh_heartbeat(message_id.clone(), worker_id).await {
                tracing::warn!(
                    ?error,
                    message_id,
                    "failed to refresh daemon LLM gating heartbeat"
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
        if decision.is_rejected()
            && should_drop_reject_before_automatic_hook_llm_gate(context.daemon.config, &decision)
        {
            record_gating_audit_async(
                context.db,
                context.daemon.runtime_writer_lease,
                &drawer_id,
                &decision,
                record.project_id.clone(),
                None,
            )
            .await?;
            return Ok(drawer_id);
        }
        gating_decision = Some(decision);
        vector = Some(candidate_vector);
    }
    if !gating_audit_recorded && automatic_hook_llm_gate_required(context.daemon.config) {
        let classifier_decision = gating_decision.clone();
        let llm_decision = judge_automatic_hook_llm_gate(
            context,
            &drawer_id,
            &candidate.content,
            Some(&llm_heartbeat),
        )
        .await?;
        let audit_decision = classifier_decision
            .as_ref()
            .map(|decision| audit_decision_with_llm_outcome(decision, &llm_decision))
            .unwrap_or_else(|| llm_decision.clone());
        record_gating_audit_async(
            context.db,
            context.daemon.runtime_writer_lease,
            &drawer_id,
            &audit_decision,
            record.project_id.clone(),
            (!audit_decision.is_rejected()).then_some(candidate.content.as_str()),
        )
        .await?;
        record_llm_verdict_async(
            context.db,
            context.daemon.runtime_writer_lease,
            &drawer_id,
            &llm_decision,
        )
        .await?;
        gating_audit_recorded = true;
        if llm_decision.is_rejected() {
            return Ok(drawer_id);
        }
        gating_decision = Some(llm_decision);
    }
    if !gating_audit_recorded && let Some(decision) = gating_decision.as_ref() {
        record_gating_audit_async(
            context.db,
            context.daemon.runtime_writer_lease,
            &drawer_id,
            decision,
            record.project_id.clone(),
            (!decision.is_rejected()).then_some(candidate.content.as_str()),
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
        insert_drawer_with_vector_async(
            context.db,
            context.daemon.runtime_writer_lease,
            &drawer_id,
            record.clone(),
            vector.clone(),
        )
        .await?;
        persist_deferred_raw_payload(&record)?;
        enqueue_llm_gating_after_durable_insert(
            context.db,
            context.store,
            context.daemon.runtime_writer_lease,
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
                    context.daemon.runtime_writer_lease,
                    DaemonNoveltyAudit {
                        drawer_id: &drawer_id,
                        action: NoveltyAction::Insert,
                        near_drawer_id: novelty.near_drawer_id.as_deref(),
                        cosine: novelty.cosine,
                        audit_decision: novelty.audit_decision,
                        project_id: record.project_id.as_deref(),
                    },
                )
                .await?;
            }
            insert_drawer_with_vector_async(
                context.db,
                context.daemon.runtime_writer_lease,
                &drawer_id,
                record.clone(),
                vector.clone(),
            )
            .await?;
            persist_deferred_raw_payload(&record)?;
            enqueue_llm_gating_after_durable_insert(
                context.db,
                context.store,
                context.daemon.runtime_writer_lease,
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
                    context.daemon.runtime_writer_lease,
                    DaemonNoveltyAudit {
                        drawer_id: &drawer_id,
                        action: NoveltyAction::Drop,
                        near_drawer_id: novelty.near_drawer_id.as_deref(),
                        cosine: novelty.cosine,
                        audit_decision: novelty.audit_decision,
                        project_id: record.project_id.as_deref(),
                    },
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
                    context.daemon.runtime_writer_lease,
                    DaemonNoveltyAudit {
                        drawer_id: &drawer_id,
                        action: NoveltyAction::Insert,
                        near_drawer_id: Some(target_id.as_str()),
                        cosine: novelty.cosine,
                        audit_decision: Some("insert_due_to_merge_cap"),
                        project_id: record.project_id.as_deref(),
                    },
                )
                .await?;
                insert_drawer_with_vector_async(
                    context.db,
                    context.daemon.runtime_writer_lease,
                    &drawer_id,
                    record.clone(),
                    vector.clone(),
                )
                .await?;
                persist_deferred_raw_payload(&record)?;
                enqueue_llm_gating_after_durable_insert(
                    context.db,
                    context.store,
                    context.daemon.runtime_writer_lease,
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
                            context.daemon.runtime_writer_lease,
                            DaemonNoveltyAudit {
                                drawer_id: &drawer_id,
                                action: NoveltyAction::Insert,
                                near_drawer_id: Some(target_id.as_str()),
                                cosine: novelty.cosine,
                                audit_decision: Some("insert_due_to_embed_error"),
                                project_id: record.project_id.as_deref(),
                            },
                        )
                        .await?;
                        insert_drawer_with_vector_async(
                            context.db,
                            context.daemon.runtime_writer_lease,
                            &drawer_id,
                            record.clone(),
                            vector.clone(),
                        )
                        .await?;
                        persist_deferred_raw_payload(&record)?;
                        enqueue_llm_gating_after_durable_insert(
                            context.db,
                            context.store,
                            context.daemon.runtime_writer_lease,
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
                let runtime_writer_lease = context.daemon.runtime_writer_lease.cloned();
                context
                    .db
                    .run_write_anyhow(move |db| {
                        ensure_daemon_runtime_writer_lease_active(
                            db,
                            runtime_writer_lease.as_ref(),
                            "merge daemon hook drawer",
                        )?;
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
                persist_deferred_raw_payload(&record)?;
                Ok(target_id)
            }
        }
    }
}

fn automatic_hook_llm_gate_required(config: &crate::core::config::Config) -> bool {
    config.ingest_gating.enabled
        && config
            .ingest_gating
            .llm_judge
            .as_ref()
            .is_some_and(|judge| judge.enabled)
}

fn should_drop_reject_before_automatic_hook_llm_gate(
    config: &crate::core::config::Config,
    decision: &GatingDecision,
) -> bool {
    if !decision.is_rejected() {
        return false;
    }
    if automatic_hook_llm_gate_required(config)
        && decision.gating_reason.as_deref() == Some("prototype_below_threshold")
    {
        return false;
    }
    !should_enqueue_llm_gating(config, &Some(decision.clone()))
}

async fn judge_automatic_hook_llm_gate(
    context: &DrawerIngestContext<'_, impl Embedder + ?Sized>,
    drawer_id: &str,
    content: &str,
    heartbeat: Option<&crate::llm::retry::HeartbeatCallback>,
) -> Result<GatingDecision> {
    judge_automatic_content_llm_gate(
        context.daemon.llm_gate,
        context.daemon.config,
        drawer_id,
        content,
        heartbeat,
    )
    .await
}

async fn judge_automatic_content_llm_gate(
    gate: Option<&HookLlmGateRuntime>,
    config: &crate::core::config::Config,
    candidate_hash: &str,
    content: &str,
    heartbeat: Option<&crate::llm::retry::HeartbeatCallback>,
) -> Result<GatingDecision> {
    let gate = gate.ok_or_else(|| {
        anyhow::anyhow!(
            "LLM gating is required for automatic hook writes but no LLM gate runtime is available"
        )
    })?;
    let task = crate::llm::LlmTaskPayload {
        task_type: "gating".to_string(),
        drawer_id: candidate_hash.to_string(),
        drawer_ids: vec![candidate_hash.to_string()],
        content: content.to_string(),
        system_prompt: config
            .ingest_gating
            .llm_judge
            .as_ref()
            .and_then(|judge| judge.system_prompt.clone()),
    };
    let deadline = automatic_hook_llm_gate_deadline(config);
    let outcome = match tokio::time::timeout(deadline, gate.judge(config, &task, heartbeat)).await {
        Ok(outcome) => outcome,
        Err(_) => {
            return Err(anyhow::Error::new(AutomaticHookLlmGateTerminalFailure {
                reason: format!("timed out after {}s", deadline.as_secs()),
            })
            .context("automatic hook LLM gate failed before durable insert"));
        }
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            let error_chain = format!("{error:#}");
            return Err(error.context(format!(
                "automatic hook LLM gate failed before durable insert: {error_chain}"
            )));
        }
    };
    Ok(gating_decision_from_llm_outcome(outcome))
}

fn automatic_hook_llm_gate_deadline(config: &crate::core::config::Config) -> Duration {
    Duration::from_secs(
        config
            .llm
            .request_timeout_secs
            .clamp(1, AUTOMATIC_HOOK_LLM_GATE_MAX_SECS),
    )
}

fn gating_decision_from_llm_outcome(
    outcome: crate::llm::worker::GatingJudgeOutcome,
) -> GatingDecision {
    let score = Some(outcome.score as f32);
    if outcome.verdict.is_keep() {
        GatingDecision::accepted(0, Some("llm_keep".to_string()), score)
    } else {
        GatingDecision::rejected(0, Some("llm_reject".to_string()), None, score)
    }
}

fn audit_decision_with_llm_outcome(
    classifier_decision: &GatingDecision,
    llm_decision: &GatingDecision,
) -> GatingDecision {
    let mut audit_decision = classifier_decision.clone();
    audit_decision.decision = if llm_decision.is_rejected() {
        "rejected".to_string()
    } else {
        "accepted".to_string()
    };
    audit_decision
}

async fn record_llm_verdict_async(
    db: &AsyncDb,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    drawer_id: &str,
    decision: &GatingDecision,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let verdict = if decision.is_rejected() {
        "reject"
    } else {
        "keep"
    }
    .to_string();
    let score = decision.score.map(f64::from);
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        ensure_daemon_runtime_writer_lease_active(
            db,
            runtime_writer_lease.as_ref(),
            "record daemon LLM verdict",
        )?;
        db.upsert_llm_verdict_by_candidate_hash(&drawer_id, &verdict, score)
            .with_context(|| format!("failed to record LLM verdict {}", drawer_id))?;
        Ok(())
    })
    .await
}

async fn enqueue_llm_gating_after_durable_insert(
    db: &AsyncDb,
    store: &AsyncPendingMessageStore,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    config: &crate::core::config::Config,
    gating_decision: &Option<GatingDecision>,
    drawer_id: &str,
    content: &str,
) -> Result<()> {
    if !should_enqueue_llm_gating(config, gating_decision) {
        return Ok(());
    }
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        ensure_daemon_runtime_writer_lease_active(
            db,
            runtime_writer_lease.as_ref(),
            "enqueue daemon LLM gating task",
        )
    })
    .await?;

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
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    drawer_id: &str,
    decision: &GatingDecision,
    project_id: Option<String>,
    content: Option<&str>,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let decision = decision.clone();
    let content = content.map(str::to_string);
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        ensure_daemon_runtime_writer_lease_active(
            db,
            runtime_writer_lease.as_ref(),
            "record daemon gating audit",
        )?;
        db.record_gating_audit(
            &drawer_id,
            &decision,
            project_id.as_deref(),
            content.as_deref(),
        )
        .with_context(|| format!("failed to record gating audit {}", drawer_id))?;
        Ok(())
    })
    .await
}

struct DaemonNoveltyAudit<'a> {
    drawer_id: &'a str,
    action: NoveltyAction,
    near_drawer_id: Option<&'a str>,
    cosine: Option<f32>,
    audit_decision: Option<&'a str>,
    project_id: Option<&'a str>,
}

async fn record_novelty_audit_async(
    db: &AsyncDb,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    audit: DaemonNoveltyAudit<'_>,
) -> Result<()> {
    let drawer_id = audit.drawer_id.to_string();
    let near_drawer_id = audit.near_drawer_id.map(str::to_string);
    let audit_decision = audit.audit_decision.map(str::to_string);
    let project_id = audit.project_id.map(str::to_string);
    let action = audit.action;
    let cosine = audit.cosine;
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        ensure_daemon_runtime_writer_lease_active(
            db,
            runtime_writer_lease.as_ref(),
            "record daemon novelty audit",
        )?;
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
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    drawer_id: &str,
    record: DrawerRecord,
    vector: Vec<f32>,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        ensure_daemon_runtime_writer_lease_active(
            db,
            runtime_writer_lease.as_ref(),
            "insert daemon hook drawer",
        )?;
        insert_drawer_with_vector(db, &drawer_id, &record, &vector)
    })
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

fn raw_payload_storage_path(raw_payload: &str, mempal_home: &Path) -> PathBuf {
    let digest = blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
    mempal_home
        .join("hook-payloads")
        .join(format!("{digest}.json"))
}

fn persist_deferred_raw_payload(record: &DrawerRecord) -> Result<()> {
    let Some(raw_payload) = record.deferred_raw_payload.as_deref() else {
        return Ok(());
    };
    persist_raw_payload_at(raw_payload, Path::new(&record.source_file))
}

fn persist_raw_payload_at(raw_payload: &str, path: &Path) -> Result<()> {
    let payload_dir = path.parent().ok_or_else(|| {
        anyhow::anyhow!("raw hook payload path has no parent: {}", path.display())
    })?;
    fs::create_dir_all(payload_dir)
        .with_context(|| format!("failed to create {}", payload_dir.display()))?;
    if !path.exists() {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(raw_payload.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    Ok(())
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
    config: crate::core::config::Config,
    runtime_init_lock: tokio::sync::Mutex<()>,
    runtime: Mutex<Option<DaemonEmbedderRuntime>>,
    status_path: Option<PathBuf>,
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
static SHUTDOWN_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
#[cfg(unix)]
static SHUTDOWN_NOTIFY: OnceLock<Notify> = OnceLock::new();

#[cfg(unix)]
fn shutdown_notify() -> &'static Notify {
    SHUTDOWN_NOTIFY.get_or_init(Notify::new)
}

#[cfg(unix)]
fn request_shutdown_and_notify() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    shutdown_notify().notify_waiters();
}

#[cfg(all(unix, test))]
fn request_shutdown() {
    request_shutdown_and_notify();
}

#[cfg(unix)]
fn reset_shutdown_request() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(all(not(unix), test))]
fn request_shutdown() {}

#[cfg(not(unix))]
fn reset_shutdown_request() {}

#[cfg(unix)]
extern "C" fn daemon_signal_handler(_signal: i32) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    write_shutdown_signal_byte();
}

#[cfg(unix)]
fn install_shutdown_handlers() -> Result<()> {
    reset_shutdown_request();
    let (read_fd, write_fd) = create_shutdown_signal_pipe()?;
    let previous_write_fd = SHUTDOWN_SIGNAL_WRITE_FD.swap(write_fd, Ordering::SeqCst);
    if previous_write_fd >= 0 {
        // SAFETY: the previous descriptor was installed by this process as the
        // shutdown self-pipe write end. Closing it during handler installation is
        // outside signal-handler context.
        unsafe {
            libc::close(previous_write_fd);
        }
    }
    spawn_shutdown_signal_bridge(read_fd);

    // SAFETY: installs a process signal handler that only writes an AtomicBool
    // and a byte to a nonblocking self-pipe, both async-signal-safe operations.
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

#[cfg(unix)]
fn create_shutdown_signal_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` points to two valid c_int slots for libc to initialize.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to create shutdown pipe");
    }

    if let Err(error) =
        configure_shutdown_signal_fd(fds[0]).and_then(|()| configure_shutdown_signal_fd(fds[1]))
    {
        // SAFETY: both descriptors were returned by `pipe` above and are still
        // owned by this function on this error path.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return Err(error);
    }

    Ok((fds[0], fds[1]))
}

#[cfg(unix)]
fn configure_shutdown_signal_fd(fd: RawFd) -> Result<()> {
    set_fd_flag(fd, libc::F_GETFL, libc::F_SETFL, libc::O_NONBLOCK)
        .context("failed to set shutdown pipe nonblocking")?;
    set_fd_flag(fd, libc::F_GETFD, libc::F_SETFD, libc::FD_CLOEXEC)
        .context("failed to set shutdown pipe close-on-exec")?;
    Ok(())
}

#[cfg(unix)]
fn set_fd_flag(
    fd: RawFd,
    get_cmd: libc::c_int,
    set_cmd: libc::c_int,
    flag: libc::c_int,
) -> Result<()> {
    // SAFETY: `fd` is an open file descriptor and `fcntl` is called with the
    // standard get/set flag commands for that descriptor.
    let current = unsafe { libc::fcntl(fd, get_cmd) };
    if current == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to read fd flags");
    }
    // SAFETY: `fd` is valid and `current | flag` is the updated flag set for
    // the matching `set_cmd`.
    if unsafe { libc::fcntl(fd, set_cmd, current | flag) } == -1 {
        return Err(std::io::Error::last_os_error()).context("failed to update fd flags");
    }
    Ok(())
}

#[cfg(unix)]
fn write_shutdown_signal_byte() {
    let write_fd = SHUTDOWN_SIGNAL_WRITE_FD.load(Ordering::SeqCst);
    if write_fd < 0 {
        return;
    }

    let byte = [1_u8];
    // SAFETY: `write_fd` is the nonblocking write end of the process-global
    // shutdown self-pipe. `write(2)` is async-signal-safe; errors are ignored
    // because the atomic flag already records shutdown intent.
    unsafe {
        let _ = libc::write(write_fd, byte.as_ptr().cast(), byte.len());
    }
}

#[cfg(unix)]
fn spawn_shutdown_signal_bridge(read_fd: RawFd) {
    // SAFETY: `read_fd` was just returned by `pipe` and ownership moves into
    // the async bridge task.
    let read_fd = unsafe { OwnedFd::from_raw_fd(read_fd) };
    tokio::spawn(async move {
        let async_fd = match tokio::io::unix::AsyncFd::new(read_fd) {
            Ok(async_fd) => async_fd,
            Err(error) => {
                tracing::warn!(?error, "failed to create shutdown signal bridge");
                return;
            }
        };
        let mut buffer = [0_u8; 64];

        loop {
            let mut ready = match async_fd.readable().await {
                Ok(ready) => ready,
                Err(error) => {
                    tracing::warn!(?error, "shutdown signal bridge readiness failed");
                    return;
                }
            };

            loop {
                // SAFETY: the buffer is valid for writes and the descriptor is
                // owned by `async_fd` for the lifetime of this task.
                let bytes_read = unsafe {
                    libc::read(
                        async_fd.get_ref().as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };

                if bytes_read > 0 {
                    request_shutdown_and_notify();
                    return;
                }
                if bytes_read == 0 {
                    return;
                }

                let error = std::io::Error::last_os_error();
                match error.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => {
                        ready.clear_ready();
                        break;
                    }
                    _ => {
                        tracing::warn!(?error, "shutdown signal bridge read failed");
                        return;
                    }
                }
            }
        }
    });
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
    async fn from_config(
        config: &crate::core::config::Config,
        mempal_home: &Path,
    ) -> crate::embed::Result<Self> {
        config
            .validate_daemon_embedder_mode()
            .map_err(|error| crate::embed::EmbedError::InvalidConfiguration(error.to_string()))?;
        let daemon_config = config.daemon_embedder_config();
        let name = daemon_config
            .embed
            .effective_model_summary()
            .unwrap_or_else(|| daemon_config.embed.backend.clone());
        let status_path = Some(crate::daemon_status::embedder_status_path(mempal_home));
        let embedder = Self {
            name,
            config: config.clone(),
            runtime_init_lock: tokio::sync::Mutex::new(()),
            runtime: Mutex::new(None),
            status_path,
        };
        embedder.write_unloaded_status(config, "daemon-hook-worker");
        Ok(embedder)
    }

    async fn runtime_snapshot(&self) -> crate::embed::Result<DaemonEmbedderRuntimeSnapshot> {
        let generation = crate::core::config::ConfigHandle::current_embed_generation();
        if let Some(snapshot) = self.snapshot_if_generation(generation) {
            return Ok(snapshot);
        }

        let _init_guard = self.runtime_init_lock.lock().await;
        if let Some(snapshot) = self.snapshot_if_generation(generation) {
            return Ok(snapshot);
        }

        let config = self.active_config();
        let replacement = DaemonEmbedderRuntime::from_config(config.as_ref(), generation).await?;
        let mut guard = self
            .runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned");
        let status = if guard
            .as_ref()
            .is_none_or(|runtime| runtime.generation != generation)
        {
            *guard = Some(replacement);
            let runtime = guard
                .as_ref()
                .expect("daemon embedder runtime was just loaded");
            Some(
                crate::daemon_status::DaemonEmbedderRuntimeStatus::loaded_from_config(
                    config.as_ref(),
                    runtime.primary.dimensions(),
                    runtime
                        .fallback
                        .as_ref()
                        .map(|fallback| fallback.name().to_string()),
                    "daemon-hook-worker-reload",
                ),
            )
        } else {
            None
        };
        let snapshot = guard
            .as_ref()
            .expect("daemon embedder runtime must be loaded")
            .snapshot();
        drop(guard);
        if let (Some(status_path), Some(status)) = (&self.status_path, status) {
            write_daemon_embedder_status_path(status_path, &status);
        }
        Ok(snapshot)
    }

    fn snapshot_if_generation(&self, generation: u64) -> Option<DaemonEmbedderRuntimeSnapshot> {
        let guard = self
            .runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned");
        guard
            .as_ref()
            .filter(|runtime| runtime.generation == generation)
            .map(DaemonEmbedderRuntime::snapshot)
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
            config: crate::core::config::Config::default(),
            runtime_init_lock: tokio::sync::Mutex::new(()),
            runtime: Mutex::new(Some(runtime)),
            status_path: None,
        }
    }

    fn active_config(&self) -> std::sync::Arc<crate::core::config::Config> {
        let current = crate::core::config::ConfigHandle::current();
        if current.db_path == self.config.db_path
            && !daemon_config_is_default_snapshot(current.as_ref())
        {
            current
        } else {
            std::sync::Arc::new(self.config.clone())
        }
    }

    fn write_unloaded_status(&self, config: &crate::core::config::Config, source: &str) {
        let Some(status_path) = &self.status_path else {
            return;
        };
        let status =
            crate::daemon_status::DaemonEmbedderRuntimeStatus::unloaded_from_config(config, source);
        write_daemon_embedder_status_path(status_path, &status);
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
        if let Some(runtime) = self
            .runtime
            .lock()
            .expect("daemon embedder runtime mutex poisoned")
            .as_ref()
        {
            return runtime.primary.dimensions();
        }
        self.active_config()
            .daemon_embedder_config()
            .embed
            .resolved_openai_dim()
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
        let config = config.daemon_embedder_config();
        let primary: Arc<dyn Embedder> =
            Arc::from(build_backend_from_name(&config, config.embed.backend.as_str()).await?);
        let fallback = match config.embed.fallback.as_deref() {
            Some(name) if name.eq_ignore_ascii_case(config.embed.backend.as_str()) => None,
            Some(name) => Some(Arc::from(build_backend_from_name(&config, name).await?)),
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

fn daemon_config_is_default_snapshot(config: &crate::core::config::Config) -> bool {
    match (
        config.effective_hash(),
        crate::core::config::Config::default().effective_hash(),
    ) {
        (Ok(current), Ok(default)) => current == default,
        _ => false,
    }
}

fn write_daemon_embedder_status(
    mempal_home: &Path,
    status: &crate::daemon_status::DaemonEmbedderRuntimeStatus,
) {
    if let Err(error) = crate::daemon_status::write_embedder_status_atomic(mempal_home, status) {
        tracing::warn!(%error, "failed to write daemon embedder status");
    }
}

fn write_daemon_embedder_status_path(
    status_path: &Path,
    status: &crate::daemon_status::DaemonEmbedderRuntimeStatus,
) {
    let Some(mempal_home) = status_path.parent() else {
        tracing::warn!("failed to write daemon embedder status: status path has no parent");
        return;
    };
    write_daemon_embedder_status(mempal_home, status);
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
    use crate::llm::LlmError;
    use crate::observability::{OperationTelemetrySummaryOptions, operation_telemetry_summary};
    use arc_swap::ArcSwap;
    use std::pin::Pin;
    use tokio::sync::Notify;

    #[cfg(not(feature = "model2vec"))]
    use crate::core::config::DaemonEmbedderMode;

    use super::{
        AutomaticHookLlmGateTerminalFailure, ClaimNextSource, ClaimPollResult, DaemonEmbedder,
        DaemonIngestContext, EndpointRecoveryConfigProvider, EndpointRecoveryRequeuePlan,
        EndpointRecoveryRequeueState, HookWorkerState, automatic_hook_llm_gate_deadline,
        build_drawer_records, compile_classifier_from_embedder, llm_worker_claim_enabled,
        poll_claim_next, process_claimed_message_with_embedder, queue_failure_disposition,
        request_shutdown, reset_shutdown_request, run_hook_worker, wait_for_hook_worker_or_tick,
        wing_from_cwd,
    };

    #[test]
    fn queue_failure_disposition_dead_letters_non_retryable_llm_errors() {
        let decode_error = anyhow::Error::new(LlmError::DecodeResponse(
            "error decoding response body".to_string(),
        ))
        .context("LLM gating request failed")
        .context("automatic hook LLM gate failed before durable insert");

        assert_eq!(
            queue_failure_disposition(&decode_error),
            QueueFailureDisposition::Terminal
        );
    }

    #[test]
    fn queue_failure_disposition_dead_letters_automatic_hook_llm_gate_deadline() {
        let timeout_error = anyhow::Error::new(AutomaticHookLlmGateTerminalFailure {
            reason: "timed out after 30s".to_string(),
        })
        .context("automatic hook LLM gate failed before durable insert");

        assert_eq!(
            queue_failure_disposition(&timeout_error),
            QueueFailureDisposition::Terminal
        );
    }

    #[test]
    fn automatic_hook_llm_gate_deadline_caps_global_llm_timeout() {
        let mut config = Config::default();

        config.llm.request_timeout_secs = 3_000;
        assert_eq!(
            automatic_hook_llm_gate_deadline(&config),
            Duration::from_secs(30)
        );

        config.llm.request_timeout_secs = 2;
        assert_eq!(
            automatic_hook_llm_gate_deadline(&config),
            Duration::from_secs(2)
        );
    }

    #[tokio::test]
    async fn daemon_embedder_from_config_defers_explicit_model2vec_construction() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut config = Config {
            db_path: tmp.path().join("palace.db").display().to_string(),
            ..Config::default()
        };
        config.embed.backend = "model2vec".to_string();
        config.embed.model = Some("explicit-model2vec-not-loaded-at-startup".to_string());

        let _embedder = DaemonEmbedder::from_config(&config, tmp.path())
            .await
            .expect("daemon startup must not construct explicit model2vec");

        let status = crate::daemon_status::read_embedder_status(tmp.path())
            .expect("read daemon embedder status")
            .expect("daemon embedder status should be written");
        assert_eq!(status.backend, "model2vec");
        assert!(
            !status.cache_loaded,
            "daemon startup/status path must not load the configured embedder"
        );
        assert_eq!(status.dimensions, None);
    }

    #[tokio::test]
    #[cfg(not(feature = "model2vec"))]
    async fn daemon_embedder_from_config_rejects_small_local_without_model2vec_feature() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut config = Config {
            db_path: tmp.path().join("palace.db").display().to_string(),
            ..Config::default()
        };
        config.daemon.embedder_mode = DaemonEmbedderMode::SmallLocal;

        let error = match DaemonEmbedder::from_config(&config, tmp.path()).await {
            Ok(_) => {
                panic!("small_local must fail before daemon embedder startup without model2vec")
            }
            Err(error) => error,
        };

        let rendered = error.to_string();
        assert!(
            rendered.contains("requires building mempal with the `model2vec` Cargo feature"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn daemon_embedder_marks_cache_loaded_only_after_first_embed() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut config = Config {
            db_path: tmp.path().join("palace.db").display().to_string(),
            ..Config::default()
        };
        config.embed.backend = "stub".to_string();
        config.embed.openai_compat.dim = Some(7);

        let embedder = DaemonEmbedder::from_config(&config, tmp.path())
            .await
            .expect("daemon startup should defer stub construction too");
        let initial_status = crate::daemon_status::read_embedder_status(tmp.path())
            .expect("read initial daemon embedder status")
            .expect("initial daemon embedder status should be written");
        assert!(!initial_status.cache_loaded);

        let vectors = embedder
            .embed(&["lazy daemon embedder"])
            .await
            .expect("embed");
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), 7);

        let loaded_status = crate::daemon_status::read_embedder_status(tmp.path())
            .expect("read loaded daemon embedder status")
            .expect("loaded daemon embedder status should be written");
        assert!(loaded_status.cache_loaded);
        assert_eq!(loaded_status.backend, "stub");
        assert_eq!(loaded_status.dimensions, Some(7));
    }

    struct ShutdownResetGuard;

    impl ShutdownResetGuard {
        fn new() -> Self {
            reset_shutdown_request();
            Self
        }
    }

    impl Drop for ShutdownResetGuard {
        fn drop(&mut self) {
            reset_shutdown_request();
        }
    }

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

    #[tokio::test]
    async fn test_daemon_claim_skips_ingest_async_rows() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let sync_store = PendingMessageStore::new_without_reclaim(&db_path);
        let ingest_id = sync_store
            .enqueue("ingest_async", r#"{"request":{}}"#)
            .expect("enqueue async ingest");
        let hook_id = sync_store
            .enqueue("hook_user_prompt", r#"{"event":"UserPromptSubmit"}"#)
            .expect("enqueue hook");
        let async_store = AsyncPendingMessageStore::from_store(sync_store.clone());

        let claimed = poll_claim_next(&async_store, "daemon-hook-worker", 60, |_| {
            Box::pin(std::future::ready(()))
        })
        .await;

        match claimed {
            ClaimPollResult::Claimed(message) => {
                assert_eq!(message.id, hook_id);
                assert_eq!(message.kind, "hook_user_prompt");
            }
            ClaimPollResult::Idle | ClaimPollResult::RetryAfterError => {
                panic!("daemon hook worker should claim the hook row")
            }
        }

        let ingest_status = sync_store
            .operation_status(&ingest_id)
            .expect("load async ingest operation status")
            .expect("async ingest row remains pending for its dedicated worker");
        assert_eq!(ingest_status.op_state, "queued");
        assert!(ingest_status.claimed_at.is_none());
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
        let _shutdown_guard = ShutdownResetGuard::new();
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
        let idle_observer = Arc::new(Notify::new());
        let worker = tokio::spawn(run_hook_worker(
            HookWorkerState {
                async_db,
                db_path: db_path.clone(),
                store: async_store,
                worker_id: "bounded-continuation-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: None,
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: Some(Arc::clone(&idle_observer)),
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

        let hook_row = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let telemetry_db = Database::open(&db_path).expect("open telemetry db");
                let telemetry = operation_telemetry_summary(
                    &telemetry_db,
                    OperationTelemetrySummaryOptions {
                        since_unix_ms: None,
                        limit: 10,
                    },
                )
                .expect("summarize daemon hook telemetry");
                if let Some(row) = telemetry.into_iter().find(|row| {
                    row.source == "daemon"
                        && row.operation == "hook hook_post_tool"
                        && row.call_site == "daemon.hook_worker.message"
                        && row.operation_count == 2
                }) {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon hook operation telemetry should record both completed hooks");
        assert_eq!(hook_row.operation_count, 2);
        assert_eq!(hook_row.success_count, 2);
        assert_eq!(hook_row.error_count, 0);

        tokio::time::timeout(Duration::from_secs(5), idle_observer.notified())
            .await
            .expect("worker should enter idle after completing queued hooks");

        request_shutdown();
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("worker should observe shutdown")
            .expect("worker task should not panic");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_main_loop_wait_wakes_on_shutdown_with_active_worker() {
        let _shutdown_guard = ShutdownResetGuard::new();
        let mut hook_workers = tokio::task::JoinSet::new();
        hook_workers.spawn(async {
            std::future::pending::<()>().await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(
                wait_for_hook_worker_or_tick(&mut hook_workers, Duration::from_secs(60)),
                async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    request_shutdown();
                }
            );
        })
        .await
        .expect("shutdown should wake main-loop wait without waiting for poll tick");

        assert_eq!(
            hook_workers.len(),
            1,
            "active worker should remain for the drain-budget path"
        );
        hook_workers.abort_all();
        while hook_workers.join_next().await.is_some() {}
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

    #[tokio::test]
    async fn test_hook_worker_stops_before_drawer_write_after_writer_lease_loss() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let mempal_home = tmp.path().join(".mempal");
        std::fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(&db_path).expect("store");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let lease = db
            .runtime_writer_lease_acquire(
                super::SQLITE_WRITER_LEASE_NAME,
                "daemon-lease-loss-test",
                "daemon",
                300,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("lease acquired");

        let hook_payload = serde_json::json!({
            "tool_name": "Bash",
            "input": "printf lease-lost",
            "output": "this hook must not become durable",
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
        let message = async_store
            .claim_next("lease-loss-worker".to_string(), 60)
            .await
            .expect("claim hook")
            .expect("queued hook claimed");

        assert!(
            db.runtime_writer_lease_release(&lease.name, &lease.owner, &lease.session_id)
                .expect("release daemon writer lease"),
            "test must force the daemon writer lease to be lost"
        );

        super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                db_path: db_path.clone(),
                store: async_store,
                worker_id: "lease-loss-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: None,
                config: Arc::new(Config::default()),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: Some(lease),
                idle_observer: None,
            },
            message.clone(),
            60,
        )
        .await;

        let drawer_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .expect("count drawers");
        let vector_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'drawer_vectors'",
                [],
                |row| row.get(0),
            )
            .expect("check drawer_vectors table");
        let vector_count = if vector_count == 0 {
            0
        } else {
            db.conn()
                .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| row.get(0))
                .expect("count drawer vectors")
        };
        let gating_audit_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM gating_audit", [], |row| row.get(0))
            .expect("count gating audit");
        assert_eq!(drawer_count, 0, "lost lease must stop drawer writes");
        assert_eq!(vector_count, 0, "lost lease must stop vector writes");
        assert_eq!(
            gating_audit_count, 0,
            "lost lease must stop audit writes before drawer ingest"
        );

        let (status, retry_count, last_error): (String, i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT status, retry_count, last_error FROM pending_messages WHERE id = ?1",
                [message.id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query queue row");
        assert_eq!(status, "pending");
        assert_eq!(retry_count, 1);
        assert!(
            last_error
                .as_deref()
                .is_some_and(|error| error.contains("lost before build daemon hook drawer records")),
            "queue failure must record lease loss: {last_error:?}"
        );
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
                db_path: db_path.clone(),
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
                llm_gate: None,
                config: std::sync::Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
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
                db_path: db_path.clone(),
                store: async_store,
                worker_id: "merge-conflict-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    MergeConflictProbeEmbedder {
                        db_path: db_path.clone(),
                        injected: Arc::clone(&injected),
                    },
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: None,
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
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
                db_path: db_path.clone(),
                store: async_store.clone(),
                worker_id: worker_id.to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    SlowEmbedder {
                        delay: Duration::from_secs(3),
                    },
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: None,
                config: Arc::new(config),
                mempal_home,
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
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
                llm_gate: None,
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
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
    async fn test_automatic_hook_without_llm_gate_does_not_write_drawer() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record automatic hook llm requirement",
            "output": "Automatic hook captures must not become durable memories until the local LLM gate explicitly keeps them.",
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
            .claim_next("llm-required-worker", 60)
            .expect("claim next")
            .expect("claimed message");
        assert_eq!(message.id, queued_id);

        let mut config = Config::default();
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let error = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "llm-required-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: None,
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect_err("automatic hook write must fail safe when LLM gate is unavailable");

        assert!(
            error.to_string().contains("LLM") || error.to_string().contains("llm"),
            "error should mention missing LLM gate: {error:#}"
        );
        let drawer_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("count drawers");
        assert_eq!(
            drawer_count, 0,
            "automatic hook must not durably insert without an LLM keep verdict"
        );
        assert!(
            store
                .claim_next_by_kind("llm-required-final", 60, "llm_task")
                .expect("claim llm task")
                .is_none(),
            "automatic hook must not queue a post-insert LLM task when no drawer was written"
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_default_score_keep_precedes_durable_insert() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"score\":0.95,\"reason\":\"important design note\"}"}}]}"#,
            )
            .create_async()
            .await;
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
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 1.1;
        config.ingest_gating.embedding_classifier.prototypes = vec!["keep".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);
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
            "llm-unclassified-worker",
            &message,
            &embedder,
            DaemonIngestContext {
                prototype_classifier: Some(&classifier),
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect("process hook envelope");

        llm_mock.assert_async().await;
        assert_eq!(embedder.embed_calls.load(Ordering::SeqCst), 1);
        assert!(
            db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "automatic hook candidate may become durable only after LLM keep"
        );
        assert!(
            store
                .claim_next_by_kind("llm-unclassified-final", 60, "llm_task")
                .expect("claim llm task")
                .is_none(),
            "automatic hook must not enqueue a post-insert LLM task after synchronous keep"
        );
        let vector_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                [drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("query vector");
        assert_eq!(vector_count, 1);

        let (audit_decision, label, audit_drawer_id, llm_verdict, llm_score): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<f64>,
        ) = db
            .conn()
            .query_row(
                "SELECT decision, label, drawer_id, llm_verdict, llm_score FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("query daemon gating audit");
        assert_eq!(audit_decision, "keep");
        assert!(
            label.is_none(),
            "LLM keep must not overwrite classifier audit label"
        );
        assert_eq!(audit_drawer_id.as_deref(), Some(drawer_id.as_str()));
        assert_eq!(llm_verdict.as_deref(), Some("keep"));
        assert!(
            llm_score.is_some_and(|score| (score - 0.95).abs() < 1e-6),
            "llm_score={llm_score:?}"
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
            "LLM-kept automatic hook candidates must not increment drop counters"
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_prototype_hard_reject_bypasses_llm_gate() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record prototype hard reject",
            "output": "This candidate should match the noise prototype and be rejected before any LLM call.",
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
        let message = store
            .claim_next("prototype-hard-reject-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some("http://127.0.0.1:1/v1".to_string());
        config.llm.model = Some("unreachable-test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 0.0;
        config.ingest_gating.embedding_classifier.prototypes = vec!["noise".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);
        let classifier_embedder = StaticEmbedder;
        let classifier =
            compile_classifier_from_embedder(&classifier_embedder, &config.ingest_gating)
                .await
                .expect("compile classifier")
                .expect("classifier enabled");

        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "prototype-hard-reject-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: Some(&classifier),
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect("prototype hard reject should not call unreachable LLM");

        assert!(
            !db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "prototype hard-rejected hook candidates must not become durable"
        );
        let (audit_decision, tier, reason, llm_verdict, content_preview): (
            String,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT decision, tier, reason, llm_verdict, content_preview FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("query daemon gating audit");
        assert_eq!(audit_decision, "skip");
        assert_eq!(tier, 2);
        assert_eq!(reason.as_deref(), Some("prototype.noise"));
        assert!(llm_verdict.is_none());
        assert!(
            content_preview.is_none(),
            "automatic hook fast-reject audit must not persist candidate content"
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_soft_prototype_reject_retries_when_llm_inactive() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record soft prototype reject",
            "output": "This candidate falls below prototype threshold and requires an LLM decision before any drop.",
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
        let message = store
            .claim_next("soft-prototype-reject-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = false;
        config.llm.enabled_for = Vec::new();
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 2.0;
        config.ingest_gating.embedding_classifier.prototypes = vec!["keep".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);
        let classifier = compile_classifier_from_embedder(&StaticEmbedder, &config.ingest_gating)
            .await
            .expect("compile classifier")
            .expect("classifier enabled");

        super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                db_path: db_path.clone(),
                store: async_store,
                worker_id: "soft-prototype-reject-worker".to_string(),
                embedder: std::sync::Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: std::sync::Arc::new(ArcSwap::from_pointee(Some(classifier))),
                llm_gate: Some(llm_gate),
                config: std::sync::Arc::new(config),
                mempal_home: tmp.path().to_path_buf(),
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
            },
            message.clone(),
            60,
        )
        .await;

        let queue_row: (String, i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT status, retry_count, last_error FROM pending_messages WHERE id = ?1",
                [message.id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query queue row");
        assert!(
            queue_row
                .2
                .as_deref()
                .is_some_and(|error| error.contains("LLM judge is not active")),
            "unexpected queue error: {:?}",
            queue_row.2
        );
        assert_eq!(queue_row.0, "pending");
        assert_eq!(queue_row.1, 1);
        assert!(
            !tmp.path().join("hook-payloads").exists(),
            "soft prototype reject must not persist raw payload before LLM keep"
        );
        let drawer_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .expect("query drawer count");
        assert_eq!(drawer_count, 0);
        let audit_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM gating_audit", [], |row| row.get(0))
            .expect("query audit count");
        assert_eq!(audit_count, 0);
    }

    #[tokio::test]
    async fn test_automatic_hook_llm_gate_preserves_classifier_audit() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"score\":0.95,\"reason\":\"important design note\"}"}}]}"#,
            )
            .create_async()
            .await;
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record classifier audit preservation",
            "output": "Retain this candidate only after LLM keep while preserving the prototype classifier audit fields.",
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
        let message = store
            .claim_next("classifier-audit-preserve-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.embedding_classifier.enabled = true;
        config.ingest_gating.embedding_classifier.threshold = 1.1;
        config.ingest_gating.embedding_classifier.prototypes = vec!["keep".to_string()];
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);
        let classifier_embedder = StaticEmbedder;
        let classifier =
            compile_classifier_from_embedder(&classifier_embedder, &config.ingest_gating)
                .await
                .expect("compile classifier")
                .expect("classifier enabled");

        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "classifier-audit-preserve-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: Some(&classifier),
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect("process hook envelope");

        llm_mock.assert_async().await;
        struct GatingAuditRow {
            decision: String,
            tier: i64,
            reason: Option<String>,
            score: Option<f64>,
            label: Option<String>,
            llm_verdict: Option<String>,
            llm_score: Option<f64>,
        }

        let audit = db
            .conn()
            .query_row(
                "SELECT decision, tier, reason, score, label, llm_verdict, llm_score FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| {
                    Ok(GatingAuditRow {
                        decision: row.get(0)?,
                        tier: row.get(1)?,
                        reason: row.get(2)?,
                        score: row.get(3)?,
                        label: row.get(4)?,
                        llm_verdict: row.get(5)?,
                        llm_score: row.get(6)?,
                    })
                },
            )
            .expect("query daemon gating audit");
        assert_eq!(audit.decision, "keep");
        assert_eq!(audit.tier, 2, "classifier tier must be preserved");
        assert_eq!(audit.reason.as_deref(), Some("prototype_below_threshold"));
        assert!(
            audit.score.is_some_and(|value| (value - 1.0).abs() < 1e-6),
            "classifier score={:?}",
            audit.score
        );
        assert!(audit.label.is_none());
        assert_eq!(audit.llm_verdict.as_deref(), Some("keep"));
        assert!(
            audit
                .llm_score
                .is_some_and(|value| (value - 0.95).abs() < 1e-6),
            "llm_score={:?}",
            audit.llm_score
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_malformed_llm_gate_does_not_write_drawer() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"not a verdict"}}]}"#,
            )
            .create_async()
            .await;
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record malformed llm fail safe",
            "output": "Automatic hook captures must fail safe when the LLM gate returns malformed output.",
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
        let message = store
            .claim_next("llm-malformed-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);

        super::process_hook_worker_message(
            HookWorkerState {
                async_db,
                db_path: db_path.clone(),
                store: async_store,
                worker_id: "llm-malformed-worker".to_string(),
                embedder: Arc::new(DaemonEmbedder::from_primary_for_test(Box::new(
                    StaticEmbedder,
                ))),
                prototype_classifier: Arc::new(ArcSwap::from_pointee(None)),
                llm_gate: Some(llm_gate),
                config: Arc::new(config),
                mempal_home: tmp.path().to_path_buf(),
                write_observer: crate::daemon_bootstrap::DaemonWriteObserver::for_test(),
                runtime_writer_lease: None,
                idle_observer: None,
            },
            message,
            60,
        )
        .await;

        llm_mock.assert_async().await;
        let (status, retry_count, last_error): (String, i64, Option<String>) = db
            .conn()
            .query_row(
                "SELECT status, retry_count, last_error FROM pending_messages WHERE kind = ?1",
                [HookEvent::PostToolUse.queue_kind()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query queue row");
        assert_eq!(status, "pending");
        assert_eq!(retry_count, 1);
        assert!(
            last_error
                .as_deref()
                .is_some_and(|error| error.contains("verdict")),
            "last_error={last_error:?}"
        );
        let drawer_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("count drawers");
        assert_eq!(drawer_count, 0);
        assert!(
            !tmp.path().join("hook-payloads").exists(),
            "malformed automatic hook gate must not persist raw payload before retry"
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_llm_reject_records_verdict_without_drawer() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"verdict\":\"reject\",\"score\":0.12}"}}]}"#,
            )
            .create_async()
            .await;
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record reject audit",
            "output": "Reject this low-signal hook capture and keep only aggregate audit metadata.",
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
        let message = store
            .claim_next("llm-reject-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);

        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "llm-reject-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect("process hook envelope");

        llm_mock.assert_async().await;
        assert!(
            !db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "LLM-rejected automatic hook candidate must not become durable"
        );
        assert!(
            !tmp.path().join("hook-payloads").exists(),
            "LLM-rejected automatic hook candidate must not persist raw payload"
        );
        let (audit_decision, audit_drawer_id, llm_verdict, llm_score, content_preview): (
            String,
            Option<String>,
            Option<String>,
            Option<f64>,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT decision, drawer_id, llm_verdict, llm_score, content_preview FROM gating_audit WHERE candidate_hash = ?1",
                [drawer_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("query daemon gating audit");
        assert_eq!(audit_decision, "skip");
        assert!(audit_drawer_id.is_none());
        assert_eq!(llm_verdict.as_deref(), Some("reject"));
        assert!(
            llm_score.is_some_and(|score| (score - 0.12).abs() < 1e-6),
            "llm_score={llm_score:?}"
        );
        assert!(
            content_preview.is_none(),
            "LLM-rejected automatic hook audit must not persist candidate content"
        );
    }

    #[tokio::test]
    async fn test_automatic_hook_truncated_reject_does_not_persist_oversize_payload() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(
                r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"verdict\":\"reject\",\"score\":0.10}"}}]}"#,
            )
            .create_async()
            .await;
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-05-01T12:34:56Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: None,
            payload_path: None,
            payload_preview: Some("oversize synthetic preview".to_string()),
            original_size_bytes: 10 * 1024 * 1024 + 1,
            truncated: true,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("truncated-reject-worker", 60)
            .expect("claim next")
            .expect("claimed message");

        let mut config = Config::default();
        config.llm.enabled = true;
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = super::HookLlmGateRuntime::new(&config.llm);

        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "truncated-reject-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
            },
        )
        .await
        .expect("process truncated hook envelope");

        llm_mock.assert_async().await;
        assert!(
            !db.drawer_exists(&drawer_id).expect("drawer exists query"),
            "LLM-rejected truncated hook candidate must not become durable"
        );
        assert!(
            !tmp.path().join("hook-oversize").exists(),
            "truncated automatic hook reject must not persist oversized raw payload"
        );
        assert!(
            !tmp.path().join("hook-payloads").exists(),
            "truncated automatic hook reject must not persist raw payload"
        );
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

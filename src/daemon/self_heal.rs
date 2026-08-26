#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::sync::atomic::AtomicUsize;
#[cfg(all(test, unix))]
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(unix)]
use crate::core::queue::AsyncPendingMessageStore;
use crate::core::queue::QueueError;

#[cfg(unix)]
pub(super) const HOOK_IPC_HANDLER_DRAIN_BUDGET: Duration = Duration::from_secs(3);
const DAEMON_CLAIM_LOCK_BACKOFF_MAX: Duration = Duration::from_secs(30);

#[cfg(unix)]
pub(super) const HOOK_IPC_HANDLER_LIMIT: usize = 32;

#[derive(Debug, Default)]
pub(super) struct ClaimBackoffState {
    pub(super) consecutive_sqlite_lock_errors: u64,
    pub(super) write_observer: Option<crate::daemon_bootstrap::DaemonWriteObserver>,
}

impl ClaimBackoffState {
    pub(super) fn reset(&mut self) {
        self.consecutive_sqlite_lock_errors = 0;
    }

    pub(super) fn delay_after_error(&mut self, error: &QueueError) -> Duration {
        if let Some(observer) = &self.write_observer {
            observer.record_claim_error(error);
        }
        if error.is_sqlite_lock() {
            self.consecutive_sqlite_lock_errors =
                self.consecutive_sqlite_lock_errors.saturating_add(1);
            claim_sqlite_lock_backoff_delay(self.consecutive_sqlite_lock_errors)
        } else {
            self.reset();
            Duration::from_secs(1)
        }
    }
}

fn claim_sqlite_lock_backoff_delay(retry_count: u64) -> Duration {
    if retry_count == 0 {
        return Duration::ZERO;
    }
    let shift = retry_count.saturating_sub(1).min(4);
    Duration::from_secs(2_u64.saturating_mul(2_u64.pow(shift as u32)))
        .min(DAEMON_CLAIM_LOCK_BACKOFF_MAX)
}

#[cfg(all(test, unix))]
pub(super) async fn run_hook_ipc_listener(
    listener: tokio::net::UnixListener,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
    spool: Arc<crate::ingress_spool::IngressSpool>,
) {
    run_hook_ipc_listener_with_lease(listener, store, write_observer, spool, None).await
}

#[cfg(unix)]
pub(super) async fn run_hook_ipc_listener_with_lease(
    listener: tokio::net::UnixListener,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
    spool: Arc<crate::ingress_spool::IngressSpool>,
    lease: Option<crate::core::types::RuntimeWriterLease>,
) {
    let drain_spool = Arc::clone(&spool);
    let drain_store = store.clone();
    let drain_observer = write_observer.clone();
    let drain_lease = lease;
    let drain_task = tokio::spawn(async move {
        loop {
            if super::shutdown_requested() {
                break;
            }
            let drain_result = match drain_lease.as_ref() {
                Some(lease) => drain_spool.drain_once_fenced(&drain_store, lease).await,
                None => drain_spool.drain_once(&drain_store).await,
            };
            match drain_result {
                Ok(0) => tokio::time::sleep(Duration::from_millis(200)).await,
                Ok(drained) => {
                    drain_observer.record_successful_write();
                    tracing::debug!(drained, "replayed ingress spool records");
                }
                Err(error) => {
                    match &error {
                        crate::ingress_spool::IngressSpoolError::Queue(queue_error) => {
                            drain_observer
                                .record_queue_error("ingress spool drain failed", queue_error);
                        }
                        _ => drain_observer
                            .record_error(format!("ingress spool drain failed: {error}")),
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
    });
    let mut handlers = tokio::task::JoinSet::new();
    loop {
        if super::shutdown_requested() {
            break;
        }

        if handlers.len() >= HOOK_IPC_HANDLER_LIMIT {
            wait_for_hook_ipc_handler_slot(&mut handlers).await;
            continue;
        }

        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let store = store.clone();
                        let write_observer = write_observer.clone();
                        let spool = Arc::clone(&spool);
                        handlers.spawn(async move {
                            handle_hook_ipc_connection(
                                stream,
                                store,
                                write_observer,
                                spool,
                            )
                            .await;
                        });
                    }
                    Err(error) => {
                        tracing::warn!(?error, "hook IPC accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            joined = handlers.join_next(), if !handlers.is_empty() => {
                record_hook_ipc_handler_join(joined);
            }
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
    drain_hook_ipc_handlers(&mut handlers, HOOK_IPC_HANDLER_DRAIN_BUDGET).await;
    drain_task.abort();
    let _ = drain_task.await;
}

#[cfg(unix)]
async fn wait_for_hook_ipc_handler_slot(handlers: &mut tokio::task::JoinSet<()>) {
    tokio::select! {
        joined = handlers.join_next() => {
            record_hook_ipc_handler_join(joined);
        }
        () = super::wait_for_shutdown_or_sleep(Duration::from_millis(200)) => {}
    }
}

#[cfg(unix)]
async fn drain_hook_ipc_handlers(handlers: &mut tokio::task::JoinSet<()>, budget: Duration) {
    let started = tokio::time::Instant::now();
    while !handlers.is_empty() {
        let remaining = budget.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, handlers.join_next()).await {
            Ok(joined) => record_hook_ipc_handler_join(joined),
            Err(_) => break,
        }
    }
    if !handlers.is_empty() {
        tracing::warn!(
            active_handlers = handlers.len(),
            "aborting hook IPC handlers after drain budget"
        );
        handlers.abort_all();
        while let Some(joined) = handlers.join_next().await {
            record_hook_ipc_handler_join(Some(joined));
        }
    }
}

#[cfg(unix)]
fn record_hook_ipc_handler_join(joined: Option<std::result::Result<(), tokio::task::JoinError>>) {
    if let Some(Err(error)) = joined {
        if error.is_cancelled() {
            tracing::debug!(?error, "hook IPC handler cancelled");
        } else {
            tracing::warn!(?error, "hook IPC handler failed");
        }
    }
}

#[cfg(all(test, unix))]
static HOOK_IPC_ACTIVE_HANDLER_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, unix))]
static HOOK_IPC_PEAK_HANDLER_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(all(test, unix))]
struct HookIpcHandlerCounterGuard;

#[cfg(all(test, unix))]
impl HookIpcHandlerCounterGuard {
    fn new() -> Self {
        let active = HOOK_IPC_ACTIVE_HANDLER_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
        let _ =
            HOOK_IPC_PEAK_HANDLER_COUNT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |peak| {
                (active > peak).then_some(active)
            });
        Self
    }
}

#[cfg(all(test, unix))]
impl Drop for HookIpcHandlerCounterGuard {
    fn drop(&mut self) {
        HOOK_IPC_ACTIVE_HANDLER_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(all(test, unix))]
fn reset_hook_ipc_handler_counters_for_test() {
    HOOK_IPC_ACTIVE_HANDLER_COUNT.store(0, Ordering::SeqCst);
    HOOK_IPC_PEAK_HANDLER_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(all(test, unix))]
fn hook_ipc_handler_counts_for_test() -> (usize, usize) {
    (
        HOOK_IPC_ACTIVE_HANDLER_COUNT.load(Ordering::SeqCst),
        HOOK_IPC_PEAK_HANDLER_COUNT.load(Ordering::SeqCst),
    )
}

#[cfg(unix)]
async fn handle_hook_ipc_connection(
    mut stream: tokio::net::UnixStream,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
    spool: Arc<crate::ingress_spool::IngressSpool>,
) {
    #[cfg(all(test, unix))]
    let _active_handler = HookIpcHandlerCounterGuard::new();

    let request_result = tokio::time::timeout(
        crate::hook_ipc::HOOK_IPC_READ_TIMEOUT,
        crate::hook_ipc::read_request(&mut stream),
    )
    .await;
    let response = match request_result {
        Ok(Ok(crate::hook_ipc::HookIpcRequest::Enqueue(request))) => {
            crate::hook_ipc::HookIpcResponse::Enqueue(
                persist_hook_ipc_request(&store, &spool, &write_observer, request).await,
            )
        }
        Ok(Ok(crate::hook_ipc::HookIpcRequest::Readiness(_))) => {
            crate::hook_ipc::HookIpcResponse::Readiness(
                crate::hook_ipc::HookIpcReadinessResponse::Ready {
                    pid: std::process::id(),
                    process_identity: crate::core::process_identity::current_process_identity()
                        .to_string(),
                },
            )
        }
        Ok(Err(error)) => crate::hook_ipc::HookIpcResponse::Enqueue(
            crate::hook_ipc::HookIpcEnqueueResponse::Error {
                message: format!("invalid hook IPC request: {error}"),
            },
        ),
        Err(_) => crate::hook_ipc::HookIpcResponse::Enqueue(
            crate::hook_ipc::HookIpcEnqueueResponse::Error {
                message: "invalid hook IPC request: timed out reading frame".to_string(),
            },
        ),
    };

    if let Err(error) = crate::hook_ipc::write_response(&mut stream, &response).await {
        if is_hook_ipc_peer_disconnect(&error) {
            tracing::debug!(?error, "hook IPC client disconnected before response");
        } else {
            tracing::warn!(?error, "failed to write hook IPC response");
        }
    }
}

#[cfg(unix)]
async fn persist_hook_ipc_request(
    _store: &AsyncPendingMessageStore,
    spool: &crate::ingress_spool::IngressSpool,
    _write_observer: &crate::daemon_bootstrap::DaemonWriteObserver,
    request: crate::hook_ipc::HookIpcEnqueueRequest,
) -> crate::hook_ipc::HookIpcEnqueueResponse {
    let kind = request.kind.clone();
    let append_result = {
        let spool = spool.clone();
        tokio::task::spawn_blocking(move || spool.append(&request)).await
    };
    match append_result {
        Ok(Ok(_)) => {
            tracing::debug!(%kind, "fsynced hook IPC capture in ingress spool");
            crate::hook_ipc::HookIpcEnqueueResponse::Accepted
        }
        Ok(Err(error)) => {
            let message = format!("failed to fsync ingress spool capture: {error}");
            tracing::warn!(?error, %kind, "failed to fsync ingress spool capture");
            crate::hook_ipc::HookIpcEnqueueResponse::Error { message }
        }
        Err(error) => {
            let message = format!("failed to fsync ingress spool capture: {error}");
            tracing::warn!(?error, %kind, "ingress spool task failed");
            crate::hook_ipc::HookIpcEnqueueResponse::Error { message }
        }
    }
}

#[cfg(unix)]
fn is_hook_ipc_peer_disconnect(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_error| {
                matches!(
                    io_error.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
                )
            })
    })
}

#[cfg(all(test, unix))]
#[path = "self_heal_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "self_heal_replay_tests.rs"]
mod replay_tests;

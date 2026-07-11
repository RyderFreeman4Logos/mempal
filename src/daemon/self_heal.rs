#[cfg(unix)]
use std::io::ErrorKind;
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
}

impl ClaimBackoffState {
    pub(super) fn reset(&mut self) {
        self.consecutive_sqlite_lock_errors = 0;
    }

    pub(super) fn delay_after_error(&mut self, error: &QueueError) -> Duration {
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

#[cfg(unix)]
pub(super) async fn run_hook_ipc_listener(
    listener: tokio::net::UnixListener,
    store: AsyncPendingMessageStore,
    write_observer: crate::daemon_bootstrap::DaemonWriteObserver,
) {
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
                        handlers.spawn(async move {
                            handle_hook_ipc_connection(stream, store, write_observer).await;
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
) {
    #[cfg(all(test, unix))]
    let _active_handler = HookIpcHandlerCounterGuard::new();

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
        if is_hook_ipc_peer_disconnect(&error) {
            tracing::debug!(?error, "hook IPC client disconnected before response");
        } else {
            tracing::warn!(?error, "failed to write hook IPC response");
        }
    }
}

#[cfg(unix)]
async fn persist_hook_ipc_request(
    store: &AsyncPendingMessageStore,
    write_observer: &crate::daemon_bootstrap::DaemonWriteObserver,
    request: crate::hook_ipc::HookIpcEnqueueRequest,
) -> crate::hook_ipc::HookIpcEnqueueResponse {
    match store
        .enqueue_idempotent_with_key_fail_fast(
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

use super::{AsyncPendingMessageStore, ClaimedMessage, INGEST_ASYNC_KIND, INGEST_CLAIM_TTL_SECS};

pub(super) async fn claim_next_ingest_with_io(
    queue: &AsyncPendingMessageStore,
    worker_id: &str,
    shutdown_rx: Option<&mut tokio::sync::watch::Receiver<bool>>,
) -> crate::core::queue::Result<Option<ClaimedMessage>> {
    let io_guard =
        crate::observability::IoBurstGuard::start(crate::observability::IoOperationPath::Queue);
    let result = match shutdown_rx {
        Some(shutdown_rx) => {
            queue
                .claim_next_by_kind_until_shutdown(
                    worker_id.to_string(),
                    INGEST_CLAIM_TTL_SECS,
                    INGEST_ASYNC_KIND.to_string(),
                    shutdown_rx,
                )
                .await
        }
        None => {
            queue
                .claim_next_by_kind(
                    worker_id.to_string(),
                    INGEST_CLAIM_TTL_SECS,
                    INGEST_ASYNC_KIND.to_string(),
                )
                .await
        }
    };
    io_guard.finish();
    result
}

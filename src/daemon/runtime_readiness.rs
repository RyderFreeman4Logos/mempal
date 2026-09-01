use super::*;

#[cfg(feature = "rest")]
pub(super) async fn drain_rest_server(task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
        let _ = task.await;
    }
}

pub(super) fn ensure_daemon_runtime_writer_lease_active(
    db: &Database,
    lease: &RuntimeWriterLease,
) -> Result<()> {
    let active = db
        .runtime_writer_lease_is_active(lease)
        .with_context(|| format!("writer lease `{}` stale before READY", lease.name))?;
    if active {
        Ok(())
    } else {
        Err(DbError::RuntimeWriterLeaseLost {
            lease_name: lease.name.clone(),
            owner: lease.owner.clone(),
            generation: lease.generation,
            operation: "READY",
        }
        .into())
    }
}

pub(super) fn spawn_daemon_ingest_drain_worker(
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
    .with_daemon_owned_async_db(context.async_db.clone())
    .with_external_ingest_writer_lease(writer_lease.lease().clone())
    .with_daemon_write_observer(context.write_observer.clone());
    let handle = server.spawn_scoped_ingest_drain_worker();
    tracing::info!("daemon async ingest worker started");
    Ok(handle)
}

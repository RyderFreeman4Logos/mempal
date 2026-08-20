use std::{
    net::ToSocketAddrs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};

use crate::{
    api::{ApiState, serve_with_optional_mcp},
    core::{async_db::AsyncDb, config::Config},
    daemon_bootstrap::{DaemonContext, DaemonWriteObserver},
    mcp::MempalMcpServer,
};

use super::writer_lease::RuntimeWriterLeaseHandle;

pub(super) async fn spawn_rest(
    context: &DaemonContext,
    db_path: &Path,
    writer_lease: &RuntimeWriterLeaseHandle,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let addr = context.config.api.addr.clone();
    let config = context.config.as_ref().clone();
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!("daemon REST server failed to bind {addr}: {error}");
            eprintln!("warning: daemon REST server failed to bind {addr}: {error}");
            return Ok(None);
        }
    };
    let local_addr = listener
        .local_addr()
        .context("failed to resolve REST server address")?;
    tracing::info!("daemon REST listening on http://{local_addr}");
    eprintln!("daemon REST listening on http://{local_addr}");
    let state = ApiState::new(
        db_path.to_path_buf(),
        Arc::new(crate::embed::ConfiguredEmbedderFactory::new_for_daemon(
            config.clone(),
        )),
    );
    let loopback = addr.to_socket_addrs().ok().is_some_and(|mut addresses| {
        addresses
            .next()
            .is_some_and(|first| first.ip().is_loopback())
            && addresses.all(|address| address.ip().is_loopback())
    });
    let mcp_server = server_for_rest(
        loopback,
        &addr,
        db_path.to_path_buf(),
        config,
        context.async_db.clone(),
        writer_lease,
        context.write_observer.clone(),
    )?;
    Ok(Some(tokio::spawn(async move {
        if let Err(error) = serve_with_optional_mcp(listener, state, mcp_server, async {
            while !super::shutdown_requested() {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
        .await
        {
            tracing::error!("daemon REST server error: {error}");
        }
    })))
}

pub(super) fn server_for_rest(
    loopback: bool,
    addr: &str,
    db_path: PathBuf,
    config: Config,
    async_db: AsyncDb,
    writer_lease: &RuntimeWriterLeaseHandle,
    write_observer: DaemonWriteObserver,
) -> Result<Option<MempalMcpServer>> {
    if !loopback {
        tracing::warn!("daemon MCP endpoint disabled because REST address is not loopback: {addr}");
        return Ok(None);
    }
    Ok(Some(
        MempalMcpServer::new_with_factory_and_config(
            db_path,
            config.clone(),
            Arc::new(crate::embed::ConfiguredEmbedderFactory::new_for_daemon(
                config,
            )),
        )?
        .with_daemon_owned_async_db(async_db)
        .with_external_ingest_writer_lease(writer_lease.lease().clone())
        .with_daemon_write_observer(write_observer),
    ))
}

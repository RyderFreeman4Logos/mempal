use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};

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
            return Err(error).context(format!("daemon REST server failed to bind {addr}"));
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
    let mcp_server = server_for_rest(
        local_addr,
        db_path.to_path_buf(),
        config,
        context.async_db.clone(),
        writer_lease,
        context.write_observer.clone(),
    )?;
    let task = tokio::spawn(async move {
        if let Err(error) = serve_with_optional_mcp(listener, state, mcp_server, async {
            while !super::shutdown_requested() {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        })
        .await
        {
            tracing::error!("daemon REST server error: {error}");
        }
    });
    if let Err(error) = wait_for_rest_server(local_addr).await {
        task.abort();
        return Err(error);
    }
    Ok(Some(task))
}

async fn wait_for_rest_server(addr: SocketAddr) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(100))
        .timeout(Duration::from_millis(250))
        .build()
        .context("failed to build REST readiness client")?;
    // ponytail: any HTTP response proves Axum is polling; keep the probe off
    // DB-backed handlers unless readiness gains its own response contract.
    let url = format!("http://{addr}/api/_serve_ready");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client.get(&url).send().await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("daemon REST server did not become ready at {addr}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rest_readiness_does_not_wait_for_status_diagnostics() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind REST readiness fixture");
        let addr = listener.local_addr().expect("read REST readiness address");
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route(
                    "/api/status",
                    axum::routing::get(|| async { std::future::pending::<()>().await }),
                ),
            )
            .await
        });

        let readiness =
            tokio::time::timeout(Duration::from_millis(500), wait_for_rest_server(addr)).await;
        server.abort();
        let _ = server.await;
        assert!(
            matches!(readiness, Ok(Ok(()))),
            "REST serve-loop readiness must not depend on status diagnostics: {readiness:?}"
        );
    }
}

pub(super) fn server_for_rest(
    local_addr: SocketAddr,
    db_path: PathBuf,
    config: Config,
    async_db: AsyncDb,
    writer_lease: &RuntimeWriterLeaseHandle,
    write_observer: DaemonWriteObserver,
) -> Result<Option<MempalMcpServer>> {
    if !local_addr.ip().is_loopback() {
        tracing::warn!(
            "daemon MCP endpoint disabled because REST listener is not loopback: {local_addr}"
        );
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

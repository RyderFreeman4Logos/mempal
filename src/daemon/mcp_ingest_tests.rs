use std::sync::Arc;
use std::time::Duration;

use crate::core::db::Database;
use crate::observability::test_support::global_observability_test_lock;

use super::{
    global_rest_listen_test_lock, global_shutdown_test_lock, request_shutdown,
    reset_shutdown_request, run_loop,
};

#[tokio::test]
async fn daemon_mcp_listen_port_completes_wait_ingest_when_hooks_disabled() {
    let _rest_lock = global_rest_listen_test_lock().lock_owned().await;
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    let _observability_lock = global_observability_test_lock().lock_owned().await;
    let tempdir = tempfile::TempDir::new().expect("create daemon MCP fixture");
    let db_path = tempdir.path().join("palace.db");
    let config_path = tempdir.path().join("config.toml");
    let runtime_root = tempdir.path().join("runtime");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve daemon MCP port");
    let api_addr = listener.local_addr().expect("read daemon MCP port");
    drop(listener);
    std::fs::write(
        &config_path,
        format!(
            "db_path = \"{}\"\n\n[api]\nenabled = true\naddr = \"{}\"\n\n[embed]\nbackend = \"stub\"\n\n[hooks]\nenabled = false\n\n[ingest_gating]\nenabled = false\n",
            db_path.display(),
            api_addr
        ),
    )
    .expect("write daemon MCP config");

    let mut context = crate::daemon_bootstrap::DaemonContext::bootstrap_with_events_for_test(
        config_path,
        true,
        None,
        &runtime_root,
    )
    .expect("bootstrap daemon MCP fixture");
    context
        .runtime
        .take()
        .expect("daemon bootstrap runtime")
        .shutdown_background();
    assert!(
        !context.config.hooks.enabled,
        "fixture must use the default hooks-disabled daemon configuration"
    );
    reset_shutdown_request();
    let daemon_async_db = context.async_db.clone();
    let daemon_write_observer = context.write_observer.clone();
    let daemon_config = context.config.as_ref().clone();
    let run_task = tokio::spawn(async move { run_loop(&context).await });
    let writer_lease = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status_db_path = db_path.clone();
            let leases = tokio::task::spawn_blocking(move || {
                Database::open(&status_db_path)
                    .and_then(|db| db.runtime_writer_lease_status(Some("sqlite-writer")))
            })
            .await
            .expect("daemon writer lease status task panicked")
            .expect("read daemon writer lease status");
            if let Some(lease) = leases.into_iter().next() {
                break lease;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("daemon writer lease did not become visible");
    let server = crate::mcp::MempalMcpServer::new_with_factory_and_config(
        db_path,
        daemon_config.clone(),
        Arc::new(crate::embed::ConfiguredEmbedderFactory::new_for_daemon(
            daemon_config,
        )),
    )
    .expect("create daemon MCP server")
    .with_daemon_owned_async_db(daemon_async_db)
    .with_external_ingest_writer_lease(writer_lease)
    .with_daemon_write_observer(daemon_write_observer);
    let response = server
        .mempal_ingest(rmcp::handler::server::wrapper::Parameters(
            crate::mcp::IngestRequest {
                content: "daemon hooks-disabled wait ingest".to_string(),
                wing: "smoke".to_string(),
                room: Some("mcp".to_string()),
                smoke: Some(true),
                wait: Some(true),
                wait_timeout_secs: Some(5),
                ..crate::mcp::IngestRequest::default()
            },
        ))
        .await
        .expect("daemon MCP wait ingest");
    let operation_id = response
        .0
        .operation_id
        .clone()
        .expect("daemon MCP ingest operation id");
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = server
                .mempal_operation_status(rmcp::handler::server::wrapper::Parameters(
                    crate::mcp::OperationStatusRequest {
                        operation_id: operation_id.clone(),
                    },
                ))
                .await
                .expect("read daemon MCP ingest status")
                .0;
            if matches!(
                status.state,
                Some(crate::mcp::IngestOperationState::Completed)
                    | Some(crate::mcp::IngestOperationState::Rejected)
                    | Some(crate::mcp::IngestOperationState::Failed)
            ) {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("hooks-disabled daemon MCP wait ingest remained queued");
    assert!(
        matches!(
            terminal.state,
            Some(crate::mcp::IngestOperationState::Completed)
                | Some(crate::mcp::IngestOperationState::Rejected)
                | Some(crate::mcp::IngestOperationState::Failed)
        ),
        "hooks-disabled daemon MCP wait ingest was not terminal: {:?}",
        terminal
    );

    request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), run_task)
        .await
        .expect("daemon MCP fixture did not stop")
        .expect("daemon MCP fixture task panicked")
        .expect("daemon MCP fixture failed");
}

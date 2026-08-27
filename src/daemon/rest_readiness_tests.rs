use std::{net::SocketAddr, time::Duration};

use crate::{
    bootstrap_events::BootstrapEvent, core::db::Database, daemon_bootstrap::DaemonContext,
};

use super::{global_shutdown_test_lock, request_shutdown, reset_shutdown_request, run_loop};

fn write_fixture(
    tempdir: &tempfile::TempDir,
    api_addr: SocketAddr,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let db_path = tempdir.path().join("palace.db");
    let config_path = tempdir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = \"{}\"\n\n[api]\nenabled = true\naddr = \"{}\"\n\n[embed]\nbackend = \"stub\"\n\n[hooks]\nenabled = false\n\n[ingest_gating]\nenabled = false\n",
            db_path.display(), api_addr
        ),
    )
    .expect("write REST readiness config");
    (db_path, config_path)
}

async fn bootstrap_fixture(
    config_path: std::path::PathBuf,
    tempdir: &tempfile::TempDir,
    events: tokio::sync::mpsc::Sender<BootstrapEvent>,
) -> DaemonContext {
    let runtime_root = tempdir.path().join("runtime");
    let context = tokio::task::spawn_blocking(move || {
        DaemonContext::bootstrap_with_events_for_test(
            config_path,
            true,
            Some(events),
            &runtime_root,
        )
        .expect("bootstrap REST readiness fixture")
    })
    .await
    .expect("REST readiness bootstrap task panicked");
    let mut context = context;
    context
        .runtime
        .take()
        .expect("daemon bootstrap runtime")
        .shutdown_background();
    context
}

fn has_ready_event(receiver: &mut tokio::sync::mpsc::Receiver<BootstrapEvent>) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(BootstrapEvent::Ready) => return true,
            Ok(_) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

#[tokio::test]
async fn daemon_ready_requires_a_serving_rest_listener_and_rejects_bind_failure() {
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

    let occupied_tempdir = tempfile::tempdir().expect("create occupied REST fixture");
    let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve occupied REST address");
    let occupied_addr = occupied_listener
        .local_addr()
        .expect("read occupied REST address");
    let (occupied_db_path, occupied_config_path) = write_fixture(&occupied_tempdir, occupied_addr);
    let (occupied_events, mut occupied_receiver) = tokio::sync::mpsc::channel(16);
    let occupied_context =
        bootstrap_fixture(occupied_config_path, &occupied_tempdir, occupied_events).await;

    assert!(
        !has_ready_event(&mut occupied_receiver),
        "daemon-ready was advertised before REST bind"
    );

    let run_task = tokio::spawn(async move { run_loop(&occupied_context).await });
    let result = tokio::time::timeout(Duration::from_secs(5), run_task)
        .await
        .expect("REST bind failure must be reported promptly")
        .expect("daemon task panicked");
    assert!(
        result.is_err(),
        "daemon must not continue after REST bind failure"
    );
    assert!(
        !has_ready_event(&mut occupied_receiver),
        "daemon-ready was advertised after REST bind failure"
    );
    drop(occupied_listener);
    assert!(
        Database::open(&occupied_db_path).is_ok(),
        "REST readiness fixture database should remain readable"
    );

    let serving_tempdir = tempfile::tempdir().expect("create serving REST fixture");
    let reserved_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve REST address");
    let serving_addr = reserved_listener.local_addr().expect("read REST address");
    drop(reserved_listener);
    let (serving_db_path, serving_config_path) = write_fixture(&serving_tempdir, serving_addr);
    let (serving_events, mut serving_receiver) = tokio::sync::mpsc::channel(16);
    let serving_context =
        bootstrap_fixture(serving_config_path, &serving_tempdir, serving_events).await;

    assert!(
        !has_ready_event(&mut serving_receiver),
        "daemon-ready was advertised before REST serving"
    );
    let serving_task = tokio::spawn(async move { run_loop(&serving_context).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match serving_receiver.recv().await {
                Some(BootstrapEvent::Ready) => break,
                Some(_) => {}
                None => panic!("REST readiness event channel closed"),
            }
        }
    })
    .await
    .expect("REST readiness event did not arrive");

    let response = reqwest::Client::new()
        .post(format!("http://{serving_addr}/api/ingest/durable"))
        .json(&serde_json::json!({
            "idempotency_key": "rest-readiness-test",
            "request": {"content": "serving REST", "wing": "smoke"}
        }))
        .send()
        .await
        .expect("send durable ingest through serving REST listener");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);

    request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), serving_task)
        .await
        .expect("serving REST fixture did not stop")
        .expect("serving REST fixture task panicked")
        .expect("serving REST fixture failed");
    assert!(Database::open(&serving_db_path).is_ok());
}

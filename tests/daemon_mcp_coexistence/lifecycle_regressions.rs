use std::sync::mpsc;
use std::thread;

use super::*;

#[tokio::test]
async fn mcp_initialize_timeout_reaps_escaped_leaf() -> Result<()> {
    let _process_lock = local_gate_child::PROCESS_LIFECYCLE_TEST_LOCK.lock().await;
    let tempdir = TempDir::new().context("create hostile MCP test directory")?;
    let descendant = tempdir.path().join("initialize-descendant.pid");
    let mut client = spawn_hostile_mcp(false, &descendant)?;
    let leader_pid = client.id();
    let started = Instant::now();
    let error = client
        .initialize()
        .await
        .expect_err("hostile child must time out during initialize");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(client.is_reaped(), "child {leader_pid} not reaped");
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("MCP initialize timed out"));
    assert!(diagnostic.contains("hostile initialize fixture"));
    assert_process_exited(&descendant).await?;
    assert_process_exited(&descendant.with_extension("leaf")).await
}

#[tokio::test]
async fn mcp_shutdown_timeout_reaps_escaped_leaf() -> Result<()> {
    let _process_lock = local_gate_child::PROCESS_LIFECYCLE_TEST_LOCK.lock().await;
    let tempdir = TempDir::new().context("create hostile MCP test directory")?;
    let descendant = tempdir.path().join("shutdown-descendant.pid");
    let mut client = spawn_hostile_mcp(true, &descendant)?;
    client.initialize().await?;
    let leader_pid = client.id();
    let started = Instant::now();
    let error = client
        .shutdown()
        .await
        .expect_err("hostile child must time out during shutdown");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(client.is_reaped(), "child {leader_pid} not reaped");
    let diagnostic = format!("{error:#}");
    assert!(diagnostic.contains("MCP shutdown response timed out"));
    assert!(diagnostic.contains("hostile shutdown fixture"));
    assert_process_exited(&descendant).await?;
    assert_process_exited(&descendant.with_extension("leaf")).await
}

async fn assert_marker_publication_is_atomic(
    name: &str,
    spawn: fn(&Path) -> Result<McpStdio>,
) -> Result<()> {
    let tempdir = TempDir::new().context("create marker publication directory")?;
    let identity_path = tempdir.path().join(format!("{name}.pid"));
    let pause_path = tempdir.path().join(format!("{name}.pid.pause"));
    let opened_path = tempdir.path().join(format!("{name}.pid.opened"));
    let release_path = tempdir.path().join(format!("{name}.pid.release"));
    fs::write(&pause_path, b"")?;
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker_path = identity_path.clone();
    let runtime = tokio::runtime::Handle::current();
    let worker = thread::spawn(move || {
        let _runtime_guard = runtime.enter();
        let result = spawn(&worker_path).map_err(|error| format!("{error:#}"));
        let _ = result_tx.send(result);
    });

    wait_for_fixture_marker(&opened_path, &format!("{name} marker publication barrier"))?;
    let early = result_rx.recv_timeout(Duration::from_millis(150)).ok();
    let returned_before_release = early.is_some();
    fs::write(&release_path, b"")?;
    let result = if let Some(result) = early {
        result
    } else {
        result_rx
            .recv_timeout(Duration::from_secs(2))
            .context("fixture did not finish after marker publication release")?
    };
    worker.join().expect("marker fixture thread");
    assert!(
        !returned_before_release,
        "{name} constructor observed the marker pathname before publication completed"
    );
    let mut client = result.map_err(anyhow::Error::msg)?;
    client
        .fence_process_group_and_reap(tokio::time::Instant::now(), None)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_descendant_identity_marker_publishes_atomically() -> Result<()> {
    let _process_lock = local_gate_child::PROCESS_LIFECYCLE_TEST_LOCK.lock().await;
    assert_marker_publication_is_atomic("malformed", spawn_malformed_initialize_mcp).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_descendant_identity_marker_publishes_atomically() -> Result<()> {
    let _process_lock = local_gate_child::PROCESS_LIFECYCLE_TEST_LOCK.lock().await;
    assert_marker_publication_is_atomic("graceful", spawn_graceful_mcp_with_descendant).await
}

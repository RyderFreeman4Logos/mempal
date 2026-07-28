use std::time::{Duration, Instant};

use super::{
    Database, DeleteRequest, Parameters, hold_sqlite_write_lock, insert_drawer, setup_server,
};

#[tokio::test]
async fn test_mcp_delete_retries_cached_async_pool_write_after_transient_sqlite_lock() {
    let (_tempdir, db_path, server) = setup_server();
    let drawer_id = "mcp-delete-busy-retry-target";
    insert_drawer(
        &db_path,
        drawer_id,
        "MCP delete must retry a cached async-pool SQLite write after transient contention",
        "mcp",
        Some("busy"),
        "/tmp/mcp-delete-busy-retry.md",
        2,
    );
    server
        .async_db()
        .await
        .expect("fixture must use the cached MCP async database pool");

    // The cached pool writer has SQLite's five-second busy timeout. Hold the
    // lock longer so this exercises the retry after run_write returns busy.
    let lock = hold_sqlite_write_lock(db_path.clone(), Duration::from_millis(5_500));
    let result = tokio::time::timeout(
        Duration::from_secs(9),
        server.mempal_delete(Parameters(DeleteRequest {
            drawer_id: drawer_id.to_string(),
        })),
    )
    .await;
    lock.join().expect("release SQLite write lock");

    let delete = match result {
        Ok(Ok(response)) => response.0,
        Ok(Err(_)) => panic!("MCP delete should retry the transient SQLite lock"),
        Err(_) => panic!("MCP delete retry should stay within its bounded lock budget"),
    };
    assert!(delete.deleted);
    assert!(
        !Database::open(&db_path)
            .expect("open database after delete")
            .drawer_exists(drawer_id)
            .expect("check drawer after delete"),
        "successful MCP delete must persist the soft deletion"
    );
}

#[tokio::test]
async fn test_mcp_delete_retry_deadline_rejects_queued_late_write_without_soft_delete() {
    let (_tempdir, db_path, server) = setup_server();
    let drawer_id = "mcp-delete-busy-retry-deadline-target";
    insert_drawer(
        &db_path,
        drawer_id,
        "MCP delete must not commit after its shared SQLite retry deadline expires",
        "mcp",
        Some("busy-deadline"),
        "/tmp/mcp-delete-busy-retry-deadline.md",
        2,
    );
    let async_db = server
        .async_db()
        .await
        .expect("fixture must use the cached MCP async database pool");

    // Hold a real external SQLite write lock beyond the retry deadline. A
    // queued pooled writer keeps the delete waiting for its sole writer permit
    // until after that deadline, which used to let a late attempt soft-delete
    // after the caller had already timed out.
    let lock = hold_sqlite_write_lock(db_path.clone(), Duration::from_millis(11_000));
    let (writer_ready_tx, writer_ready_rx) = tokio::sync::oneshot::channel();
    let writer = tokio::spawn(async move {
        async_db
            .run_write(move |_| {
                writer_ready_tx
                    .send(())
                    .expect("signal queued writer holds the pool permit");
                std::thread::sleep(Duration::from_millis(10_250));
                Ok::<(), crate::core::db::DbError>(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), writer_ready_rx)
        .await
        .expect("queued writer must acquire the pool permit")
        .expect("queued writer readiness sender must stay alive");

    let started = Instant::now();
    let mut delete = tokio::spawn(async move {
        server
            .mempal_delete(Parameters(DeleteRequest {
                drawer_id: drawer_id.to_string(),
            }))
            .await
    });
    let bounded = tokio::time::timeout(Duration::from_millis(10_750), &mut delete).await;
    let elapsed = started.elapsed();

    lock.join().expect("release SQLite write lock");
    writer
        .await
        .expect("queued writer task")
        .expect("queued writer");
    let delete = match bounded {
        Ok(result) => result.expect("MCP delete task must not panic"),
        Err(_) => delete
            .await
            .expect("drain a formerly late MCP delete after lock release"),
    };

    let error = match delete {
        Ok(_) => panic!("MCP delete must not report success after its retry deadline"),
        Err(error) => error,
    };
    assert!(
        elapsed < Duration::from_millis(10_750),
        "MCP delete exceeded its wall-clock retry budget: {elapsed:?}"
    );
    let data = error
        .data
        .as_ref()
        .expect("structured database lock error data");
    assert_eq!(
        data.get("reason").and_then(serde_json::Value::as_str),
        Some("database_locked")
    );
    assert_eq!(
        data.get("action").and_then(serde_json::Value::as_str),
        Some("retry_after_transient_lock")
    );
    assert!(
        Database::open(&db_path)
            .expect("open database after exhausted retry")
            .drawer_exists(drawer_id)
            .expect("check drawer after exhausted retry"),
        "an expired MCP delete must not soft-delete after its caller receives database_locked"
    );
}

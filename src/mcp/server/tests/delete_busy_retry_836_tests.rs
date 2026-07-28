use std::time::Duration;

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

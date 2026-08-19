use super::{Database, DeleteRequest, Parameters, insert_drawer, setup_server};

#[tokio::test]
async fn delete_already_soft_deleted_drawer_returns_success_receipt() {
    let (_tempdir, db_path, server) = setup_server();
    insert_drawer(
        &db_path,
        "mcp-delete-already-gone-target",
        "delete receipt target already soft-deleted",
        "mcp",
        Some("receipt"),
        "/tmp/mcp-delete-already-gone.md",
        2,
    );
    // Simulate a prior operation (e.g. an ingest `supersedes`) that already
    // soft-deleted this drawer before the MCP cleanup deletes it in batch.
    {
        let db = Database::open(&db_path).expect("open db");
        assert!(
            db.soft_delete_drawer("mcp-delete-already-gone-target")
                .expect("prior soft-delete")
        );
    }

    let delete = server
        .mempal_delete(Parameters(DeleteRequest {
            drawer_id: "mcp-delete-already-gone-target".to_string(),
        }))
        .await
        .expect("MCP delete of an already-gone drawer must not error")
        .0;

    assert!(
        delete.deleted,
        "a completed delete must not be classified as delete_false: the drawer is already gone"
    );
    let db = Database::open(&db_path).expect("open db after delete");
    assert!(
        !db.drawer_exists("mcp-delete-already-gone-target")
            .expect("drawer exists")
    );
}

#[tokio::test]
async fn delete_missing_drawer_returns_false_receipt() {
    let (_tempdir, _db_path, server) = setup_server();
    let delete = server
        .mempal_delete(Parameters(DeleteRequest {
            drawer_id: "mcp-delete-never-created".to_string(),
        }))
        .await
        .expect("MCP delete of a missing drawer must not error")
        .0;
    assert!(
        !delete.deleted,
        "a drawer that never existed must not be reported as deleted"
    );
}

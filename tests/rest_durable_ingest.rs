#![cfg(feature = "rest")]

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::db::Database;
use mempal::durable_ingest;
use mempal::embed::{Embedder, EmbedderFactory};
use mempal::mcp::{IngestOperationState, MempalMcpServer};
use tempfile::TempDir;

#[derive(Clone)]
struct StubEmbedderFactory;

struct StubEmbedder;

#[async_trait]
impl EmbedderFactory for StubEmbedderFactory {
    async fn build(&self) -> mempal::embed::Result<Box<dyn Embedder>> {
        Ok(Box::new(StubEmbedder))
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "durable-rest-test"
    }
}

fn setup() -> (TempDir, PathBuf, MempalMcpServer) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("initialize database");
    let server = MempalMcpServer::new_with_factory(db_path.clone(), Arc::new(StubEmbedderFactory))
        .expect("create ingest worker");
    (tempdir, db_path, server)
}

#[tokio::test]
async fn durable_ingest_idempotent_replay() {
    let (_tempdir, db_path, server) = setup();
    let request = serde_json::json!({
        "content": "synthetic durable receipt evidence",
        "wing": "receipt-test",
        "room": "idempotency",
        "importance": 3
    });
    let first = durable_ingest::admit(&db_path, "receipt-event-1", request.clone())
        .expect("first durable admission");
    let replay = durable_ingest::admit(&db_path, "receipt-event-1", request.clone())
        .expect("idempotent durable replay");
    let distinct = durable_ingest::admit(&db_path, "receipt-event-2", request)
        .expect("distinct durable event");

    assert_eq!(first.operation_id, replay.operation_id);
    assert_ne!(first.operation_id, distinct.operation_id);

    let worker = server.spawn_scoped_ingest_drain_worker();
    let completed = server
        .wait_for_operation_completion(&first.operation_id)
        .await
        .expect("wait for first operation");
    let distinct_completed = server
        .wait_for_operation_completion(&distinct.operation_id)
        .await
        .expect("wait for distinct operation");
    worker.shutdown_and_drain().await;

    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(!completed.drawer_id.is_empty());
    assert_eq!(
        distinct_completed.state,
        Some(IngestOperationState::Completed)
    );
    let db = Database::open(&db_path).expect("open completed database");
    assert_eq!(
        db.find_active_drawers_by_content(
            "synthetic durable receipt evidence",
            "receipt-test",
            Some("idempotency"),
            None,
        )
        .expect("query durable drawer")
        .len(),
        1,
    );
}

#[tokio::test]
async fn durable_ingest_survives_restart() {
    let (_tempdir, db_path, initial_server) = setup();
    let receipt = durable_ingest::admit(
        &db_path,
        "restart-event-1",
        serde_json::json!({
            "content": "synthetic restart evidence",
            "wing": "receipt-test",
            "room": "restart"
        }),
    )
    .expect("durable admission before restart");
    drop(initial_server);

    let restarted =
        MempalMcpServer::new_with_factory(db_path.clone(), Arc::new(StubEmbedderFactory))
            .expect("restart ingest worker");
    let queued = durable_ingest::status(&db_path, &receipt.operation_id)
        .expect("receipt remains queryable after restart");
    assert_eq!(queued.state, "queued");

    let worker = restarted.spawn_scoped_ingest_drain_worker();
    let completed = restarted
        .wait_for_operation_completion(&receipt.operation_id)
        .await
        .expect("wait after restart");
    worker.shutdown_and_drain().await;
    assert_eq!(completed.state, Some(IngestOperationState::Completed));
    assert!(!completed.drawer_id.is_empty());
}

#[tokio::test]
async fn durable_delete_reuses_queue_and_survives_restart() {
    let (_tempdir, db_path, server) = setup();
    let ingest = durable_ingest::admit(
        &db_path,
        "delete-seed-event",
        serde_json::json!({
            "content": "synthetic delete seed",
            "wing": "receipt-test",
            "room": "delete"
        }),
    )
    .expect("admit seed ingest");
    let worker = server.spawn_scoped_ingest_drain_worker();
    let inserted = server
        .wait_for_operation_completion(&ingest.operation_id)
        .await
        .expect("complete seed ingest");
    worker.shutdown_and_drain().await;
    let drawer_id = inserted.drawer_id;

    let first = durable_ingest::admit_delete(&db_path, "delete-event-1", drawer_id.clone())
        .expect("admit durable delete");
    let replay = durable_ingest::admit_delete(&db_path, "delete-event-1", drawer_id.clone())
        .expect("replay durable delete");
    assert_eq!(first.operation_id, replay.operation_id);
    drop(server);

    let restarted =
        MempalMcpServer::new_with_factory(db_path.clone(), Arc::new(StubEmbedderFactory))
            .expect("restart delete worker");
    let worker = restarted.spawn_scoped_ingest_drain_worker();
    let deleted = restarted
        .wait_for_operation_completion(&first.operation_id)
        .await
        .expect("complete durable delete after restart");
    worker.shutdown_and_drain().await;

    assert_eq!(deleted.state, Some(IngestOperationState::Completed));
    assert_eq!(deleted.drawer_id, drawer_id);
    let db = Database::open(&db_path).expect("open deleted database");
    assert!(
        db.get_drawer(&deleted.drawer_id)
            .expect("query deleted drawer")
            .is_none()
    );
}

use super::*;

#[tokio::test]
async fn fsynced_spool_receipt_is_publicly_followable_through_replay() {
    let tempdir = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("open database");
    let server = MempalMcpServer::new(db_path.clone(), Config::default()).expect("create server");
    let request = crate::hook_ipc::HookIpcEnqueueRequest {
        kind: INGEST_ASYNC_KIND.to_string(),
        payload: "{}".to_string(),
        idempotency_key: "public-spool-receipt".to_string(),
    };
    let operation_id =
        PendingMessageStore::idempotent_message_id(&request.kind, &request.idempotency_key);
    let spool = crate::ingress_spool::IngressSpool::new(tempdir.path());
    spool.append(&request).expect("fsync daemon receipt");

    let pending = server
        .mempal_operation_status(Parameters(OperationStatusRequest {
            operation_id: operation_id.clone(),
        }))
        .await
        .expect("fsynced spool receipt must be publicly followable")
        .0;
    assert_eq!(pending.operation_id.as_deref(), Some(operation_id.as_str()));
    assert_eq!(pending.state, Some(IngestOperationState::Queued));
    assert!(pending.created_drawer_ids.is_empty());
    assert!(
        server
            .mempal_operation_status(Parameters(OperationStatusRequest {
                operation_id: "msg-random-unknown".to_string(),
            }))
            .await
            .is_err(),
        "an unrelated operation id must not inherit queued status"
    );

    let replay_store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    assert_eq!(
        spool
            .drain_once(&replay_store)
            .await
            .expect("replay fsynced receipt"),
        1
    );
    let replayed = server
        .mempal_operation_status(Parameters(OperationStatusRequest { operation_id }))
        .await
        .expect("replayed queue row remains followable")
        .0;
    assert_eq!(replayed.state, Some(IngestOperationState::Queued));
}

use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expired_daemon_generation_cannot_claim_durable_ingest() {
    let (_tempdir, db_path, server) = setup_server();
    let db = Database::open(&db_path).expect("open database");
    let lease = db
        .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 120, None)
        .expect("acquire daemon lease")
        .expect("daemon lease available");
    let operation_id = server
        .async_queue
        .enqueue(INGEST_ASYNC_KIND.to_string(), "{}".to_string())
        .await
        .expect("durably enqueue ingest before expiry");
    db.conn()
        .execute(
            "UPDATE runtime_writer_leases SET expires_at = '1970-01-01T00:00:00Z' \
             WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4",
            rusqlite::params![
                &lease.name,
                &lease.owner,
                &lease.session_id,
                lease.generation as i64
            ],
        )
        .expect("force daemon lease expiry");
    let daemon = server.with_external_ingest_writer_lease(lease.clone());

    let result = tokio::time::timeout(
        Duration::from_millis(200),
        daemon.run_ingest_drain_worker_loop(
            daemon.async_queue.clone(),
            "expired-daemon-worker".to_string(),
            None,
        ),
    )
    .await
    .expect("expired lease must terminate the worker instead of polling")
    .expect_err("expired lease must reject the queue claim");
    assert!(result.chain().any(|cause| matches!(
        cause.downcast_ref::<QueueError>(),
        Some(QueueError::RuntimeWriterLeaseLost { generation, .. }) if *generation == lease.generation
    )));
    let record = daemon
        .async_queue
        .operation_status(operation_id)
        .await
        .expect("read durable queue record")
        .expect("queued ingest remains durable");
    assert_eq!(record.op_state, "queued");
}

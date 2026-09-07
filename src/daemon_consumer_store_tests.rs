use super::*;

fn assert_lease_lost<T>(result: crate::core::queue::Result<T>, generation: u64) {
    assert!(matches!(
        result,
        Err(QueueError::RuntimeWriterLeaseLost {
            generation: rejected,
            ..
        }) if rejected == generation
    ));
}

async fn assert_consumer_kind_is_fenced(kind: &str) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let producer = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let message_id = producer
        .enqueue(kind.to_string(), "{}".to_string())
        .await
        .expect("producer enqueue before lease");
    let stale = db
        .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 120, None)
        .expect("acquire daemon lease")
        .expect("daemon lease available");
    let stale_consumer = bind_daemon_consumer_store(&producer, &stale);
    let worker_id = format!("{kind}-worker");
    let claim = stale_consumer
        .claim_next_by_kind(worker_id.clone(), 60, kind.to_string())
        .await
        .expect("valid generation may claim")
        .expect("queued message");
    assert_eq!(claim.id, message_id);
    db.conn()
        .execute(
            "UPDATE runtime_writer_leases SET expires_at = '1970-01-01T00:00:00Z' \
             WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4",
            rusqlite::params![
                &stale.name,
                &stale.owner,
                &stale.session_id,
                stale.generation as i64
            ],
        )
        .expect("force lease expiry");

    assert_lease_lost(
        stale_consumer
            .refresh_heartbeat(message_id.clone(), worker_id.clone())
            .await,
        stale.generation,
    );
    assert_lease_lost(
        stale_consumer
            .complete_operation(
                claim.clone(),
                "completed".to_string(),
                None,
                None,
                None,
                None,
            )
            .await,
        stale.generation,
    );
    assert_lease_lost(
        stale_consumer
            .mark_failed(claim.clone(), "retry".to_string())
            .await,
        stale.generation,
    );
    assert_lease_lost(
        stale_consumer.release_claim(claim.clone()).await,
        stale.generation,
    );
    assert_lease_lost(
        stale_consumer
            .claim_next_by_kind(worker_id, 60, kind.to_string())
            .await,
        stale.generation,
    );

    producer
        .enqueue(kind.to_string(), "producer remains durable".to_string())
        .await
        .expect("producer enqueue stays independent of consumer lease");
    let current = db
        .runtime_writer_lease_acquire("sqlite-writer", "replacement", "daemon", 120, None)
        .expect("replace expired generation")
        .expect("replacement lease available");
    db.conn()
        .execute(
            "UPDATE pending_messages SET heartbeat_at = 0 WHERE id = ?1",
            [&message_id],
        )
        .expect("make expired claim reclaimable");
    let current_consumer = bind_daemon_consumer_store(&producer, &current);
    let recovered = current_consumer
        .claim_next_by_kind("replacement-worker".to_string(), 0, kind.to_string())
        .await
        .expect("replacement generation may claim")
        .expect("expired claim recovered");
    assert_eq!(recovered.id, message_id);
    current_consumer
        .release_claim(recovered)
        .await
        .expect("replacement generation may release");
}

#[tokio::test]
async fn hook_consumer_lifecycle_uses_daemon_generation_fence() {
    assert_consumer_kind_is_fenced("hook_user_prompt").await;
}

#[tokio::test]
async fn llm_consumer_lifecycle_uses_daemon_generation_fence() {
    assert_consumer_kind_is_fenced("llm_task").await;
}

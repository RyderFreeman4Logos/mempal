//! Generation-fenced queue mutations for long-lived runtime writers.

use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    AsyncPendingMessageStore, INGEST_ASYNC_KIND, OVERSIZE_REJECTION_TOTAL_KEY, PendingMessageStore,
    QueueError, Result, active_payload_bytes_for_kind, explicit_key_conflicts, hash_source,
    idempotent_key_message_id, increment_meta_counter, next_id, now_secs,
};
use crate::core::types::RuntimeWriterLease;

impl AsyncPendingMessageStore {
    /// Fence consumer lifecycle mutations without blocking durable producer enqueue.
    pub fn with_lifecycle_writer_lease(mut self, lease: RuntimeWriterLease) -> Self {
        self.inner.lifecycle_writer_lease = Some(lease);
        self
    }

    pub async fn enqueue_fenced(
        &self,
        lease: Option<RuntimeWriterLease>,
        kind: String,
        payload: String,
        operation: &'static str,
    ) -> Result<String> {
        self.run(move |store| store.enqueue_fenced(lease.as_ref(), &kind, &payload, operation))
            .await
    }

    pub async fn enqueue_idempotent_with_key_fail_fast_fenced(
        &self,
        lease: Option<RuntimeWriterLease>,
        kind: String,
        payload: String,
        idempotency_key: String,
        operation: &'static str,
    ) -> Result<String> {
        self.run(move |store| {
            store.enqueue_idempotent_with_key_fail_fast_fenced(
                lease.as_ref(),
                &kind,
                &payload,
                &idempotency_key,
                operation,
            )
        })
        .await
    }
}

impl PendingMessageStore {
    /// Fence consumer lifecycle mutations without blocking durable producer enqueue.
    pub fn with_lifecycle_writer_lease(mut self, lease: RuntimeWriterLease) -> Self {
        self.lifecycle_writer_lease = Some(lease);
        self
    }

    pub(super) fn require_lifecycle_writer_lease(
        &self,
        conn: &Connection,
        operation: &'static str,
    ) -> Result<()> {
        if let Some(lease) = &self.lifecycle_writer_lease {
            require_runtime_writer_lease(conn, lease, operation)?;
        }
        Ok(())
    }

    /// Enqueue under the same SQLite write lock that validates a runtime lease.
    pub fn enqueue_fenced(
        &self,
        lease: Option<&RuntimeWriterLease>,
        kind: &str,
        payload: &str,
        operation: &'static str,
    ) -> Result<String> {
        let Some(lease) = lease else {
            return self.enqueue(kind, payload);
        };
        let created_at = now_secs();
        let source_hash = hash_source(kind, payload);
        let id = next_id("msg");
        let payload_bytes = u64::try_from(payload.len()).unwrap_or(u64::MAX);

        self.with_connection(|conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            require_runtime_writer_lease(&tx, lease, operation)?;

            if kind == INGEST_ASYNC_KIND {
                let active_bytes = active_payload_bytes_for_kind(&tx, INGEST_ASYNC_KIND)?;
                if payload_bytes > self.config.max_ingest_active_bytes
                    || active_bytes
                        > self
                            .config
                            .max_ingest_active_bytes
                            .saturating_sub(payload_bytes)
                {
                    increment_meta_counter(&tx, OVERSIZE_REJECTION_TOTAL_KEY)?;
                    tx.commit()?;
                    return Err(QueueError::IngestByteBudgetExceeded {
                        payload_bytes,
                        active_bytes,
                        limit_bytes: self.config.max_ingest_active_bytes,
                    });
                }
            }

            tx.execute(
                r#"
                INSERT INTO pending_messages (
                    id,
                    kind,
                    source_hash,
                    status,
                    payload,
                    created_at,
                    next_attempt_at
                )
                VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?5)
                "#,
                params![id, kind, source_hash, payload, created_at],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }

    /// Fail-fast explicit-key enqueue under the same Immediate lease fence.
    pub fn enqueue_idempotent_with_key_fail_fast_fenced(
        &self,
        lease: Option<&RuntimeWriterLease>,
        kind: &str,
        payload: &str,
        idempotency_key: &str,
        operation: &'static str,
    ) -> Result<String> {
        let Some(lease) = lease else {
            return self.enqueue_idempotent_with_key_fail_fast(kind, payload, idempotency_key);
        };
        let created_at = now_secs();
        let source_hash = hash_source(kind, payload);
        let id = idempotent_key_message_id(kind, idempotency_key);
        self.with_connection_with_busy_timeout(Some(Duration::ZERO), |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            require_runtime_writer_lease(&tx, lease, operation)?;
            if explicit_key_conflicts(&tx, &id, kind, &source_hash)? {
                return Err(QueueError::IdempotencyConflict);
            }
            tx.execute(
                r#"
                INSERT INTO pending_messages (
                    id,
                    kind,
                    source_hash,
                    status,
                    payload,
                    created_at,
                    next_attempt_at
                )
                SELECT ?1, ?2, ?3, 'pending', ?4, ?5, ?5
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM pending_message_completions
                    WHERE message_id = ?1
                )
                ON CONFLICT(id) DO NOTHING
                "#,
                params![id, kind, source_hash, payload, created_at],
            )?;
            tx.commit()?;
            Ok(id)
        })
    }
}

fn require_runtime_writer_lease(
    conn: &Connection,
    lease: &RuntimeWriterLease,
    operation: &'static str,
) -> Result<()> {
    let active = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM runtime_writer_leases
             WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4
               AND expires_at >= strftime('%Y-%m-%dT%H:%M:%fZ','now')
         )",
        params![
            lease.name,
            lease.owner,
            lease.session_id,
            lease.generation as i64
        ],
        |row| row.get::<_, i64>(0),
    )?;
    if active != 0 {
        return Ok(());
    }
    Err(QueueError::RuntimeWriterLeaseLost {
        lease_name: lease.name.clone(),
        owner: lease.owner.clone(),
        generation: lease.generation,
        operation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;

    #[test]
    fn fenced_enqueue_rejects_generation_replaced_after_preflight() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let stale = db
            .runtime_writer_lease_acquire("sqlite-writer", "old", "daemon", 300, None)
            .expect("acquire old lease")
            .expect("old lease available");
        assert!(
            db.runtime_writer_lease_is_active(&stale)
                .expect("preflight old generation")
        );
        assert!(
            db.runtime_writer_lease_release(&stale)
                .expect("release old generation")
        );
        let current = db
            .runtime_writer_lease_acquire("sqlite-writer", "new", "daemon", 300, None)
            .expect("acquire current lease")
            .expect("current lease available");

        let error = store
            .enqueue_fenced(Some(&stale), "llm_task", "{}", "enqueue LLM task")
            .expect_err("stale generation must not enqueue after takeover");
        assert!(matches!(
            error,
            QueueError::RuntimeWriterLeaseLost { generation, .. }
                if generation == stale.generation
        ));
        assert_eq!(store.stats().expect("queue stats").pending, 0);

        store
            .enqueue_fenced(Some(&current), "llm_task", "{}", "enqueue LLM task")
            .expect("current generation may enqueue");
        assert_eq!(store.stats().expect("queue stats").pending, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fenced_enqueue_rejects_expired_live_generation() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let expired = db
            .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 120, None)
            .expect("acquire daemon lease")
            .expect("daemon lease available");
        db.conn()
            .execute(
                "UPDATE runtime_writer_leases SET expires_at = '1970-01-01T00:00:00Z' \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4",
                params![
                    &expired.name,
                    &expired.owner,
                    &expired.session_id,
                    expired.generation as i64
                ],
            )
            .expect("force lease expiry");

        let error = store
            .enqueue_fenced(Some(&expired), "llm_task", "{}", "enqueue LLM task")
            .expect_err("expired live generation must not enqueue as lease owner");
        assert!(matches!(
            error,
            QueueError::RuntimeWriterLeaseLost { generation, .. }
                if generation == expired.generation
        ));
        assert_eq!(store.stats().expect("queue stats").pending, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queue_lifecycle_requires_current_unexpired_generation_and_recovers_claim() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let store = PendingMessageStore::new(&db_path).expect("open queue");
        let message_id = store
            .enqueue(INGEST_ASYNC_KIND, "{}")
            .expect("durable enqueue is independent of lifecycle lease");
        let stale = db
            .runtime_writer_lease_acquire_for_daemon_start("sqlite-writer", 120, None)
            .expect("acquire daemon lease")
            .expect("daemon lease available");
        let stale_store = store.clone().with_lifecycle_writer_lease(stale.clone());
        let first_claim = stale_store
            .claim_next_by_kind("old-worker", 60, INGEST_ASYNC_KIND)
            .expect("valid generation may claim")
            .expect("queued ingest");
        assert_eq!(first_claim.id, message_id);
        db.conn()
            .execute(
                "UPDATE runtime_writer_leases SET expires_at = '1970-01-01T00:00:00Z' \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4",
                params![
                    &stale.name,
                    &stale.owner,
                    &stale.session_id,
                    stale.generation as i64
                ],
            )
            .expect("force lease expiry");

        let rejected_lifecycle = [
            stale_store.confirm(&first_claim),
            stale_store.complete_operation(&first_claim, "completed", None, None, None, None),
            stale_store.mark_failed(&first_claim, "retry"),
            stale_store.release_claim(&first_claim),
        ];
        assert!(rejected_lifecycle.into_iter().all(|result| matches!(
            result,
            Err(QueueError::RuntimeWriterLeaseLost { generation, .. })
                if generation == stale.generation
        )));
        let current = db
            .runtime_writer_lease_acquire("sqlite-writer", "new", "daemon", 120, None)
            .expect("explicit takeover")
            .expect("expired generation is replaceable by explicit policy");
        assert!(current.generation > stale.generation);
        assert!(matches!(
            stale_store.refresh_heartbeat(&first_claim.id, "old-worker"),
            Err(QueueError::RuntimeWriterLeaseLost { generation, .. })
                if generation == stale.generation
        ));

        db.conn()
            .execute(
                "UPDATE pending_messages SET heartbeat_at = 0 WHERE id = ?1",
                [&message_id],
            )
            .expect("make abandoned claim reclaimable");
        let current_store = store.with_lifecycle_writer_lease(current);
        let recovered = current_store
            .claim_next_by_kind("new-worker", 0, INGEST_ASYNC_KIND)
            .expect("current generation may reclaim")
            .expect("abandoned durable ingest is recovered");
        assert_eq!(recovered.id, message_id);
        current_store
            .release_claim(&recovered)
            .expect("current generation may release recovered claim");
        assert_eq!(current_store.stats().expect("queue stats").pending, 1);
    }
}

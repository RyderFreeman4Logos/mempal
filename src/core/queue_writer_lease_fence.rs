//! Generation-fenced queue mutations for long-lived runtime writers.

use rusqlite::{Connection, TransactionBehavior, params};

use super::{
    AsyncPendingMessageStore, INGEST_ASYNC_KIND, OVERSIZE_REJECTION_TOTAL_KEY, PendingMessageStore,
    QueueError, Result, active_payload_bytes_for_kind, hash_source, increment_meta_counter,
    next_id, now_secs,
};
use crate::core::types::RuntimeWriterLease;

impl AsyncPendingMessageStore {
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
}

impl PendingMessageStore {
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
}

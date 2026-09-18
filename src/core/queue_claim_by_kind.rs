//! Kind-filtered claim: probe the reader cache before opening a claim writer.

use rusqlite::{OptionalExtension, params};

use super::{
    ClaimedMessage, PendingMessageStore, QueueError, Result, claim_work_available, next_id,
    now_secs, reclaim_stale_tx, saturating_cutoff, transaction_immediate,
};

impl PendingMessageStore {
    pub(super) fn claim_next_by_kind_once(
        &self,
        worker_id: &str,
        claim_ttl_secs: i64,
        kind_filter: &str,
        approval: &mut impl FnMut() -> bool,
    ) -> Result<Option<ClaimedMessage>> {
        let now = now_secs();
        let stale_cutoff = saturating_cutoff(now, claim_ttl_secs);
        if !self.with_query_connection(|conn| {
            Ok(claim_work_available(
                conn,
                stale_cutoff,
                now,
                Some(kind_filter),
                false,
            )?)
        })? {
            return Ok(None);
        }
        self.with_claim_connection_if(approval, |conn| {
            self.require_lifecycle_writer_lease(conn, "claim queued message")?;
            let now = now_secs();
            let stale_cutoff = saturating_cutoff(now, claim_ttl_secs);
            if !claim_work_available(conn, stale_cutoff, now, Some(kind_filter), false)? {
                return Ok(None);
            }
            let tx = transaction_immediate(conn, "claim queued message")?;
            self.require_lifecycle_writer_lease(&tx, "claim queued message")?;
            reclaim_stale_tx(&tx, stale_cutoff)?;

            let now = now_secs();
            let row = tx
                .query_row(
                    r#"
                    SELECT id, kind, payload, retry_count, source_hash, created_at
                    FROM pending_messages
                    WHERE status = 'pending' AND next_attempt_at <= ?1 AND kind = ?2
                    ORDER BY next_attempt_at ASC, id ASC
                    LIMIT 1
                    "#,
                    params![now, kind_filter],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                        ))
                    },
                )
                .optional()?;

            let Some((id, kind, payload, retry_count_i64, source_hash, created_at)) = row else {
                tx.commit()?;
                return Ok(None);
            };
            let retry_count = u32::try_from(retry_count_i64)
                .map_err(|_| QueueError::RetryCountOverflow { id: id.clone() })?;
            let claim_token = format!("{worker_id}:{}", next_id("claim"));
            let updated = tx.execute(
                r#"
                UPDATE pending_messages
                SET status = 'claimed',
                    claim_token = ?2,
                    claimed_at = ?3,
                    heartbeat_at = ?3,
                    op_state = 'running'
                WHERE id = ?1 AND status = 'pending'
                "#,
                params![id, claim_token, now],
            )?;
            if updated == 0 {
                tx.commit()?;
                return Ok(None);
            }

            tx.commit()?;
            Ok(Some(ClaimedMessage {
                id,
                kind,
                payload,
                retry_count,
                claim_token,
                source_hash,
                created_at,
                claimed_at: now,
            }))
        })
        .map(Option::flatten)
    }
}

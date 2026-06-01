use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use super::config::scrub_sensitive_text;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);
const STARTUP_RECLAIM_STALE_SECS: i64 = 60;
const COMPLETION_METRICS_WINDOW_MINS: u64 = 10;
pub const LAST_ERROR_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("pending message not found: {0}")]
    MessageNotFound(String),
    #[error("retry count does not fit in u32 for message {id}")]
    RetryCountOverflow { id: String },
}

pub type Result<T> = std::result::Result<T, QueueError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedMessage {
    pub id: String,
    pub kind: String,
    pub payload: String,
    pub retry_count: u32,
    pub claim_token: String,
    pub source_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueStats {
    pub pending: u64,
    pub claimed: u64,
    pub failed: u64,
    pub oldest_pending_age_secs: Option<u64>,
    pub rate_per_min: f64,
    pub avg_processing_ms: Option<u64>,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    pub max_retries: u32,
}

/// Controls whether a processing failure is retried or dead-lettered immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueFailureDisposition {
    /// Retry with bounded queue backoff until `QueueConfig::max_retries` is exhausted.
    Retryable,
    /// Move directly to the failed/dead-letter state without another attempt.
    Terminal,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            base_delay_ms: 5_000,
            max_delay_ms: 3_600_000,
            max_retries: 10,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingMessageStore {
    db_path: PathBuf,
    config: QueueConfig,
}

impl PendingMessageStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_config(path, QueueConfig::default())
    }

    pub fn with_config(path: impl AsRef<Path>, config: QueueConfig) -> Result<Self> {
        let store = Self {
            db_path: path.as_ref().to_path_buf(),
            config,
        };
        store.reclaim_stale(STARTUP_RECLAIM_STALE_SECS)?;
        Ok(store)
    }

    pub fn enqueue(&self, kind: &str, payload: &str) -> Result<String> {
        let id = next_id("msg");
        let created_at = now_secs();
        let source_hash = hash_source(kind, payload);

        let conn = self.open_connection()?;
        conn.execute(
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

        Ok(id)
    }

    pub fn claim_next(
        &self,
        worker_id: &str,
        claim_ttl_secs: i64,
    ) -> Result<Option<ClaimedMessage>> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        reclaim_stale_tx(&tx, saturating_cutoff(now_secs(), claim_ttl_secs))?;

        let now = now_secs();
        let row = tx
            .query_row(
                r#"
                SELECT id, kind, payload, retry_count, source_hash
                FROM pending_messages
                WHERE status = 'pending' AND next_attempt_at <= ?1
                  AND kind != 'llm_task'
                ORDER BY next_attempt_at ASC, created_at ASC, id ASC
                LIMIT 1
                "#,
                [now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, kind, payload, retry_count_i64, source_hash)) = row else {
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
                heartbeat_at = ?3
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
        }))
    }

    pub fn claim_next_by_kind(
        &self,
        worker_id: &str,
        claim_ttl_secs: i64,
        kind_filter: &str,
    ) -> Result<Option<ClaimedMessage>> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        reclaim_stale_tx(&tx, saturating_cutoff(now_secs(), claim_ttl_secs))?;

        let now = now_secs();
        let row = tx
            .query_row(
                r#"
                SELECT id, kind, payload, retry_count, source_hash
                FROM pending_messages
                WHERE status = 'pending' AND next_attempt_at <= ?1 AND kind = ?2
                ORDER BY next_attempt_at ASC, created_at ASC, id ASC
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
                    ))
                },
            )
            .optional()?;

        let Some((id, kind, payload, retry_count_i64, source_hash)) = row else {
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
                heartbeat_at = ?3
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
        }))
    }

    pub fn confirm(&self, id: &str) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                r#"
                SELECT kind, created_at, claimed_at
                FROM pending_messages
                WHERE id = ?1
                "#,
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| QueueError::MessageNotFound(id.to_string()))?;
        let completed_at = now_millis();
        let processing_ms = row
            .2
            .map(|claimed_at| completed_at.saturating_sub(claimed_at.saturating_mul(1_000)));
        tx.execute(
            r#"
            INSERT INTO pending_message_completions (
                message_id,
                kind,
                created_at,
                claimed_at,
                completed_at,
                processing_ms
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(message_id) DO UPDATE SET
                kind = excluded.kind,
                created_at = excluded.created_at,
                claimed_at = excluded.claimed_at,
                completed_at = excluded.completed_at,
                processing_ms = excluded.processing_ms
            "#,
            params![
                id,
                row.0,
                row.1.saturating_mul(1_000),
                row.2.map(|s| s.saturating_mul(1_000)),
                completed_at,
                processing_ms
            ],
        )?;
        let updated = tx.execute("DELETE FROM pending_messages WHERE id = ?1", [id])?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_failed(&self, id: &str, error: &str) -> Result<()> {
        self.mark_failed_with_disposition(id, error, QueueFailureDisposition::Retryable)
    }

    /// Record a failed processing attempt with explicit retry/dead-letter policy.
    pub fn mark_failed_with_disposition(
        &self,
        id: &str,
        error: &str,
        disposition: QueueFailureDisposition,
    ) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let redacted_error = sanitize_last_error(error);
        let current_retry = tx
            .query_row(
                "SELECT retry_count FROM pending_messages WHERE id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| QueueError::MessageNotFound(id.to_string()))?;
        let next_retry = current_retry.saturating_add(1);
        let next_retry_u32 = u32::try_from(next_retry)
            .map_err(|_| QueueError::RetryCountOverflow { id: id.to_string() })?;
        let terminal = disposition == QueueFailureDisposition::Terminal
            || next_retry_u32 > self.config.max_retries;
        let backoff_ms = if terminal {
            0
        } else {
            self.compute_backoff_ms(next_retry_u32)
        };
        let next_attempt_at = if terminal {
            now_secs()
        } else {
            now_secs().saturating_add(div_ceil(backoff_ms, 1_000))
        };
        let status = if terminal { "failed" } else { "pending" };

        let updated = tx.execute(
            r#"
            UPDATE pending_messages
            SET retry_count = ?2,
                retry_backoff_ms = ?3,
                next_attempt_at = ?4,
                status = ?5,
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                last_error = ?6
            WHERE id = ?1
            "#,
            params![
                id,
                next_retry,
                backoff_ms,
                next_attempt_at,
                status,
                redacted_error
            ],
        )?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }

        tx.commit()?;
        Ok(())
    }

    /// Return dead-lettered embed-queue messages to pending for a targeted retry.
    ///
    /// LLM tasks share the same storage table but are not embed queue work, so
    /// they are intentionally left untouched.
    pub fn retry_failed_embed_messages(&self) -> Result<u64> {
        let now = now_secs();
        let conn = self.open_connection()?;
        let updated = conn.execute(
            r#"
            UPDATE pending_messages
            SET status = 'pending',
                retry_count = 0,
                retry_backoff_ms = 0,
                next_attempt_at = ?1,
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                last_error = NULL
            WHERE status = 'failed'
              AND kind != 'llm_task'
            "#,
            [now],
        )?;
        Ok(updated as u64)
    }

    /// Return a claimed message to pending without counting it as a failure.
    ///
    /// Used by LLM workers cancelled due to a config hot-reload so the task is
    /// retried with the new configuration rather than charged a retry.
    pub fn release_claim(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        let updated = conn.execute(
            r#"
            UPDATE pending_messages
            SET status = 'pending',
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL
            WHERE id = ?1 AND status = 'claimed'
            "#,
            [id],
        )?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn refresh_heartbeat(&self, id: &str, worker_id: &str) -> Result<()> {
        let claim_prefix = format!("{worker_id}:");
        let now = now_secs();
        let conn = self.open_connection()?;
        let updated = conn.execute(
            r#"
            UPDATE pending_messages
            SET heartbeat_at = ?2
            WHERE id = ?1
              AND status = 'claimed'
              AND claim_token LIKE ?3
            "#,
            params![id, now, format!("{claim_prefix}%")],
        )?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn reclaim_stale(&self, stale_secs: i64) -> Result<u64> {
        let conn = self.open_connection()?;
        let reclaimed = reclaim_stale_conn(&conn, saturating_cutoff(now_secs(), stale_secs))?;
        Ok(reclaimed)
    }

    pub fn stats(&self) -> Result<QueueStats> {
        let conn = self.open_connection()?;
        compute_queue_stats(&conn)
    }

    fn open_connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    fn compute_backoff_ms(&self, retry_count: u32) -> i64 {
        let shift = retry_count.saturating_sub(1).min(30);
        let multiplier = 1_i64 << shift;
        self.config
            .base_delay_ms
            .saturating_mul(multiplier)
            .min(self.config.max_delay_ms)
    }
}

/// Open a WAL-mode-compatible read-only connection and return queue statistics.
///
/// Unlike `PendingMessageStore::new(...).stats()`, this skips `reclaim_stale` and
/// opens with `SQLITE_OPEN_READ_ONLY` so WAL readers never block daemon write transactions.
/// Used by `mempal status` to avoid the ~5s lock contention described in issue #182.
pub fn queue_stats_readonly(path: &Path) -> Result<QueueStats> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    compute_queue_stats(&conn)
}

fn compute_queue_stats(conn: &Connection) -> Result<QueueStats> {
    let mut pending = 0i64;
    let mut claimed = 0i64;
    let mut failed = 0i64;

    let mut statement = conn.prepare(
        r#"
        SELECT status, COUNT(*)
        FROM pending_messages
        GROUP BY status
        "#,
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (status, count) = row?;
        match status.as_str() {
            "pending" => pending = count,
            "claimed" => claimed = count,
            "failed" => failed = count,
            _ => {}
        }
    }

    let oldest_pending_created_at = conn
        .query_row(
            "SELECT MIN(created_at) FROM pending_messages WHERE status = 'pending'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    let oldest_pending_age_secs = oldest_pending_created_at
        .map(|created_at| i64_to_u64(now_secs().saturating_sub(created_at)));

    let (rate_per_min, avg_processing_ms) = if table_exists(conn, "pending_message_completions")? {
        let window_cutoff_ms = now_millis()
            .saturating_sub((COMPLETION_METRICS_WINDOW_MINS as i64).saturating_mul(60_000));
        let (completed_count, avg_processing_ms) = conn.query_row(
            r#"
            SELECT COUNT(*), AVG(processing_ms)
            FROM pending_message_completions
            WHERE completed_at >= ?1
            "#,
            [window_cutoff_ms],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<f64>>(1)?)),
        )?;
        (
            (completed_count as f64) / (COMPLETION_METRICS_WINDOW_MINS as f64),
            avg_processing_ms.map(|value| value.round() as u64),
        )
    } else {
        (0.0, None)
    };
    let pending_u64 = i64_to_u64(pending);
    let eta_secs = if rate_per_min > 0.0 {
        Some(((pending_u64 as f64 / rate_per_min) * 60.0).ceil() as u64)
    } else {
        None
    };

    Ok(QueueStats {
        pending: pending_u64,
        claimed: i64_to_u64(claimed),
        failed: i64_to_u64(failed),
        oldest_pending_age_secs,
        rate_per_min,
        avg_processing_ms,
        eta_secs,
    })
}

pub fn failure_headline_count(live_fail_count: u64, queue_stats: &QueueStats) -> u64 {
    live_fail_count.max(queue_stats.failed)
}

fn reclaim_stale_tx(conn: &rusqlite::Transaction<'_>, stale_cutoff: i64) -> rusqlite::Result<u64> {
    let updated = conn.execute(
        r#"
        UPDATE pending_messages
        SET status = 'pending',
            claim_token = NULL,
            claimed_at = NULL,
            heartbeat_at = NULL
        WHERE status = 'claimed'
          AND (heartbeat_at IS NULL OR heartbeat_at < ?1)
        "#,
        [stale_cutoff],
    )?;
    Ok(updated as u64)
}

fn reclaim_stale_conn(conn: &Connection, stale_cutoff: i64) -> rusqlite::Result<u64> {
    let updated = conn.execute(
        r#"
        UPDATE pending_messages
        SET status = 'pending',
            claim_token = NULL,
            claimed_at = NULL,
            heartbeat_at = NULL
        WHERE status = 'claimed'
          AND (heartbeat_at IS NULL OR heartbeat_at < ?1)
        "#,
        [stale_cutoff],
    )?;
    Ok(updated as u64)
}

fn hash_source(kind: &str, payload: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(&[0]);
    hasher.update(payload.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn now_secs() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

fn next_id(prefix: &str) -> String {
    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis(),
        Err(_) => 0,
    };
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now_ms:016x}-{counter:016x}")
}

fn saturating_cutoff(now: i64, window_secs: i64) -> i64 {
    now.saturating_sub(window_secs.max(0))
}

fn div_ceil(lhs: i64, rhs: i64) -> i64 {
    if lhs <= 0 {
        return 0;
    }
    ((lhs - 1) / rhs) + 1
}

fn i64_to_u64(value: i64) -> u64 {
    if value <= 0 { 0 } else { value as u64 }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count > 0)
}

fn sanitize_last_error(error: &str) -> String {
    truncate_to_byte_limit(scrub_sensitive_text(error), LAST_ERROR_MAX_BYTES)
}

fn truncate_to_byte_limit(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }

    let mut truncate_at = max_bytes;
    while truncate_at > 0 && !value.is_char_boundary(truncate_at) {
        truncate_at -= 1;
    }
    value.truncate(truncate_at);
    value
}

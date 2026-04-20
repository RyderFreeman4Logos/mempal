use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueStats {
    pub pending: u64,
    pub claimed: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    pub max_retries: u32,
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
        Ok(Self {
            db_path: path.as_ref().to_path_buf(),
            config,
        })
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

    pub fn confirm(&self, id: &str) -> Result<()> {
        let conn = self.open_connection()?;
        let deleted = conn.execute("DELETE FROM pending_messages WHERE id = ?1", [id])?;
        if deleted == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn mark_failed(&self, id: &str, error: &str) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let backoff_ms = self.compute_backoff_ms(next_retry_u32);
        let next_attempt_at = now_secs().saturating_add(div_ceil(backoff_ms, 1_000));
        let terminal = next_retry_u32 > self.config.max_retries;
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
            params![id, next_retry, backoff_ms, next_attempt_at, status, error],
        )?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }

        tx.commit()?;
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
        let (pending, claimed, failed): (i64, i64, i64) = conn.query_row(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'claimed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0)
            FROM pending_messages
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        Ok(QueueStats {
            pending: i64_to_u64(pending),
            claimed: i64_to_u64(claimed),
            failed: i64_to_u64(failed),
        })
    }

    fn open_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
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

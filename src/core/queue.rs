use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;
use tokio::sync::Semaphore;

use super::config::scrub_sensitive_text;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);
const STARTUP_RECLAIM_STALE_SECS: i64 = 60;
const COMPLETION_METRICS_WINDOW_MINS: u64 = 10;
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub const LAST_ERROR_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("pending message not found: {0}")]
    MessageNotFound(String),
    #[error("pending message claim lost: {0}")]
    ClaimLost(String),
    #[error("retry count does not fit in u32 for message {id}")]
    RetryCountOverflow { id: String },
    #[error("blocking queue task failed: {0}")]
    BlockingTaskFailed(String),
    #[error("unsupported model task kind for auto-requeue: {0}")]
    UnsupportedModelTaskKind(String),
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
    pub created_at: i64,
    pub claimed_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOperationRecord {
    pub id: String,
    pub kind: String,
    pub created_at: i64,
    pub claimed_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub op_state: String,
    pub result_drawer_id: Option<String>,
    pub rejected_reason: Option<String>,
    pub failure_detail: Option<String>,
    pub result_json: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueStats {
    pub pending: u64,
    pub claimed: u64,
    pub failed: u64,
    pub failed_retryable: u64,
    pub failed_terminal: u64,
    pub failed_retryable_embed: u64,
    pub failed_retryable_llm: u64,
    pub last_auto_requeue_at_unix_ms: Option<u64>,
    pub oldest_pending_age_secs: Option<u64>,
    pub rate_per_min: f64,
    pub avg_processing_ms: Option<u64>,
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueConfig {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
    /// Legacy compatibility knob. Terminal dead-lettering is controlled by
    /// `QueueFailureDisposition::Terminal`, not by retryable attempt count.
    pub max_retries: u32,
}

/// Controls whether a processing failure is retried or dead-lettered immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueFailureDisposition {
    /// Retry with queue backoff without converting the item to a terminal dead-letter.
    Retryable,
    /// Retry after an explicit producer-provided delay, such as a model router
    /// cooldown hint. This keeps retry intent durable without holding a worker.
    RetryableAfter { delay_ms: i64 },
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

/// Bounded async facade for [`PendingMessageStore`].
///
/// The queue store intentionally remains a synchronous SQLite API for CLI and
/// test callers. Daemon and MCP async tasks must use this facade so claim,
/// confirm, retry, release, heartbeat, and stats calls run on Tokio's blocking
/// pool instead of occupying runtime worker threads.
#[derive(Debug, Clone)]
pub struct AsyncPendingMessageStore {
    inner: PendingMessageStore,
    permits: Arc<Semaphore>,
    #[cfg(any(test, feature = "db-test-seam"))]
    blocking_delay: Option<Duration>,
}

impl AsyncPendingMessageStore {
    const DEFAULT_BLOCKING_PERMITS: usize = 4;

    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        PendingMessageStore::new(path).map(Self::from_store)
    }

    pub fn new_without_reclaim(path: impl AsRef<Path>) -> Self {
        Self::from_store(PendingMessageStore::new_without_reclaim(path))
    }

    pub fn from_store(inner: PendingMessageStore) -> Self {
        Self {
            inner,
            permits: Arc::new(Semaphore::new(Self::DEFAULT_BLOCKING_PERMITS)),
            #[cfg(any(test, feature = "db-test-seam"))]
            blocking_delay: None,
        }
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_blocking_delay(mut self, delay: Duration) -> Self {
        self.blocking_delay = Some(delay);
        self
    }

    pub async fn enqueue(&self, kind: String, payload: String) -> Result<String> {
        self.run(move |store| store.enqueue(&kind, &payload)).await
    }

    /// Enqueue a capture once for a deterministic `kind + payload` identity.
    ///
    /// This is for retry/fallback paths where two writers may race to persist the
    /// same already-captured event. Normal queue callers should use `enqueue` so
    /// repeated logical events remain distinct messages.
    pub async fn enqueue_idempotent(&self, kind: String, payload: String) -> Result<String> {
        self.run(move |store| store.enqueue_idempotent(&kind, &payload))
            .await
    }

    /// Enqueue a capture once for an explicit producer-owned idempotency key.
    ///
    /// Use this when the producer can name one delivery attempt across racing
    /// writers. Do not use payload-derived idempotency for ordinary captures,
    /// because repeated logical events with the same payload must remain
    /// distinct queue messages.
    pub async fn enqueue_idempotent_with_key(
        &self,
        kind: String,
        payload: String,
        idempotency_key: String,
    ) -> Result<String> {
        self.run(move |store| store.enqueue_idempotent_with_key(&kind, &payload, &idempotency_key))
            .await
    }

    /// Enqueue without waiting on SQLite write locks.
    ///
    /// Use this when the caller owns a stricter fallback deadline than the
    /// queue's normal busy timeout, such as daemon hook IPC ACK handling.
    pub async fn enqueue_fail_fast(&self, kind: String, payload: String) -> Result<String> {
        self.run(move |store| store.enqueue_fail_fast(&kind, &payload))
            .await
    }

    /// Idempotent variant of `enqueue_fail_fast`.
    pub async fn enqueue_idempotent_fail_fast(
        &self,
        kind: String,
        payload: String,
    ) -> Result<String> {
        self.run(move |store| store.enqueue_idempotent_fail_fast(&kind, &payload))
            .await
    }

    /// Idempotent-key variant of `enqueue_fail_fast`.
    pub async fn enqueue_idempotent_with_key_fail_fast(
        &self,
        kind: String,
        payload: String,
        idempotency_key: String,
    ) -> Result<String> {
        self.run(move |store| {
            store.enqueue_idempotent_with_key_fail_fast(&kind, &payload, &idempotency_key)
        })
        .await
    }

    pub async fn claim_next(
        &self,
        worker_id: String,
        claim_ttl_secs: i64,
    ) -> Result<Option<ClaimedMessage>> {
        self.run(move |store| store.claim_next(&worker_id, claim_ttl_secs))
            .await
    }

    pub async fn claim_next_by_kind(
        &self,
        worker_id: String,
        claim_ttl_secs: i64,
        kind_filter: String,
    ) -> Result<Option<ClaimedMessage>> {
        self.run(move |store| store.claim_next_by_kind(&worker_id, claim_ttl_secs, &kind_filter))
            .await
    }

    pub async fn confirm(&self, claim: ClaimedMessage) -> Result<()> {
        self.run(move |store| store.confirm(&claim)).await
    }

    pub async fn complete_operation(
        &self,
        claim: ClaimedMessage,
        op_state: String,
        result_drawer_id: Option<String>,
        rejected_reason: Option<String>,
        failure_detail: Option<String>,
        result_json: Option<String>,
    ) -> Result<()> {
        self.run(move |store| {
            store.complete_operation(
                &claim,
                &op_state,
                result_drawer_id.as_deref(),
                rejected_reason.as_deref(),
                failure_detail.as_deref(),
                result_json.as_deref(),
            )
        })
        .await
    }

    pub async fn operation_status(&self, id: String) -> Result<Option<PendingOperationRecord>> {
        self.run(move |store| store.operation_status(&id)).await
    }

    pub async fn mark_failed(&self, claim: ClaimedMessage, error: String) -> Result<()> {
        self.run(move |store| store.mark_failed(&claim, &error))
            .await
    }

    pub async fn mark_failed_with_disposition(
        &self,
        claim: ClaimedMessage,
        error: String,
        disposition: QueueFailureDisposition,
    ) -> Result<()> {
        self.run(move |store| store.mark_failed_with_disposition(&claim, &error, disposition))
            .await
    }

    pub async fn auto_requeue_failed_model_tasks(&self, model_kind: String) -> Result<u64> {
        self.run(move |store| store.auto_requeue_failed_model_tasks(&model_kind))
            .await
    }

    pub async fn release_claim(&self, claim: ClaimedMessage) -> Result<()> {
        self.run(move |store| store.release_claim(&claim)).await
    }

    pub async fn refresh_heartbeat(&self, id: String, worker_id: String) -> Result<()> {
        self.run(move |store| store.refresh_heartbeat(&id, &worker_id))
            .await
    }

    pub async fn reclaim_stale(&self, stale_secs: i64) -> Result<u64> {
        self.run(move |store| store.reclaim_stale(stale_secs)).await
    }

    pub async fn stats(&self) -> Result<QueueStats> {
        self.run(|store| store.stats()).await
    }

    async fn run<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(PendingMessageStore) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let permit =
            self.permits.clone().acquire_owned().await.map_err(|_| {
                QueueError::BlockingTaskFailed("queue semaphore closed".to_string())
            })?;
        let store = self.inner.clone();
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.blocking_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let join = tokio::task::spawn_blocking(move || {
            let permit = permit;
            tracing::dispatcher::with_default(&dispatch, || {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                let out = f(store);
                drop(permit);
                out
            })
        })
        .await;
        match join {
            Ok(out) => out,
            Err(error) => Err(QueueError::BlockingTaskFailed(error.to_string())),
        }
    }
}

impl PendingMessageStore {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_config(path, QueueConfig::default())
    }

    pub fn new_without_reclaim(path: impl AsRef<Path>) -> Self {
        Self {
            db_path: path.as_ref().to_path_buf(),
            config: QueueConfig::default(),
        }
    }

    pub fn with_config(path: impl AsRef<Path>, config: QueueConfig) -> Result<Self> {
        let store = Self {
            db_path: path.as_ref().to_path_buf(),
            config,
        };
        store.reclaim_stale(STARTUP_RECLAIM_STALE_SECS)?;
        Ok(store)
    }

    pub fn idempotent_message_id(kind: &str, idempotency_key: &str) -> String {
        idempotent_key_message_id(kind, idempotency_key)
    }

    pub fn enqueue(&self, kind: &str, payload: &str) -> Result<String> {
        self.enqueue_with_busy_timeout(kind, payload, None, EnqueueIdentity::Fresh)
    }

    /// Enqueue a capture once for a deterministic `kind + payload` identity.
    pub fn enqueue_idempotent(&self, kind: &str, payload: &str) -> Result<String> {
        self.enqueue_with_busy_timeout(kind, payload, None, EnqueueIdentity::SourceHash)
    }

    /// Enqueue a capture once for a producer-owned idempotency key.
    pub fn enqueue_idempotent_with_key(
        &self,
        kind: &str,
        payload: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        self.enqueue_with_busy_timeout(
            kind,
            payload,
            None,
            EnqueueIdentity::ExplicitKey(idempotency_key.to_string()),
        )
    }

    /// Enqueue without waiting for SQLite busy locks.
    pub fn enqueue_fail_fast(&self, kind: &str, payload: &str) -> Result<String> {
        self.enqueue_with_busy_timeout(kind, payload, Some(Duration::ZERO), EnqueueIdentity::Fresh)
    }

    /// Idempotent variant of `enqueue_fail_fast`.
    pub fn enqueue_idempotent_fail_fast(&self, kind: &str, payload: &str) -> Result<String> {
        self.enqueue_with_busy_timeout(
            kind,
            payload,
            Some(Duration::ZERO),
            EnqueueIdentity::SourceHash,
        )
    }

    /// Idempotent-key variant of `enqueue_fail_fast`.
    pub fn enqueue_idempotent_with_key_fail_fast(
        &self,
        kind: &str,
        payload: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        self.enqueue_with_busy_timeout(
            kind,
            payload,
            Some(Duration::ZERO),
            EnqueueIdentity::ExplicitKey(idempotency_key.to_string()),
        )
    }

    fn enqueue_with_busy_timeout(
        &self,
        kind: &str,
        payload: &str,
        busy_timeout: Option<Duration>,
        identity: EnqueueIdentity,
    ) -> Result<String> {
        let created_at = now_secs();
        let source_hash = hash_source(kind, payload);
        let id = match &identity {
            EnqueueIdentity::Fresh => next_id("msg"),
            EnqueueIdentity::SourceHash => idempotent_source_message_id(&source_hash),
            EnqueueIdentity::ExplicitKey(idempotency_key) => {
                idempotent_key_message_id(kind, idempotency_key)
            }
        };

        let conn = self.open_connection_with_busy_timeout(busy_timeout)?;
        match identity {
            EnqueueIdentity::Fresh => {
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
            }
            EnqueueIdentity::SourceHash | EnqueueIdentity::ExplicitKey(_) => {
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
            }
        }

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
                SELECT id, kind, payload, retry_count, source_hash, created_at
                FROM pending_messages
                WHERE status = 'pending' AND next_attempt_at <= ?1
                  AND kind != 'llm_task'
                ORDER BY next_attempt_at ASC, id ASC
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
    }

    pub fn confirm(&self, claim: &ClaimedMessage) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        confirm_in_tx(&tx, claim, "completed")?;
        tx.commit()?;
        Ok(())
    }

    pub fn complete_operation(
        &self,
        claim: &ClaimedMessage,
        op_state: &str,
        result_drawer_id: Option<&str>,
        rejected_reason: Option<&str>,
        failure_detail: Option<&str>,
        result_json: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            r#"
            UPDATE pending_messages
            SET op_state = ?2,
                result_drawer_id = ?3,
                rejected_reason = ?4,
                failure_detail = ?5,
                result_json = ?6
            WHERE id = ?1 AND status = 'claimed' AND claim_token = ?7
            "#,
            params![
                claim.id,
                op_state,
                result_drawer_id,
                rejected_reason,
                failure_detail,
                result_json,
                claim.claim_token
            ],
        )?;
        if updated == 0 {
            return Err(claim_miss_error(&tx, &claim.id)?);
        }
        confirm_in_tx(&tx, claim, op_state)?;
        tx.commit()?;
        Ok(())
    }

    pub fn operation_status(&self, id: &str) -> Result<Option<PendingOperationRecord>> {
        let conn = self.open_connection()?;
        if let Some(record) = operation_status_from_pending(&conn, id)? {
            return Ok(Some(record));
        }
        operation_status_from_completion(&conn, id)
    }

    pub fn mark_failed(&self, claim: &ClaimedMessage, error: &str) -> Result<()> {
        self.mark_failed_with_disposition(claim, error, QueueFailureDisposition::Retryable)
    }

    /// Record a failed processing attempt with explicit retry/dead-letter policy.
    pub fn mark_failed_with_disposition(
        &self,
        claim: &ClaimedMessage,
        error: &str,
        disposition: QueueFailureDisposition,
    ) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let redacted_error = sanitize_last_error(error);
        let current_retry = match tx
            .query_row(
                r#"
                SELECT retry_count
                FROM pending_messages
                WHERE id = ?1 AND status = 'claimed' AND claim_token = ?2
                "#,
                params![claim.id, claim.claim_token],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            Some(retry_count) => retry_count,
            None => return Err(claim_miss_error(&tx, &claim.id)?),
        };
        let next_retry = current_retry.saturating_add(1);
        let next_retry_u32 =
            u32::try_from(next_retry).map_err(|_| QueueError::RetryCountOverflow {
                id: claim.id.clone(),
            })?;
        let terminal = matches!(disposition, QueueFailureDisposition::Terminal);
        let backoff_ms = match disposition {
            QueueFailureDisposition::Terminal => 0,
            QueueFailureDisposition::Retryable => self.compute_backoff_ms(next_retry_u32),
            QueueFailureDisposition::RetryableAfter { delay_ms } => delay_ms.max(0),
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
                op_state = CASE WHEN ?5 = 'failed' THEN 'failed' ELSE 'queued' END,
                failure_class = CASE WHEN ?5 = 'failed' THEN 'terminal' ELSE NULL END,
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                last_error = ?6
            WHERE id = ?1 AND status = 'claimed' AND claim_token = ?7
            "#,
            params![
                claim.id,
                next_retry,
                backoff_ms,
                next_attempt_at,
                status,
                redacted_error,
                claim.claim_token
            ],
        )?;
        if updated == 0 {
            return Err(claim_miss_error(&tx, &claim.id)?);
        }

        tx.commit()?;
        Ok(())
    }

    /// Mark a model-backed task as failed but retryable once the corresponding endpoint recovers.
    pub fn mark_model_task_failed_retryable(&self, id: &str, error: &str) -> Result<()> {
        let now = now_secs();
        let redacted_error = sanitize_last_error(error);
        let conn = self.open_connection()?;
        let updated = conn.execute(
            r#"
            UPDATE pending_messages
            SET status = 'failed',
                retry_count = 0,
                retry_backoff_ms = 0,
                next_attempt_at = ?2,
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                last_error = ?3,
                op_state = 'failed',
                failure_class = 'retryable_model'
            WHERE id = ?1
            "#,
            params![id, now, redacted_error],
        )?;
        if updated == 0 {
            return Err(QueueError::MessageNotFound(id.to_string()));
        }
        Ok(())
    }

    /// Requeue retryable failed model tasks for the recovered model family.
    pub fn auto_requeue_failed_model_tasks(&self, model_kind: &str) -> Result<u64> {
        let now = now_secs();
        let conn = self.open_connection()?;
        let updated = match model_kind {
            "embedding" => requeue_failed_model_tasks(&conn, now, "embedding")?,
            "llm" => requeue_failed_model_tasks(&conn, now, "llm")?,
            other => return Err(QueueError::UnsupportedModelTaskKind(other.to_string())),
        };
        if updated > 0 {
            let now_ms = now_millis().to_string();
            conn.execute(
                r#"
                INSERT INTO fork_ext_meta (key, value)
                VALUES ('queue.auto_requeue.last_at_unix_ms', ?1)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                "#,
                [now_ms],
            )?;
        }
        Ok(updated)
    }

    /// Return dead-lettered embed-queue messages to pending for a targeted retry.
    ///
    /// Retried messages keep their original FIFO position among immediately
    /// available work instead of moving behind already-pending items.
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
                next_attempt_at = MIN(created_at, ?1),
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                last_error = NULL,
                op_state = 'queued',
                result_drawer_id = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE result_drawer_id
                END,
                rejected_reason = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE rejected_reason
                END,
                failure_detail = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE failure_detail
                END,
                result_json = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE result_json
                END
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
    pub fn release_claim(&self, claim: &ClaimedMessage) -> Result<()> {
        let mut conn = self.open_connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = tx.execute(
            r#"
            UPDATE pending_messages
            SET status = 'pending',
                claim_token = NULL,
                claimed_at = NULL,
                heartbeat_at = NULL,
                op_state = 'queued',
                result_drawer_id = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE result_drawer_id
                END,
                rejected_reason = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE rejected_reason
                END,
                failure_detail = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE failure_detail
                END,
                result_json = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE result_json
                END
            WHERE id = ?1 AND status = 'claimed' AND claim_token = ?2
            "#,
            params![claim.id, claim.claim_token],
        )?;
        if updated == 0 {
            return Err(claim_miss_error(&tx, &claim.id)?);
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
        compute_queue_stats(&conn)
    }

    fn open_connection(&self) -> Result<Connection> {
        self.open_connection_with_busy_timeout(None)
    }

    fn open_connection_with_busy_timeout(
        &self,
        busy_timeout: Option<Duration>,
    ) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.busy_timeout(busy_timeout.unwrap_or(DEFAULT_BUSY_TIMEOUT))?;
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

fn requeue_failed_model_tasks(conn: &Connection, now: i64, model_kind: &str) -> Result<u64> {
    let kind_predicate = match model_kind {
        "embedding" => "kind != 'llm_task'",
        "llm" => "kind = 'llm_task'",
        other => return Err(QueueError::UnsupportedModelTaskKind(other.to_string())),
    };
    let sql = format!(
        r#"
        UPDATE pending_messages
        SET status = 'pending',
            retry_count = 0,
            retry_backoff_ms = 0,
            next_attempt_at = MIN(created_at, ?1),
            claim_token = NULL,
            claimed_at = NULL,
            heartbeat_at = NULL,
            last_error = NULL,
            op_state = 'queued',
            failure_class = NULL,
            result_drawer_id = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE result_drawer_id
            END,
            rejected_reason = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE rejected_reason
            END,
            failure_detail = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE failure_detail
            END,
            result_json = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE result_json
            END
        WHERE status = 'failed'
          AND failure_class = 'retryable_model'
          AND {kind_predicate}
        "#
    );
    let updated = conn.execute(&sql, [now])?;
    Ok(updated as u64)
}

fn confirm_in_tx(
    tx: &rusqlite::Transaction<'_>,
    claim: &ClaimedMessage,
    op_state: &str,
) -> Result<()> {
    let row = match tx
        .query_row(
            r#"
            SELECT kind,
                   created_at,
                   claimed_at,
                   result_drawer_id,
                   rejected_reason,
                   failure_detail,
                   result_json
            FROM pending_messages
            WHERE id = ?1 AND status = 'claimed' AND claim_token = ?2
            "#,
            params![claim.id, claim.claim_token],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?
    {
        Some(row) => row,
        None => return Err(claim_miss_error(tx, &claim.id)?),
    };
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
            processing_ms,
            result_drawer_id,
            op_state,
            rejected_reason,
            failure_detail,
            result_json
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ON CONFLICT(message_id) DO UPDATE SET
            kind = excluded.kind,
            created_at = excluded.created_at,
            claimed_at = excluded.claimed_at,
            completed_at = excluded.completed_at,
            processing_ms = excluded.processing_ms,
            result_drawer_id = excluded.result_drawer_id,
            op_state = excluded.op_state,
            rejected_reason = excluded.rejected_reason,
            failure_detail = excluded.failure_detail,
            result_json = excluded.result_json
        "#,
        params![
            claim.id,
            row.0,
            row.1.saturating_mul(1_000),
            row.2.map(|s| s.saturating_mul(1_000)),
            completed_at,
            processing_ms,
            row.3,
            op_state,
            row.4,
            row.5,
            row.6
        ],
    )?;
    let updated = tx.execute(
        r#"
        DELETE FROM pending_messages
        WHERE id = ?1 AND status = 'claimed' AND claim_token = ?2
        "#,
        params![claim.id, claim.claim_token],
    )?;
    if updated == 0 {
        return Err(claim_miss_error(tx, &claim.id)?);
    }
    Ok(())
}

fn claim_miss_error(conn: &Connection, id: &str) -> Result<QueueError> {
    let exists = conn
        .query_row("SELECT 1 FROM pending_messages WHERE id = ?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    if exists {
        Ok(QueueError::ClaimLost(id.to_string()))
    } else {
        Ok(QueueError::MessageNotFound(id.to_string()))
    }
}

fn operation_status_from_pending(
    conn: &Connection,
    id: &str,
) -> Result<Option<PendingOperationRecord>> {
    conn.query_row(
        r#"
            SELECT id,
                   kind,
                   created_at,
                   claimed_at,
                   op_state,
                   result_drawer_id,
                   rejected_reason,
                   failure_detail,
                   result_json
            FROM pending_messages
            WHERE id = ?1
            "#,
        [id],
        |row| {
            Ok(PendingOperationRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_at: row.get(2)?,
                claimed_at: row.get(3)?,
                completed_at: None,
                op_state: row.get(4)?,
                result_drawer_id: row.get(5)?,
                rejected_reason: row.get(6)?,
                failure_detail: row.get(7)?,
                result_json: row.get(8)?,
            })
        },
    )
    .optional()
    .map_err(QueueError::from)
}

fn operation_status_from_completion(
    conn: &Connection,
    id: &str,
) -> Result<Option<PendingOperationRecord>> {
    conn.query_row(
        r#"
            SELECT message_id,
                   kind,
                   created_at,
                   claimed_at,
                   completed_at,
                   op_state,
                   result_drawer_id,
                   rejected_reason,
                   failure_detail,
                   result_json
            FROM pending_message_completions
            WHERE message_id = ?1
            "#,
        [id],
        |row| {
            Ok(PendingOperationRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_at: row.get(2)?,
                claimed_at: row.get(3)?,
                completed_at: row.get(4)?,
                op_state: row.get(5)?,
                result_drawer_id: row.get(6)?,
                rejected_reason: row.get(7)?,
                failure_detail: row.get(8)?,
                result_json: row.get(9)?,
            })
        },
    )
    .optional()
    .map_err(QueueError::from)
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

    let has_failure_class = column_exists(conn, "pending_messages", "failure_class")?;
    let (failed_retryable, failed_terminal, failed_retryable_embed, failed_retryable_llm) =
        if has_failure_class {
            let failed_retryable = conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM pending_messages
                WHERE status = 'failed'
                  AND failure_class = 'retryable_model'
                "#,
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let failed_retryable_embed = conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM pending_messages
                WHERE status = 'failed'
                  AND failure_class = 'retryable_model'
                  AND kind != 'llm_task'
                "#,
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let failed_retryable_llm = conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM pending_messages
                WHERE status = 'failed'
                  AND failure_class = 'retryable_model'
                  AND kind = 'llm_task'
                "#,
                [],
                |row| row.get::<_, i64>(0),
            )?;
            (
                failed_retryable,
                failed.saturating_sub(failed_retryable),
                failed_retryable_embed,
                failed_retryable_llm,
            )
        } else {
            (0, failed, 0, 0)
        };
    let last_auto_requeue_at_unix_ms = if table_exists(conn, "fork_ext_meta")? {
        conn.query_row(
            r#"
            SELECT value
            FROM fork_ext_meta
            WHERE key = 'queue.auto_requeue.last_at_unix_ms'
            "#,
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    } else {
        None
    };

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
        failed_retryable: i64_to_u64(failed_retryable),
        failed_terminal: i64_to_u64(failed_terminal),
        failed_retryable_embed: i64_to_u64(failed_retryable_embed),
        failed_retryable_llm: i64_to_u64(failed_retryable_llm),
        last_auto_requeue_at_unix_ms,
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
                heartbeat_at = NULL,
                op_state = 'queued',
                result_drawer_id = CASE
                    WHEN kind = 'ingest_async' THEN NULL
                    ELSE result_drawer_id
                END,
            rejected_reason = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE rejected_reason
            END,
            failure_detail = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE failure_detail
            END,
            result_json = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE result_json
            END
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
            heartbeat_at = NULL,
            op_state = 'queued',
            result_drawer_id = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE result_drawer_id
            END,
            rejected_reason = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE rejected_reason
            END,
            failure_detail = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE failure_detail
            END,
            result_json = CASE
                WHEN kind = 'ingest_async' THEN NULL
                ELSE result_json
            END
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

fn idempotent_source_message_id(source_hash: &str) -> String {
    format!("msg-dedup-{source_hash}")
}

fn idempotent_key_message_id(kind: &str, idempotency_key: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(b"mempal queue idempotency key v1");
    hasher.update(&[0]);
    hasher.update(kind.as_bytes());
    hasher.update(&[0]);
    hasher.update(idempotency_key.as_bytes());
    format!("msg-dedup-{}", hasher.finalize().to_hex())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EnqueueIdentity {
    Fresh,
    SourceHash,
    ExplicitKey(String),
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
    let pid = std::process::id();
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{now_ms:016x}-{pid:08x}-{counter:016x}")
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

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let escaped_table = table.replace('"', "\"\"");
    let mut statement = conn.prepare(&format!("PRAGMA table_info(\"{escaped_table}\")"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn pending_message_ids_include_process_component() {
        let id = next_id("msg");
        let pid_component = format!("{:08x}", std::process::id());

        assert!(
            id.contains(&pid_component),
            "pending message id {id} should include process component {pid_component}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_claim_confirm_run_off_runtime() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let sync_store = PendingMessageStore::new(&db_path).expect("open queue");
        sync_store
            .enqueue("hook_user_prompt", "{\"event\":\"UserPromptSubmit\"}")
            .expect("enqueue");
        let store = AsyncPendingMessageStore::from_store(sync_store)
            .with_blocking_delay(Duration::from_millis(300));

        let ticks = Arc::new(AtomicU64::new(0));
        let ticks_bg = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks_bg.fetch_add(1, Ordering::SeqCst);
            }
        });

        let claimed = store
            .claim_next("worker-a".to_string(), 60)
            .await
            .expect("claim")
            .expect("claimed message");
        store.confirm(claimed).await.expect("confirm off runtime");
        ticker.abort();

        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed >= 5,
            "ticker advanced {observed} times while delayed claim/confirm ran; \
             queue SQLite must run off the Tokio worker"
        );
    }
}

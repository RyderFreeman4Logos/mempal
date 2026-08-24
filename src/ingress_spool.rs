use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::queue::{AsyncPendingMessageStore, QueueError};
use crate::hook_ipc::HookIpcEnqueueRequest;

pub(crate) const INGRESS_SPOOL_DIR: &str = "ingress-spool";
const DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub(crate) enum IngressSpoolError {
    #[error("ingress spool I/O failed")]
    Io(#[source] io::Error),
    #[error("ingress spool record encoding failed")]
    Encode(#[source] serde_json::Error),
    #[error("ingress spool record decoding failed: {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "ingress spool is full: record_bytes={record_bytes} active_bytes={active_bytes} limit_bytes={limit_bytes}"
    )]
    Full {
        record_bytes: u64,
        active_bytes: u64,
        limit_bytes: u64,
    },
    #[error("ingress spool operation key conflicts with an existing record")]
    Conflict,
    #[error(transparent)]
    Queue(#[from] QueueError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SpoolRecord {
    kind: String,
    payload: String,
    idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    Appended,
    AlreadyPresent,
}

#[derive(Debug, Clone)]
struct StoredRecord {
    path: PathBuf,
    request: HookIpcEnqueueRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct IngressSpool {
    dir: PathBuf,
    max_bytes: u64,
    append_lock: Arc<Mutex<()>>,
}

impl IngressSpool {
    pub(crate) fn new(mempal_home: impl AsRef<Path>) -> Self {
        Self::with_max_bytes(mempal_home, DEFAULT_MAX_BYTES)
    }

    pub(crate) fn with_max_bytes(mempal_home: impl AsRef<Path>, max_bytes: u64) -> Self {
        Self {
            dir: mempal_home.as_ref().join(INGRESS_SPOOL_DIR),
            max_bytes,
            append_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn append(
        &self,
        request: &HookIpcEnqueueRequest,
    ) -> Result<AppendOutcome, IngressSpoolError> {
        let _guard = self
            .append_lock
            .lock()
            .map_err(|_| IngressSpoolError::Io(io::Error::other("spool mutex poisoned")))?;
        fs::create_dir_all(&self.dir).map_err(IngressSpoolError::Io)?;

        let path = self.record_path(request);
        let bytes = serde_json::to_vec(&SpoolRecord {
            kind: request.kind.clone(),
            payload: request.payload.clone(),
            idempotency_key: request.idempotency_key.clone(),
        })
        .map_err(IngressSpoolError::Encode)?;

        if path.exists() {
            let existing = read_record(&path)?;
            return if existing == *request {
                Ok(AppendOutcome::AlreadyPresent)
            } else {
                Err(IngressSpoolError::Conflict)
            };
        }

        let active_bytes = spool_bytes(&self.dir)?;
        let record_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if record_bytes > self.max_bytes
            || active_bytes > self.max_bytes.saturating_sub(record_bytes)
        {
            return Err(IngressSpoolError::Full {
                record_bytes,
                active_bytes,
                limit_bytes: self.max_bytes,
            });
        }

        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            path.file_name().unwrap().to_string_lossy(),
            std::process::id(),
            counter
        ));
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)
                .map_err(IngressSpoolError::Io)?;
            file.write_all(&bytes).map_err(IngressSpoolError::Io)?;
            file.sync_all().map_err(IngressSpoolError::Io)?;
            fs::rename(&temp_path, &path).map_err(IngressSpoolError::Io)?;
            sync_directory(&self.dir).map_err(IngressSpoolError::Io)
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        write_result.map(|()| AppendOutcome::Appended)
    }

    pub(crate) async fn drain_once(
        &self,
        store: &AsyncPendingMessageStore,
    ) -> Result<usize, IngressSpoolError> {
        let records = self.records()?;
        let mut drained = 0;
        for record in records {
            store
                .enqueue_idempotent_with_key_fail_fast(
                    record.request.kind.clone(),
                    record.request.payload.clone(),
                    record.request.idempotency_key.clone(),
                )
                .await?;
            fs::remove_file(&record.path).map_err(IngressSpoolError::Io)?;
            sync_directory(&self.dir).map_err(IngressSpoolError::Io)?;
            drained += 1;
        }
        Ok(drained)
    }

    fn records(&self) -> Result<Vec<StoredRecord>, IngressSpoolError> {
        let mut records = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(records),
            Err(error) => return Err(IngressSpoolError::Io(error)),
        };
        for entry in entries {
            let path = entry.map_err(IngressSpoolError::Io)?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            records.push(StoredRecord {
                request: read_record(&path)?,
                path,
            });
        }
        records.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(records)
    }

    fn record_path(&self, request: &HookIpcEnqueueRequest) -> PathBuf {
        let mut identity =
            Vec::with_capacity(request.kind.len() + request.idempotency_key.len() + 1);
        identity.extend_from_slice(request.kind.as_bytes());
        identity.push(0);
        identity.extend_from_slice(request.idempotency_key.as_bytes());
        let digest = blake3::hash(&identity).to_hex();
        self.dir.join(format!("{digest}.json"))
    }
}

fn read_record(path: &Path) -> Result<HookIpcEnqueueRequest, IngressSpoolError> {
    let bytes = fs::read(path).map_err(IngressSpoolError::Io)?;
    let record = serde_json::from_slice::<SpoolRecord>(&bytes).map_err(|source| {
        IngressSpoolError::Decode {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(HookIpcEnqueueRequest {
        kind: record.kind,
        payload: record.payload,
        idempotency_key: record.idempotency_key,
    })
}

fn spool_bytes(dir: &Path) -> Result<u64, IngressSpoolError> {
    let mut total = 0_u64;
    for entry in fs::read_dir(dir).map_err(IngressSpoolError::Io)? {
        let path = entry.map_err(IngressSpoolError::Io)?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("json") {
            total = total.saturating_add(fs::metadata(path).map_err(IngressSpoolError::Io)?.len());
        }
    }
    Ok(total)
}

fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;
    use crate::core::queue::PendingMessageStore;

    fn request(key: &str, payload: &str) -> HookIpcEnqueueRequest {
        HookIpcEnqueueRequest {
            kind: "hook_user_prompt".to_string(),
            payload: payload.to_string(),
            idempotency_key: key.to_string(),
        }
    }

    #[test]
    fn appends_a_fsynced_record_and_deduplicates_same_operation_key() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let spool = IngressSpool::new(tempdir.path());
        let first = request("same-key", "payload");

        assert_eq!(
            spool.append(&first).expect("append"),
            AppendOutcome::Appended
        );
        assert_eq!(
            spool.append(&first).expect("duplicate append"),
            AppendOutcome::AlreadyPresent
        );
        assert_eq!(spool.records().expect("records").len(), 1);
    }

    #[tokio::test]
    async fn drain_replays_idempotently_into_sqlite() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let spool = IngressSpool::new(tempdir.path());
        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let first = request("same-key", "payload");
        spool.append(&first).expect("append");

        assert_eq!(spool.drain_once(&store).await.expect("drain"), 1);
        assert_eq!(spool.drain_once(&store).await.expect("second drain"), 0);
        assert_eq!(
            store
                .operation_status(PendingMessageStore::idempotent_message_id(
                    &first.kind,
                    &first.idempotency_key,
                ))
                .await
                .expect("status")
                .expect("operation")
                .id,
            PendingMessageStore::idempotent_message_id(&first.kind, &first.idempotency_key)
        );
    }

    #[tokio::test]
    async fn busy_sqlite_after_ack_keeps_record_for_later_replay() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let spool = IngressSpool::new(tempdir.path());
        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let first = request("busy-key", "payload");
        spool.append(&first).expect("fsynced append");

        let lock = rusqlite::Connection::open(&db_path).expect("lock connection");
        lock.execute_batch("BEGIN IMMEDIATE;").expect("hold lock");
        let error = spool
            .drain_once(&store)
            .await
            .expect_err("busy SQLite must leave spool record");
        assert!(matches!(
            error,
            IngressSpoolError::Queue(ref queue_error) if queue_error.is_sqlite_lock()
        ));
        assert_eq!(spool.records().expect("spool records").len(), 1);

        lock.execute_batch("ROLLBACK;").expect("release lock");
        assert_eq!(spool.drain_once(&store).await.expect("replay"), 1);
        assert_eq!(spool.drain_once(&store).await.expect("empty replay"), 0);
    }

    #[tokio::test]
    async fn a_reopened_spool_replays_an_acknowledged_record() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let first = request("restart-key", "payload after process death");
        IngressSpool::new(tempdir.path())
            .append(&first)
            .expect("fsynced append");

        let reopened = IngressSpool::new(tempdir.path());
        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        assert_eq!(reopened.drain_once(&store).await.expect("replay"), 1);
        assert_eq!(reopened.records().expect("records after replay").len(), 0);
    }

    #[test]
    fn full_spool_backpressures_without_dropping_existing_records() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let spool = IngressSpool::with_max_bytes(tempdir.path(), 256);
        let first = request("full-first", "payload");
        spool.append(&first).expect("first append");
        let second = request("full-second", &"x".repeat(1024));

        assert!(matches!(
            spool.append(&second),
            Err(IngressSpoolError::Full { .. })
        ));
        assert_eq!(
            spool.records().expect("records after backpressure").len(),
            1
        );
    }
}

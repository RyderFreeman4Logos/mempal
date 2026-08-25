#[cfg(test)]
use std::cell::Cell;
use std::collections::HashSet;
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
#[cfg(test)]
thread_local! {
    static SYNC_DIRECTORY_CALLS: Cell<u64> = const { Cell::new(0) };
}

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
    active_claims: Arc<Mutex<HashSet<PathBuf>>>,
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
            active_claims: Arc::new(Mutex::new(HashSet::new())),
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

        let claim_path = self.claim_path(&path);
        if path.exists() || claim_path.exists() {
            let existing = read_record(if path.exists() { &path } else { &claim_path })?;
            return if existing == *request {
                sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io)?;
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
        let file_name = path.file_name().ok_or_else(|| {
            IngressSpoolError::Io(io::Error::other(
                "ingress spool record path has no file name",
            ))
        })?;
        let temp_path = self.dir.join(format!(
            ".{}.{}.{}.tmp",
            file_name.to_string_lossy(),
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
            sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io)
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
            let Some(claim_path) = self.claim(&record.path)? else {
                continue;
            };
            let enqueue_result = store
                .enqueue_idempotent_with_key_fail_fast(
                    record.request.kind.clone(),
                    record.request.payload.clone(),
                    record.request.idempotency_key.clone(),
                )
                .await;
            match enqueue_result {
                Ok(_) => {
                    let delete_result = fs::remove_file(&claim_path)
                        .map_err(IngressSpoolError::Io)
                        .and_then(|()| {
                            sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io)
                        });
                    self.forget_claim(&claim_path)?;
                    delete_result?;
                }
                Err(QueueError::IdempotencyConflict) => {
                    let park = self.quarantine(&claim_path);
                    self.forget_claim(&claim_path)?;
                    park?;
                    continue;
                }
                Err(error) => {
                    self.release_claim(&record.path, &claim_path)?;
                    return Err(error.into());
                }
            }
            drained += 1;
        }
        Ok(drained)
    }

    fn records(&self) -> Result<Vec<StoredRecord>, IngressSpoolError> {
        let _guard = self
            .append_lock
            .lock()
            .map_err(|_| IngressSpoolError::Io(io::Error::other("spool mutex poisoned")))?;
        self.recover_stale_claims()?;
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
            match read_record(&path) {
                Ok(request) => records.push(StoredRecord { request, path }),
                Err(IngressSpoolError::Decode { .. }) => {
                    self.quarantine(&path)?;
                }
                Err(error) => return Err(error),
            }
        }
        records.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(records)
    }

    fn claim(&self, path: &Path) -> Result<Option<PathBuf>, IngressSpoolError> {
        let claim_path = self.claim_path(path);
        match fs::rename(path, &claim_path) {
            Ok(()) => {
                self.active_claims
                    .lock()
                    .map_err(|_| {
                        IngressSpoolError::Io(io::Error::other("spool claim mutex poisoned"))
                    })?
                    .insert(claim_path.clone());
                Ok(Some(claim_path))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(IngressSpoolError::Io(error)),
        }
    }

    fn release_claim(&self, path: &Path, claim_path: &Path) -> Result<(), IngressSpoolError> {
        let result = if path.exists() {
            let claimed = read_record(claim_path)?;
            let existing = read_record(path)?;
            if claimed == existing {
                fs::remove_file(claim_path)
            } else {
                Err(io::Error::other(
                    "ingress spool claim conflicts with replacement",
                ))
            }
        } else {
            fs::rename(claim_path, path)
        };
        self.forget_claim(claim_path)?;
        result
            .map_err(IngressSpoolError::Io)
            .and_then(|()| sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io))
    }

    fn forget_claim(&self, claim_path: &Path) -> Result<(), IngressSpoolError> {
        self.active_claims
            .lock()
            .map_err(|_| IngressSpoolError::Io(io::Error::other("spool claim mutex poisoned")))?
            .remove(claim_path);
        Ok(())
    }

    fn recover_stale_claims(&self) -> Result<(), IngressSpoolError> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(IngressSpoolError::Io(error)),
        };
        let active_claims = self
            .active_claims
            .lock()
            .map_err(|_| IngressSpoolError::Io(io::Error::other("spool claim mutex poisoned")))?
            .clone();
        let mut recovered = false;
        for entry in entries {
            let claim_path = entry.map_err(IngressSpoolError::Io)?.path();
            if claim_path.extension().and_then(|value| value.to_str()) != Some("claim")
                || active_claims.contains(&claim_path)
            {
                continue;
            }
            let record_path = claim_path.with_extension("json");
            if record_path.exists() {
                let claimed = match read_record(&claim_path) {
                    Ok(claimed) => claimed,
                    Err(IngressSpoolError::Decode { .. }) => {
                        self.quarantine(&claim_path)?;
                        recovered = true;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let existing = match read_record(&record_path) {
                    Ok(existing) => existing,
                    Err(IngressSpoolError::Decode { .. }) => {
                        self.quarantine(&record_path)?;
                        recovered = true;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if claimed != existing {
                    return Err(IngressSpoolError::Conflict);
                }
                fs::remove_file(&claim_path).map_err(IngressSpoolError::Io)?;
            } else {
                fs::rename(&claim_path, &record_path).map_err(IngressSpoolError::Io)?;
            }
            recovered = true;
        }
        if recovered {
            sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io)?;
        }
        Ok(())
    }

    fn quarantine(&self, path: &Path) -> Result<(), IngressSpoolError> {
        let file_name = path.file_name().ok_or_else(|| {
            IngressSpoolError::Io(io::Error::other(
                "ingress spool record path has no file name",
            ))
        })?;
        let dest = self
            .dir
            .join(format!("{}.quarantine", file_name.to_string_lossy()));
        fs::rename(path, dest).map_err(IngressSpoolError::Io)?;
        sync_spool_namespace(&self.dir).map_err(IngressSpoolError::Io)
    }

    fn claim_path(&self, path: &Path) -> PathBuf {
        path.with_extension("claim")
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
        if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("json" | "tmp" | "claim" | "quarantine")
        ) {
            total = total.saturating_add(fs::metadata(path).map_err(IngressSpoolError::Io)?.len());
        }
    }
    Ok(total)
}

fn sync_directory(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    SYNC_DIRECTORY_CALLS.with(|calls| calls.set(calls.get() + 1));
    File::open(dir)?.sync_all()
}

fn sync_spool_namespace(dir: &Path) -> io::Result<()> {
    sync_directory(dir)?;
    let parent = dir
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;
    use crate::core::queue::PendingMessageStore;
    use std::time::Duration;

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

    #[test]
    fn first_append_syncs_spool_and_parent_directories() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let spool = IngressSpool::new(tempdir.path());
        let before = SYNC_DIRECTORY_CALLS.with(Cell::get);

        spool
            .append(&request("namespace-key", "payload"))
            .expect("append");

        let calls = SYNC_DIRECTORY_CALLS.with(|value| value.get() - before);
        assert!(
            calls >= 2,
            "first positive append must sync ingress-spool and its parent, calls={calls}"
        );
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
    async fn concurrent_drainers_cannot_delete_a_replacement_record() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let spool = IngressSpool::new(tempdir.path());
        let first = request("race-key", "original");
        let replacement = request("race-key", "replacement");
        spool.append(&first).expect("append");

        let slow_store = AsyncPendingMessageStore::new_without_reclaim(&db_path)
            .with_blocking_delay(Duration::from_millis(400));
        let fast_store = AsyncPendingMessageStore::new_without_reclaim(&db_path)
            .with_blocking_delay(Duration::from_millis(50));
        let slow_spool = spool.clone();
        let slow = tokio::spawn(async move { slow_spool.drain_once(&slow_store).await });
        tokio::task::yield_now().await;
        let fast_spool = spool.clone();
        let fast = tokio::spawn(async move { fast_spool.drain_once(&fast_store).await });

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if spool.records().expect("records").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("one drainer should claim the record");

        assert!(matches!(
            spool.append(&replacement),
            Err(IngressSpoolError::Conflict)
        ));
        fast.await.expect("fast drainer task").expect("fast drain");
        slow.await.expect("slow drainer task").expect("slow drain");
        assert_eq!(spool.records().expect("records after drain").len(), 0);
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

    #[test]
    fn interrupted_temp_files_count_toward_spool_limit() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let spool = IngressSpool::with_max_bytes(tempdir.path(), 128);
        fs::create_dir_all(&spool.dir).expect("spool dir");
        fs::write(spool.dir.join(".interrupted.tmp"), vec![b'x'; 64]).expect("leftover temp");

        assert!(matches!(
            spool.append(&request("temp-limit", "payload")),
            Err(IngressSpoolError::Full { .. })
        ));
    }

    fn ordered_conflict_and_later(
        spool: &IngressSpool,
    ) -> (HookIpcEnqueueRequest, HookIpcEnqueueRequest) {
        for index in 0..64 {
            let conflict = request(&format!("conflict-{index}"), "payload-b");
            let later = request(&format!("later-{index}"), "payload-valid");
            if spool.record_path(&conflict) < spool.record_path(&later) {
                return (conflict, later);
            }
        }
        panic!("could not find spool keys with conflict-before-later order");
    }

    fn quarantined_paths(spool: &IngressSpool) -> Vec<PathBuf> {
        fs::read_dir(&spool.dir)
            .expect("spool dir")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("quarantine"))
            .collect()
    }

    #[tokio::test]
    async fn terminal_idempotency_conflict_is_quarantined_and_later_record_drains() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let spool = IngressSpool::new(tempdir.path());
        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let (conflict, later) = ordered_conflict_and_later(&spool);

        store
            .enqueue_idempotent_with_key(
                conflict.kind.clone(),
                "seeded-original".to_string(),
                conflict.idempotency_key.clone(),
            )
            .await
            .expect("seed original key");
        spool.append(&conflict).expect("append conflict");
        spool.append(&later).expect("append later valid");

        let before = spool.records().expect("records before drain");
        assert_eq!(before[0].request.idempotency_key, conflict.idempotency_key);
        assert_eq!(before[1].request.idempotency_key, later.idempotency_key);

        let drained = spool
            .drain_once(&store)
            .await
            .expect("drain continues past terminal conflict");
        assert_eq!(drained, 1);
        assert_eq!(spool.records().expect("active records").len(), 0);

        let quarantined = quarantined_paths(&spool);
        assert_eq!(quarantined.len(), 1);
        let parked = read_record(&quarantined[0]).expect("quarantined conflict remains readable");
        assert_eq!(parked.idempotency_key, conflict.idempotency_key);
        assert_eq!(parked.payload, conflict.payload);

        assert_eq!(
            store
                .operation_status(PendingMessageStore::idempotent_message_id(
                    &later.kind,
                    &later.idempotency_key,
                ))
                .await
                .expect("later status")
                .expect("later operation")
                .id,
            PendingMessageStore::idempotent_message_id(&later.kind, &later.idempotency_key)
        );
    }

    #[tokio::test]
    async fn malformed_spool_record_is_quarantined_and_later_record_drains() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("database");
        let spool = IngressSpool::new(tempdir.path());
        let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let later = request("later-valid", "payload-valid");

        fs::create_dir_all(&spool.dir).expect("spool dir");
        fs::write(spool.dir.join("0.json"), b"not-json").expect("malformed record");
        spool.append(&later).expect("append later valid");

        let drained = spool
            .drain_once(&store)
            .await
            .expect("drain continues past malformed record");
        assert_eq!(drained, 1);
        assert_eq!(spool.records().expect("active records").len(), 0);
        assert_eq!(quarantined_paths(&spool).len(), 1);
        assert_eq!(
            store
                .operation_status(PendingMessageStore::idempotent_message_id(
                    &later.kind,
                    &later.idempotency_key,
                ))
                .await
                .expect("later status")
                .expect("later operation")
                .id,
            PendingMessageStore::idempotent_message_id(&later.kind, &later.idempotency_key)
        );
    }
}

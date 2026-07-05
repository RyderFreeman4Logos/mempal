#![warn(clippy::all)]
//! Off-runtime async facade over the synchronous [`Database`].
//!
//! Issue #345: the daemon and the MCP server ran synchronous `rusqlite` calls
//! directly on their tokio worker threads, behind a single
//! `Arc<AsyncMutex<Database>>`. On a multi-GB `palace.db` whose working set
//! exceeds the page cache, a read cold-misses into a blocking `pread`; the
//! worker thread parks in `D` state while still holding the global lock, so the
//! whole runtime stalls and the queue makes zero progress.
//!
//! [`AsyncDb`] removes that pathology by running every `Database` call on a
//! dedicated blocking thread (via [`tokio::task::spawn_blocking`]) over a small
//! bounded pool of pre-opened connections: `n_read` `query_only` readers and
//! exactly one writer. The tokio worker is never the thread that blocks on disk,
//! and a connection is never held across an unrelated `.await` — each closure is
//! self-contained and returns before the connection is checked back in.
//!
//! ## Connection model
//!
//! * **Readers** — `n_read` connections opened read-write-capable, then flipped
//!   to `PRAGMA query_only=ON`. Opening them read-write avoids the read-only-WAL
//!   `-shm` creation footgun while still enforcing read-only semantics at the
//!   SQLite layer. A [`tokio::sync::Semaphore`] of `n_read` permits caps
//!   concurrent reads at exactly the connection count.
//! * **Writer** — a single read-write connection (size-1 pool); the 1-permit
//!   semaphore makes the single-writer invariant structural.
//!
//! ## Invariants
//!
//! * One SQL transaction lives entirely within one `run_write` closure — no
//!   open `Transaction` is ever handed across calls.
//! * A task holds at most one pooled connection at a time, so the pool can never
//!   self-deadlock waiting on its own checkout.
//! * `(n_read + 1) × 16 MiB` page cache stays under a 256 MiB cap for
//!   long-lived read paths (#525); high-throughput maintenance jobs opt into a
//!   larger cache outside this pool.
//! * `mmap_size` stays `0`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use super::db::{Database, DbError, SQLITE_CACHE_SIZE_KIB_DEFAULT};

/// Hard cap on the aggregate SQLite page cache across all pooled connections.
///
/// Each long-lived connection carries a 16 MiB cache
/// ([`SQLITE_CACHE_SIZE_KIB_DEFAULT`]). The default daemon/MCP/API pools use two
/// readers plus one writer, so their configured resident page-cache ceiling is
/// 48 MiB before SQLite and allocator overhead.
const PAGE_CACHE_BUDGET_MIB: i64 = 256;

/// Conservative production reader count for daemon/MCP/REST read pools.
pub const RESOURCE_BOUNDED_READERS: usize = 2;

const SQLITE_PROGRESS_HANDLER_OPS: i32 = 1_000;

#[derive(Debug, thiserror::Error)]
#[error("database read deadline exceeded")]
pub(crate) struct ReadDeadlineExceeded;

pub(crate) fn anyhow_error_is_read_deadline_exceeded(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ReadDeadlineExceeded>().is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsyncDbResourceSnapshot {
    pub reader_connections: usize,
    pub writer_connections: usize,
    pub total_connections: usize,
    pub per_connection_cache_kib: i64,
    pub per_connection_cache_bytes: u64,
    pub configured_page_cache_bytes: u64,
    pub page_cache_budget_bytes: u64,
}

/// Bounded connection pool: a tokio [`Semaphore`] whose permit count equals the
/// connection count, paired with the idle connections themselves. Acquiring a
/// permit guarantees a connection is available (or can be reopened to self-heal
/// after a panicking checkout lost one).
struct ConnPool {
    sem: Arc<Semaphore>,
    idle: Mutex<Vec<Database>>,
    path: PathBuf,
    query_only: bool,
    count: usize,
}

impl ConnPool {
    /// Pre-open `count` connections at `path`. Readers (`query_only == true`)
    /// are opened read-write-capable then flipped to `PRAGMA query_only=ON`.
    fn open(path: &Path, count: usize, query_only: bool) -> Result<Self, DbError> {
        let mut idle = Vec::with_capacity(count);
        for _ in 0..count {
            idle.push(Self::open_one(path, query_only)?);
        }
        Ok(Self {
            sem: Arc::new(Semaphore::new(count)),
            idle: Mutex::new(idle),
            path: path.to_path_buf(),
            query_only,
            count,
        })
    }

    fn open_one(path: &Path, query_only: bool) -> Result<Database, DbError> {
        if query_only {
            Database::open_query_only(path)
        } else {
            Database::open(path)
        }
    }

    /// Pop an idle connection; reopen a fresh one if the pool was transiently
    /// drained by a cancelled checkout (self-heal — the pool never shrinks).
    fn take_or_open(&self) -> Result<Database, DbError> {
        let popped = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        match popped {
            Some(db) => Ok(db),
            None => Self::open_one(&self.path, self.query_only),
        }
    }

    fn checkin(&self, db: Database) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(db);
    }
}

/// Off-runtime async facade over the sync [`Database`]. Cheap to clone (the
/// connection pools live behind `Arc`s).
#[derive(Clone)]
pub struct AsyncDb {
    readers: Arc<ConnPool>,
    writer: Arc<ConnPool>,
    /// Injected cold-read latency for the runtime-liveness / read-concurrency
    /// regression tests (#345). Never present in production builds.
    #[cfg(any(test, feature = "db-test-seam"))]
    read_delay: Option<Duration>,
    #[cfg(any(test, feature = "db-test-seam"))]
    write_delay: Option<Duration>,
}

impl AsyncDb {
    /// Open an [`AsyncDb`] backed by the database at `path` with `n_read`
    /// read-only connections plus one writer.
    ///
    /// Rejects pools whose aggregate page cache would exceed
    /// [`PAGE_CACHE_BUDGET_MIB`] (issue #311). `n_read` is clamped up to at
    /// least 1 so the read pool always has a connection to hand out.
    pub fn open(path: &Path, n_read: usize) -> Result<Self, DbError> {
        let n_read = n_read.max(1);
        let per_conn_mib = (-SQLITE_CACHE_SIZE_KIB_DEFAULT) / 1024;
        let conns = (n_read as i64) + 1;
        let requested_mib = conns * per_conn_mib;
        if requested_mib > PAGE_CACHE_BUDGET_MIB {
            return Err(DbError::PoolCacheBudgetExceeded {
                conns: conns as usize,
                requested_mib,
                budget_mib: PAGE_CACHE_BUDGET_MIB,
            });
        }

        let writer = ConnPool::open(path, 1, false)?;
        let readers = ConnPool::open(path, n_read, true)?;
        Ok(Self {
            readers: Arc::new(readers),
            writer: Arc::new(writer),
            #[cfg(any(test, feature = "db-test-seam"))]
            read_delay: None,
            #[cfg(any(test, feature = "db-test-seam"))]
            write_delay: None,
        })
    }

    pub fn resource_snapshot(&self) -> AsyncDbResourceSnapshot {
        let reader_connections = self.readers.count;
        let writer_connections = self.writer.count;
        let total_connections = reader_connections + writer_connections;
        let per_connection_cache_bytes = sqlite_cache_size_bytes(SQLITE_CACHE_SIZE_KIB_DEFAULT);
        AsyncDbResourceSnapshot {
            reader_connections,
            writer_connections,
            total_connections,
            per_connection_cache_kib: SQLITE_CACHE_SIZE_KIB_DEFAULT,
            per_connection_cache_bytes,
            configured_page_cache_bytes: per_connection_cache_bytes
                .saturating_mul(total_connections as u64),
            page_cache_budget_bytes: (PAGE_CACHE_BUDGET_MIB as u64) * 1024 * 1024,
        }
    }

    /// Inject a synthetic cold-read delay into every `run_read` (tests only).
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_read_delay(mut self, delay: Duration) -> Self {
        self.read_delay = Some(delay);
        self
    }

    /// Inject a synthetic cold-write delay into every `run_write` (tests only).
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_write_delay(mut self, delay: Duration) -> Self {
        self.write_delay = Some(delay);
        self
    }

    /// Run a read-only closure against a pooled reader connection off the
    /// runtime.
    ///
    /// The closure receives a `&Database` and must be self-contained: it MUST
    /// NOT span an `.await` (it runs on a blocking thread) and any SQL
    /// transaction it opens MUST commit or roll back before it returns.
    pub async fn run_read<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.read_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec(Arc::clone(&self.readers), delay, f).await
    }

    /// Run a read-only closure that returns [`anyhow::Result`] off the runtime.
    ///
    /// This is for higher-level orchestration paths that already compose
    /// database calls with filesystem, parsing, or domain validation errors.
    /// The same self-containment rules as [`run_read`](Self::run_read) apply.
    pub async fn run_read_anyhow<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.read_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec_anyhow(Arc::clone(&self.readers), delay, None, f).await
    }

    /// Run a read-only [`anyhow::Result`] closure with a cooperative SQLite
    /// deadline.
    ///
    /// The deadline is installed on the checked-out reader connection via
    /// SQLite's progress handler, so long-running SQL is interrupted inside the
    /// blocking thread instead of continuing after the async caller times out.
    pub(crate) async fn run_read_anyhow_until<F, R>(
        &self,
        deadline: Instant,
        f: F,
    ) -> anyhow::Result<R>
    where
        F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.read_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec_anyhow(Arc::clone(&self.readers), delay, Some(deadline), f).await
    }

    /// Run a read-write closure against the single writer connection off the
    /// runtime. Writes are serialized by the 1-permit writer semaphore; the same
    /// self-containment rules as [`run_read`](Self::run_read) apply.
    pub async fn run_write<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.write_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec(Arc::clone(&self.writer), delay, f).await
    }

    /// Run a read-write closure that returns [`anyhow::Result`] off the runtime.
    ///
    /// Writes are still serialized by the single writer connection. Use this
    /// only when the closure needs to compose `Database` calls with non-DB
    /// errors; pure database operations should prefer [`run_write`](Self::run_write).
    pub async fn run_write_anyhow<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.write_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec_anyhow(Arc::clone(&self.writer), delay, None, f).await
    }
}

fn sqlite_cache_size_bytes(cache_size_kib: i64) -> u64 {
    cache_size_kib.unsigned_abs().saturating_mul(1024)
}

/// Execute `f` on a blocking thread over a connection borrowed from `pool`.
///
/// Acquires a permit, hands the owned connection to the closure, then checks the
/// connection back in *before* releasing the permit so a waiter never wakes to
/// an empty pool. The permit lives inside the blocking closure, so cancelling
/// the async caller cannot release capacity while the connection is still
/// checked out. On a closure panic (only observable with unwinding builds) the
/// connection is dropped on the blocking thread and the pool self-heals on the
/// next checkout.
async fn exec<F, R>(pool: Arc<ConnPool>, delay: Option<Duration>, f: F) -> Result<R, DbError>
where
    F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
    R: Send + 'static,
{
    let permit =
        pool.sem.clone().acquire_owned().await.map_err(|_| {
            DbError::BlockingTaskFailed("connection pool semaphore closed".to_string())
        })?;
    let conn = pool.take_or_open()?;
    let checkin_pool = Arc::clone(&pool);
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let join = tokio::task::spawn_blocking(move || {
        let permit = permit;
        tracing::dispatcher::with_default(&dispatch, || {
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            let out = f(&conn);
            checkin_pool.checkin(conn);
            drop(permit);
            out
        })
    })
    .await;
    match join {
        Ok(out) => out,
        Err(join_err) => Err(DbError::BlockingTaskFailed(join_err.to_string())),
    }
}

async fn exec_anyhow<F, R>(
    pool: Arc<ConnPool>,
    delay: Option<Duration>,
    deadline: Option<Instant>,
    f: F,
) -> anyhow::Result<R>
where
    F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    let permit = if let Some(deadline) = deadline {
        if Instant::now() >= deadline {
            return Err(anyhow::Error::new(ReadDeadlineExceeded));
        }
        tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            pool.sem.clone().acquire_owned(),
        )
        .await
        .map_err(|_| anyhow::Error::new(ReadDeadlineExceeded))?
        .map_err(|_| anyhow::anyhow!("connection pool semaphore closed"))?
    } else {
        pool.sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("connection pool semaphore closed"))?
    };
    let conn = pool.take_or_open()?;
    let checkin_pool = Arc::clone(&pool);
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let join = tokio::task::spawn_blocking(move || {
        let permit = permit;
        tracing::dispatcher::with_default(&dispatch, || {
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            if let Some(deadline) = deadline {
                conn.conn().progress_handler(
                    SQLITE_PROGRESS_HANDLER_OPS,
                    Some(move || Instant::now() >= deadline),
                );
            }
            let out = f(&conn);
            if deadline.is_some() {
                conn.conn().progress_handler(0, None::<fn() -> bool>);
            }
            checkin_pool.checkin(conn);
            drop(permit);
            match out {
                Err(error) if should_map_read_deadline_error(deadline, &error) => {
                    Err(anyhow::Error::new(ReadDeadlineExceeded))
                }
                other => other,
            }
        })
    })
    .await;
    match join {
        Ok(out) => out,
        Err(join_err) => Err(anyhow::anyhow!("blocking database task failed: {join_err}")),
    }
}

fn should_map_read_deadline_error(deadline: Option<Instant>, error: &anyhow::Error) -> bool {
    deadline.is_some_and(|deadline| {
        anyhow_error_contains_sqlite_interrupt(error) || Instant::now() >= deadline
    })
}

fn anyhow_error_contains_sqlite_interrupt(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(rusqlite_error_is_sqlite_interrupt)
            || matches!(
                source.downcast_ref::<DbError>(),
                Some(DbError::Sqlite(sqlite)) if rusqlite_error_is_sqlite_interrupt(sqlite)
            )
    })
}

fn rusqlite_error_is_sqlite_interrupt(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite, _)
            if sqlite.code == rusqlite::ErrorCode::OperationInterrupted
                || sqlite.extended_code == rusqlite::ffi::SQLITE_INTERRUPT
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// T1 — runtime-liveness (the core #345 property). On a single-worker
    /// runtime a ticker must keep advancing while an off-runtime read with a
    /// 300 ms cold-read delay is outstanding. RED if `run_read` ran its closure
    /// inline on the worker (ticker frozen at 0); GREEN with `spawn_blocking`.
    #[tokio::test(flavor = "current_thread")]
    async fn t1_runtime_liveness_read_off_runtime() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(300));

        let ticks = Arc::new(AtomicU64::new(0));
        let ticks_bg = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks_bg.fetch_add(1, Ordering::SeqCst);
            }
        });

        let out: i64 = adb.run_read(|_db| Ok(1)).await.expect("off-runtime read");
        ticker.abort();

        assert_eq!(out, 1);
        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed >= 5,
            "ticker advanced {observed} times during a 300ms off-runtime read; expected >= 5 \
             (read must not occupy the only runtime worker)"
        );
    }

    /// T2 — read concurrency up to N. Four concurrent 200 ms reads must finish
    /// in well under their serial sum. RED on a single-shared-connection model
    /// (serializes to ~800 ms); GREEN on the `n_read = 4` read pool (~200 ms).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn t2_read_concurrency_up_to_n() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(200));

        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for _ in 0..4 {
            let adb = adb.clone();
            handles.push(tokio::spawn(
                async move { adb.run_read(|_db| Ok(1_i64)).await },
            ));
        }
        for handle in handles {
            handle.await.expect("join").expect("read");
        }
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(400),
            "4 concurrent 200ms reads took {elapsed:?}; expected < 400ms (reads run in parallel \
             across the pool, not serialized)"
        );
    }

    /// Every reader connection must enforce `query_only`, carry the low-RSS
    /// cache profile, and carry no `mmap`.
    #[tokio::test]
    async fn readers_are_query_only_low_cache_without_mmap() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4).expect("open async db");

        let (query_only, cache_size, mmap_size): (i64, i64, i64) = adb
            .run_read(|db| {
                let query_only = db
                    .conn()
                    .query_row("PRAGMA query_only", [], |row| row.get(0))?;
                let cache_size = db
                    .conn()
                    .query_row("PRAGMA cache_size", [], |row| row.get(0))?;
                let mmap_size = db
                    .conn()
                    .query_row("PRAGMA mmap_size", [], |row| row.get(0))?;
                Ok((query_only, cache_size, mmap_size))
            })
            .await
            .expect("read pragmas");

        assert_eq!(query_only, 1, "readers must be query_only");
        assert_eq!(
            cache_size, SQLITE_CACHE_SIZE_KIB_DEFAULT,
            "long-lived read pools must use the low-RSS cache profile"
        );
        assert_eq!(mmap_size, 0, "issue #311: pooled readers must not add mmap");
    }

    /// The writer connection must be writable (not flagged `query_only`).
    #[tokio::test]
    async fn writer_is_writable() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4).expect("open async db");

        let query_only: i64 = adb
            .run_write(|db| {
                Ok(db
                    .conn()
                    .query_row("PRAGMA query_only", [], |row| row.get(0))?)
            })
            .await
            .expect("read writer pragma");

        assert_eq!(query_only, 0, "writer must allow writes");
    }

    /// Startup must reject a read pool whose aggregate page cache would blow the
    /// long-lived process budget; the default-sized pool must be accepted.
    #[test]
    fn open_rejects_oversized_read_pool() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("palace.db");

        // (n_read + 1) × 16 MiB: n_read = 15 ⇒ 16 conns ⇒ 256 MiB == budget.
        AsyncDb::open(&path, 15).expect("at-budget pool opens");

        // n_read = 16 ⇒ 17 conns ⇒ 272 MiB > 256 MiB budget.
        let result = AsyncDb::open(&path, 16);
        assert!(
            matches!(result, Err(DbError::PoolCacheBudgetExceeded { .. })),
            "oversized pool must be rejected with PoolCacheBudgetExceeded"
        );
    }

    #[test]
    fn resource_snapshot_reports_configured_page_cache_budget() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), RESOURCE_BOUNDED_READERS)
            .expect("open async db");

        let snapshot = adb.resource_snapshot();

        assert_eq!(snapshot.reader_connections, RESOURCE_BOUNDED_READERS);
        assert_eq!(snapshot.writer_connections, 1);
        assert_eq!(snapshot.total_connections, RESOURCE_BOUNDED_READERS + 1);
        assert_eq!(
            snapshot.per_connection_cache_kib,
            SQLITE_CACHE_SIZE_KIB_DEFAULT
        );
        assert_eq!(snapshot.per_connection_cache_bytes, 16 * 1024 * 1024);
        assert_eq!(
            snapshot.configured_page_cache_bytes,
            48 * 1024 * 1024,
            "daemon/MCP/API default async DB pool must stay well below old GiB-scale cache"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_db_error_read_keeps_permit_until_checkin() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let adb_for_cancel = adb.clone();

        let handle = tokio::spawn(async move {
            adb_for_cancel
                .run_read(move |_db| {
                    let _ = started_tx.send(());
                    std::thread::sleep(Duration::from_millis(300));
                    Ok::<_, DbError>(1_i64)
                })
                .await
        });

        started_rx.await.expect("blocking read started");
        handle.abort();
        let abort_err = handle.await.expect_err("read task should be cancelled");
        assert!(abort_err.is_cancelled(), "read task must be cancelled");

        let blocked = tokio::time::timeout(
            Duration::from_millis(100),
            adb.run_read(|_db| Ok::<_, DbError>(2_i64)),
        )
        .await;
        assert!(
            blocked.is_err(),
            "cancelled read must retain its permit until the blocking task checks the connection in"
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        let out = adb
            .run_read(|_db| Ok::<_, DbError>(3_i64))
            .await
            .expect("reader recovers after cancelled blocking task returns");
        assert_eq!(out, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_anyhow_read_keeps_permit_until_checkin() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let adb_for_cancel = adb.clone();

        let handle = tokio::spawn(async move {
            adb_for_cancel
                .run_read_anyhow(move |_db| {
                    let _ = started_tx.send(());
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(1_i64)
                })
                .await
        });

        started_rx.await.expect("blocking read started");
        handle.abort();
        let abort_err = handle.await.expect_err("read task should be cancelled");
        assert!(abort_err.is_cancelled(), "read task must be cancelled");

        let blocked = tokio::time::timeout(
            Duration::from_millis(100),
            adb.run_read_anyhow(|_db| Ok(2_i64)),
        )
        .await;
        assert!(
            blocked.is_err(),
            "cancelled anyhow read must retain its permit until the blocking task checks the connection in"
        );

        tokio::time::sleep(Duration::from_millis(250)).await;
        let out = adb
            .run_read_anyhow(|_db| Ok(3_i64))
            .await
            .expect("reader recovers after cancelled blocking task returns");
        assert_eq!(out, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deadline_anyhow_read_interrupts_sqlite_and_releases_reader() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
        let start = Instant::now();
        let deadline = start + Duration::from_millis(100);
        let error = adb
            .run_read_anyhow_until(deadline, |db| {
                db.conn()
                    .query_row(
                        r#"
                        WITH RECURSIVE seq(n) AS (
                            SELECT 1
                            UNION ALL
                            SELECT n + 1 FROM seq WHERE n < 100000000
                        )
                        SELECT sum(n) FROM seq
                        "#,
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|_| ())
                    .map_err(anyhow::Error::new)
            })
            .await
            .expect_err("deadline must interrupt the SQLite scan");
        assert!(
            anyhow_error_is_read_deadline_exceeded(&error),
            "deadline error should be explicit, got {error:#}"
        );
        let interrupted_after = start.elapsed();
        assert!(
            interrupted_after >= Duration::from_millis(50),
            "deadline must be reached by SQLite progress_handler, not the pre-deadline fast path; \
             elapsed {interrupted_after:?}"
        );

        let recovery = tokio::time::timeout(
            Duration::from_millis(100),
            adb.run_read_anyhow(|_db| Ok(7_i64)),
        )
        .await
        .expect("reader must be checked in as soon as SQLite is interrupted")
        .expect("recovery read");
        assert_eq!(recovery, 7);
    }
}

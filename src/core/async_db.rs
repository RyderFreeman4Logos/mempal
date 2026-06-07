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
//! * `(n_read + 1) × 256 MiB` page cache stays under a 1.5 GiB cap (issue #311);
//!   `mmap_size` stays `0`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Semaphore;

use super::db::{Database, DbError, SQLITE_CACHE_SIZE_KIB_256_MIB};

/// Hard cap on the aggregate SQLite page cache across all pooled connections.
///
/// Each connection carries a 256 MiB cache ([`SQLITE_CACHE_SIZE_KIB_256_MIB`]);
/// the default `n_read = 4` plus the writer is `5 × 256 MiB = 1.28 GiB`, well
/// under this 1.5 GiB cap and far below issue #311's 4 GiB peak-memory budget.
const PAGE_CACHE_BUDGET_MIB: i64 = 1536;

/// Bounded connection pool: a tokio [`Semaphore`] whose permit count equals the
/// connection count, paired with the idle connections themselves. Acquiring a
/// permit guarantees a connection is available (or can be reopened to self-heal
/// after a cancelled checkout lost one).
struct ConnPool {
    sem: Arc<Semaphore>,
    idle: Mutex<Vec<Database>>,
    path: PathBuf,
    query_only: bool,
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
        })
    }

    fn open_one(path: &Path, query_only: bool) -> Result<Database, DbError> {
        let db = Database::open(path)?;
        if query_only {
            db.conn().pragma_update(None, "query_only", "ON")?;
        }
        Ok(db)
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
        let per_conn_mib = (-SQLITE_CACHE_SIZE_KIB_256_MIB) / 1024;
        let conns = (n_read as i64) + 1;
        let requested_mib = conns * per_conn_mib;
        if requested_mib > PAGE_CACHE_BUDGET_MIB {
            return Err(DbError::PoolCacheBudgetExceeded {
                conns: conns as usize,
                requested_mib,
                budget_mib: PAGE_CACHE_BUDGET_MIB,
            });
        }

        let readers = ConnPool::open(path, n_read, true)?;
        let writer = ConnPool::open(path, 1, false)?;
        Ok(Self {
            readers: Arc::new(readers),
            writer: Arc::new(writer),
            #[cfg(any(test, feature = "db-test-seam"))]
            read_delay: None,
        })
    }

    /// Inject a synthetic cold-read delay into every `run_read` (tests only).
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_read_delay(mut self, delay: Duration) -> Self {
        self.read_delay = Some(delay);
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

    /// Run a read-write closure against the single writer connection off the
    /// runtime. Writes are serialized by the 1-permit writer semaphore; the same
    /// self-containment rules as [`run_read`](Self::run_read) apply.
    pub async fn run_write<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        exec(Arc::clone(&self.writer), None, f).await
    }
}

/// Execute `f` on a blocking thread over a connection borrowed from `pool`.
///
/// Acquires a permit, hands the owned connection to the closure, then checks the
/// connection back in *before* releasing the permit so a waiter never wakes to
/// an empty pool on the happy path. On a closure panic (only observable with
/// unwinding builds) the connection is dropped on the blocking thread and the
/// pool self-heals on the next checkout.
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
    let join = tokio::task::spawn_blocking(move || {
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        let out = f(&conn);
        (conn, out)
    })
    .await;
    match join {
        Ok((conn, out)) => {
            checkin_pool.checkin(conn);
            drop(permit);
            out
        }
        Err(join_err) => {
            drop(permit);
            Err(DbError::BlockingTaskFailed(join_err.to_string()))
        }
    }
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

    /// Every reader connection must enforce `query_only` and carry no `mmap`
    /// (issue #311 RSS budget).
    #[tokio::test]
    async fn readers_are_query_only_without_mmap() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4).expect("open async db");

        let (query_only, mmap_size): (i64, i64) = adb
            .run_read(|db| {
                let query_only = db
                    .conn()
                    .query_row("PRAGMA query_only", [], |row| row.get(0))?;
                let mmap_size = db
                    .conn()
                    .query_row("PRAGMA mmap_size", [], |row| row.get(0))?;
                Ok((query_only, mmap_size))
            })
            .await
            .expect("read pragmas");

        assert_eq!(query_only, 1, "readers must be query_only");
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
    /// issue #311 budget; the default-sized pool must be accepted.
    #[test]
    fn open_rejects_oversized_read_pool() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("palace.db");

        // (n_read + 1) × 256 MiB: n_read = 5 ⇒ 6 conns ⇒ 1536 MiB == budget (ok).
        AsyncDb::open(&path, 5).expect("at-budget pool opens");

        // n_read = 6 ⇒ 7 conns ⇒ 1792 MiB > 1536 MiB budget (rejected before any
        // connection is opened).
        let result = AsyncDb::open(&path, 6);
        assert!(
            matches!(result, Err(DbError::PoolCacheBudgetExceeded { .. })),
            "oversized pool must be rejected with PoolCacheBudgetExceeded"
        );
    }
}

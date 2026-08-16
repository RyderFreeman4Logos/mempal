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
//! exactly one writer. [`QueryOnlyAsyncDb`] uses the same off-runtime read pool
//! without opening a writer connection, for MCP/read-only surfaces that must stay
//! available while a daemon owns the writer lease. The tokio worker is never the
//! thread that blocks on disk, and a connection is never held across an unrelated
//! `.await` — each closure is self-contained and returns before the connection is
//! checked back in.
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use super::db::{Database, DbError, SQLITE_CACHE_SIZE_KIB_DEFAULT};
use super::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};
use super::sqlite_retry::content_mutation_sqlite_lock_retry_deadline_error;

#[path = "async_db_conn_pool.rs"]
mod conn_pool;

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
    #[cfg(any(test, feature = "db-test-seam"))]
    reopen_delay: Mutex<Option<Duration>>,
    #[cfg(any(test, feature = "db-test-seam"))]
    write_busy_timeout: Mutex<Option<Duration>>,
    #[cfg(any(test, feature = "db-test-seam"))]
    write_busy_events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
}

/// Off-runtime async facade over the sync [`Database`]. Cheap to clone (the
/// connection pools live behind `Arc`s).
#[derive(Clone)]
pub struct AsyncDb {
    readers: Arc<ConnPool>,
    writer: Arc<ConnPool>,
    _admission: Arc<ProfileDbAdmission>,
    /// Injected cold-read latency for the runtime-liveness / read-concurrency
    /// regression tests (#345). Never present in production builds.
    #[cfg(any(test, feature = "db-test-seam"))]
    read_delay: Option<Duration>,
    #[cfg(any(test, feature = "db-test-seam"))]
    write_delay: Option<Duration>,
}

/// Off-runtime async facade for bounded read-only database work.
///
/// Unlike [`AsyncDb`], this facade never opens a writer connection. It is for
/// MCP/query surfaces that should continue to read while the daemon is the
/// singleton writer.
#[derive(Clone)]
pub struct QueryOnlyAsyncDb {
    readers: Arc<ConnPool>,
    _admission: Arc<ProfileDbAdmission>,
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
        Self::open_for(path, n_read, DbHolderClass::current_process())
    }

    pub fn open_for(
        path: &Path,
        n_read: usize,
        holder_class: DbHolderClass,
    ) -> Result<Self, DbError> {
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

        let admission = ProfileDbAdmission::acquire(
            path,
            DbAdmissionRequest::new(
                holder_class,
                conns as usize,
                (requested_mib as u64) * 1024 * 1024,
            ),
        )?;
        // Admission resolves identity (canonical path). Unadmitted pool opens must
        // consume that resolved path only — never the original config path, which may
        // be a symlink rejected by SQLITE_OPEN_NOFOLLOW / SymlinkDatabasePath.
        let admitted_path = admission.database_path();
        let writer = ConnPool::open(admitted_path, 1, false)?;
        let readers = ConnPool::open(admitted_path, n_read, true)?;
        Ok(Self {
            readers: Arc::new(readers),
            writer: Arc::new(writer),
            _admission: Arc::new(admission),
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

    /// Delay a writer-pool self-heal open (tests only).
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_writer_reopen_delay(self, delay: Duration) -> Self {
        self.writer.set_reopen_delay(delay);
        self
    }

    /// Override the deadline-bound writer busy timeout and report a real Busy.
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_write_busy_timeout_for_test(
        self,
        busy_timeout: Duration,
        busy_events: tokio::sync::mpsc::UnboundedSender<()>,
    ) -> Self {
        self.writer
            .set_write_busy_timeout_and_events(busy_timeout, busy_events);
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
        exec(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            f,
        )
        .await
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
        exec_anyhow(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            None,
            f,
        )
        .await
    }

    /// Run a read-only [`anyhow::Result`] closure with a cooperative SQLite
    /// deadline.
    ///
    /// The deadline is installed on the checked-out reader connection via
    /// SQLite's progress handler, so long-running SQL is interrupted inside the
    /// blocking thread instead of continuing after the async caller times out.
    #[cfg(any(feature = "rest", test))]
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
        exec_anyhow(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            Some(deadline),
            f,
        )
        .await
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
        exec(
            Arc::clone(&self.writer),
            Arc::clone(&self._admission),
            delay,
            f,
        )
        .await
    }

    /// Run a read-write closure that cannot outlive `deadline`.
    ///
    /// The deadline covers writer-permit acquisition, pool self-healing, test
    /// delay, and SQLite's busy wait. A blocking worker must receive explicit
    /// pre-deadline approval before it can invoke the closure; once it starts,
    /// this method awaits its definitive result so a committed mutation cannot
    /// be reported as a late lock failure.
    pub(crate) async fn run_write_until<F, R>(&self, deadline: Instant, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.write_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec_with_deadline(
            Arc::clone(&self.writer),
            Arc::clone(&self._admission),
            delay,
            deadline,
            f,
        )
        .await
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
        exec_anyhow(
            Arc::clone(&self.writer),
            Arc::clone(&self._admission),
            delay,
            None,
            f,
        )
        .await
    }
}

impl QueryOnlyAsyncDb {
    /// Open a query-only async facade backed by `n_read` read-only connections.
    ///
    /// Rejects pools whose aggregate page cache would exceed
    /// [`PAGE_CACHE_BUDGET_MIB`]. `n_read` is clamped up to at least 1 so the
    /// read pool always has a connection to hand out.
    pub fn open(path: &Path, n_read: usize) -> Result<Self, DbError> {
        Self::open_for(path, n_read, DbHolderClass::current_process())
    }

    pub fn open_for(
        path: &Path,
        n_read: usize,
        holder_class: DbHolderClass,
    ) -> Result<Self, DbError> {
        let n_read = n_read.max(1);
        let per_conn_mib = (-SQLITE_CACHE_SIZE_KIB_DEFAULT) / 1024;
        let requested_mib = (n_read as i64) * per_conn_mib;
        if requested_mib > PAGE_CACHE_BUDGET_MIB {
            return Err(DbError::PoolCacheBudgetExceeded {
                conns: n_read,
                requested_mib,
                budget_mib: PAGE_CACHE_BUDGET_MIB,
            });
        }

        let admission = ProfileDbAdmission::acquire(
            path,
            DbAdmissionRequest::new(holder_class, n_read, (requested_mib as u64) * 1024 * 1024),
        )?;
        // See AsyncDb::open_for — unadmitted pools open the admitted identity only.
        let admitted_path = admission.database_path();
        let readers = ConnPool::open(admitted_path, n_read, true)?;
        Ok(Self {
            readers: Arc::new(readers),
            _admission: Arc::new(admission),
            #[cfg(any(test, feature = "db-test-seam"))]
            read_delay: None,
        })
    }

    pub fn resource_snapshot(&self) -> AsyncDbResourceSnapshot {
        let reader_connections = self.readers.count;
        let writer_connections = 0;
        let total_connections = reader_connections;
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

    /// Inject a synthetic cold-read delay into every read (tests only).
    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_read_delay(mut self, delay: Duration) -> Self {
        self.read_delay = Some(delay);
        self
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub(crate) fn available_reader_permits_for_test(&self) -> usize {
        self.readers.sem.available_permits()
    }

    /// Run a read-only closure against a pooled reader connection off the
    /// runtime.
    pub async fn run_read<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.read_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            f,
        )
        .await
    }

    /// Run a read-only closure that returns [`anyhow::Result`] off the runtime.
    pub async fn run_read_anyhow<F, R>(&self, f: F) -> anyhow::Result<R>
    where
        F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        #[cfg(any(test, feature = "db-test-seam"))]
        let delay = self.read_delay;
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let delay: Option<Duration> = None;
        exec_anyhow(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            None,
            f,
        )
        .await
    }

    /// Run a read-only [`anyhow::Result`] closure with a cooperative SQLite
    /// deadline.
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
        exec_anyhow(
            Arc::clone(&self.readers),
            Arc::clone(&self._admission),
            delay,
            Some(deadline),
            f,
        )
        .await
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
///
/// The `_admission` clone ensures the profile admission holder outlives the
/// blocking task even if every AsyncDb facade is dropped while the closure
/// is still executing — without this, the admission could be released and the
/// budget slot reused while a real SQLite connection is still alive.
async fn exec<F, R>(
    pool: Arc<ConnPool>,
    _admission: Arc<ProfileDbAdmission>,
    delay: Option<Duration>,
    f: F,
) -> Result<R, DbError>
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
        let _permit = permit;
        let _admission = _admission; // keep admission alive until closure ends
        tracing::dispatcher::with_default(&dispatch, || {
            if let Some(d) = delay {
                std::thread::sleep(d);
            }
            let out = f(&conn);
            checkin_pool.checkin(conn);
            out
        })
    })
    .await;
    match join {
        Ok(out) => out,
        Err(join_err) => Err(DbError::BlockingTaskFailed(join_err.to_string())),
    }
}

async fn await_write_join<R>(
    join: tokio::task::JoinHandle<Result<R, DbError>>,
) -> Result<R, DbError> {
    match join.await {
        Ok(out) => out,
        Err(join_err) => Err(DbError::BlockingTaskFailed(join_err.to_string())),
    }
}

/// Execute a deadline-bound write without cancelling its owned blocking task.
///
/// The deadline bounds permit acquisition, blocking-pool dispatch, connection
/// self-healing, and authorization to invoke `f`. A worker can only start `f`
/// after the caller approves it before expiry; after that point the worker is
/// joined for its definitive SQLite result instead of being timed out mid-write.
async fn exec_with_deadline<F, R>(
    pool: Arc<ConnPool>,
    _admission: Arc<ProfileDbAdmission>,
    delay: Option<Duration>,
    deadline: Instant,
    f: F,
) -> Result<R, DbError>
where
    F: FnOnce(&Database) -> Result<R, DbError> + Send + 'static,
    R: Send + 'static,
{
    if Instant::now() >= deadline {
        return Err(write_deadline_exceeded_error());
    }
    let permit = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        pool.sem.clone().acquire_owned(),
    )
    .await
    .map_err(|_| write_deadline_exceeded_error())?
    .map_err(|_| DbError::BlockingTaskFailed("connection pool semaphore closed".to_string()))?;
    if Instant::now() >= deadline {
        return Err(write_deadline_exceeded_error());
    }

    let checkin_pool = Arc::clone(&pool);
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<tokio::sync::oneshot::Sender<()>>();
    let join = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _admission = _admission; // keep admission alive until closure ends
        tracing::dispatcher::with_default(&dispatch, || {
            let conn = checkin_pool.take_or_open_until(deadline)?;
            if Instant::now() >= deadline {
                checkin_pool.checkin(conn);
                return Err(write_deadline_exceeded_error());
            }

            let (approval_tx, approval_rx) = tokio::sync::oneshot::channel::<()>();
            if ready_tx.send(approval_tx).is_err() {
                checkin_pool.checkin(conn);
                return Err(write_deadline_exceeded_error());
            }
            if approval_rx.blocking_recv().is_err() {
                checkin_pool.checkin(conn);
                return Err(write_deadline_exceeded_error());
            }

            let out = execute_write_until(checkin_pool.as_ref(), &conn, delay, deadline, f);
            checkin_pool.checkin(conn);
            out
        })
    });
    let approval_tx =
        match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), ready_rx).await {
            Ok(Ok(approval_tx)) => approval_tx,
            Ok(Err(_)) => return await_write_join(join).await,
            Err(_) => return Err(write_deadline_exceeded_error()),
        };
    if Instant::now() >= deadline {
        return Err(write_deadline_exceeded_error());
    }
    if approval_tx.send(()).is_err() {
        return await_write_join(join).await;
    }
    await_write_join(join).await
}

fn execute_write_until<F, R>(
    #[cfg(any(test, feature = "db-test-seam"))] pool: &ConnPool,
    #[cfg(not(any(test, feature = "db-test-seam")))] _pool: &ConnPool,
    conn: &Database,
    delay: Option<Duration>,
    deadline: Instant,
    f: F,
) -> Result<R, DbError>
where
    F: FnOnce(&Database) -> Result<R, DbError>,
{
    if let Some(delay) = delay {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(write_deadline_exceeded_error());
        }
        std::thread::sleep(delay.min(remaining));
    }

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(write_deadline_exceeded_error());
    }

    // Pooled writer connections normally use Database::open's five-second busy
    // timeout. Narrow it to this attempt's remaining shared retry budget, then
    // restore that pool default before the connection is checked back in.
    #[cfg(any(test, feature = "db-test-seam"))]
    let busy_timeout = pool
        .write_busy_timeout()
        .unwrap_or(remaining)
        .min(remaining);
    #[cfg(not(any(test, feature = "db-test-seam")))]
    let busy_timeout = remaining;
    conn.conn().busy_timeout(busy_timeout)?;
    conn.conn().progress_handler(
        SQLITE_PROGRESS_HANDLER_OPS,
        Some(move || Instant::now() >= deadline),
    );
    if Instant::now() >= deadline {
        conn.conn().progress_handler(0, None::<fn() -> bool>);
        conn.conn().busy_timeout(Duration::from_secs(5))?;
        return Err(write_deadline_exceeded_error());
    }
    let out = f(conn);
    #[cfg(any(test, feature = "db-test-seam"))]
    pool.report_busy_write_attempt(&out);
    conn.conn().progress_handler(0, None::<fn() -> bool>);
    conn.conn().busy_timeout(Duration::from_secs(5))?;

    match out {
        Err(DbError::Sqlite(error)) if rusqlite_error_is_sqlite_interrupt(&error) => {
            Err(write_deadline_exceeded_error())
        }
        other => other,
    }
}

fn write_deadline_exceeded_error() -> DbError {
    DbError::from(content_mutation_sqlite_lock_retry_deadline_error())
}

async fn exec_anyhow<F, R>(
    pool: Arc<ConnPool>,
    _admission: Arc<ProfileDbAdmission>,
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
        let _permit = permit;
        let _admission = _admission; // keep admission alive until closure ends
        tracing::dispatcher::with_default(&dispatch, || {
            if let Some(d) = delay {
                let delay = deadline
                    .map(|deadline| d.min(deadline.saturating_duration_since(Instant::now())))
                    .unwrap_or(d);
                std::thread::sleep(delay);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                checkin_pool.checkin(conn);
                return Err(anyhow::Error::new(ReadDeadlineExceeded));
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
            match out {
                Ok(_) if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                    Err(anyhow::Error::new(ReadDeadlineExceeded))
                }
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
#[path = "async_db_tests.rs"]
mod tests;

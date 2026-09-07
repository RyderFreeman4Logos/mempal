//! Process-local SQLite writer-lock evidence.
//!
//! Label a connection as owner only after `BEGIN IMMEDIATE` succeeds. A thread
//! still waiting on BEGIN is a waiter and must not invent a foreign owner PID.
//! Contention against unlabeled foreign holders is `UnknownExternal`; this
//! census does not claim external lock-owner coverage.
//! ponytail: process-local fence census only; add OS lock-owner probe if live
//! unlabeled holders need a foreign PID.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, TransactionBehavior};

macro_rules! writer_stall_event {
    ($log:path, $diagnostic:expr, $message:literal) => {
        $log!(
            queued_count = $diagnostic.queued_count,
            seconds_since_successful_write = $diagnostic.seconds_since_successful_write,
            last_error = %$diagnostic.last_error,
            uptime_secs = $diagnostic.uptime_secs,
            writer_evidence = ?$diagnostic.writer_evidence,
            $message
        )
    };
}

pub(crate) use writer_stall_event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterLockClass {
    HeldWriter,
    BlockedWaiter,
    UnknownExternal,
    Released,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLockEvidence {
    pub class: WriterLockClass,
    pub owner_pid: Option<u32>,
    pub operation: Option<&'static str>,
    pub elapsed: Option<Duration>,
}

#[derive(Clone, Copy)]
enum ThreadRole {
    Waiting,
    Owning,
}

struct OwnerSlot {
    pid: u32,
    operation: &'static str,
    started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum DatabaseIdentity {
    #[cfg(unix)]
    FileSystem {
        device: u64,
        inode: u64,
    },
    File([u8; 32]),
    Connection(usize),
}

#[derive(Default)]
struct DatabaseState {
    owner: Option<OwnerSlot>,
    waiters: usize,
    release_unverified: bool,
}

struct ProcessState {
    databases: HashMap<DatabaseIdentity, DatabaseState>,
}

#[derive(Clone, Copy)]
struct ThreadContext {
    database: DatabaseIdentity,
    role: ThreadRole,
}

thread_local! {
    static THREAD_CONTEXT: Cell<Option<ThreadContext>> = const { Cell::new(None) };
}

fn process_state() -> &'static Mutex<ProcessState> {
    static STATE: OnceLock<Mutex<ProcessState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(ProcessState {
            databases: HashMap::new(),
        })
    })
}

fn identity_for_path(path: &Path) -> DatabaseIdentity {
    #[cfg(unix)]
    if let Ok(metadata) = fs::metadata(path) {
        use std::os::unix::fs::MetadataExt;
        return DatabaseIdentity::FileSystem {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
    }
    DatabaseIdentity::File(*blake3::hash(path.as_os_str().as_encoded_bytes()).as_bytes())
}

fn identity_for_connection(conn: &Connection) -> DatabaseIdentity {
    conn.path()
        .filter(|path| !path.is_empty())
        .map(|path| identity_for_path(Path::new(path)))
        .unwrap_or(DatabaseIdentity::Connection(
            conn as *const Connection as usize,
        ))
}

#[cfg(test)]
fn current_writer_lock_evidence(conn: &Connection) -> WriterLockEvidence {
    writer_lock_evidence(identity_for_connection(conn))
}

pub fn writer_lock_evidence_for_path(path: &Path) -> WriterLockEvidence {
    writer_lock_evidence(identity_for_path(path))
}

fn writer_lock_evidence(database: DatabaseIdentity) -> WriterLockEvidence {
    let role = THREAD_CONTEXT
        .with(Cell::get)
        .and_then(|context| (context.database == database).then_some(context.role));
    let Ok(state) = process_state().lock() else {
        return unknown_evidence();
    };
    let Some(state) = state.databases.get(&database) else {
        return released_evidence();
    };
    if state.release_unverified {
        return unknown_evidence();
    }
    if let Some(owner) = state.owner.as_ref() {
        let class = if matches!(role, Some(ThreadRole::Owning)) || state.waiters == 0 {
            WriterLockClass::HeldWriter
        } else {
            WriterLockClass::BlockedWaiter
        };
        return WriterLockEvidence {
            class,
            owner_pid: Some(owner.pid),
            operation: Some(owner.operation),
            elapsed: Some(owner.started.elapsed()),
        };
    }
    if matches!(role, Some(ThreadRole::Waiting)) || state.waiters > 0 {
        return unknown_evidence();
    }
    released_evidence()
}

fn unknown_evidence() -> WriterLockEvidence {
    WriterLockEvidence {
        class: WriterLockClass::UnknownExternal,
        owner_pid: None,
        operation: None,
        elapsed: None,
    }
}

fn released_evidence() -> WriterLockEvidence {
    WriterLockEvidence {
        class: WriterLockClass::Released,
        owner_pid: None,
        operation: None,
        elapsed: None,
    }
}

pub struct WriterWaitGuard {
    database: DatabaseIdentity,
    operation: &'static str,
    active: bool,
}

impl WriterWaitGuard {
    pub fn enter(conn: &Connection, operation: &'static str) -> Self {
        let database = identity_for_connection(conn);
        THREAD_CONTEXT.with(|context| {
            context.set(Some(ThreadContext {
                database,
                role: ThreadRole::Waiting,
            }));
        });
        if let Ok(mut state) = process_state().lock() {
            let state = state.databases.entry(database).or_default();
            state.waiters = state.waiters.saturating_add(1);
        }
        Self {
            database,
            operation,
            active: true,
        }
    }

    pub fn into_owner(mut self, conn: &Connection) -> WriterOwnerGuard<'_> {
        self.active = false;
        publish_owner(self.database, self.operation);
        WriterOwnerGuard {
            conn,
            database: self.database,
            active: true,
        }
    }

    fn into_transaction(mut self, transaction: Transaction<'_>) -> WriterOwnerTransaction<'_> {
        self.active = false;
        publish_owner(self.database, self.operation);
        WriterOwnerTransaction {
            transaction: Some(transaction),
            database: self.database,
        }
    }

    fn failed(mut self) {
        self.active = false;
        finish_wait(self.database, true);
    }
}

impl Drop for WriterWaitGuard {
    fn drop(&mut self) {
        if self.active {
            finish_wait(self.database, false);
        }
    }
}

pub struct WriterOwnerGuard<'conn> {
    conn: &'conn Connection,
    database: DatabaseIdentity,
    active: bool,
}

impl<'conn> WriterOwnerGuard<'conn> {
    pub fn release(mut self) {
        self.active = false;
        finish_owner(self.database, self.conn.is_autocommit());
    }
}

impl Drop for WriterOwnerGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let rollback = self.conn.execute_batch("ROLLBACK");
            finish_owner(self.database, rollback.is_ok() || self.conn.is_autocommit());
        }
    }
}

pub struct WriterOwnerTransaction<'conn> {
    transaction: Option<Transaction<'conn>>,
    database: DatabaseIdentity,
}

impl WriterOwnerTransaction<'_> {
    pub fn commit(mut self) -> rusqlite::Result<()> {
        let result = self
            .transaction
            .take()
            .expect("writer transaction must exist")
            .commit();
        finish_owner(self.database, result.is_ok());
        result
    }
}

impl<'conn> Deref for WriterOwnerTransaction<'conn> {
    type Target = Transaction<'conn>;

    fn deref(&self) -> &Self::Target {
        self.transaction
            .as_ref()
            .expect("writer transaction must exist")
    }
}

impl Drop for WriterOwnerTransaction<'_> {
    fn drop(&mut self) {
        let Some(transaction) = self.transaction.take() else {
            return;
        };
        let released = transaction.is_autocommit() || transaction.rollback().is_ok();
        finish_owner(self.database, released);
    }
}

fn publish_owner(database: DatabaseIdentity, operation: &'static str) {
    THREAD_CONTEXT.with(|context| {
        context.set(Some(ThreadContext {
            database,
            role: ThreadRole::Owning,
        }));
    });
    if let Ok(mut state) = process_state().lock() {
        let state = state.databases.entry(database).or_default();
        state.waiters = state.waiters.saturating_sub(1);
        state.release_unverified = false;
        state.owner = Some(OwnerSlot {
            pid: std::process::id(),
            operation,
            started: Instant::now(),
        });
    }
}

fn finish_wait(database: DatabaseIdentity, unverified: bool) {
    THREAD_CONTEXT.with(|context| context.set(None));
    if let Ok(mut process) = process_state().lock() {
        let database_state = process.databases.entry(database).or_default();
        database_state.waiters = database_state.waiters.saturating_sub(1);
        if unverified && database_state.owner.is_none() {
            database_state.release_unverified = true;
        }
        if database_state.waiters == 0
            && database_state.owner.is_none()
            && !database_state.release_unverified
        {
            process.databases.remove(&database);
        }
    }
}

fn finish_owner(database: DatabaseIdentity, released: bool) {
    THREAD_CONTEXT.with(|context| context.set(None));
    if let Ok(mut process) = process_state().lock() {
        let database_state = process.databases.entry(database).or_default();
        database_state.owner = None;
        database_state.release_unverified = !released;
        if database_state.waiters == 0 && released {
            process.databases.remove(&database);
        }
    }
}

pub(crate) fn begin_immediate<'conn>(
    conn: &'conn Connection,
    operation: &'static str,
) -> rusqlite::Result<WriterOwnerGuard<'conn>> {
    let wait = WriterWaitGuard::enter(conn, operation);
    match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => Ok(wait.into_owner(conn)),
        Err(error) => {
            wait.failed();
            Err(error)
        }
    }
}

pub(crate) fn transaction_immediate<'conn>(
    conn: &'conn mut Connection,
    operation: &'static str,
) -> rusqlite::Result<WriterOwnerTransaction<'conn>> {
    let wait = WriterWaitGuard::enter(conn, operation);
    match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => Ok(wait.into_transaction(transaction)),
        Err(error) => {
            wait.failed();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Barrier, Mutex, MutexGuard, PoisonError, mpsc};
    use std::time::Duration;

    static EVIDENCE_LOCK: Mutex<()> = Mutex::new(());

    fn lock_process_evidence() -> MutexGuard<'static, ()> {
        EVIDENCE_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn assert_privacy(evidence: &WriterLockEvidence) {
        if let Some(operation) = evidence.operation {
            assert!(!operation.contains("BEGIN"), "evidence must not carry SQL");
            assert!(
                !operation.contains('/') && !operation.contains('\\'),
                "evidence must not carry paths"
            );
        }
    }

    #[test]
    fn successful_begin_labels_held_writer_not_waiter() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let wait = WriterWaitGuard::enter(&conn, "insert fenced drawer");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin immediate");
        let owner = wait.into_owner(&conn);
        let evidence = current_writer_lock_evidence(&conn);
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(evidence.owner_pid, Some(std::process::id()));
        assert_eq!(evidence.operation, Some("insert fenced drawer"));
        assert!(evidence.elapsed.is_some());
        conn.execute_batch("COMMIT").expect("commit");
        drop(owner);
        let released = current_writer_lock_evidence(&conn);
        assert_eq!(released.class, WriterLockClass::Released);
        assert_eq!(released.owner_pid, None);
        assert_eq!(released.operation, None);
    }

    #[test]
    fn rollback_releases_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let wait = WriterWaitGuard::enter(&conn, "complete queued operation");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin immediate");
        let owner = wait.into_owner(&conn);
        assert_eq!(
            current_writer_lock_evidence(&conn).class,
            WriterLockClass::HeldWriter
        );
        conn.execute_batch("ROLLBACK").expect("rollback");
        drop(owner);
        assert_eq!(
            current_writer_lock_evidence(&conn).class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn panic_drop_releases_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let wait = WriterWaitGuard::enter(&conn, "claim queued message");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("begin immediate");
            let _owner = wait.into_owner(&conn);
            assert_eq!(
                current_writer_lock_evidence(&conn).class,
                WriterLockClass::HeldWriter
            );
            panic!("force owner drop");
        }));
        assert!(panicked.is_err());
        assert!(
            conn.is_autocommit(),
            "panic drop must roll back the transaction"
        );
        assert_eq!(
            current_writer_lock_evidence(&conn).class,
            WriterLockClass::Released
        );
    }

    fn deny_transaction_end(context: AuthContext<'_>) -> Authorization {
        match context.action {
            AuthAction::Transaction { operation }
                if !matches!(operation, TransactionOperation::Begin) =>
            {
                Authorization::Deny
            }
            _ => Authorization::Allow,
        }
    }

    #[test]
    fn panic_drop_with_failed_rollback_preserves_unverified_evidence() {
        let _lock = lock_process_evidence();
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("panic-rollback.db");
        let conn = Connection::open(&path).expect("open database");

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _owner = begin_immediate(&conn, "panic failed cleanup").expect("begin");
            conn.authorizer(Some(deny_transaction_end));
            panic!("force failed owner drop");
        }));
        assert!(panicked.is_err());
        assert!(!conn.is_autocommit(), "ROLLBACK must be denied");
        let evidence = writer_lock_evidence_for_path(&path);
        assert_eq!(evidence.class, WriterLockClass::UnknownExternal);
        assert_eq!(evidence.owner_pid, None);
        assert_eq!(evidence.operation, None);

        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        conn.execute_batch("ROLLBACK").expect("cleanup transaction");
    }

    #[test]
    fn local_waiter_sees_known_held_writer_not_waiter_pid_as_owner() {
        let _lock = lock_process_evidence();
        let path = std::env::temp_dir().join(format!(
            "mempal-writer-owner-known-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let holder = Connection::open(&path).expect("holder db");
        let wait = WriterWaitGuard::enter(&holder, "insert fenced drawer");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold writer");
        let owner = wait.into_owner(&holder);
        holder
            .busy_timeout(Duration::ZERO)
            .expect("holder busy timeout");

        let (ready_tx, ready_rx) = mpsc::channel();
        let waiter_path = path.clone();
        let waiter = std::thread::spawn(move || {
            let conn = Connection::open(&waiter_path).expect("waiter db");
            conn.busy_timeout(Duration::from_secs(1))
                .expect("waiter busy timeout");
            let wait = WriterWaitGuard::enter(&conn, "claim queued message");
            ready_tx
                .send(current_writer_lock_evidence(&conn))
                .expect("send");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("waiter acquires after holder release");
            let owner = wait.into_owner(&conn);
            let evidence = current_writer_lock_evidence(&conn);
            conn.execute_batch("ROLLBACK").expect("waiter rollback");
            drop(owner);
            evidence
        });
        let waiter_evidence = ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter evidence");
        assert_privacy(&waiter_evidence);
        assert_eq!(waiter_evidence.class, WriterLockClass::BlockedWaiter);
        assert_eq!(waiter_evidence.owner_pid, Some(std::process::id()));
        assert_eq!(waiter_evidence.operation, Some("insert fenced drawer"));
        let owner_evidence = current_writer_lock_evidence(&holder);
        assert_eq!(owner_evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(owner_evidence.owner_pid, Some(std::process::id()));
        holder.execute_batch("ROLLBACK").expect("rollback holder");
        drop(owner);
        let acquired_evidence = waiter.join().expect("join waiter");
        assert_eq!(acquired_evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(acquired_evidence.operation, Some("claim queued message"));
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            current_writer_lock_evidence(&holder).class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn unlabeled_holder_is_unknown_external_not_local_waiter_pid() {
        let _lock = lock_process_evidence();
        let path = std::env::temp_dir().join(format!(
            "mempal-writer-owner-unknown-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let holder = Connection::open(&path).expect("unlabeled holder");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold unlabeled writer");

        let waiter = Connection::open(&path).expect("waiter db");
        waiter
            .busy_timeout(Duration::ZERO)
            .expect("waiter busy timeout");
        let wait = WriterWaitGuard::enter(&waiter, "claim queued message");
        waiter
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("unlabeled holder blocks waiter");
        let evidence = current_writer_lock_evidence(&waiter);
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::UnknownExternal);
        assert_eq!(evidence.owner_pid, None);
        assert_eq!(evidence.operation, None);
        drop(wait);
        holder.execute_batch("ROLLBACK").expect("rollback");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            current_writer_lock_evidence(&waiter).class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn successful_immediate_helper_labels_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let owner = begin_immediate(&conn, "complete queued operation").expect("begin");
        let evidence = current_writer_lock_evidence(&conn);
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(evidence.owner_pid, Some(std::process::id()));
        assert_eq!(evidence.operation, Some("complete queued operation"));
        conn.execute_batch("COMMIT").expect("commit");
        drop(owner);
        assert_eq!(
            current_writer_lock_evidence(&conn).class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn simultaneous_distinct_databases_keep_distinct_owners() {
        let _lock = lock_process_evidence();
        let temp = tempfile::tempdir().expect("tempdir");
        let first_path = temp.path().join("first.db");
        let second_path = temp.path().join("second.db");
        let barrier = Arc::new(Barrier::new(3));

        let hold = |path: std::path::PathBuf, operation: &'static str, barrier: Arc<Barrier>| {
            std::thread::spawn(move || {
                let mut conn = Connection::open(path).expect("open database");
                let tx = transaction_immediate(&mut conn, operation).expect("begin immediate");
                barrier.wait();
                barrier.wait();
                drop(tx);
            })
        };
        let first = hold(
            first_path.clone(),
            "first database owner",
            Arc::clone(&barrier),
        );
        let second = hold(
            second_path.clone(),
            "second database owner",
            Arc::clone(&barrier),
        );

        barrier.wait();
        let first_evidence = writer_lock_evidence_for_path(&first_path);
        let second_evidence = writer_lock_evidence_for_path(&second_path);
        assert_eq!(first_evidence.operation, Some("first database owner"));
        assert_eq!(second_evidence.operation, Some("second database owner"));
        assert_eq!(first_evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(second_evidence.class, WriterLockClass::HeldWriter);
        barrier.wait();

        first.join().expect("join first owner");
        second.join().expect("join second owner");
        assert_eq!(
            writer_lock_evidence_for_path(&first_path).class,
            WriterLockClass::Released
        );
        assert_eq!(
            writer_lock_evidence_for_path(&second_path).class,
            WriterLockClass::Released
        );
    }
}

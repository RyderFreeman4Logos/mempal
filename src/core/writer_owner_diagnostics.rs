//! Process-local SQLite writer-lock evidence.
//!
//! Label a connection as owner only after `BEGIN IMMEDIATE` succeeds. A thread
//! still waiting on BEGIN is a waiter and must not invent a foreign owner PID.
//! Contention against unlabeled queue/startup/foreign holders is
//! `UnknownExternal`; this census does not claim full lock-owner coverage.
//! ponytail: process-local fence census only; add OS lock-owner probe if live
//! unlabeled holders need a foreign PID.

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::Connection;

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
    None,
    Waiting,
    Owning,
}

struct OwnerSlot {
    pid: u32,
    operation: &'static str,
    started: Instant,
}

struct ProcessState {
    owner: Option<OwnerSlot>,
    waiters: usize,
}

thread_local! {
    static THREAD_ROLE: Cell<ThreadRole> = const { Cell::new(ThreadRole::None) };
}

fn process_state() -> &'static Mutex<ProcessState> {
    static STATE: OnceLock<Mutex<ProcessState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(ProcessState {
            owner: None,
            waiters: 0,
        })
    })
}

pub fn current_writer_lock_evidence() -> WriterLockEvidence {
    let role = THREAD_ROLE.with(Cell::get);
    let Ok(state) = process_state().lock() else {
        return WriterLockEvidence {
            class: WriterLockClass::UnknownExternal,
            owner_pid: None,
            operation: None,
            elapsed: None,
        };
    };
    if let Some(owner) = state.owner.as_ref() {
        let class = if matches!(role, ThreadRole::Owning) || state.waiters == 0 {
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
    if matches!(role, ThreadRole::Waiting) || state.waiters > 0 {
        return WriterLockEvidence {
            class: WriterLockClass::UnknownExternal,
            owner_pid: None,
            operation: None,
            elapsed: None,
        };
    }
    WriterLockEvidence {
        class: WriterLockClass::Released,
        owner_pid: None,
        operation: None,
        elapsed: None,
    }
}

pub struct WriterWaitGuard {
    operation: &'static str,
    active: bool,
}

impl WriterWaitGuard {
    pub fn enter(operation: &'static str) -> Self {
        THREAD_ROLE.with(|role| role.set(ThreadRole::Waiting));
        if let Ok(mut state) = process_state().lock() {
            state.waiters = state.waiters.saturating_add(1);
        }
        Self {
            operation,
            active: true,
        }
    }

    pub fn into_owner(mut self, conn: &Connection) -> WriterOwnerGuard<'_> {
        self.active = false;
        WriterOwnerGuard::acquire(conn, self.operation)
    }
}

impl Drop for WriterWaitGuard {
    fn drop(&mut self) {
        if self.active {
            THREAD_ROLE.with(|role| role.set(ThreadRole::None));
            if let Ok(mut state) = process_state().lock() {
                state.waiters = state.waiters.saturating_sub(1);
            }
        }
    }
}

pub struct WriterOwnerGuard<'conn> {
    conn: &'conn Connection,
    active: bool,
}

impl<'conn> WriterOwnerGuard<'conn> {
    pub fn acquire(conn: &'conn Connection, operation: &'static str) -> Self {
        THREAD_ROLE.with(|role| role.set(ThreadRole::Owning));
        if let Ok(mut state) = process_state().lock() {
            state.waiters = state.waiters.saturating_sub(1);
            state.owner = Some(OwnerSlot {
                pid: std::process::id(),
                operation,
                started: Instant::now(),
            });
        }
        Self { conn, active: true }
    }

    pub fn release(mut self) {
        self.active = false;
        clear_owner();
    }
}

impl Drop for WriterOwnerGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.conn.execute_batch("ROLLBACK");
            clear_owner();
        }
    }
}

fn clear_owner() {
    THREAD_ROLE.with(|role| role.set(ThreadRole::None));
    if let Ok(mut state) = process_state().lock() {
        state.owner = None;
    }
}

pub(crate) fn begin_immediate<'conn>(
    conn: &'conn Connection,
    operation: &'static str,
) -> rusqlite::Result<WriterOwnerGuard<'conn>> {
    let wait = WriterWaitGuard::enter(operation);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let owner = wait.into_owner(conn);
    let evidence = current_writer_lock_evidence();
    tracing::debug!(
        writer_class = ?evidence.class,
        writer_owner_pid = evidence.owner_pid,
        writer_operation = evidence.operation,
        writer_elapsed_ms = evidence.elapsed.map(|elapsed| elapsed.as_millis() as u64),
        "sqlite writer lock acquired"
    );
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
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
        let wait = WriterWaitGuard::enter("insert fenced drawer");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin immediate");
        let owner = wait.into_owner(&conn);
        let evidence = current_writer_lock_evidence();
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(evidence.owner_pid, Some(std::process::id()));
        assert_eq!(evidence.operation, Some("insert fenced drawer"));
        assert!(evidence.elapsed.is_some());
        conn.execute_batch("COMMIT").expect("commit");
        drop(owner);
        let released = current_writer_lock_evidence();
        assert_eq!(released.class, WriterLockClass::Released);
        assert_eq!(released.owner_pid, None);
        assert_eq!(released.operation, None);
    }

    #[test]
    fn rollback_releases_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let wait = WriterWaitGuard::enter("complete queued operation");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin immediate");
        let owner = wait.into_owner(&conn);
        assert_eq!(
            current_writer_lock_evidence().class,
            WriterLockClass::HeldWriter
        );
        conn.execute_batch("ROLLBACK").expect("rollback");
        drop(owner);
        assert_eq!(
            current_writer_lock_evidence().class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn panic_drop_releases_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let wait = WriterWaitGuard::enter("claim queued message");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("begin immediate");
            let _owner = wait.into_owner(&conn);
            assert_eq!(
                current_writer_lock_evidence().class,
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
            current_writer_lock_evidence().class,
            WriterLockClass::Released
        );
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
        let wait = WriterWaitGuard::enter("insert fenced drawer");
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
            let wait = WriterWaitGuard::enter("claim queued message");
            ready_tx.send(current_writer_lock_evidence()).expect("send");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("waiter acquires after holder release");
            let owner = wait.into_owner(&conn);
            let evidence = current_writer_lock_evidence();
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
        let owner_evidence = current_writer_lock_evidence();
        assert_eq!(owner_evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(owner_evidence.owner_pid, Some(std::process::id()));
        holder.execute_batch("ROLLBACK").expect("rollback holder");
        drop(owner);
        let acquired_evidence = waiter.join().expect("join waiter");
        assert_eq!(acquired_evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(acquired_evidence.operation, Some("claim queued message"));
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            current_writer_lock_evidence().class,
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
        let wait = WriterWaitGuard::enter("claim queued message");
        waiter
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("unlabeled holder blocks waiter");
        let evidence = current_writer_lock_evidence();
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::UnknownExternal);
        assert_eq!(evidence.owner_pid, None);
        assert_eq!(evidence.operation, None);
        drop(wait);
        holder.execute_batch("ROLLBACK").expect("rollback");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            current_writer_lock_evidence().class,
            WriterLockClass::Released
        );
    }

    #[test]
    fn successful_immediate_helper_labels_held_writer() {
        let _lock = lock_process_evidence();
        let conn = Connection::open_in_memory().expect("memory db");
        let owner = begin_immediate(&conn, "complete queued operation").expect("begin");
        let evidence = current_writer_lock_evidence();
        assert_privacy(&evidence);
        assert_eq!(evidence.class, WriterLockClass::HeldWriter);
        assert_eq!(evidence.owner_pid, Some(std::process::id()));
        assert_eq!(evidence.operation, Some("complete queued operation"));
        conn.execute_batch("COMMIT").expect("commit");
        drop(owner);
        assert_eq!(
            current_writer_lock_evidence().class,
            WriterLockClass::Released
        );
    }
}

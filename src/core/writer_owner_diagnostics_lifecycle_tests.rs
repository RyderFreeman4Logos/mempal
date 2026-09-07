use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tracing_subscriber::fmt::MakeWriter;

static COMMIT_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
static ROLLBACK_ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

fn assert_no_tracked_owner(evidence: &WriterLockEvidence) {
    assert_eq!(evidence.class, WriterLockClass::NoTrackedLocalOwner);
    assert_eq!(evidence.owner_pid, None);
    assert_eq!(evidence.operation, None);
    assert_eq!(evidence.elapsed, None);
}

fn count_transaction_end(context: AuthContext<'_>) -> Option<TransactionOperation> {
    match context.action {
        AuthAction::Transaction { operation }
            if !matches!(operation, TransactionOperation::Begin) =>
        {
            match operation {
                TransactionOperation::Unknown => {
                    COMMIT_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
                }
                TransactionOperation::Rollback => {
                    ROLLBACK_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
                }
                TransactionOperation::Begin => {}
                _ => {}
            }
            Some(operation)
        }
        _ => None,
    }
}

fn deny_commit(context: AuthContext<'_>) -> Authorization {
    if matches!(
        count_transaction_end(context),
        Some(TransactionOperation::Unknown)
    ) {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

fn deny_rollback(context: AuthContext<'_>) -> Authorization {
    if matches!(
        count_transaction_end(context),
        Some(TransactionOperation::Rollback)
    ) {
        Authorization::Deny
    } else {
        Authorization::Allow
    }
}

fn count_transaction_end_allow(context: AuthContext<'_>) -> Authorization {
    count_transaction_end(context);
    Authorization::Allow
}

fn reset_attempts() {
    COMMIT_ATTEMPTS.store(0, Ordering::Relaxed);
    ROLLBACK_ATTEMPTS.store(0, Ordering::Relaxed);
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(Arc::clone(&self.0))
    }
}

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        )
        .expect("tracing output must be UTF-8")
    }
}

#[test]
fn failed_begin_only_removes_waiter_for_busy_and_non_lock_errors() {
    let _lock = lock_process_evidence();
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("failed-begin.db");
    let holder = Connection::open(&path).expect("holder");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold writer");

    let contender = Connection::open(&path).expect("contender");
    contender
        .busy_timeout(Duration::ZERO)
        .expect("busy timeout");
    assert!(
        begin_immediate(&contender, "busy acquisition").is_err(),
        "raw holder must block acquisition"
    );
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
    holder.execute_batch("ROLLBACK").expect("release holder");

    contender
        .execute_batch("BEGIN DEFERRED")
        .expect("open deferred transaction");
    assert!(
        begin_immediate(&contender, "nested acquisition").is_err(),
        "nested BEGIN must fail without acquiring a writer"
    );
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
    contender.execute_batch("ROLLBACK").expect("cleanup");
}

#[test]
fn commit_error_preserves_native_automatic_rollback_once() {
    let _lock = lock_process_evidence();
    reset_attempts();
    let mut conn = Connection::open_in_memory().expect("memory db");
    conn.authorizer(Some(deny_commit));
    let tx = transaction_immediate(&mut conn, "commit denied once").expect("begin");

    tx.commit().expect_err("COMMIT must be denied");

    assert_eq!(COMMIT_ATTEMPTS.load(Ordering::Relaxed), 1);
    assert_eq!(ROLLBACK_ATTEMPTS.load(Ordering::Relaxed), 1);
    assert!(conn.is_autocommit(), "native Drop rollback must succeed");
    assert_no_tracked_owner(&current_writer_lock_evidence(&conn));
}

#[test]
fn denied_native_drop_attempts_rollback_once_and_forgets_current_owner() {
    let _lock = lock_process_evidence();
    reset_attempts();
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("denied-drop.db");
    let mut conn = Connection::open(&path).expect("database");
    let identity = identity_for_connection(&conn);
    conn.authorizer(Some(deny_rollback));
    let tx = transaction_immediate(&mut conn, "drop denied once").expect("begin");
    let output = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || drop(tx));

    assert_eq!(COMMIT_ATTEMPTS.load(Ordering::Relaxed), 0);
    assert_eq!(ROLLBACK_ATTEMPTS.load(Ordering::Relaxed), 1);
    assert!(
        !conn.is_autocommit(),
        "denied native rollback remains active"
    );
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
    assert!(
        !process_state()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .databases
            .contains_key(&identity),
        "connection-address reuse cannot inherit an entry that was deleted"
    );
    let log = output.text();
    assert_eq!(
        log.matches("SQLite writer cleanup remains unverified")
            .count(),
        1
    );
    assert!(log.contains("operation=\"drop denied once\""));
    assert!(!log.contains(path.to_string_lossy().as_ref()));
    assert!(!log.contains("ROLLBACK"));

    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    conn.execute_batch("ROLLBACK").expect("raw cleanup");
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
    drop(conn);
    let reopened = Connection::open(&path).expect("reopen same inode");
    assert_no_tracked_owner(&current_writer_lock_evidence(&reopened));
}

#[test]
fn stale_finalizer_cannot_erase_same_database_successor() {
    let _lock = lock_process_evidence();
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("handoff.db");
    let first = Connection::open(&path).expect("first");
    let second = Connection::open(&path).expect("second");
    let third = Connection::open(&path).expect("third");

    let first_owner = begin_immediate(&first, "first owner").expect("first begin");
    first.execute_batch("COMMIT").expect("first commit");
    let blocked_waiter = WriterWaitGuard::enter(&third, "blocked waiter");
    let second_owner = begin_immediate(&second, "successor owner").expect("second begin");

    first_owner.release();
    let evidence = current_writer_lock_evidence(&second);
    assert_eq!(evidence.class, WriterLockClass::HeldWriter);
    assert_eq!(evidence.operation, Some("successor owner"));

    second.execute_batch("ROLLBACK").expect("second rollback");
    second_owner.release();
    drop(blocked_waiter);
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
}

#[test]
fn manual_guard_panic_observes_without_adding_rollback() {
    let _lock = lock_process_evidence();
    reset_attempts();
    let conn = Connection::open_in_memory().expect("memory db");
    conn.authorizer(Some(count_transaction_end_allow));

    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _owner = begin_immediate(&conn, "manual panic").expect("begin");
        panic!("force manual guard drop");
    }));

    assert!(panicked.is_err());
    assert_eq!(ROLLBACK_ATTEMPTS.load(Ordering::Relaxed), 0);
    assert!(
        !conn.is_autocommit(),
        "diagnostics must preserve the pre-instrumentation manual transaction state"
    );
    assert_no_tracked_owner(&current_writer_lock_evidence(&conn));
    conn.execute_batch("ROLLBACK").expect("raw cleanup");
}

#[test]
fn external_lock_without_local_activity_has_no_tracked_local_owner() {
    let _lock = lock_process_evidence();
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("external.db");
    let holder = Connection::open(&path).expect("holder");
    let waiter = Connection::open(&path).expect("waiter");
    holder
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold raw lock");

    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));
    let wait = WriterWaitGuard::enter(&waiter, "waiting for external holder");
    let evidence = current_writer_lock_evidence(&waiter);
    assert_eq!(evidence.class, WriterLockClass::UnknownExternal);
    assert_eq!(evidence.owner_pid, None);
    drop(wait);
    assert_no_tracked_owner(&writer_lock_evidence_for_path(&path));

    holder.execute_batch("ROLLBACK").expect("cleanup");
}

#[test]
fn stall_event_keeps_historical_error_age_separate_from_owner_evidence() {
    struct Diagnostic {
        queued_count: u64,
        seconds_since_successful_write: u64,
        last_error: &'static str,
        last_error_age_secs: Option<u64>,
        uptime_secs: u64,
        writer_evidence: WriterLockEvidence,
    }

    let output = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(output.clone())
        .finish();
    let diagnostic = Diagnostic {
        queued_count: 1,
        seconds_since_successful_write: 300,
        last_error: "database is locked",
        last_error_age_secs: Some(37),
        uptime_secs: 600,
        writer_evidence: no_tracked_owner_evidence(),
    };

    assert_eq!(diagnostic.last_error_age_secs, Some(37));
    tracing::subscriber::with_default(subscriber, || {
        writer_stall_event!(tracing::warn, diagnostic, "writer stall probe");
    });

    let log = output.text();
    assert!(log.contains("last_error_age_secs=37"), "{log}");
    assert!(log.contains("NoTrackedLocalOwner"), "{log}");
}

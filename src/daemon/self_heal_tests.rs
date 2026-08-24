use std::io::{self, Write};
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::hook::HookEvent;

struct ShutdownResetGuard;

impl ShutdownResetGuard {
    fn new() -> Self {
        super::super::reset_shutdown_request();
        Self
    }
}

impl Drop for ShutdownResetGuard {
    fn drop(&mut self) {
        super::super::reset_shutdown_request();
    }
}

// These tests share test-only handler counters and the process-wide shutdown flag.
static HOOK_IPC_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn lock_hook_ipc_tests() -> (
    tokio::sync::MutexGuard<'static, ()>,
    tokio::sync::OwnedMutexGuard<()>,
) {
    let handler_guard = HOOK_IPC_TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let shutdown_guard = super::super::global_shutdown_test_lock().lock_owned().await;
    (handler_guard, shutdown_guard)
}

fn short_tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new_in("/tmp").expect("short tempdir")
}

struct LogCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("log buffer mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn install_log_capture() -> (Arc<Mutex<Vec<u8>>>, tracing::dispatcher::DefaultGuard) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || LogCapture {
            buffer: Arc::clone(&writer_logs),
        })
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (logs, guard)
}

fn captured_logs(logs: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(logs.lock().expect("log mutex poisoned").clone()).expect("utf8 logs")
}

#[cfg(target_os = "linux")]
fn pending_message_total(db_path: &Path) -> i64 {
    rusqlite::Connection::open(db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending messages")
}

#[cfg(target_os = "linux")]
fn sqlite_db_fd_count(db_path: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read process fd directory")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|path| path == db_path)
        .count()
}

#[cfg(target_os = "linux")]
async fn wait_for_active_handler_count(expected: usize, label: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (active, _) = super::hook_ipc_handler_counts_for_test();
            if active == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let (active, peak) = super::hook_ipc_handler_counts_for_test();
        panic!(
            "hook IPC active handler count did not return to {expected} after {label}; active={active}, peak={peak}"
        );
    });
}

#[cfg(target_os = "linux")]
async fn send_hook_ipc_client(
    mempal_home: PathBuf,
    request: crate::hook_ipc::HookIpcEnqueueRequest,
) -> crate::hook_ipc::HookIpcClientOutcome {
    tokio::time::timeout(
        Duration::from_secs(3),
        tokio::task::spawn_blocking(move || {
            crate::hook_ipc::enqueue_with_timeout(
                &mempal_home,
                request,
                crate::hook_ipc::HOOK_IPC_TIMEOUT,
            )
        }),
    )
    .await
    .expect("hook IPC client should finish within bounded time")
    .expect("hook IPC client blocking task should not panic")
}

#[cfg(target_os = "linux")]
async fn send_contention_wave(
    mempal_home: &Path,
    wave: usize,
    count: usize,
) -> Vec<(
    crate::hook_ipc::HookIpcEnqueueRequest,
    crate::hook_ipc::HookIpcClientOutcome,
)> {
    let mut requests = Vec::with_capacity(count);
    let mut tasks = Vec::with_capacity(count);
    for attempt in 0..count {
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
            HookEvent::UserPromptSubmit.queue_kind(),
            &format!(r#"{{"event":"UserPromptSubmit","wave":{wave},"attempt":{attempt}}}"#),
        );
        let client_request = request.clone();
        requests.push(request);
        tasks.push(send_hook_ipc_client(
            mempal_home.to_path_buf(),
            client_request,
        ));
    }

    let mut outcomes = Vec::with_capacity(count);
    for (request, task) in requests.into_iter().zip(tasks) {
        outcomes.push((request, task.await));
    }
    outcomes
}

#[cfg(target_os = "linux")]
fn assert_all_accepted(
    outcomes: &[(
        crate::hook_ipc::HookIpcEnqueueRequest,
        crate::hook_ipc::HookIpcClientOutcome,
    )],
) {
    for (_, outcome) in outcomes {
        assert!(
            matches!(outcome, crate::hook_ipc::HookIpcClientOutcome::Accepted),
            "locked contention wave must ACK from the durable spool"
        );
    }
}

async fn send_hook_ipc_request(
    store: AsyncPendingMessageStore,
    observer: crate::daemon_bootstrap::DaemonWriteObserver,
    spool: Arc<crate::ingress_spool::IngressSpool>,
    request: crate::hook_ipc::HookIpcEnqueueRequest,
) -> crate::hook_ipc::HookIpcEnqueueResponse {
    let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let handler = tokio::spawn(super::handle_hook_ipc_connection(
        server, store, observer, spool,
    ));
    let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
    frame.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush request");

    let mut reader = tokio::io::BufReader::new(client);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .expect("read response");
    handler.await.expect("handler task");
    serde_json::from_str(line.trim()).expect("hook IPC response")
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_hook_ipc_readiness_does_not_persist_queue_row() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let handler = tokio::spawn(super::handle_hook_ipc_connection(
        server,
        store,
        observer,
        Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path())),
    ));
    tokio::io::AsyncWriteExt::write_all(&mut client, b"{\"probe\":\"readiness\"}\n")
        .await
        .expect("write readiness request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush readiness request");

    let mut reader = tokio::io::BufReader::new(client);
    let mut line = String::new();
    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
        .await
        .expect("read readiness response");
    handler.await.expect("handler task");
    let response = crate::hook_ipc::HookIpcResponse::Readiness(
        serde_json::from_str(line.trim()).expect("hook IPC readiness response"),
    );
    assert!(matches!(
        response,
        crate::hook_ipc::HookIpcResponse::Readiness(
            crate::hook_ipc::HookIpcReadinessResponse::Ready { .. }
        )
    ));
    assert_eq!(pending_message_total(&db_path), 0);
}

#[tokio::test]
async fn test_hook_ipc_ack_requires_sqlite_persistence() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"durable before ack"}"#,
    );

    let response = send_hook_ipc_request(
        store,
        observer,
        Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path())),
        request,
    )
    .await;
    assert_eq!(response, crate::hook_ipc::HookIpcEnqueueResponse::Accepted);
    let (kind, payload): (String, String) = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT kind, payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query persisted IPC message");
    assert_eq!(kind, HookEvent::UserPromptSubmit.queue_kind());
    assert!(
        payload.contains("durable before ack"),
        "stored payload missing expected marker"
    );
}

#[tokio::test]
async fn test_hook_ipc_spools_before_ack_when_sqlite_locked() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let lock_conn = rusqlite::Connection::open(&db_path).expect("open lock connection");
    lock_conn
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"durable after lock"}"#,
    );
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));

    let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let handler = tokio::spawn(super::handle_hook_ipc_connection(
        server,
        store,
        observer,
        spool.clone(),
    ));
    wait_for_active_handler_count(1, "starting locked SQLite enqueue").await;
    let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
    frame.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush request");

    let response = tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, async {
        let mut reader = tokio::io::BufReader::new(client);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read response");
        handler.await.expect("handler task");
        serde_json::from_str(line.trim()).expect("hook IPC response")
    })
    .await
    .expect("locked SQLite enqueue must ACK from the fsynced spool");
    match response {
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted => {}
        crate::hook_ipc::HookIpcEnqueueResponse::Error { message } => {
            panic!("durable spool should ACK before SQLite replay: {message}")
        }
    }
    let count_while_locked: i64 = rusqlite::Connection::open(&db_path)
        .expect("open read connection")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending while locked");
    assert_eq!(count_while_locked, 0);

    lock_conn.execute_batch("ROLLBACK;").expect("release lock");
    let replay_store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    assert_eq!(
        spool.drain_once(&replay_store).await.expect("replay spool"),
        1
    );
    let stored_id =
        PendingMessageStore::idempotent_message_id(&request.kind, &request.idempotency_key);
    let (count_after_unlock, actual_id): (i64, String) = rusqlite::Connection::open(&db_path)
        .expect("open read connection")
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(id), '') FROM pending_messages",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query pending after unlock");
    assert_eq!(count_after_unlock, 1);
    assert_eq!(stored_id, actual_id);
}

#[tokio::test]
async fn test_hook_ipc_stalled_request_times_out_without_persisting() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let (client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let handler = tokio::spawn(super::handle_hook_ipc_connection(
        server,
        store,
        observer,
        Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path())),
    ));

    let mut reader = tokio::io::BufReader::new(client);
    let mut line = String::new();
    let bytes_read = tokio::time::timeout(
        crate::hook_ipc::HOOK_IPC_READ_TIMEOUT + Duration::from_secs(1),
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line),
    )
    .await
    .expect("stalled request must receive timeout response")
    .expect("read response");
    assert!(bytes_read > 0, "daemon should write an error response");

    handler.await.expect("handler task");
    match serde_json::from_str::<crate::hook_ipc::HookIpcEnqueueResponse>(line.trim())
        .expect("hook IPC response")
    {
        crate::hook_ipc::HookIpcEnqueueResponse::Accepted => {
            panic!("stalled IPC request must not be accepted")
        }
        crate::hook_ipc::HookIpcEnqueueResponse::Error { message } => {
            assert!(message.contains("timed out reading frame"), "{message}");
        }
    }

    let count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_hook_ipc_timeout_fallback_is_idempotent_with_slow_daemon_persist() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");

    let kind = HookEvent::UserPromptSubmit.queue_kind().to_string();
    let payload =
        r#"{"event":"UserPromptSubmit","payload":"timeout fallback same capture"}"#.to_string();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(&kind, &payload);
    let idempotency_key = request.idempotency_key.clone();

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_blocking_delay(crate::hook_ipc::HOOK_IPC_TIMEOUT + Duration::from_millis(200));
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let handler = tokio::spawn(super::handle_hook_ipc_connection(
        server,
        store,
        observer,
        Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path())),
    ));

    let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
    frame.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush request");

    let timed_out = tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, async move {
        let mut reader = tokio::io::BufReader::new(client);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await
    })
    .await;
    assert!(
        timed_out.is_err(),
        "client should time out before daemon persist"
    );

    let fallback_store = PendingMessageStore::new_without_reclaim(&db_path);
    let fallback_id = fallback_store
        .enqueue_idempotent_with_key(&kind, &payload, &idempotency_key)
        .expect("fallback enqueue");

    handler.await.expect("handler task");

    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending");
    assert_eq!(
        count, 1,
        "daemon and fallback must collapse the same capture"
    );
    let (stored_id, stored_kind, stored_payload): (String, String, String) = conn
        .query_row(
            "SELECT id, kind, payload FROM pending_messages LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read pending row");
    assert_eq!(stored_id, fallback_id);
    assert_eq!(stored_kind, kind);
    assert!(
        stored_payload == payload,
        "stored payload did not match expected request body"
    );
}

#[tokio::test]
async fn test_hook_ipc_closed_peer_write_is_debug_only() {
    let _test_guard = lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");

    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","payload":"closed peer synthetic"}"#,
    );
    let (mut client, server) = tokio::net::UnixStream::pair().expect("unix stream pair");
    let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
    frame.push(b'\n');
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush request");
    drop(client);

    let (logs, _log_guard) = install_log_capture();
    super::handle_hook_ipc_connection(
        server,
        store,
        observer,
        Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path())),
    )
    .await;
    let logs = captured_logs(&logs);
    assert!(
        logs.contains("hook IPC client disconnected before response"),
        "{logs}"
    );
    assert!(!logs.contains("WARN"), "{logs}");
    let count: i64 = rusqlite::Connection::open(&db_path)
        .expect("open sqlite")
        .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
            row.get(0)
        })
        .expect("count pending");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_hook_ipc_listener_bounds_active_handlers() {
    let _test_guard = lock_hook_ipc_tests().await;
    let _shutdown_guard = ShutdownResetGuard::new();
    super::reset_hook_ipc_handler_counters_for_test();
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("open db");
    let (listener, socket_guard) =
        crate::hook_ipc::bind_listener(&mempal_home).expect("bind hook IPC listener");
    let socket_path = socket_guard.path().to_path_buf();
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path)
        .with_blocking_delay(Duration::from_millis(250));
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(&mempal_home));
    let listener_task = tokio::spawn(super::run_hook_ipc_listener(
        listener, store, observer, spool,
    ));
    let mut clients = Vec::new();

    for attempt in 0..(super::HOOK_IPC_HANDLER_LIMIT + 8) {
        let mut client = match tokio::time::timeout(
            Duration::from_millis(100),
            tokio::net::UnixStream::connect(&socket_path),
        )
        .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(_)) | Err(_) => continue,
        };
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new(
            HookEvent::UserPromptSubmit.queue_kind(),
            &format!(r#"{{"event":"UserPromptSubmit","attempt":{attempt}}}"#),
        );
        let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
        frame.push(b'\n');
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .expect("write request");
        tokio::io::AsyncWriteExt::flush(&mut client)
            .await
            .expect("flush request");
        clients.push(client);
    }

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (_, peak) = super::hook_ipc_handler_counts_for_test();
            if peak >= super::HOOK_IPC_HANDLER_LIMIT {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("listener should reach the declared handler limit");
    let (_, peak) = super::hook_ipc_handler_counts_for_test();
    assert!(
        peak <= super::HOOK_IPC_HANDLER_LIMIT,
        "active handler peak {peak} exceeded limit {}",
        super::HOOK_IPC_HANDLER_LIMIT
    );

    drop(clients);
    super::super::request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), listener_task)
        .await
        .expect("listener should stop within bounded drain")
        .expect("listener task should not panic");
    let (active, _) = super::hook_ipc_handler_counts_for_test();
    assert_eq!(active, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_hook_ipc_listener_recovers_after_real_sqlite_contention() {
    let _test_guard = lock_hook_ipc_tests().await;
    let _shutdown_guard = ShutdownResetGuard::new();
    super::reset_hook_ipc_handler_counters_for_test();
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("open db");
    let (listener, _socket_guard) =
        crate::hook_ipc::bind_listener(&mempal_home).expect("bind hook IPC listener");
    let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
    assert_eq!(sqlite_db_fd_count(&db_path), 0);
    let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
    let spool = Arc::new(crate::ingress_spool::IngressSpool::new(&mempal_home));
    let listener_task = tokio::spawn(super::run_hook_ipc_listener(
        listener,
        store.clone(),
        observer,
        spool,
    ));

    let lock_conn = rusqlite::Connection::open(&db_path).expect("open lock connection");
    lock_conn
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");

    let mut spooled_count = 0_usize;
    for wave in 0..2 {
        let wave_outcomes = send_contention_wave(&mempal_home, wave, 4).await;
        assert_all_accepted(&wave_outcomes);
        spooled_count += wave_outcomes.len();
        wait_for_active_handler_count(0, "contention wave").await;
        assert_eq!(pending_message_total(&db_path), 0);
        let fd_count = sqlite_db_fd_count(&db_path);
        assert!(
            fd_count <= 4,
            "SQLite DB fd count grew during contention: {fd_count}"
        );
    }

    lock_conn.execute_batch("ROLLBACK;").expect("release lock");
    drop(lock_conn);
    let expected_spooled = i64::try_from(spooled_count).expect("spooled request count fits i64");
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if pending_message_total(&db_path) == expected_spooled {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spool records should replay after SQLite contention");
    assert_eq!(pending_message_total(&db_path), expected_spooled);

    let sentinel_request = crate::hook_ipc::HookIpcEnqueueRequest::new(
        HookEvent::UserPromptSubmit.queue_kind(),
        r#"{"event":"UserPromptSubmit","sentinel":"daemon-recovered"}"#,
    );
    let sentinel_outcome =
        send_hook_ipc_client(mempal_home.clone(), sentinel_request.clone()).await;
    assert_eq!(
        sentinel_outcome,
        crate::hook_ipc::HookIpcClientOutcome::Accepted
    );
    wait_for_active_handler_count(0, "sentinel recovery request").await;
    assert_eq!(
        pending_message_total(&db_path),
        i64::try_from(spooled_count + 1).expect("request count fits i64")
    );

    let claimed = store
        .claim_next("daemon-linux-contention-test".to_string(), 60)
        .await
        .expect("claim after contention")
        .expect("claim should recover after contention");
    store
        .confirm(claimed)
        .await
        .expect("confirm after contention");
    let recovered_fd_count = sqlite_db_fd_count(&db_path);
    assert!(
        recovered_fd_count <= 4,
        "daemon queue DB fd count should return to fixed claim/writer baseline: {recovered_fd_count}"
    );

    let claimed_again = store
        .claim_next("daemon-linux-contention-test".to_string(), 60)
        .await
        .expect("second claim after contention")
        .expect("second claim should reuse queue connections");
    store
        .confirm(claimed_again)
        .await
        .expect("second confirm after contention");
    assert_eq!(
        sqlite_db_fd_count(&db_path),
        recovered_fd_count,
        "daemon queue DB fd count should not grow after recovery"
    );

    super::super::request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), listener_task)
        .await
        .expect("listener should stop within bounded drain")
        .expect("listener task should not panic");
    let (active, _) = super::hook_ipc_handler_counts_for_test();
    assert_eq!(active, 0);
}

use std::sync::Arc;

use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::hook::HookEvent;

#[tokio::test]
async fn test_hook_ipc_spools_before_ack_when_sqlite_locked() {
    let _test_guard = super::lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = tempfile::TempDir::new().expect("short tempdir");
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
    super::wait_for_active_handler_count(1, "starting locked SQLite enqueue").await;
    let mut frame = serde_json::to_vec(&request).expect("serialize hook IPC request");
    frame.push(b'\n');
    super::reset_hook_ipc_ack_checkpoints_for_test();
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .expect("write request");
    tokio::io::AsyncWriteExt::flush(&mut client)
        .await
        .expect("flush request");

    let mut reader = tokio::io::BufReader::new(client);
    let mut line = String::new();
    let response = match tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, async {
        tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
            .await
            .expect("read response");
        serde_json::from_str(line.trim()).expect("hook IPC response")
    })
    .await
    {
        Ok(response) => {
            eprintln!(
                "hook_ipc_ack_checkpoints ok {}",
                super::hook_ipc_ack_checkpoint_report_for_test()
            );
            response
        }
        Err(_) => panic!(
            "locked SQLite enqueue must ACK from the fsynced spool: Elapsed(()) {}",
            super::hook_ipc_ack_checkpoint_report_for_test()
        ),
    };
    tokio::time::timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT, handler)
        .await
        .expect("hook IPC handler must finish after ACK")
        .expect("handler task");
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

// Withhold the current-thread runtime's continuation after dispatch. A real
// idempotent append takes the same spool mutex and confirms durable completion;
// the observation must not mistake an unpolled JoinHandle for unfinished fsync.
#[test]
fn append_checkpoint_precedes_runtime_resumption() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("controlled fixture runtime");
    runtime.block_on(async {
        let _guard = super::lock_hook_ipc_tests().await;
        let tmp = tempfile::tempdir().expect("fixture dir");
        let store = AsyncPendingMessageStore::new_without_reclaim(tmp.path().join("palace.db"));
        let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
        let spool = crate::ingress_spool::IngressSpool::new(tmp.path());
        let request = crate::hook_ipc::HookIpcEnqueueRequest::new("fixture", "fixture");
        let control = request.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv_timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT);
        });
        entered_rx
            .await
            .expect("blocking worker owns the only slot");
        let mut persist = Box::pin(super::persist_hook_ipc_request(
            &store, &spool, &observer, request,
        ));
        std::future::poll_fn(|cx| {
            assert!(
                persist.as_mut().poll(cx).is_pending(),
                "first poll dispatches append"
            );
            std::task::Poll::Ready(())
        })
        .await;

        drop(release_tx);
        // Do not yield the async continuation. Bound only the fixture discovery;
        // this is not a larger IPC deadline or a substitute for the 250ms ACK test.
        let deadline = std::time::Instant::now() + crate::hook_ipc::HOOK_IPC_READ_TIMEOUT;
        let directory = tmp.path().join(crate::ingress_spool::INGRESS_SPOOL_DIR);
        let published = loop {
            if std::fs::read_dir(&directory).is_ok_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "json")
                })
            }) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        if published {
            spool
                .append(&control)
                .expect("same-key durable completion control");
        }
        let before_resume = loop {
            let report = super::hook_ipc_ack_checkpoint_report_for_test();
            if !report.contains("append_return=none") || std::time::Instant::now() >= deadline {
                break report;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        };
        // Always await the owned append before asserting or removing its fixture.
        let response = persist.await;
        blocker.await.expect("release blocking worker");
        assert!(published, "blocking append never published its record");
        assert_eq!(response, crate::hook_ipc::HookIpcEnqueueResponse::Accepted);
        assert!(
            before_resume.contains("append_resumed=none"),
            "{before_resume}"
        );
        assert!(
            !before_resume.contains("append_return=none"),
            "{before_resume}"
        );
    });
}

// A real client times out while the append is queued; fallback and late replay
// must use one identity, while a distinct intentional identical capture survives.
#[test]
fn hook_ipc_timeout_fallback_and_late_append_share_identity() {
    use std::os::fd::AsRawFd;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("fixture runtime");
    std::thread::scope(|scope| {
        let scenario = async {
            let _guard = super::lock_hook_ipc_tests().await;
            let tmp = tempfile::tempdir().expect("fixture directory");
            let db_path = tmp.path().join("palace.db");
            Database::open(&db_path).expect("initialize fixture");
            let fallback_store = PendingMessageStore::new(&db_path).expect("fallback store");
            let store = AsyncPendingMessageStore::new_without_reclaim(&db_path);
            let spool = Arc::new(crate::ingress_spool::IngressSpool::new(tmp.path()));
            let observer = crate::daemon_bootstrap::DaemonWriteObserver::for_test();
            let directory = std::fs::File::open(tmp.path()).expect("pin socket directory");
            let home = std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
            let (listener, _socket) = crate::hook_ipc::bind_listener(&home).expect("listener");
            let request = crate::hook_ipc::HookIpcEnqueueRequest::new("fixture", "{}");
            let control = request.clone();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = release_rx.recv_timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT);
            });
            entered_rx.await.expect("own sole blocking slot");
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            super::reset_hook_ipc_ack_checkpoints_for_test();
            let client = scope.spawn(move || {
                let result = crate::hook_ipc::enqueue_with_default_timeout(&home, request);
                let _ = done_tx.send(result);
            });
            let (stream, _) =
                tokio::time::timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT, listener.accept())
                    .await
                    .expect("client connects")
                    .expect("accept");
            let handler = tokio::spawn(super::handle_hook_ipc_connection(
                stream,
                store.clone(),
                observer,
                spool.clone(),
            ));
            let outcome =
                tokio::time::timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT, done_rx).await;
            let queued = super::hook_ipc_ack_checkpoint_report_for_test();
            // Match the actual hook's uncertain-delivery classification and write
            // primitive. Perform the fallback BEFORE releasing the late append.
            let fallback = match &outcome {
                Ok(Ok(crate::hook_ipc::HookIpcClientOutcome::Fallback(reason)))
                    if reason.may_have_reached_daemon() =>
                {
                    fallback_store.enqueue_idempotent_with_key(
                        &control.kind,
                        &control.payload,
                        &control.idempotency_key,
                    )
                }
                _ => fallback_store.enqueue(&control.kind, &control.payload),
            };
            let fallback_row: (i64, Option<String>) = rusqlite::Connection::open(&db_path)
                .expect("inspect committed fallback")
                .query_row(
                    "SELECT COUNT(*), MIN(id) FROM pending_messages",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("fallback identity before append release");
            drop(release_tx);
            blocker.await.expect("reap blocking owner");
            tokio::time::timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT, handler)
                .await
                .expect("handler cleanup")
                .expect("handler task");
            client.join().expect("reap client");
            assert_eq!(
                outcome.expect("client watchdog").expect("client result"),
                crate::hook_ipc::HookIpcClientOutcome::Fallback(
                    crate::hook_ipc::HookIpcFallbackReason::Timeout
                )
            );
            assert!(queued.contains("last=blocking_submit"), "{queued}");
            assert!(queued.contains("blocking_closure_enter=none"), "{queued}");
            assert!(queued.contains("append_return=none"), "{queued}");
            fallback.expect("fallback write");
            assert_eq!(
                fallback_row,
                (
                    1,
                    Some(PendingMessageStore::idempotent_message_id(
                        &control.kind,
                        &control.idempotency_key,
                    ))
                ),
                "original-key fallback must commit before late append runs"
            );
            // Confirm namespace durability through the existing same-key append,
            // then replay with a fresh spool owner (not a response-attempt marker).
            assert_eq!(
                spool
                    .append(&control)
                    .expect("durable same-key confirmation"),
                crate::ingress_spool::AppendOutcome::AlreadyPresent,
                "confirmation must not create a missing daemon record"
            );
            let restarted = crate::ingress_spool::IngressSpool::new(tmp.path());
            assert_eq!(restarted.drain_once(&store).await.expect("late replay"), 1);
            let connection = rusqlite::Connection::open(&db_path).expect("verify database");
            let (count, id): (i64, String) = connection
                .query_row(
                    "SELECT COUNT(*), MIN(id) FROM pending_messages",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("original identity");
            assert_eq!(count, 1, "fallback plus daemon replay is one mutation");
            assert_eq!(
                id,
                PendingMessageStore::idempotent_message_id(&control.kind, &control.idempotency_key)
            );
            let distinct =
                crate::hook_ipc::HookIpcEnqueueRequest::new(&control.kind, &control.payload);
            assert_ne!(distinct.idempotency_key, control.idempotency_key);
            restarted
                .append(&distinct)
                .expect("separate identical intent");
            assert_eq!(
                restarted.drain_once(&store).await.expect("distinct replay"),
                1
            );
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
                    row.get(0)
                })
                .expect("distinct count");
            assert_eq!(count, 2);
            eprintln!(
                "hook_ipc_timeout_contract queued=[{queued}] fallback_plus_late=1 distinct_intent_total={count}"
            );
        };
        runtime.block_on(scenario);
    });
}

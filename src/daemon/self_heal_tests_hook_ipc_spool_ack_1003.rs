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

#[test]
fn blocking_pool_queue_is_distinct_from_append_execution() {
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
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = entered_tx.send(());
            // Sender drop also releases the owned worker during panic cleanup.
            let _ = release_rx.recv_timeout(crate::hook_ipc::HOOK_IPC_READ_TIMEOUT);
        });
        entered_rx
            .await
            .expect("blocking worker owns the only slot");
        super::reset_hook_ipc_ack_checkpoints_for_test();
        let mut persist = Box::pin(super::persist_hook_ipc_request(
            &store, &spool, &observer, request,
        ));
        let result = tokio::time::timeout(crate::hook_ipc::HOOK_IPC_TIMEOUT, &mut persist).await;
        let queued = super::hook_ipc_ack_checkpoint_report_for_test();
        drop(release_tx);
        blocker.await.expect("release blocking worker");
        let timed_out = result.is_err();
        let response = match result {
            Ok(response) => response,
            Err(_) => persist.await,
        };
        assert!(timed_out, "queued work cannot ACK before it executes");
        assert!(queued.contains("last=blocking_submit"), "{queued}");
        assert!(queued.contains("blocking_closure_enter=none"), "{queued}");
        assert!(queued.contains("append_return=none"), "{queued}");
        assert_eq!(response, crate::hook_ipc::HookIpcEnqueueResponse::Accepted);
    });
}

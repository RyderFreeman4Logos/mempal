use std::sync::Arc;

use crate::core::db::Database;
use crate::core::queue::{AsyncPendingMessageStore, PendingMessageStore};
use crate::hook::HookEvent;

#[tokio::test]
async fn test_hook_ipc_spools_before_ack_when_sqlite_locked() {
    let _test_guard = super::lock_hook_ipc_tests().await;
    super::super::SHUTDOWN_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
    let tmp = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
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

use std::time::Instant;

use anyhow::{Context, Result, bail};

use super::RuntimeWriterLeaseHandle;
use crate::core::AsyncDb;

impl RuntimeWriterLeaseHandle {
    pub(in crate::daemon) fn release<'a>(
        mut self,
        async_db: &'a AsyncDb,
    ) -> impl std::future::Future<Output = Result<()>> + 'a {
        self.heartbeat.abort();
        self.release_handed_off = true;
        async move {
            let _ = (&mut self.heartbeat).await;
            let lease = self.lease.clone();
            let deadline = Instant::now() + super::super::DAEMON_BLOCKING_TASK_DRAIN_BUDGET;
            let released = async_db
                .run_write_until(deadline, move |db| {
                    db.runtime_writer_lease_release_until(&lease, deadline)
                })
                .await
                .context("failed to release daemon writer lease")?;
            if !released {
                bail!("daemon writer lease release was fenced or already absent");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;
    use crate::daemon::writer_lease::{DAEMON_WRITER_LEASE_TTL_SECS, SQLITE_WRITER_LEASE_NAME};
    use std::future::Future;
    use std::task::Poll;
    use std::time::Duration;

    #[tokio::test]
    async fn dropping_unpolled_release_does_not_run_synchronous_fallback() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");
        let handle = RuntimeWriterLeaseHandle::new(
            db_path.clone(),
            lease,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
        );

        let unpolled = handle.release(&async_db);
        drop(unpolled);

        assert_eq!(
            Database::open(&db_path)
                .expect("reopen database")
                .runtime_writer_lease_status_read_only(Some(SQLITE_WRITER_LEASE_NAME))
                .expect("read lease")
                .len(),
            1,
            "dropping an unpolled release must not synchronously mutate SQLite"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelling_release_while_heartbeat_stops_does_not_run_synchronous_fallback() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");
        let (heartbeat_started_tx, heartbeat_started_rx) = std::sync::mpsc::sync_channel(1);
        let (heartbeat_release_tx, heartbeat_release_rx) = std::sync::mpsc::sync_channel(1);
        let heartbeat = tokio::spawn(async move {
            heartbeat_started_tx
                .send(())
                .expect("signal heartbeat started");
            let _ = heartbeat_release_rx.recv_timeout(Duration::from_secs(2));
        });
        heartbeat_started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("heartbeat started");
        let handle = RuntimeWriterLeaseHandle {
            db_path: db_path.clone(),
            lease,
            heartbeat,
            release_handed_off: false,
        };

        let mut release = Box::pin(handle.release(&async_db));
        let first_poll = std::future::poll_fn(|cx| Poll::Ready(release.as_mut().poll(cx))).await;
        assert!(first_poll.is_pending(), "release must await heartbeat exit");
        drop(release);
        let _ = heartbeat_release_tx.send(());

        assert_eq!(
            Database::open(&db_path)
                .expect("reopen database")
                .runtime_writer_lease_status_read_only(Some(SQLITE_WRITER_LEASE_NAME))
                .expect("read lease")
                .len(),
            1,
            "cancelling heartbeat shutdown must not synchronously mutate SQLite"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_release_timeout_cannot_delete_after_returning() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");

        let lock = rusqlite::Connection::open(&db_path).expect("open lock holder");
        lock.execute_batch("BEGIN IMMEDIATE;")
            .expect("hold WAL reserved writer lock");
        let reader = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open WAL reader while writer is reserved");
        let lease_count = || {
            reader
                .query_row("SELECT COUNT(*) FROM runtime_writer_leases", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read lease count")
        };
        assert_eq!(lease_count(), 1, "WAL reads must remain available");

        let handle = RuntimeWriterLeaseHandle::new(
            db_path,
            lease,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
        );
        let started = Instant::now();
        let error = handle
            .release(&async_db)
            .await
            .expect_err("held writer lock must exhaust the release budget");
        assert!(
            started.elapsed()
                < super::super::super::DAEMON_BLOCKING_TASK_DRAIN_BUDGET + Duration::from_secs(1),
            "release must respect its shared budget: {error:#}"
        );

        lock.execute_batch("ROLLBACK;")
            .expect("release WAL writer lock");
        let observation_deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < observation_deadline {
            assert_eq!(
                lease_count(),
                1,
                "terminal release error must forbid a detached late DELETE"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_release_reports_success_after_writer_contention() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");

        let lock_path = db_path.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let lock = rusqlite::Connection::open(lock_path).expect("open lock holder");
            lock.execute_batch("BEGIN IMMEDIATE;")
                .expect("hold WAL reserved writer lock");
            ready_tx.send(()).expect("signal writer lock");
            std::thread::sleep(Duration::from_millis(600));
            lock.execute_batch("ROLLBACK;")
                .expect("release WAL writer lock");
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer lock ready");

        let handle = RuntimeWriterLeaseHandle::new(
            db_path.clone(),
            lease,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
        );
        let started = Instant::now();
        handle
            .release(&async_db)
            .await
            .expect("committed release after contention must report success");
        holder.join().expect("join writer lock holder");
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert_eq!(
            rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("open lease reader")
            .query_row("SELECT COUNT(*) FROM runtime_writer_leases", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read lease count"),
            0,
            "reported release success must match durable state"
        );
    }

    #[test]
    fn cancelled_release_before_blocking_start_cannot_mutate() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("build constrained runtime");
        runtime.block_on(async {
            let handle = RuntimeWriterLeaseHandle {
                db_path: db_path.clone(),
                lease,
                heartbeat: tokio::spawn(async {}),
                release_handed_off: false,
            };

            let release_blocker =
                std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let blocker_for_worker = std::sync::Arc::clone(&release_blocker);
            let async_db_for_blocker = async_db.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let blocker = tokio::spawn(async move {
                async_db_for_blocker
                    .run_write(move |_db| {
                        started_tx.send(()).expect("signal blocking writer");
                        let (released, condvar) = blocker_for_worker.as_ref();
                        let released = released.lock().expect("lock blocker");
                        let _released = condvar
                            .wait_while(released, |released| !*released)
                            .expect("wait to release blocker");
                        Ok(())
                    })
                    .await
                    .expect("blocking writer")
            });
            started_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("blocking writer started");

            let async_db = async_db.clone();
            let release = tokio::spawn(async move { handle.release(&async_db).await });
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                !release.is_finished(),
                "release must await writer admission"
            );
            release.abort();
            assert!(release.await.expect_err("release cancelled").is_cancelled());
            let (released, condvar) = release_blocker.as_ref();
            *released.lock().expect("lock release") = true;
            condvar.notify_all();
            blocker.await.expect("join blocking writer");
        });

        assert_eq!(
            rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("open lease reader")
            .query_row("SELECT COUNT(*) FROM runtime_writer_leases", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("read lease count"),
            1,
            "cancelled pre-approval release must not dispatch its DELETE"
        );
    }

    #[tokio::test]
    async fn stale_explicit_release_remains_fenced_from_successor() {
        let tempdir = tempfile::tempdir().expect("create tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let stale = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire original lease")
            .expect("original lease available");
        assert!(
            db.runtime_writer_lease_release(&stale)
                .expect("clear original")
        );
        let successor = db
            .runtime_writer_lease_acquire_for_daemon_start(
                SQLITE_WRITER_LEASE_NAME,
                DAEMON_WRITER_LEASE_TTL_SECS,
                None,
            )
            .expect("acquire successor")
            .expect("successor lease available");
        drop(db);
        let async_db = AsyncDb::open(&db_path, 1).expect("open async database");

        let handle = RuntimeWriterLeaseHandle::new(
            db_path.clone(),
            stale,
            crate::daemon_recovery::DaemonRecoveryFaultReporter::new(
                crate::daemon_recovery::DaemonRecovery::new(tempdir.path()),
            ),
        );
        let error = handle
            .release(&async_db)
            .await
            .expect_err("stale explicit release must report fencing");
        assert!(error.to_string().contains("fenced or already absent"));
        let active = Database::open(&db_path)
            .expect("reopen database")
            .runtime_writer_lease_status_read_only(Some(SQLITE_WRITER_LEASE_NAME))
            .expect("read successor");
        let [actual] = active.as_slice() else {
            panic!("stale release must retain one successor: {active:?}");
        };
        assert_eq!(
            (
                &actual.name,
                &actual.owner,
                &actual.session_id,
                actual.generation,
            ),
            (
                &successor.name,
                &successor.owner,
                &successor.session_id,
                successor.generation,
            ),
            "stale release must preserve the successor fencing identity"
        );
    }
}

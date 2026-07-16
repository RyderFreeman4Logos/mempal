use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

/// T1 — runtime-liveness (the core #345 property). On a single-worker runtime
/// a ticker must keep advancing while a cold read is outstanding.
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
    assert!(observed >= 5, "ticker advanced only {observed} times");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t2_read_concurrency_up_to_n() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4)
        .expect("open async db")
        .with_read_delay(Duration::from_millis(200));
    let start = Instant::now();
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
    assert!(start.elapsed() < Duration::from_millis(400));
}

#[tokio::test]
async fn readers_are_query_only_low_cache_without_mmap() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), 4).expect("open async db");
    let (query_only, cache_size, mmap_size): (i64, i64, i64) = adb
        .run_read(|db| {
            Ok((
                db.conn()
                    .query_row("PRAGMA query_only", [], |row| row.get(0))?,
                db.conn()
                    .query_row("PRAGMA cache_size", [], |row| row.get(0))?,
                db.conn()
                    .query_row("PRAGMA mmap_size", [], |row| row.get(0))?,
            ))
        })
        .await
        .expect("read pragmas");
    assert_eq!(query_only, 1);
    assert_eq!(cache_size, SQLITE_CACHE_SIZE_KIB_DEFAULT);
    assert_eq!(mmap_size, 0);
}

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
    assert_eq!(query_only, 0);
}

#[test]
fn open_rejects_oversized_read_pool() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("palace.db");
    AsyncDb::open(&path, 15).expect("at-budget pool opens");
    assert!(matches!(
        AsyncDb::open(&path, 16),
        Err(DbError::PoolCacheBudgetExceeded { .. })
    ));
}

#[test]
fn resource_snapshot_reports_configured_page_cache_budget() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), RESOURCE_BOUNDED_READERS)
        .expect("open async db");
    let snapshot = adb.resource_snapshot();
    assert_eq!(snapshot.reader_connections, RESOURCE_BOUNDED_READERS);
    assert_eq!(snapshot.writer_connections, 1);
    assert_eq!(snapshot.total_connections, RESOURCE_BOUNDED_READERS + 1);
    assert_eq!(
        snapshot.per_connection_cache_kib,
        SQLITE_CACHE_SIZE_KIB_DEFAULT
    );
    assert_eq!(snapshot.per_connection_cache_bytes, 16 * 1024 * 1024);
    assert_eq!(snapshot.configured_page_cache_bytes, 48 * 1024 * 1024);
}

#[test]
fn reader_only_async_pool_does_not_open_writer() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("palace.db");
    Database::open(&path).expect("create database");
    let adb =
        QueryOnlyAsyncDb::open(&path, RESOURCE_BOUNDED_READERS).expect("open query-only async db");
    let snapshot = adb.resource_snapshot();
    assert_eq!(snapshot.reader_connections, RESOURCE_BOUNDED_READERS);
    assert_eq!(snapshot.writer_connections, 0);
    assert_eq!(snapshot.total_connections, RESOURCE_BOUNDED_READERS);
    assert_eq!(snapshot.configured_page_cache_bytes, 32 * 1024 * 1024);
}

#[tokio::test]
async fn reader_only_async_pool_runs_bounded_read_without_writer() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let path = tmp.path().join("palace.db");
    Database::open(&path).expect("create database");
    let adb = QueryOnlyAsyncDb::open(&path, 1).expect("open query-only async db");
    let deadline = Instant::now() + Duration::from_secs(1);
    let query_only = adb
        .run_read_anyhow_until(deadline, |db| {
            db.conn()
                .query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
                .map_err(anyhow::Error::new)
        })
        .await
        .expect("bounded read");
    assert_eq!(query_only, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_db_error_read_keeps_permit_until_checkin() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let adb_for_cancel = adb.clone();
    let handle = tokio::spawn(async move {
        adb_for_cancel
            .run_read(move |_db| {
                let _ = started_tx.send(());
                std::thread::sleep(Duration::from_millis(300));
                Ok::<_, DbError>(1_i64)
            })
            .await
    });
    started_rx.await.expect("blocking read started");
    handle.abort();
    assert!(handle.await.expect_err("cancelled").is_cancelled());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            adb.run_read(|_db| Ok::<_, DbError>(2_i64)),
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        adb.run_read(|_db| Ok::<_, DbError>(3_i64))
            .await
            .expect("recovered"),
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_anyhow_read_keeps_permit_until_checkin() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let adb_for_cancel = adb.clone();
    let handle = tokio::spawn(async move {
        adb_for_cancel
            .run_read_anyhow(move |_db| {
                let _ = started_tx.send(());
                std::thread::sleep(Duration::from_millis(300));
                Ok(1_i64)
            })
            .await
    });
    started_rx.await.expect("blocking read started");
    handle.abort();
    assert!(handle.await.expect_err("cancelled").is_cancelled());
    assert!(
        tokio::time::timeout(
            Duration::from_millis(100),
            adb.run_read_anyhow(|_db| Ok(2_i64)),
        )
        .await
        .is_err()
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        adb.run_read_anyhow(|_db| Ok(3_i64))
            .await
            .expect("recovered"),
        3
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deadline_anyhow_read_interrupts_sqlite_and_releases_reader() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let adb = AsyncDb::open(&tmp.path().join("palace.db"), 1).expect("open async db");
    let start = Instant::now();
    let error = adb
        .run_read_anyhow_until(start + Duration::from_millis(100), |db| {
            db.conn()
                .query_row(
                    "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 100000000) SELECT sum(n) FROM seq",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|_| ())
                .map_err(anyhow::Error::new)
        })
        .await
        .expect_err("deadline must interrupt SQLite");
    assert!(anyhow_error_is_read_deadline_exceeded(&error));
    assert!(start.elapsed() >= Duration::from_millis(50));
    let recovery = tokio::time::timeout(
        Duration::from_millis(100),
        adb.run_read_anyhow(|_db| Ok(7_i64)),
    )
    .await
    .expect("reader checked in")
    .expect("recovery read");
    assert_eq!(recovery, 7);
}

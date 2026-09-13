//! Integration tests for P9-B per-source ingest lock.
//!
//! Validates TOCTOU protection for concurrent Claude↔Codex ingest of
//! the same source file, plus timeout / dry-run / panic-release
//! semantics.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_trait::async_trait;
use mempal::core::db::Database;
use mempal::embed::{Embedder, Result as EmbedResult};
use mempal::ingest::lock::set_contention_observer_for_test;
use mempal::ingest::{
    IngestOptions, IngestStats, ingest_dir_with_options, ingest_file_with_options,
};
use tempfile::TempDir;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// Stub embedder: returns a fixed vector regardless of input. 3 dims so
/// `sqlite-vec` can store it without bloating the test DB.
struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }
    fn dimensions(&self) -> usize {
        3
    }
    fn name(&self) -> &str {
        "stub"
    }
}

struct HoldEmbedder {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

#[async_trait]
impl Embedder for HoldEmbedder {
    async fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        self.entered.send(()).expect("signal holder entered");
        self.release
            .lock()
            .expect("lock holder release channel")
            .recv_timeout(HANDSHAKE_TIMEOUT)
            .expect("release holder");
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "hold"
    }
}

fn write_file(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write fixture");
    path
}

/// Open each worker-owned database before admitting its ingest. This mirrors
/// separate processes without racing the independent profile-admission lock.
fn spawn_ingest_worker<E: Embedder + 'static>(
    db_path: PathBuf,
    file: PathBuf,
    embedder: E,
    start: Arc<Barrier>,
    contention: Option<mpsc::Sender<()>>,
) -> (mpsc::Receiver<()>, JoinHandle<IngestStats>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let db = Database::open(&db_path).expect("open db");
        if let Some(observer) = contention {
            set_contention_observer_for_test(observer);
        }
        ready_tx.send(()).expect("signal database ready");
        start.wait();
        runtime.block_on(async move {
            ingest_file_with_options(&db, &embedder, &file, "test", IngestOptions::default())
                .await
                .expect("ingest")
        })
    });
    (ready_rx, handle)
}

fn wait_until_ready(ready: &mpsc::Receiver<()>) {
    ready
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("database worker ready");
}

fn ingest_same_source_pair(db_path: &Path, file: &Path) -> (IngestStats, IngestStats) {
    let (holder_entered_tx, holder_entered_rx) = mpsc::channel();
    let (release_holder_tx, release_holder_rx) = mpsc::channel();
    let holder_start = Arc::new(Barrier::new(2));
    let (holder_ready, holder) = spawn_ingest_worker(
        db_path.to_path_buf(),
        file.to_path_buf(),
        HoldEmbedder {
            entered: holder_entered_tx,
            release: Mutex::new(release_holder_rx),
        },
        Arc::clone(&holder_start),
        None,
    );
    wait_until_ready(&holder_ready);
    holder_start.wait();
    holder_entered_rx
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("holder entered critical section");

    let (waiter_contended_tx, waiter_contended_rx) = mpsc::channel();
    let waiter_start = Arc::new(Barrier::new(2));
    let (waiter_ready, waiter) = spawn_ingest_worker(
        db_path.to_path_buf(),
        file.to_path_buf(),
        StubEmbedder,
        Arc::clone(&waiter_start),
        Some(waiter_contended_tx),
    );
    wait_until_ready(&waiter_ready);
    waiter_start.wait();
    waiter_contended_rx
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("waiter observed source-lock contention");
    release_holder_tx.send(()).expect("release holder");

    (
        holder.join().expect("holder thread"),
        waiter.join().expect("waiter thread"),
    )
}

#[test]
fn test_concurrent_ingest_same_source_single_drawer() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let file = write_file(tmp.path(), "doc.md", "hello P9-B test content");
    let (stats_a, stats_b) = ingest_same_source_pair(&db_path, &file);

    let db = Database::open(&db_path).expect("reopen");
    let drawer_count = db.drawer_count().expect("drawer_count");

    // Content-addressed drawer_id means both threads target the same id;
    // only one inserts, the other sees `drawer_exists == true`.
    assert_eq!(
        drawer_count, 1,
        "expected exactly 1 drawer; a={stats_a:?} b={stats_b:?}"
    );

    // Both threads must have recorded lock_wait_ms (non-dry-run path).
    assert!(stats_a.lock_wait_ms.is_some());
    assert!(stats_b.lock_wait_ms.is_some());

    let waits = [
        stats_a.lock_wait_ms.unwrap_or(0),
        stats_b.lock_wait_ms.unwrap_or(0),
    ];
    let waited = waits.into_iter().filter(|ms| *ms > 0).count();
    assert_eq!(
        waited, 1,
        "expected exactly one waiter; a={stats_a:?} b={stats_b:?}"
    );
}

#[tokio::test]
async fn test_ingest_dir_stats_include_created_drawer_ids() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("init db");
    let source_dir = tmp.path().join("source");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    write_file(&source_dir, "alpha.md", "alpha drawer id fixture");
    write_file(&source_dir, "beta.md", "beta drawer id fixture");

    let stats = ingest_dir_with_options(
        &db,
        &StubEmbedder,
        &source_dir,
        "test",
        IngestOptions::default(),
    )
    .await
    .expect("ingest dir");

    assert_eq!(stats.files, 2);
    assert!(!stats.drawer_ids.is_empty(), "drawer_ids must be reported");
    assert_eq!(
        stats.drawer_ids.len(),
        stats.chunks,
        "each inserted chunk must report one drawer_id"
    );
}

#[tokio::test]
async fn test_dry_run_ingest_stats_do_not_include_drawer_ids() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("init db");
    let source_dir = tmp.path().join("source");
    std::fs::create_dir_all(&source_dir).expect("create source dir");
    write_file(&source_dir, "alpha.md", "dry run drawer id fixture");

    let stats = ingest_dir_with_options(
        &db,
        &StubEmbedder,
        &source_dir,
        "test",
        IngestOptions {
            dry_run: true,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("dry-run ingest dir");

    assert_eq!(stats.files, 1);
    assert_eq!(stats.chunks, 1);
    assert!(
        stats.drawer_ids.is_empty(),
        "dry-run should not report drawer_ids"
    );
}

#[test]
fn test_concurrent_ingest_different_source_no_blocking() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let file_a = write_file(tmp.path(), "a.md", "content A unique");
    let file_b = write_file(tmp.path(), "b.md", "content B unique");
    let start = Arc::new(Barrier::new(3));

    let (ready_a, worker_a) = spawn_ingest_worker(
        db_path.clone(),
        file_a,
        StubEmbedder,
        Arc::clone(&start),
        None,
    );
    wait_until_ready(&ready_a);
    let (ready_b, worker_b) = spawn_ingest_worker(
        db_path.clone(),
        file_b,
        StubEmbedder,
        Arc::clone(&start),
        None,
    );
    wait_until_ready(&ready_b);
    start.wait();

    let stats_a = worker_a.join().expect("thread a");
    let stats_b = worker_b.join().expect("thread b");

    let wait_a = stats_a.lock_wait_ms.unwrap_or(0);
    let wait_b = stats_b.lock_wait_ms.unwrap_or(0);
    assert!(
        wait_a < 100 && wait_b < 100,
        "different sources should not block: a={wait_a}ms b={wait_b}ms"
    );

    let db = Database::open(&db_path).unwrap();
    assert_eq!(db.drawer_count().unwrap(), 2);
}

#[tokio::test]
async fn test_dry_run_does_not_acquire_lock() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let file = write_file(tmp.path(), "doc.md", "dry run content");
    let db = Database::open(&db_path).expect("open");

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &file,
        "test",
        IngestOptions {
            dry_run: true,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("dry_run");

    assert!(
        stats.lock_wait_ms.is_none(),
        "dry-run must not acquire lock"
    );
    // No writes.
    assert_eq!(db.drawer_count().unwrap(), 0);
}

#[test]
fn test_double_check_after_lock_skips_duplicate() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let file = write_file(tmp.path(), "doc.md", "second ingest should dedup");
    let (stats_1, stats_2) = ingest_same_source_pair(&db_path, &file);

    assert_eq!(stats_1.chunks, 1);
    assert_eq!(stats_2.chunks, 0, "second ingest writes no new chunks");
    assert!(stats_2.skipped >= 1, "second ingest should report skipped");
    assert!(
        stats_2.lock_wait_ms.unwrap_or(0) > 0,
        "second ingest must wait for the lock"
    );

    let db = Database::open(&db_path).expect("reopen");
    assert_eq!(db.drawer_count().unwrap(), 1);
}

#[test]
fn test_lock_released_on_guard_drop() {
    use mempal::ingest::lock::{acquire_source_lock, source_key};

    let tmp = TempDir::new().unwrap();
    let key = source_key(Path::new("/tmp/test-drop-release"));

    let guard1 =
        acquire_source_lock(tmp.path(), &key, Duration::from_secs(1)).expect("first acquire");
    drop(guard1);

    // Second acquire must succeed quickly.
    let guard2 = acquire_source_lock(tmp.path(), &key, Duration::from_millis(200))
        .expect("second acquire after drop");
    assert!(guard2.wait_duration() < Duration::from_millis(200));
}

#[cfg(unix)]
#[test]
fn test_lock_timeout_returns_error() {
    use mempal::ingest::lock::{LockError, acquire_source_lock, source_key};

    let tmp = Arc::new(TempDir::new().unwrap());
    let key = source_key(Path::new("/tmp/test-timeout"));
    let (holder_ready_tx, holder_ready_rx) = mpsc::channel();
    let (release_holder_tx, release_holder_rx) = mpsc::channel();

    let tmp_a = Arc::clone(&tmp);
    let key_a = key.clone();
    let holder = thread::spawn(move || {
        let _guard = acquire_source_lock(tmp_a.path(), &key_a, Duration::from_secs(1))
            .expect("holder acquire");
        holder_ready_tx.send(()).expect("signal holder ready");
        release_holder_rx
            .recv_timeout(HANDSHAKE_TIMEOUT)
            .expect("release timeout holder");
    });
    holder_ready_rx
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("timeout holder ready");

    let result = acquire_source_lock(tmp.path(), &key, Duration::from_millis(300));
    release_holder_tx.send(()).expect("release timeout holder");
    holder.join().expect("holder thread");

    assert!(
        matches!(result, Err(LockError::Timeout { .. })),
        "expected Timeout; got {result:?}"
    );
}

#[tokio::test]
async fn test_ingest_records_lock_wait_ms_field() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let file = write_file(tmp.path(), "doc.md", "lock wait ms visibility test");
    let db = Database::open(&db_path).expect("open");

    let stats =
        ingest_file_with_options(&db, &StubEmbedder, &file, "test", IngestOptions::default())
            .await
            .expect("ingest");

    assert!(
        stats.lock_wait_ms.is_some(),
        "non-dry-run ingest must record lock_wait_ms"
    );
    // Uncontested acquire → wait should be near zero.
    assert!(stats.lock_wait_ms.unwrap() < 100);
}

#[cfg(unix)]
#[test]
fn test_panic_in_critical_section_releases_lock() {
    use mempal::ingest::lock::{acquire_source_lock, source_key};

    let tmp = Arc::new(TempDir::new().unwrap());
    let key = source_key(Path::new("/tmp/test-panic-release"));

    let tmp_panic = Arc::clone(&tmp);
    let key_panic = key.clone();
    let result = std::panic::catch_unwind(move || {
        let _guard = acquire_source_lock(tmp_panic.path(), &key_panic, Duration::from_secs(1))
            .expect("acquire in panic thread");
        panic!("simulated panic inside critical section");
    });
    assert!(result.is_err(), "panic should have been caught");

    // Second acquire from main thread must succeed — OS released flock on
    // file close when the guard was dropped during unwind.
    let guard = acquire_source_lock(tmp.path(), &key, Duration::from_millis(500))
        .expect("acquire after panic");
    assert!(guard.wait_duration() < Duration::from_millis(500));

    let lock_path = tmp.path().join("locks").join(format!("{key}.lock"));
    assert!(
        lock_path.exists(),
        "lock file should remain on disk for reuse"
    );
}

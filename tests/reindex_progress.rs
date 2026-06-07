use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use mempal::core::db::CURRENT_VECTOR_INDEX_VERSION;
use mempal::core::db::Database;
use mempal::core::reindex::ReindexProgressStore;
use mempal::core::types::{Drawer, SourceType};
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

struct MockEmbeddingServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let requests_clone = Arc::clone(&requests);
        let stop_clone = Arc::clone(&stop);
        let join = thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_clone.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    continue;
                };
                let mut buffer = [0_u8; 4096];
                let bytes_read = stream.read(&mut buffer).unwrap_or(0);
                requests_clone.fetch_add(1, Ordering::SeqCst);
                let request_text = String::from_utf8_lossy(&buffer[..bytes_read]);
                let body_start = request_text.find("\r\n\r\n").map_or(0, |index| index + 4);
                let input_count = serde_json::from_str::<Value>(&request_text[body_start..])
                    .ok()
                    .and_then(|json| {
                        json.get("input").map(|input| match input {
                            Value::Array(items) => items.len(),
                            Value::Null => 1,
                            _ => 1,
                        })
                    })
                    .unwrap_or(1);
                let data = (0..input_count)
                    .map(|_| r#"{"embedding":[0.1,0.2,0.3]}"#)
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!(r#"{{"data":[{data}]}}"#);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            stop,
            join: Some(join),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for MockEmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(
            self.base_url
                .trim_start_matches("http://")
                .trim_end_matches("/v1"),
        );
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn write_config(home: &Path, db_path: &Path, base_url: &str) {
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "Qwen/Qwen3-Embedding-8B"
dim = 3
request_timeout_secs = 5

[config_hot_reload]
enabled = false
"#,
            db_path.display(),
            base_url
        ),
    )
    .expect("write config");
}

fn seed_db(db_path: &Path) {
    let db = Database::open(db_path).expect("open db");
    for index in 0..50 {
        let id = format!("drawer-{index:02}");
        db.insert_drawer(&Drawer {
            id: id.clone(),
            content: format!("drawer content {index}"),
            wing: "test".to_string(),
            room: Some("resume".to_string()),
            source_file: Some("fixtures/source.txt".to_string()),
            source_type: SourceType::AgentInference,
            added_at: format!("17130000{index:02}"),
            chunk_index: Some(index as i64),
            importance: 0,
            ..Drawer::default()
        })
        .expect("insert drawer");
        db.insert_vector(&id, &[0.9, 0.8])
            .expect("insert old vector");
    }
}

fn run_reindex(
    home: &Path,
    stop_after: Option<usize>,
    resume: bool,
    stale: bool,
) -> std::process::Output {
    let mut command = Command::new(mempal_bin());
    command
        .env("HOME", home)
        .arg("reindex")
        .arg("--embedder")
        .arg("openai_compat");
    if stale {
        command.arg("--stale");
    }
    if let Some(limit) = stop_after {
        command.env("MEMPAL_TEST_REINDEX_STOP_AFTER", limit.to_string());
    }
    if resume {
        command.arg("--resume");
    }
    command.output().expect("run reindex command")
}

fn run_reindex_from_config(
    home: &Path,
    stop_after: Option<usize>,
    resume: bool,
    stale: bool,
) -> std::process::Output {
    let mut command = Command::new(mempal_bin());
    command
        .env("HOME", home)
        .arg("reindex")
        .arg("--from-config");
    if stale {
        command.arg("--stale");
    }
    if let Some(limit) = stop_after {
        command.env("MEMPAL_TEST_REINDEX_STOP_AFTER", limit.to_string());
    }
    if resume {
        command.arg("--resume");
    }
    command.output().expect("run reindex command")
}

fn read_reindex_progress_status_counts(db: &Database) -> (i64, i64, i64) {
    db.conn()
        .query_row(
            r#"
            SELECT
                COALESCE(SUM(CASE WHEN status = 'running' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'done' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0)
            FROM reindex_progress
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .expect("read reindex progress status counts")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_resume_from_checkpoint() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let db_path = home.join(".mempal").join("palace.db");
    let server = MockEmbeddingServer::start();

    write_config(&home, &db_path, &server.base_url);
    seed_db(&db_path);

    let first = run_reindex(&home, Some(20), false, false);
    assert!(!first.status.success());
    assert!(String::from_utf8_lossy(&first.stderr).contains("interrupted for test"));
    assert_eq!(server.request_count(), 20);

    let db = Database::open(&db_path).expect("open db after interrupt");
    let paused = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read paused checkpoint");
    assert_eq!(paused, (19, "paused".to_string()));

    let second = run_reindex(&home, None, true, false);
    assert!(
        second.status.success(),
        "resume stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(server.request_count(), 50);

    let db = Database::open(&db_path).expect("open db after resume");
    let state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read final checkpoint");
    assert_eq!(state, (49, "done".to_string()));

    let vector_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count vectors");
    assert_eq!(vector_count, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_finalizes_progress() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let db_path = home.join(".mempal").join("palace.db");
    let server = MockEmbeddingServer::start();

    write_config(&home, &db_path, &server.base_url);
    seed_db(&db_path);

    let db = Database::open(&db_path).expect("open db before stale reindex");
    let before = read_reindex_progress_status_counts(&db);
    drop(db);

    let output = run_reindex(&home, None, false, true);
    assert!(
        output.status.success(),
        "reindex stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 1);

    let db = Database::open(&db_path).expect("open db after stale reindex");
    let after = read_reindex_progress_status_counts(&db);
    assert_eq!(
        after.0, before.0,
        "successful stale reindex must not leave extra running rows"
    );
    assert_eq!(
        after.1,
        before.1 + 1,
        "successful stale reindex must settle one additional row to done"
    );
    assert_eq!(
        after.2, before.2,
        "successful stale reindex must not resurrect failed rows"
    );
    let state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read stale checkpoint");
    assert_eq!(state, (49, "done".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_progress_reconciliation_finalizes_orphan_running_row() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let db_path = home.join(".mempal").join("palace.db");
    let server = MockEmbeddingServer::start();

    write_config(&home, &db_path, &server.base_url);
    seed_db(&db_path);

    let output = run_reindex(&home, None, false, true);
    assert!(
        output.status.success(),
        "stale reindex stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.request_count(), 1);

    let db = Database::open(&db_path).expect("open db after stale reindex");
    let progress = ReindexProgressStore::new(&db_path);
    progress
        .upsert_running("fixtures/source.txt", Some(49), "openai_compat")
        .expect("insert orphan running row");

    let running = db
        .conn()
        .query_row(
            "SELECT status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read running checkpoint");
    assert_eq!(running, "running");
    let before = read_reindex_progress_status_counts(&db);

    let target_fingerprint = format!(
        "openai_compat:Qwen/Qwen3-Embedding-8B:{}:{}",
        server.base_url.trim_end_matches('/'),
        3
    );
    let reconciled = progress
        .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
        .expect("reconcile orphan running row");
    assert_eq!(reconciled, 1);
    let after_first = read_reindex_progress_status_counts(&db);
    assert_eq!(after_first.0, before.0 - 1);
    assert_eq!(after_first.1, before.1 + 1);
    assert_eq!(after_first.2, before.2);

    let reconciled_again = progress
        .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
        .expect("reconcile orphan running row twice");
    assert_eq!(reconciled_again, 0);
    let after_second = read_reindex_progress_status_counts(&db);
    assert_eq!(after_second, after_first);

    db.conn()
        .execute(
            "UPDATE reindex_progress SET status = 'failed' WHERE source_path = 'fixtures/source.txt'",
            [],
        )
        .expect("flip checkpoint to failed");

    let failed = read_reindex_progress_status_counts(&db);
    assert_eq!(failed, (0, 0, 1));

    let reconciled_failed = progress
        .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
        .expect("reconcile failed checkpoint");
    assert_eq!(reconciled_failed, 0);

    let state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read reconciled checkpoint");
    assert_eq!(state, (49, "failed".to_string()));

    db.conn()
        .execute(
            "UPDATE reindex_progress SET status = 'paused' WHERE source_path = 'fixtures/source.txt'",
            [],
        )
        .expect("flip checkpoint to paused");

    let paused = read_reindex_progress_status_counts(&db);
    assert_eq!(paused, (0, 0, 0));

    let reconciled_paused = progress
        .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
        .expect("reconcile paused checkpoint");
    assert_eq!(reconciled_paused, 0);

    let paused_state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read paused checkpoint");
    assert_eq!(paused_state, (49, "paused".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_reindex_stale_finalizes_orphan_running_row_with_zero_drawers() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let db_path = home.join(".mempal").join("palace.db");
    let server = MockEmbeddingServer::start();

    write_config(&home, &db_path, &server.base_url);
    seed_db(&db_path);

    let full = run_reindex(&home, None, false, false);
    assert!(
        full.status.success(),
        "full reindex stderr: {}",
        String::from_utf8_lossy(&full.stderr)
    );
    assert_eq!(server.request_count(), 50);

    let db = Database::open(&db_path).expect("open db before stale reconcile");
    let progress = ReindexProgressStore::new(&db_path);
    progress
        .upsert_running("fixtures/source.txt", Some(49), "openai_compat")
        .expect("insert orphan running row");

    let before = read_reindex_progress_status_counts(&db);
    assert_eq!(before, (1, 0, 0));
    drop(db);

    let stale = run_reindex_from_config(&home, None, false, true);
    assert!(
        stale.status.success(),
        "stale reindex stderr: {}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert_eq!(server.request_count(), 50);

    let db = Database::open(&db_path).expect("open db after stale reconcile");
    let after = read_reindex_progress_status_counts(&db);
    assert_eq!(after, (0, 1, 0));

    let state = db
        .conn()
        .query_row(
            "SELECT last_processed_chunk_id, status FROM reindex_progress WHERE source_path = 'fixtures/source.txt'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("read reconciled checkpoint");
    assert_eq!(state, (49, "done".to_string()));

    let repeat = run_reindex_from_config(&home, None, false, true);
    assert!(
        repeat.status.success(),
        "repeat stale reindex stderr: {}",
        String::from_utf8_lossy(&repeat.stderr)
    );
    assert_eq!(server.request_count(), 50);

    let db = Database::open(&db_path).expect("open db after repeat stale reconcile");
    let repeat_counts = read_reindex_progress_status_counts(&db);
    assert_eq!(repeat_counts, after);
}

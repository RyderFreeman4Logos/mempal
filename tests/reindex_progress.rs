use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
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
                let _ = stream.read(&mut buffer);
                requests_clone.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#;
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
            source_type: SourceType::Project,
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

fn run_reindex(home: &Path, stop_after: Option<usize>, resume: bool) -> std::process::Output {
    let mut command = Command::new(mempal_bin());
    command
        .env("HOME", home)
        .arg("reindex")
        .arg("--embedder")
        .arg("openai_compat");
    if let Some(limit) = stop_after {
        command.env("MEMPAL_TEST_REINDEX_STOP_AFTER", limit.to_string());
    }
    if resume {
        command.arg("--resume");
    }
    command.output().expect("run reindex command")
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

    let first = run_reindex(&home, Some(20), false);
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

    let second = run_reindex(&home, None, true);
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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder};
use mempal::ingest::ingest_file_with_options;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

async fn test_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[derive(Default)]
struct RecordingEmbedder {
    seen_inputs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.seen_inputs
            .lock()
            .expect("seen_inputs mutex poisoned")
            .extend(texts.iter().map(|text| (*text).to_string()));
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "recording"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
}

impl TestEnv {
    fn new(config_text: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");

        let config_path = mempal_home.join("config.toml");
        fs::write(&config_path, config_text).expect("write config");

        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

        Self { _tmp: tmp, db_path }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

fn config_text(db_path: &Path, privacy_enabled: bool) -> String {
    format!(
        r#"
db_path = "{}"

[privacy]
enabled = {}

[config_hot_reload]
enabled = false
"#,
        db_path.display(),
        privacy_enabled
    )
}

fn write_fixture(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, content).expect("write fixture");
    path
}

fn drawer_contents(db: &Database) -> Vec<String> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT content
            FROM drawers
            WHERE deleted_at IS NULL
            ORDER BY COALESCE(chunk_index, 0), id
            "#,
        )
        .expect("prepare drawer query");

    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query drawers")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect drawer rows")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scrub_catches_cross_chunk_secret() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let embedder = RecordingEmbedder::default();
    let secret = "sk-abcdef1234567890abcdef1234567890abcd";
    let content = format!("{}{} trailing text after boundary", "g".repeat(798), secret);
    let file = write_fixture(
        env.db_path.parent().expect("db parent"),
        "cross-chunk.txt",
        &content,
    );

    let stats = ingest_file_with_options(&env.db(), &embedder, &file, "test", Default::default())
        .await
        .expect("ingest cross chunk");

    let stored = drawer_contents(&env.db());
    assert!(
        stored.len() >= 2,
        "expected multi-chunk ingest, got {stored:?}"
    );
    assert_eq!(stats.chunks, stored.len());
    assert!(stored.iter().all(|chunk| !chunk.contains(secret)));
    assert!(
        stored
            .iter()
            .any(|chunk| chunk.contains("[REDACTED:openai_key]"))
    );

    let seen = embedder
        .seen_inputs
        .lock()
        .expect("seen inputs mutex poisoned")
        .clone();
    assert_eq!(seen.len(), stored.len());
    assert!(seen.iter().all(|chunk| !chunk.contains(secret)));
    assert!(
        seen.iter()
            .any(|chunk| chunk.contains("[REDACTED:openai_key]"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_privacy_disabled_skips_scrub() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), false));
    let config = config_text(&env.db_path, false);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let embedder = RecordingEmbedder::default();
    let original = "keep sk-abcdef1234567890abcdef1234567890abcd and <private>literal</private>";
    let file = write_fixture(
        env.db_path.parent().expect("db parent"),
        "privacy-disabled.txt",
        original,
    );

    let stats = ingest_file_with_options(&env.db(), &embedder, &file, "test", Default::default())
        .await
        .expect("ingest disabled");

    let stored = drawer_contents(&env.db());
    assert_eq!(stats.chunks, 1);
    assert_eq!(stored, vec![original.to_string()]);
    let seen = embedder
        .seen_inputs
        .lock()
        .expect("seen inputs mutex poisoned")
        .clone();
    assert_eq!(seen, vec![original.to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_private_tag_stripped() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let scrubbed = ConfigHandle::scrub_content("Here is the key: <private>sk-1234</private> done");
    assert_eq!(scrubbed, "Here is the key: [REDACTED:private_tag] done");
    assert!(!scrubbed.contains("<private>"));
    assert!(!scrubbed.contains("sk-1234"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_compiled_privacy_cache_reused_via_handle() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let first = ConfigHandle::current_compiled_privacy();
    let second = ConfigHandle::current_compiled_privacy();

    assert!(Arc::ptr_eq(&first, &second));
    let cfg = ConfigHandle::current();
    let scrubbed = cfg.scrub_content_with_compiled(
        "Bearer abcdefghijklmnopqrstuvwxyz0123456789",
        first.as_ref(),
    );
    assert_eq!(scrubbed, "[REDACTED:bearer_token]");
}

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle, ScrubStats};
use mempal::core::db::Database;
use mempal::embed::{EmbedError, Embedder};
use mempal::ingest::ingest_file_with_options;
use mempal::mcp::MempalMcpServer;
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

fn scrub_stats_delta(before: &ScrubStats, after: &ScrubStats) -> ScrubStats {
    let mut delta = ScrubStats {
        total_patterns_matched: after
            .total_patterns_matched
            .saturating_sub(before.total_patterns_matched),
        bytes_redacted: after.bytes_redacted.saturating_sub(before.bytes_redacted),
        ..ScrubStats::default()
    };

    for (pattern_name, after_count) in &after.redactions_per_pattern {
        let before_count = before
            .redactions_per_pattern
            .get(pattern_name)
            .copied()
            .unwrap_or(0);
        let diff = after_count.saturating_sub(before_count);
        if diff > 0 {
            delta
                .redactions_per_pattern
                .insert(pattern_name.clone(), diff);
        }
    }

    delta
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
async fn test_scrub_regex_compile_once_across_calls() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let first = ConfigHandle::current_compiled_privacy();
    let once = ConfigHandle::scrub_content("Bearer abcdefghijklmnopqrstuvwxyz0123456789");
    let second = ConfigHandle::current_compiled_privacy();
    let twice = ConfigHandle::scrub_content("Bearer zyxwvutsrqponmlkjihgfedcba9876543210");
    let third = ConfigHandle::current_compiled_privacy();

    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&second, &third));
    assert_eq!(once, "[REDACTED:bearer_token]");
    assert_eq!(twice, "[REDACTED:bearer_token]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scrub_stats_accumulates_across_ingests() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let before = ConfigHandle::scrub_stats();
    let embedder = RecordingEmbedder::default();
    let secret = "sk-abcdef1234567890abcdef1234567890abcd";
    let private_block = "<private>keep this out</private>";
    let first = write_fixture(
        env.db_path.parent().expect("db parent"),
        "scrub-stats-1.txt",
        &format!("alpha {secret} beta {secret}"),
    );
    let second = write_fixture(
        env.db_path.parent().expect("db parent"),
        "scrub-stats-2.txt",
        &format!("prefix {private_block} suffix"),
    );

    ingest_file_with_options(&env.db(), &embedder, &first, "test", Default::default())
        .await
        .expect("ingest first scrub fixture");
    ingest_file_with_options(&env.db(), &embedder, &second, "test", Default::default())
        .await
        .expect("ingest second scrub fixture");

    let after = ConfigHandle::scrub_stats();
    let delta = scrub_stats_delta(&before, &after);
    assert_eq!(delta.total_patterns_matched, 3, "{delta:?}");
    assert_eq!(
        delta.bytes_redacted,
        (secret.len() * 2 + private_block.len()) as u64
    );
    assert_eq!(delta.redactions_per_pattern.get("openai_key"), Some(&2));
    assert_eq!(delta.redactions_per_pattern.get("private_tag"), Some(&1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_status_surfaces_scrub_stats() {
    let _guard = test_guard().await;
    let env = TestEnv::new(&config_text(Path::new("/tmp/placeholder"), true));
    let config = config_text(&env.db_path, true);
    let config_path = env.db_path.parent().expect("db parent").join("config.toml");
    fs::write(&config_path, config).expect("rewrite config");
    ConfigHandle::bootstrap(&config_path).expect("rebootstrap config");

    let before = ConfigHandle::scrub_stats();
    let scrubbed = ConfigHandle::scrub_content("Bearer abcdefghijklmnopqrstuvwxyz0123456789");
    assert_eq!(scrubbed, "[REDACTED:bearer_token]");
    let expected = ConfigHandle::scrub_stats();
    let delta = scrub_stats_delta(&before, &expected);
    assert_eq!(delta.total_patterns_matched, 1, "{delta:?}");

    let server = MempalMcpServer::new(
        env.db_path.clone(),
        Config {
            db_path: env.db_path.display().to_string(),
            ..Config::default()
        },
    );
    let response = server.mempal_status().await.expect("status").0;

    assert_eq!(
        response.scrub_stats.total_patterns_matched,
        expected.total_patterns_matched
    );
    assert_eq!(response.scrub_stats.bytes_redacted, expected.bytes_redacted);
    assert_eq!(
        response
            .scrub_stats
            .redactions_per_pattern
            .get("bearer_token"),
        expected.redactions_per_pattern.get("bearer_token")
    );
}

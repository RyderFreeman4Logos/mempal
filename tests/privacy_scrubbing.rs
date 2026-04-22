use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle, ScrubStats};
use mempal::core::db::Database;
use mempal::core::utils::build_drawer_id;
use mempal::embed::{EmbedError, Embedder};
use mempal::ingest::{IngestOptions, chunk::chunk_text, ingest_file_with_options};
use mempal::mcp::{MempalMcpServer, StatusResponse};
use serde_json::Value;
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
    home_path: PathBuf,
    mempal_home: PathBuf,
    db_path: PathBuf,
}

impl TestEnv {
    fn new(privacy_enabled: bool, hooks_enabled: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home_path = tmp.path().to_path_buf();
        let mempal_home = home_path.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");

        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");

        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            config_text(&db_path, privacy_enabled, hooks_enabled),
        )
        .expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

        Self {
            _tmp: tmp,
            home_path,
            mempal_home,
            db_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn fixture(&self, name: &str, content: &str) -> PathBuf {
        let path = self.mempal_home.join(name);
        fs::write(&path, content).expect("write fixture");
        path
    }

    fn run_status(&self) -> std::process::Output {
        Command::new(mempal_bin())
            .arg("status")
            .env("HOME", &self.home_path)
            .output()
            .expect("run mempal status")
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn config_text(db_path: &Path, privacy_enabled: bool, hooks_enabled: bool) -> String {
    format!(
        r#"
db_path = "{}"

[privacy]
enabled = {}

[hooks]
enabled = {}

[config_hot_reload]
enabled = false
"#,
        db_path.display(),
        privacy_enabled,
        hooks_enabled
    )
}

fn fake_openai_key() -> String {
    format!("sk-{}", "0".repeat(40))
}

fn fake_openai_key_with_suffix() -> String {
    format!("{}_more", fake_openai_key())
}

fn fake_aws_access_key() -> String {
    format!("AKIA{}", "0".repeat(16))
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

fn scrub_stats_from_status(status: &StatusResponse) -> ScrubStats {
    ScrubStats {
        total_patterns_matched: status.scrub_stats.total_patterns_matched,
        bytes_redacted: status.scrub_stats.bytes_redacted,
        redactions_per_pattern: status.scrub_stats.redactions_per_pattern.clone(),
    }
}

fn drawer_rows(db: &Database) -> Vec<(String, String, Option<i64>)> {
    let mut statement = db
        .conn()
        .prepare(
            r#"
            SELECT id, content, chunk_index
            FROM drawers
            WHERE deleted_at IS NULL
            ORDER BY COALESCE(chunk_index, 0), id
            "#,
        )
        .expect("prepare drawer query");

    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .expect("query drawers")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect drawer rows")
}

fn vector_dims(db: &Database) -> Vec<usize> {
    let mut statement = db
        .conn()
        .prepare("SELECT vec_length(embedding) FROM drawer_vectors ORDER BY id")
        .expect("prepare vector query");
    statement
        .query_map([], |row| row.get::<_, i64>(0).map(|value| value as usize))
        .expect("query vectors")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect vector rows")
}

async fn status_response(db_path: &Path) -> StatusResponse {
    MempalMcpServer::new(
        db_path.to_path_buf(),
        Config {
            db_path: db_path.display().to_string(),
            ..Config::default()
        },
    )
    .mempal_status()
    .await
    .expect("status")
    .0
}

fn runtime_dependency_names() -> BTreeSet<String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--locked"])
        .output()
        .expect("run cargo metadata");
    assert!(output.status.success(), "{output:?}");

    let metadata: Value = serde_json::from_slice(&output.stdout).expect("parse cargo metadata");
    let root_id = metadata
        .get("resolve")
        .and_then(|value| value.get("root"))
        .and_then(Value::as_str)
        .expect("metadata resolve.root")
        .to_string();

    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .expect("metadata packages");
    let package_names = packages
        .iter()
        .map(|package| {
            (
                package
                    .get("id")
                    .and_then(Value::as_str)
                    .expect("package id")
                    .to_string(),
                package
                    .get("name")
                    .and_then(Value::as_str)
                    .expect("package name")
                    .to_string(),
            )
        })
        .collect::<HashMap<_, _>>();

    let nodes = metadata
        .get("resolve")
        .and_then(|value| value.get("nodes"))
        .and_then(Value::as_array)
        .expect("metadata resolve.nodes")
        .iter()
        .map(|node| {
            (
                node.get("id")
                    .and_then(Value::as_str)
                    .expect("node id")
                    .to_string(),
                node,
            )
        })
        .collect::<HashMap<_, _>>();

    let mut visited = BTreeSet::new();
    let mut stack = vec![root_id];
    while let Some(package_id) = stack.pop() {
        if !visited.insert(package_id.clone()) {
            continue;
        }

        let node = nodes.get(&package_id).expect("resolve node");
        for dependency in node
            .get("deps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let is_runtime = dependency
                .get("dep_kinds")
                .and_then(Value::as_array)
                .map(|kinds| {
                    kinds
                        .iter()
                        .any(|kind| kind.get("kind").is_none_or(Value::is_null))
                })
                .unwrap_or(true);
            if !is_runtime {
                continue;
            }

            let dep_package_id = dependency
                .get("pkg")
                .and_then(Value::as_str)
                .expect("dependency package id")
                .to_string();
            stack.push(dep_package_id);
        }
    }

    visited
        .into_iter()
        .filter_map(|package_id| package_names.get(&package_id).cloned())
        .filter(|package_name| package_name != env!("CARGO_PKG_NAME"))
        .collect()
}

fn lockfile_package_names(lockfile_text: &str) -> BTreeSet<String> {
    let lockfile: toml::Value = toml::from_str(lockfile_text).expect("parse Cargo.lock");
    lockfile
        .get("package")
        .and_then(toml::Value::as_array)
        .expect("lockfile package array")
        .iter()
        .filter_map(|package| {
            package
                .get("name")
                .and_then(toml::Value::as_str)
                .map(ToString::to_string)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scrub_catches_cross_chunk_secret() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);

    let embedder = RecordingEmbedder::default();
    let secret = "sk-abcdef1234567890abcdef1234567890abcd";
    let content = format!("{}{} trailing text after boundary", "g".repeat(798), secret);
    let file = env.fixture("cross-chunk.txt", &content);

    let stats = ingest_file_with_options(&env.db(), &embedder, &file, "test", Default::default())
        .await
        .expect("ingest cross chunk");

    let stored = drawer_rows(&env.db());
    assert!(
        stored.len() >= 2,
        "expected multi-chunk ingest, got {stored:?}"
    );
    assert_eq!(stats.chunks, stored.len());
    assert!(stored.iter().all(|(_, chunk, _)| !chunk.contains(secret)));
    assert!(
        stored
            .iter()
            .any(|(_, chunk, _)| chunk.contains("[REDACTED:openai_key]"))
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
async fn test_aws_access_key_scrubbed() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let before = ConfigHandle::scrub_stats();
    let aws_key = fake_aws_access_key();

    let scrubbed = ConfigHandle::scrub_content(&format!("access: {aws_key} in logs"));
    let after = ConfigHandle::scrub_stats();
    let delta = scrub_stats_delta(&before, &after);

    assert!(scrubbed.contains("[REDACTED:aws_access]"), "{scrubbed}");
    assert!(!scrubbed.contains(&aws_key), "{scrubbed}");
    assert_eq!(delta.redactions_per_pattern.get("aws_access"), Some(&1));
    drop(env);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_drawer_content_stores_scrubbed_text() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let embedder = RecordingEmbedder::default();
    let secret = fake_openai_key();
    let original = format!("key={secret} end");
    let file = env.fixture("drawer-content.txt", &original);

    ingest_file_with_options(
        &env.db(),
        &embedder,
        &file,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest drawer content fixture");

    let rows = drawer_rows(&env.db());
    assert_eq!(rows.len(), 1, "{rows:?}");
    let stored = &rows[0].1;
    let expected = "key=[REDACTED:openai_key] end";
    assert_eq!(stored.as_bytes(), expected.as_bytes());
    assert!(!stored.contains(&secret));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embedding_receives_scrubbed_text() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let embedder = RecordingEmbedder::default();
    let secret = fake_openai_key();
    let file = env.fixture("embed-input.txt", &format!("before {secret} after"));

    ingest_file_with_options(
        &env.db(),
        &embedder,
        &file,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest embedding fixture");

    let seen = embedder
        .seen_inputs
        .lock()
        .expect("seen inputs mutex poisoned")
        .clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(!seen[0].contains(&secret), "{seen:?}");
    assert!(seen[0].contains("[REDACTED:openai_key]"), "{seen:?}");
}

#[test]
fn test_invalid_regex_pattern_fails_config_load() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let config_path = mempal_home.join("config.toml");
    let db_path = mempal_home.join("palace.db");

    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[privacy]
enabled = true

[[privacy.scrub_patterns]]
name = "broken_pattern"
pattern = "("
"#,
            db_path.display()
        ),
    )
    .expect("write invalid config");

    let error = Config::load_from(&config_path).expect_err("invalid regex must fail config load");
    let message = error.to_string();
    assert!(message.contains("broken_pattern"), "{message}");
    assert!(message.contains("privacy regex"), "{message}");
}

#[test]
fn test_no_new_runtime_dependencies_introduced() {
    let runtime_packages = runtime_dependency_names();
    let baseline_output = Command::new("git")
        .args(["show", "main:Cargo.lock"])
        .output()
        .expect("read baseline Cargo.lock from main");
    assert!(baseline_output.status.success(), "{baseline_output:?}");
    let baseline_lock =
        String::from_utf8(baseline_output.stdout).expect("baseline Cargo.lock is utf8");
    let baseline_packages = lockfile_package_names(&baseline_lock);

    let new_runtime_packages = runtime_packages
        .difference(&baseline_packages)
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        new_runtime_packages.is_empty(),
        "new runtime dependencies detected: {new_runtime_packages:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_openai_key_scrubbed_to_placeholder() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let before = ConfigHandle::scrub_stats();
    let text = format!("my key is {}", fake_openai_key_with_suffix());

    let scrubbed = ConfigHandle::scrub_content(&text);
    let after = ConfigHandle::scrub_stats();
    let delta = scrub_stats_delta(&before, &after);

    assert!(scrubbed.contains("[REDACTED:openai_key]"), "{scrubbed}");
    assert!(!scrubbed.contains(&fake_openai_key()), "{scrubbed}");
    assert_eq!(delta.redactions_per_pattern.get("openai_key"), Some(&1));
    drop(env);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_privacy_disabled_preserves_content_byte_identical() {
    let _guard = test_guard().await;
    let env = TestEnv::new(false, false);
    let embedder = RecordingEmbedder::default();
    let original = format!("keep {} and <private>literal</private>", fake_openai_key());
    let file = env.fixture("privacy-disabled.txt", &original);

    ingest_file_with_options(
        &env.db(),
        &embedder,
        &file,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest disabled fixture");

    let rows = drawer_rows(&env.db());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].1.as_bytes(), original.as_bytes());
    let seen = embedder
        .seen_inputs
        .lock()
        .expect("seen inputs mutex poisoned")
        .clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].as_bytes(), original.as_bytes());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_private_tag_block_stripped() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let before = ConfigHandle::scrub_stats();
    let scrubbed = ConfigHandle::scrub_content("before<private>\nsecret\nline\n</private>after");
    let after = ConfigHandle::scrub_stats();
    let delta = scrub_stats_delta(&before, &after);

    assert_eq!(scrubbed, "beforeafter");
    assert!(!scrubbed.contains("<private>"), "{scrubbed}");
    assert!(!scrubbed.contains("</private>"), "{scrubbed}");
    assert!(!scrubbed.contains("secret"), "{scrubbed}");
    assert!(!scrubbed.contains("[REDACTED:private_tag]"), "{scrubbed}");
    assert_eq!(delta.redactions_per_pattern.get("private_tag"), Some(&1));
    drop(env);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_scrub_does_not_affect_storage_invariants() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let embedder = RecordingEmbedder::default();
    let secret = fake_openai_key();
    let original = format!("{} {} tail", "x".repeat(820), secret);
    let expected_scrubbed = ConfigHandle::scrub_content(&original);
    let expected_chunks = chunk_text(&expected_scrubbed, 800, 100);
    assert!(expected_chunks.len() >= 2, "{expected_chunks:?}");
    let file = env.fixture("storage-invariants.txt", &original);

    ingest_file_with_options(
        &env.db(),
        &embedder,
        &file,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest storage invariant fixture");

    let rows = drawer_rows(&env.db());
    let stored_chunks = rows
        .iter()
        .map(|(_, content, _)| content.clone())
        .collect::<Vec<_>>();
    let stored_ids = rows.iter().map(|(id, _, _)| id.clone()).collect::<Vec<_>>();

    assert_eq!(stored_chunks, expected_chunks);
    assert_eq!(stored_ids.len(), expected_chunks.len());
    assert_eq!(
        stored_ids.iter().cloned().collect::<BTreeSet<_>>().len(),
        stored_ids.len()
    );

    for (index, chunk) in expected_chunks.iter().enumerate() {
        assert_eq!(
            stored_ids[index],
            build_drawer_id("test", Some("privacy"), chunk),
            "chunk {index} must keep deterministic drawer_id"
        );
        let (resolved_id, exists) = env
            .db()
            .resolve_ingest_drawer_id("test", Some("privacy"), chunk, None)
            .expect("resolve drawer id");
        assert!(exists, "stored chunk should resolve as existing");
        assert_eq!(resolved_id, stored_ids[index]);
    }

    let dims = vector_dims(&env.db());
    assert_eq!(dims, vec![3; expected_chunks.len()]);

    env.db()
        .conn()
        .execute("INSERT INTO drawers_fts(drawers_fts) VALUES('rebuild')", [])
        .expect("rebuild FTS");
    let fts_count = env
        .db()
        .conn()
        .query_row("SELECT COUNT(*) FROM drawers_fts", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count FTS rows");
    assert_eq!(fts_count as usize, expected_chunks.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_command_shows_scrub_stats() {
    let _guard = test_guard().await;
    let env = TestEnv::new(true, false);
    let before = ConfigHandle::scrub_stats();
    let embedder = RecordingEmbedder::default();
    let secret = fake_openai_key();
    let private_block = "<private>keep this out</private>";
    let first = env.fixture(
        "scrub-stats-1.txt",
        &format!("alpha {secret} beta {secret}"),
    );
    let second = env.fixture(
        "scrub-stats-2.txt",
        &format!("prefix {private_block} suffix"),
    );

    ingest_file_with_options(
        &env.db(),
        &embedder,
        &first,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest first scrub fixture");
    ingest_file_with_options(
        &env.db(),
        &embedder,
        &second,
        "test",
        IngestOptions {
            room: Some("privacy"),
            source_root: Some(&env.mempal_home),
            dry_run: false,
            project_id: None,
            gating: None,
            prototype_classifier: None,
        },
    )
    .await
    .expect("ingest second scrub fixture");

    let status = status_response(&env.db_path).await;
    let delta = scrub_stats_delta(&before, &scrub_stats_from_status(&status));
    assert_eq!(delta.total_patterns_matched, 3, "{delta:?}");
    assert_eq!(delta.redactions_per_pattern.get("openai_key"), Some(&2));
    assert_eq!(delta.redactions_per_pattern.get("private_tag"), Some(&1));
    assert!(delta.bytes_redacted >= (secret.len() * 2 + private_block.len()) as u64);

    let output = env.run_status();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Scrub:"), "{stdout}");
    assert!(stdout.contains("total_patterns_matched:"), "{stdout}");
    assert!(stdout.contains("bytes_redacted:"), "{stdout}");
    assert!(stdout.contains("redactions_per_pattern:"), "{stdout}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hooks_enabled_privacy_disabled_emits_warning() {
    let _guard = test_guard().await;
    let env = TestEnv::new(false, true);

    let status = status_response(&env.db_path).await;
    let warning = status
        .system_warnings
        .iter()
        .find(|warning| {
            warning.level == "warn"
                && warning.message.contains("privacy scrubbing is disabled")
                && warning.message.contains("[privacy].enabled = true")
        })
        .expect("privacy warning must be present in status response");
    assert_eq!(warning.source, "privacy");

    let output = env.run_status();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Warnings:"), "{stdout}");
    assert!(stdout.contains("[WARN]"), "{stdout}");
    assert!(stdout.contains("privacy scrubbing is disabled"), "{stdout}");
    assert!(stdout.contains("[privacy].enabled = true"), "{stdout}");
}

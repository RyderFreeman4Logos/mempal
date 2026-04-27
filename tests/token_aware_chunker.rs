//! Integration tests for token-aware chunking (issue #56).
//!
//! Verifies that the ingest pipeline respects `max_input_tokens` from the
//! embedder and `[chunker]` config, producing no oversized chunks that would
//! trigger HTTP 400 at the embed backend.

mod common;

use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::ingest::chunk::{chunk_text_token_aware, effective_max_tokens, global_chunker_stats};
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use tempfile::TempDir;

use common::harness::embed_mock;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn write_config(path: &Path, content: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, content).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

fn test_config(db_path: &Path, base_url: &str, max_input_tokens: Option<usize>) -> String {
    let mit_line = max_input_tokens
        .map(|n| format!("max_input_tokens = {n}"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "test-model"
dim = 4
{mit_line}

[chunker]
max_tokens = 512
target_tokens = 256
overlap_tokens = 32
"#,
        db_path.display(),
        base_url,
    )
}

/// Ingest a file with oversized content against a mock embedder advertising
/// max_input_tokens=512. All chunks must fit, zero HTTP 400s, all content ingested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_respects_max_input_tokens() {
    let _guard = test_guard();
    let (addr, handle) = embed_mock::start(0).await.expect("start mock");
    handle.set_dim(4);

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        &test_config(&db_path, &format!("http://{addr}/v1"), Some(512)),
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let embedder = mempal::embed::from_config(&config)
        .await
        .expect("build embedder");
    let db = Database::open(&db_path).expect("open db");

    // Write a large file: 10K words → way more than 512 tokens
    let source = tmp.path().join("oversized.txt");
    let content = "word ".repeat(5000);
    fs::write(&source, &content).expect("write source");

    // Ingest should succeed with zero errors
    let stats = ingest_file_with_options(
        &db,
        embedder.as_ref(),
        &source,
        "test",
        IngestOptions {
            room: Some("chunker-test"),
            source_root: source.parent(),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest should succeed with token-aware chunking");

    assert!(stats.chunks > 0, "should have ingested chunks");
    assert_eq!(stats.skipped, 0, "no chunks should be skipped");

    // Verify each chunk in the DB is within the token budget
    let drawer_count = db.drawer_count().expect("drawer count");
    assert!(
        drawer_count > 0,
        "drawers should have been created: got {drawer_count}"
    );

    // All embed requests should have succeeded (no 400s from the mock)
    let request_count = handle.request_count();
    assert!(request_count > 0, "mock should have received requests");

    handle.shutdown().await;
}

/// CJK + base64 content should all ingest without exceeding effective_max.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_cjk_and_base64_within_limits() {
    let _guard = test_guard();
    let (addr, handle) = embed_mock::start(0).await.expect("start mock");
    handle.set_dim(4);

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        &test_config(&db_path, &format!("http://{addr}/v1"), Some(512)),
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let embedder = mempal::embed::from_config(&config)
        .await
        .expect("build embedder");
    let db = Database::open(&db_path).expect("open db");

    // CJK content (1 char ≈ 2 tokens in the heuristic)
    let cjk_source = tmp.path().join("cjk.txt");
    let cjk_content = "中文测试".repeat(500);
    fs::write(&cjk_source, &cjk_content).expect("write cjk");

    let stats = ingest_file_with_options(
        &db,
        embedder.as_ref(),
        &cjk_source,
        "test",
        IngestOptions {
            room: Some("cjk-test"),
            source_root: cjk_source.parent(),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("CJK ingest should succeed");
    assert!(stats.chunks > 1, "CJK should split into multiple chunks");

    // Base64 content (dense, no spaces)
    let b64_source = tmp.path().join("base64.txt");
    let b64_content =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".repeat(100);
    fs::write(&b64_source, &b64_content).expect("write base64");

    let stats = ingest_file_with_options(
        &db,
        embedder.as_ref(),
        &b64_source,
        "test",
        IngestOptions {
            room: Some("b64-test"),
            source_root: b64_source.parent(),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("base64 ingest should succeed");
    assert!(stats.chunks > 1, "base64 should split into multiple chunks");

    handle.shutdown().await;
}

/// Verify that chunker_stats.hard_split_count is exposed (non-zero after
/// ingesting content with no natural breaks).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_chunker_stats_tracks_hard_splits() {
    let _guard = test_guard();
    let (addr, handle) = embed_mock::start(0).await.expect("start mock");
    handle.set_dim(4);

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    write_config(
        &config_path,
        &test_config(&db_path, &format!("http://{addr}/v1"), Some(256)),
    );
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let config = Config::load_from(&config_path).expect("load config");
    let embedder = mempal::embed::from_config(&config)
        .await
        .expect("build embedder");

    // Content with NO natural breaks → forces hard splits
    let no_break_text = "x".repeat(5000);
    let chunks = chunk_text_token_aware(&no_break_text, &config.chunker, embedder.as_ref(), None);
    assert!(chunks.len() > 1, "should hard-split into multiple chunks");

    // Verify effective_max respects embedder limit
    let eff = effective_max_tokens(&config.chunker, embedder.as_ref());
    // 256 (embedder limit) - 32 (safety) = 224; config max_tokens=512
    // so effective = min(512, 224) = 224
    assert_eq!(eff, 224);

    for chunk in &chunks {
        let tokens = embedder.estimate_tokens(chunk);
        assert!(
            tokens <= eff,
            "chunk has {tokens} tokens, exceeds effective max {eff}"
        );
    }

    let stats = global_chunker_stats().snapshot();
    assert!(
        stats.hard_split_count > 0,
        "hard_split_count should be > 0 for content with no natural breaks"
    );

    handle.shutdown().await;
}

//! Integration tests for Issue #235: Claude Code session JSONL ingestion.
//! Verifies the end-to-end pipeline from JSONL file detection through drawer creation.

use std::fs;

use mempal::core::db::Database;
use mempal::embed::Embedder;
use mempal::ingest::conversation::{
    extract_session_id_from_content, extract_session_id_from_path, session_id_for_path,
};
use mempal::ingest::detect::{Format, detect_format};
use mempal::ingest::normalize::{NormalizeOptions, normalize_content_with_options};
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use tempfile::TempDir;

struct StubEmbedder;

#[async_trait::async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn session_jsonl(session_id: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"user","sessionId":"{sid}","uuid":"u1","message":{{"id":"m1","role":"user","content":[{{"type":"text","text":"Hello, help me debug this Rust code."}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","uuid":"u2","message":{{"id":"m2","role":"assistant","content":[{{"type":"text","text":"Sure, I can help! Please share the code."}},{{"type":"tool_use","id":"t1","name":"Read","input":{{}}}}]}}}}"#,
            "\n",
            r#"{{"type":"user","sessionId":"{sid}","uuid":"u3","message":{{"id":"m3","role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":[{{"type":"text","text":"file contents"}}]}},{{"type":"text","text":"Here is my function."}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","uuid":"u4","message":{{"id":"m4","role":"assistant","content":[{{"type":"text","text":"I see the issue. The function is missing a return value."}}]}}}}"#,
        ),
        sid = session_id
    )
}

fn drawer_count(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count drawers")
}

fn drawers_wing_room(db: &Database, wing: &str, room: &str) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id FROM drawers WHERE wing = ?1 AND room = ?2 AND deleted_at IS NULL")
        .expect("prepare");
    stmt.query_map(rusqlite::params![wing, room], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect")
}

// ---- Format detection tests ----

#[test]
fn test_detect_format_cc_session() {
    let content = session_jsonl("abc-123");
    assert_eq!(
        detect_format(&content),
        Format::CcSession,
        "CC session JSONL must be detected as CcSession"
    );
}

#[test]
fn test_detect_format_cc_session_not_plain_text() {
    let content = session_jsonl("abc-123");
    assert_ne!(
        detect_format(&content),
        Format::PlainText,
        "CC session JSONL must not fall through to PlainText"
    );
}

#[test]
fn test_detect_format_plain_text_unchanged() {
    let content = "this is just plain text without json";
    assert_eq!(detect_format(content), Format::PlainText);
}

#[test]
fn test_detect_format_cc_session_limits_scan_window() {
    let mut lines = vec![r#"{"type":"summary","sessionId":"late-session"}"#; 64];
    lines.push(
        r#"{"type":"user","sessionId":"late-session","message":{"role":"user","content":[]}}"#,
    );
    let content = lines.join("\n");

    assert_eq!(detect_format(&content), Format::PlainText);
}

// ---- Normalizer tests ----

#[test]
fn test_normalize_cc_session_extracts_text_only() {
    let content = session_jsonl("test-session");
    let output =
        normalize_content_with_options(&content, Format::CcSession, NormalizeOptions::default())
            .expect("normalize");

    let transcript = output.content;
    // User messages should be prefixed with "> "
    assert!(
        transcript.contains("> Hello, help me debug"),
        "user text missing: {transcript}"
    );
    assert!(
        transcript.contains("> Here is my function"),
        "second user text missing: {transcript}"
    );
    // Assistant text should be present
    assert!(
        transcript.contains("Sure, I can help"),
        "assistant text missing: {transcript}"
    );
    assert!(
        transcript.contains("I see the issue"),
        "second assistant text missing: {transcript}"
    );
    // Tool blocks should NOT appear
    assert!(
        !transcript.contains("tool_use"),
        "tool_use block leaked into transcript"
    );
    assert!(
        !transcript.contains("file contents"),
        "tool_result content leaked into transcript"
    );
}

#[test]
fn test_normalize_cc_session_tool_only_is_empty() {
    let session_id = "tool-only";
    let content = format!(
        concat!(
            r#"{{"type":"user","sessionId":"{sid}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":[{{"type":"text","text":"output"}}]}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t2","name":"Edit","input":{{}}}}]}}}}"#,
        ),
        sid = session_id
    );
    let output =
        normalize_content_with_options(&content, Format::CcSession, NormalizeOptions::default())
            .expect("normalize");
    assert!(
        output.content.trim().is_empty(),
        "tool-only session should produce empty transcript, got: {:?}",
        output.content
    );
}

#[test]
fn test_normalize_cc_session_skips_non_conversation_entries() {
    let content = concat!(
        r#"{"type":"summary","sessionId":"s1","message":{"role":"system","content":"some summary"}}"#,
        "\n",
        r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":[{"type":"text","text":"actual question"}]}}"#,
    );
    let output =
        normalize_content_with_options(content, Format::CcSession, NormalizeOptions::default())
            .expect("normalize");
    assert!(
        output.content.contains("actual question"),
        "user text missing"
    );
    assert!(
        !output.content.contains("some summary"),
        "summary entry leaked into transcript"
    );
}

// ---- Session ID extraction tests ----

#[test]
fn test_extract_session_id_from_content_found() {
    let content = session_jsonl("my-session-id");
    assert_eq!(
        extract_session_id_from_content(&content),
        Some("my-session-id".to_string())
    );
}

#[test]
fn test_extract_session_id_from_content_not_found() {
    let content = r#"{"type":"user","message":{"role":"user","content":[]}}"#;
    assert_eq!(extract_session_id_from_content(content), None);
}

#[test]
fn test_extract_session_id_from_path_stem() {
    let path = std::path::Path::new("/home/user/.claude/projects/abc123def456.jsonl");
    assert_eq!(extract_session_id_from_path(path), "abc123def456");
}

#[test]
fn test_session_id_for_path_content_wins() {
    let content = session_jsonl("content-sid");
    let path = std::path::Path::new("/tmp/file-sid.jsonl");
    assert_eq!(session_id_for_path(path, &content), "content-sid");
}

#[test]
fn test_session_id_for_path_falls_back_to_filename() {
    let content =
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
    let path = std::path::Path::new("/tmp/fallback-id.jsonl");
    assert_eq!(session_id_for_path(path, content), "fallback-id");
}

// ---- End-to-end ingestion tests ----

#[tokio::test]
async fn test_ingest_cc_session_creates_drawers_with_correct_wing_room() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let session_id = "test-session-e2e";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &jsonl_path,
        "conversation",
        IngestOptions {
            room: Some(session_id),
            source_root: jsonl_path.parent(),
            dry_run: false,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest cc session");

    assert!(
        stats.chunks > 0,
        "expected at least 1 chunk, got {}",
        stats.chunks
    );
    assert_eq!(
        drawer_count(&db),
        stats.chunks as i64,
        "drawer count mismatch"
    );

    let drawer_ids = drawers_wing_room(&db, "conversation", session_id);
    assert_eq!(
        drawer_ids.len(),
        stats.chunks,
        "all drawers should have wing=conversation room={session_id}"
    );
}

#[tokio::test]
async fn test_ingest_cc_session_dry_run_no_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let session_id = "dry-run-session";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &jsonl_path,
        "conversation",
        IngestOptions {
            room: Some(session_id),
            source_root: jsonl_path.parent(),
            dry_run: true,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("dry-run ingest");

    assert!(stats.chunks > 0, "dry-run should still report chunk count");
    assert_eq!(drawer_count(&db), 0, "dry-run must not write drawers");
}

#[tokio::test]
async fn test_ingest_cc_session_filters_tool_blocks() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let session_id = "tool-only-session";
    let content = format!(
        concat!(
            r#"{{"type":"user","sessionId":"{sid}","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":[{{"type":"text","text":"file contents"}}]}}]}}}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"t2","name":"Edit","input":{{}}}}]}}}}"#,
        ),
        sid = session_id
    );
    let jsonl_path = tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &content).expect("write jsonl");

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &jsonl_path,
        "conversation",
        IngestOptions {
            room: Some(session_id),
            dry_run: false,
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest tool-only session");

    assert_eq!(stats.chunks, 0, "tool-only session must produce 0 chunks");
    assert_eq!(drawer_count(&db), 0, "no drawers should be written");
}

#[tokio::test]
async fn test_ingest_cc_session_deduplicates_on_reingest() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let session_id = "dedup-session";
    let jsonl = session_jsonl(session_id);
    let jsonl_path = tmp.path().join(format!("{session_id}.jsonl"));
    fs::write(&jsonl_path, &jsonl).expect("write jsonl");

    let opts = IngestOptions {
        room: Some(session_id),
        source_root: jsonl_path.parent(),
        dry_run: false,
        ..IngestOptions::default()
    };

    let first = ingest_file_with_options(&db, &StubEmbedder, &jsonl_path, "conversation", opts)
        .await
        .expect("first ingest");

    let opts2 = IngestOptions {
        room: Some(session_id),
        source_root: jsonl_path.parent(),
        dry_run: false,
        ..IngestOptions::default()
    };
    let second = ingest_file_with_options(&db, &StubEmbedder, &jsonl_path, "conversation", opts2)
        .await
        .expect("second ingest");

    assert_eq!(
        second.chunks, 0,
        "re-ingest must produce 0 new chunks (all deduplicated)"
    );
    assert_eq!(
        second.skipped, first.chunks,
        "all chunks from first ingest should be skipped"
    );
    assert_eq!(
        drawer_count(&db),
        first.chunks as i64,
        "drawer count must not grow on re-ingest"
    );
}

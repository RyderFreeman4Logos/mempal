use std::path::PathBuf;

use mempal::core::db::Database;
use mempal::xurl::ingest;
use mempal::xurl::model::Tool;
use mempal::xurl::parser::hermes::{HermesParseOptions, parse_hermes_db_with_options};
use mempal::xurl::search::{self, SearchOptions};
use mempal::xurl::store::{self, TurnFilter};
use rusqlite::{Connection, params};
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn open_temp_db_at_fork_ext(_version: u32) -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let db = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        inner: db,
    }
}

struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    fn new_fixed_dim(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait::async_trait]
impl mempal::embed::Embedder for MockEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1f32; self.dim]).collect())
    }

    fn dimensions(&self) -> usize {
        self.dim
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn make_user_line(session_id: &str, idx: usize) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2026-05-27T12:00:00Z",
        "sessionId": session_id,
        "userType": "external",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": format!("user msg {idx}")}]
        }
    })
    .to_string()
}

fn make_user_line_with_cwd(session_id: &str, text: &str, cwd: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2026-05-27T12:00:00Z",
        "sessionId": session_id,
        "userType": "external",
        "cwd": cwd,
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    })
    .to_string()
}

fn make_assistant_line(session_id: &str, idx: usize) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": "2026-05-27T12:00:00Z",
        "sessionId": session_id,
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": format!("asst msg {idx}")}]
        }
    })
    .to_string()
}

fn make_tool_result_line(session_id: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2026-05-27T12:00:00Z",
        "sessionId": session_id,
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
        }
    })
    .to_string()
}

fn write_cc_fixture(
    dir: &TempDir,
    user_count: usize,
    asst_count: usize,
    tool_count: usize,
) -> PathBuf {
    let sid = "fixture-session";
    let mut lines = Vec::new();
    for i in 0..user_count {
        lines.push(make_user_line(sid, i));
    }
    for i in 0..asst_count {
        lines.push(make_assistant_line(sid, i));
    }
    for _ in 0..tool_count {
        lines.push(make_tool_result_line(sid));
    }
    let path = dir.path().join("fixture.jsonl");
    std::fs::write(&path, lines.join("\n")).expect("write fixture");
    path
}

fn write_cc_fixture_under_claude_projects(
    dir: &TempDir,
    session_id: &str,
    cwd: &str,
    include_cwd: bool,
) -> PathBuf {
    let encoded = cwd.replace('/', "-");
    let session_dir = dir.path().join(".claude/projects").join(encoded);
    std::fs::create_dir_all(&session_dir).expect("create cc project dir");
    let line = if include_cwd {
        make_user_line_with_cwd(session_id, "database migration from cc cwd", cwd)
    } else {
        make_user_line(session_id, 0)
    };
    let path = session_dir.join("fixture.jsonl");
    std::fs::write(&path, line).expect("write fixture");
    path
}

#[tokio::test]
async fn ingest_cc_file_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(16);
    // 10 user + 8 asst text turns; 12 tool_result user turns are skipped by parser
    let file = write_cc_fixture(&dir, 10, 8, 12);
    let embedder = MockEmbedder::new_fixed_dim(256);

    let stats = ingest::ingest_file(&db.inner, &embedder, &file, Tool::Cc, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        stats.turns_parsed, 18,
        "expected 18 screen-visible turns (10 user + 8 asst), got {}",
        stats.turns_parsed
    );
    assert_eq!(stats.turns_inserted, 18);
    assert_eq!(stats.turns_skipped, 0);

    // Re-ingest the same file → no new turns, all skipped
    let stats2 = ingest::ingest_file(&db.inner, &embedder, &file, Tool::Cc, None, None, None)
        .await
        .unwrap();
    assert_eq!(stats2.turns_inserted, 0);
    assert_eq!(stats2.turns_skipped, 18);
}

#[tokio::test]
async fn ingest_empty_file_succeeds() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(16);
    let path = dir.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();
    let embedder = MockEmbedder::new_fixed_dim(64);

    let stats = ingest::ingest_file(&db.inner, &embedder, &path, Tool::Cc, None, None, None)
        .await
        .unwrap();
    assert_eq!(stats.turns_parsed, 0);
    assert_eq!(stats.turns_inserted, 0);
}

#[tokio::test]
async fn ingest_cc_single_file_populates_project_path_and_source_path() {
    let dir = TempDir::new_in("/tmp").expect("external tempdir");
    let db = open_temp_db_at_fork_ext(16);
    let cwd = "/home/obj/project/github/RyderFreeman4Logos/mempal";
    let file = write_cc_fixture_under_claude_projects(&dir, "cc-cwd-session", cwd, true);
    let embedder = MockEmbedder::new_fixed_dim(64);

    let stats = ingest::ingest_file(&db.inner, &embedder, &file, Tool::Cc, None, None, None)
        .await
        .unwrap();

    assert_eq!(stats.turns_parsed, 1);
    assert_eq!(stats.turns_inserted, 1);

    let turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            session_id: Some("cc-cwd-session".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].project_path.as_deref(), Some(cwd));
    assert_eq!(turns[0].source_path.as_deref(), Some(cwd));

    let results = search::search(
        &db.inner,
        &embedder,
        "database migration",
        SearchOptions {
            limit: 5,
            filter: Some(TurnFilter {
                session_id: Some("cc-cwd-session".to_string()),
                limit: 5,
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(results.hits.len(), 1);
    assert_eq!(results.hits[0].source_path.as_deref(), Some(cwd));
}

#[tokio::test]
async fn ingest_cc_default_scan_populates_project_path_from_parent_dir() {
    let dir = TempDir::new_in("/tmp").expect("external tempdir");
    let db = open_temp_db_at_fork_ext(16);
    let cwd = "/home/obj/project/github/RyderFreeman4Logos/mempal";
    write_cc_fixture_under_claude_projects(&dir, "cc-fallback-session", cwd, false);
    let embedder = MockEmbedder::new_fixed_dim(64);
    let cfg = ingest::AutoScanConfig {
        cc_root: dir.path().join(".claude/projects"),
        codex_root: dir.path().join(".codex/sessions"),
        hermes_db: None,
    };

    let stats = ingest::ingest_all(&db.inner, &embedder, &cfg, None, None)
        .await
        .unwrap();

    assert_eq!(stats.turns_parsed, 1);
    assert_eq!(stats.turns_inserted, 1);
    let turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            session_id: Some("cc-fallback-session".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].source_path.as_deref(), Some(cwd));
}

fn write_hermes_export(dir: &TempDir, session_id: &str, cwd: &str) -> PathBuf {
    let lines = [
        serde_json::json!({
            "id": "msg-user-1",
            "session_id": session_id,
            "role": "user",
            "content": "why did mktd Step 7 fail?",
            "timestamp": "2026-06-01T00:00:00Z",
            "cwd": cwd,
            "session_title": "Issue 399 recall",
            "session_source": "cli"
        }),
        serde_json::json!({
            "id": "msg-assistant-1",
            "session_id": session_id,
            "role": "assistant",
            "content": "Step 7 failed because the review verdict was not PASS yet.",
            "timestamp": "2026-06-01T00:01:00Z",
            "cwd": cwd,
            "session_title": "Issue 399 recall",
            "session_source": "cli",
            "tool_name": "mktd"
        }),
    ];
    let path = dir.path().join(format!("{session_id}.jsonl"));
    let content = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).expect("write Hermes export");
    path
}

fn write_metadata_less_hermes_export(dir: &TempDir, name: &str, marker: &str) -> PathBuf {
    let lines = [
        serde_json::json!({
            "role": "user",
            "content": format!("metadata-less {marker} user turn"),
            "timestamp": "2026-06-01T00:00:00Z"
        }),
        serde_json::json!({
            "role": "assistant",
            "content": format!("metadata-less {marker} assistant turn"),
            "timestamp": "2026-06-01T00:01:00Z"
        }),
    ];
    let path = dir.path().join(name);
    let content = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, content).expect("write metadata-less Hermes export");
    path
}

fn write_metadata_less_hermes_db(dir: &TempDir, name: &str, marker: &str) -> PathBuf {
    let path = dir.path().join(name);
    let conn = Connection::open(&path).expect("open Hermes fixture db");
    conn.execute_batch(
        "CREATE TABLE messages (
            role      TEXT NOT NULL,
            content   TEXT NOT NULL,
            timestamp REAL NOT NULL
        );",
    )
    .expect("create Hermes fixture table");
    conn.execute(
        "INSERT INTO messages (role, content, timestamp) VALUES (?1, ?2, ?3)",
        params!["user", format!("metadata-less {marker} db user turn"), 1.0],
    )
    .expect("insert user row");
    conn.execute(
        "INSERT INTO messages (role, content, timestamp) VALUES (?1, ?2, ?3)",
        params![
            "assistant",
            format!("metadata-less {marker} db assistant turn"),
            2.0
        ],
    )
    .expect("insert assistant row");
    path
}

async fn ingest_hermes_options(
    db: &Database,
    embedder: &MockEmbedder,
    options: ingest::HermesIngestOptions,
) -> ingest::IngestStats {
    ingest::ingest_hermes_with_vector_fingerprint(
        db,
        embedder,
        &options,
        "mock:64",
        ingest::IngestCallbacks {
            on_file_parsed: None,
            on_embed_progress: None,
        },
    )
    .await
    .expect("ingest Hermes source")
}

#[test]
fn hermes_db_messages_join_their_own_session_metadata() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("state.db");
    let conn = Connection::open(&path).expect("open Hermes fixture db");
    conn.execute_batch(
        "CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            source TEXT,
            cwd TEXT
        );
        CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp REAL NOT NULL,
            active INTEGER NOT NULL,
            compacted INTEGER NOT NULL
        );
        INSERT INTO sessions (id, title, source, cwd) VALUES
            ('session-a', 'Session A', 'cli', '/repo/a'),
            ('session-b', 'Session B', 'telegram', '/repo/b');
        INSERT INTO messages
            (id, session_id, role, content, timestamp, active, compacted) VALUES
            ('message-a', 'session-a', 'user', 'alpha', 1.0, 1, 0),
            ('message-b', 'session-b', 'assistant', 'beta', 2.0, 1, 0);",
    )
    .expect("create multi-session Hermes fixture");
    drop(conn);

    let options = HermesParseOptions::new("fallback", "default", false);
    let turns = parse_hermes_db_with_options(&path, &options).expect("parse Hermes database");

    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].session_id, "session-a");
    assert_eq!(
        turns[0].metadata.session_title.as_deref(),
        Some("Session A")
    );
    assert_eq!(turns[0].metadata.session_source.as_deref(), Some("cli"));
    assert_eq!(turns[0].project_path.as_deref(), Some("/repo/a"));
    assert_eq!(turns[1].session_id, "session-b");
    assert_eq!(
        turns[1].metadata.session_title.as_deref(),
        Some("Session B")
    );
    assert_eq!(
        turns[1].metadata.session_source.as_deref(),
        Some("telegram")
    );
    assert_eq!(turns[1].project_path.as_deref(), Some("/repo/b"));
}

#[test]
fn hermes_db_session_filter_uses_message_session_index() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("state.db");
    let conn = Connection::open(&path).expect("open Hermes fixture db");
    conn.execute_batch(
        "CREATE TABLE message_rows (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            timestamp REAL NOT NULL,
            poison INTEGER NOT NULL
        );
        CREATE INDEX message_rows_session_id ON message_rows(session_id);
        CREATE VIEW messages AS
            SELECT id, session_id, role, content, timestamp
            FROM message_rows
            WHERE CASE WHEN poison = 1 THEN abs(-9223372036854775808) ELSE 1 END = 1;
        INSERT INTO message_rows VALUES
            ('target-message', 'target-session', 'user', 'target content', 1.0, 0),
            ('other-message', 'other-session', 'assistant', 'other content', 2.0, 1);",
    )
    .expect("create indexed multi-session Hermes fixture");
    drop(conn);

    let mut options = HermesParseOptions::new("fallback", "default", false);
    options.session_id_filter = Some("target-session".to_string());
    let turns = parse_hermes_db_with_options(&path, &options).expect("parse target session");

    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].session_id, "target-session");
    assert_eq!(turns[0].content, "target content");
}

#[tokio::test]
async fn ingest_hermes_jsonl_is_idempotent_by_profile_session_message() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(21);
    let cwd = "/home/obj/project/github/RyderFreeman4Logos/mempal";
    let export = write_hermes_export(&dir, "hermes-session-1", cwd);
    let embedder = MockEmbedder::new_fixed_dim(64);
    let options = ingest::HermesIngestOptions {
        profile: "default".to_string(),
        export_jsonl: Some(export.clone()),
        cwd: Some(cwd.to_string()),
        ..Default::default()
    };

    let first = ingest::ingest_hermes_with_vector_fingerprint(
        &db.inner,
        &embedder,
        &options,
        "mock:64",
        ingest::IngestCallbacks {
            on_file_parsed: None,
            on_embed_progress: None,
        },
    )
    .await
    .unwrap();
    let second = ingest::ingest_hermes_with_vector_fingerprint(
        &db.inner,
        &embedder,
        &options,
        "mock:64",
        ingest::IngestCallbacks {
            on_file_parsed: None,
            on_embed_progress: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(first.turns_inserted, 2);
    assert_eq!(first.vectors_created, 2);
    assert_eq!(second.turns_inserted, 0);
    assert_eq!(second.turns_skipped, 2);

    let work_profile = ingest::HermesIngestOptions {
        profile: "work".to_string(),
        export_jsonl: Some(export),
        cwd: Some(cwd.to_string()),
        ..Default::default()
    };
    let third = ingest::ingest_hermes_with_vector_fingerprint(
        &db.inner,
        &embedder,
        &work_profile,
        "mock:64",
        ingest::IngestCallbacks {
            on_file_parsed: None,
            on_embed_progress: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(third.turns_inserted, 2);

    let default_turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            hermes_profile: Some("default".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    let work_turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            hermes_profile: Some("work".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    assert_eq!(default_turns.len(), 2);
    assert_eq!(work_turns.len(), 2);
    assert_eq!(
        default_turns[0].metadata.hermes_profile.as_deref(),
        Some("default")
    );
    assert_eq!(
        default_turns[0].metadata.session_title.as_deref(),
        Some("Issue 399 recall")
    );
}

#[tokio::test]
async fn ingest_metadata_less_hermes_jsonl_uses_source_scoped_fallback_session() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(21);
    let embedder = MockEmbedder::new_fixed_dim(64);
    let first_export = write_metadata_less_hermes_export(&dir, "first.jsonl", "alpha");
    let second_export = write_metadata_less_hermes_export(&dir, "second.jsonl", "beta");

    let first = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            export_jsonl: Some(first_export.clone()),
            ..Default::default()
        },
    )
    .await;
    let second = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            export_jsonl: Some(second_export),
            ..Default::default()
        },
    )
    .await;
    let first_again = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            export_jsonl: Some(first_export),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(first.turns_inserted, 2);
    assert_eq!(second.turns_inserted, 2);
    assert_eq!(second.turns_updated, 0);
    assert_eq!(first_again.turns_inserted, 0);
    assert_eq!(first_again.turns_skipped, 2);

    let turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            hermes_profile: Some("default".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    let mut sessions = turns
        .iter()
        .map(|turn| turn.session_id.as_str())
        .collect::<Vec<_>>();
    sessions.sort_unstable();
    sessions.dedup();

    assert_eq!(turns.len(), 4);
    assert_eq!(sessions.len(), 2);
    assert!(turns.iter().any(|turn| turn.content.contains("alpha")));
    assert!(turns.iter().any(|turn| turn.content.contains("beta")));
}

#[tokio::test]
async fn ingest_metadata_less_hermes_dbs_use_source_scoped_fallback_session() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(21);
    let embedder = MockEmbedder::new_fixed_dim(64);
    let first_db = write_metadata_less_hermes_db(&dir, "first-state.db", "alpha");
    let second_db = write_metadata_less_hermes_db(&dir, "second-state.db", "beta");

    let first = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            db_path: Some(first_db.clone()),
            ..Default::default()
        },
    )
    .await;
    let second = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            db_path: Some(second_db),
            ..Default::default()
        },
    )
    .await;
    let first_again = ingest_hermes_options(
        &db.inner,
        &embedder,
        ingest::HermesIngestOptions {
            profile: "default".to_string(),
            db_path: Some(first_db),
            ..Default::default()
        },
    )
    .await;

    assert_eq!(first.turns_inserted, 2);
    assert_eq!(second.turns_inserted, 2);
    assert_eq!(second.turns_updated, 0);
    assert_eq!(first_again.turns_inserted, 0);
    assert_eq!(first_again.turns_skipped, 2);

    let turns = store::get_turns_filtered(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            hermes_profile: Some("default".to_string()),
            limit: 10,
            ..Default::default()
        },
        false,
        false,
    )
    .unwrap();
    let mut sessions = turns
        .iter()
        .map(|turn| turn.session_id.as_str())
        .collect::<Vec<_>>();
    sessions.sort_unstable();
    sessions.dedup();

    assert_eq!(turns.len(), 4);
    assert_eq!(sessions.len(), 2);
    assert!(turns.iter().any(|turn| turn.content.contains("alpha")));
    assert!(turns.iter().any(|turn| turn.content.contains("beta")));
}

#[tokio::test]
async fn search_hermes_filters_by_cwd_profile_and_returns_message_citations() {
    let dir = TempDir::new().unwrap();
    let db = open_temp_db_at_fork_ext(21);
    let cwd = "/home/obj/project/github/RyderFreeman4Logos/mempal";
    let other_cwd = "/home/obj/project/github/RyderFreeman4Logos/other";
    let export = write_hermes_export(&dir, "hermes-session-1", cwd);
    let other_export = write_hermes_export(&dir, "hermes-session-2", other_cwd);
    let embedder = MockEmbedder::new_fixed_dim(64);

    for path in [export, other_export] {
        let options = ingest::HermesIngestOptions {
            profile: "default".to_string(),
            export_jsonl: Some(path),
            ..Default::default()
        };
        ingest::ingest_hermes_with_vector_fingerprint(
            &db.inner,
            &embedder,
            &options,
            "mock:64",
            ingest::IngestCallbacks {
                on_file_parsed: None,
                on_embed_progress: None,
            },
        )
        .await
        .unwrap();
    }

    let result = search::search(
        &db.inner,
        &embedder,
        "review verdict PASS",
        SearchOptions {
            limit: 10,
            filter: Some(TurnFilter {
                tool: Some(Tool::Hermes),
                hermes_profile: Some("default".to_string()),
                cwd: Some(cwd.to_string()),
                limit: 10,
                ..Default::default()
            }),
            min_score: Some(0.0),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(result.hits.len(), 2);
    assert!(
        result
            .hits
            .iter()
            .all(|hit| hit.session_id == "hermes-session-1")
    );
    let assistant = result
        .hits
        .iter()
        .find(|hit| hit.message_id.as_deref() == Some("msg-assistant-1"))
        .expect("assistant hit");
    assert_eq!(assistant.hermes_profile.as_deref(), Some("default"));
    assert_eq!(assistant.session_source.as_deref(), Some("cli"));
    assert_eq!(assistant.tool_name.as_deref(), Some("mktd"));
    assert_eq!(assistant.previous_message_id.as_deref(), Some("msg-user-1"));
}

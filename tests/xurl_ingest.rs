use std::path::PathBuf;

use mempal::core::db::Database;
use mempal::xurl::ingest;
use mempal::xurl::model::Tool;
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    inner: Database,
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

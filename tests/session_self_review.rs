use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::{Drawer, SourceType};
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mempal::session_review::{
    SessionMetadata, SessionReviewOutcome, extract_session_review, split_session_metadata,
};
use rusqlite::Connection;
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

#[derive(Clone)]
struct DeterministicEmbedder {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                self.vectors
                    .get(*text)
                    .cloned()
                    .unwrap_or_else(|| self.default_vector.clone())
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        self.default_vector.len()
    }

    fn name(&self) -> &str {
        "deterministic"
    }
}

struct TestEnv {
    _tmp: TempDir,
    db_path: PathBuf,
    mempal_home: PathBuf,
    config: Config,
    store: PendingMessageStore,
}

impl TestEnv {
    fn new(
        extra_hooks: &str,
        extra_privacy: &str,
        extra_novelty: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let tmp = TempDir::new()?;
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home)?;
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path)?;
        let config_text = format!(
            r#"
db_path = "{}"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[hooks.session_end]
{}

[privacy]
{}

[ingest_gating]
enabled = false

[ingest_gating.novelty]
{}
"#,
            db_path.display(),
            extra_hooks,
            extra_privacy,
            extra_novelty,
        );
        let config = Config::parse(&config_text)?;
        let store = PendingMessageStore::new(&db_path)?;
        Ok(Self {
            _tmp: tmp,
            db_path,
            mempal_home,
            config,
            store,
        })
    }

    fn enqueue_session_end(&self, payload: &str) -> Result<String, Box<dyn std::error::Error>> {
        let envelope = CapturedHookEnvelope {
            event: HookEvent::SessionEnd.display_name().to_string(),
            kind: HookEvent::SessionEnd.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "1713000000".to_string(),
            claude_cwd: "/tmp/project".to_string(),
            payload: Some(payload.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: payload.len(),
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope)?;
        Ok(self
            .store
            .enqueue(HookEvent::SessionEnd.queue_kind(), &payload)?)
    }

    async fn process_once(
        &self,
        embedder: &DeterministicEmbedder,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let claimed = self
            .store
            .claim_next("worker-session-review", 120)?
            .expect("claimed message");
        let db = Database::open(&self.db_path)?;
        Ok(process_claimed_message_with_embedder(
            &db,
            &self.store,
            "worker-session-review",
            &claimed,
            embedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &self.config,
                mempal_home: &self.mempal_home,
            },
        )
        .await?)
    }
}

fn count_drawers_by_wing(db_path: &Path, wing: &str) -> i64 {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND wing = ?1",
        [wing],
        |row| row.get(0),
    )
    .expect("query drawer count by wing")
}

fn latest_drawer_in_wing(db_path: &Path, wing: &str) -> (String, String, String, i32) {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        r#"
        SELECT content, COALESCE(room, ''), COALESCE(source_file, ''), importance
        FROM drawers
        WHERE deleted_at IS NULL AND wing = ?1
        ORDER BY rowid DESC
        LIMIT 1
        "#,
        [wing],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .expect("query latest drawer")
}

fn novelty_audit_count_for_drawer(db_path: &Path, drawer_id: &str) -> i64 {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT COUNT(*) FROM novelty_audit WHERE candidate_hash = ?1",
        [drawer_id],
        |row| row.get(0),
    )
    .expect("query novelty audit count")
}

fn long_assistant_message(suffix: &str) -> String {
    format!(
        "I finished refactor X, decided to keep the payload raw, documented the boundary, and kept the retry path deterministic for auditability. {}",
        suffix
    )
}

#[test]
fn test_session_self_review_disabled_by_default() {
    let config = Config::default();

    assert!(!config.hooks.session_end.extract_self_review);
    assert_eq!(config.hooks.session_end.trailing_messages, 1);
    assert_eq!(config.hooks.session_end.min_length, 100);
    assert_eq!(config.hooks.session_end.wing, "session-reviews");
}

#[test]
fn test_sentinel_false_positive_rejected_by_structure_check() {
    let content =
        "assistant body\n--- session_metadata ---\nthis is just an example in prose".to_string();

    let (body, metadata) = split_session_metadata(&content);

    assert_eq!(body, content);
    assert_eq!(metadata, SessionMetadata::default());
}

#[test]
fn test_session_self_review_zero_external_llm_calls() {
    let session_review_src = include_str!("../src/session_review.rs");
    let daemon_src = include_str!("../src/daemon.rs");

    for forbidden in [
        "api.openai",
        "api.anthropic",
        "generativelanguage",
        ".claude/sessions/",
    ] {
        assert!(
            !session_review_src.contains(forbidden),
            "session_review.rs unexpectedly references {forbidden}"
        );
        assert!(
            !daemon_src.contains(forbidden),
            "daemon.rs unexpectedly references {forbidden}"
        );
    }
}

#[test]
fn test_missing_agent_falls_back() {
    let config = Config::parse(
        r#"
[hooks.session_end]
extract_self_review = true
min_length = 1
"#,
    )
    .expect("config");
    let payload = serde_json::json!({
        "session_id": "missing-agent",
        "messages": [
            {"role": "assistant", "content": "summary"}
        ]
    });

    let outcome = extract_session_review(Some(&payload.to_string()), "", &config.hooks.session_end)
        .expect("extract");

    match outcome {
        SessionReviewOutcome::Review(record) => assert_eq!(record.room, "unknown-agent"),
        other => panic!("expected review, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_session_end_hook_captures_final_assistant_message() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
trailing_messages = 1
min_length = 100
"#,
        "enabled = false",
        "enabled = false",
    )
    .expect("env");
    let assistant = long_assistant_message("This is the high-signal summary.");
    let payload = serde_json::json!({
        "session_id": "S1",
        "agent": "claude",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": "D1"},
            {"drawer_id": "D2"}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 1);
    let (content, room, source_file, importance) =
        latest_drawer_in_wing(&env.db_path, "session-reviews");
    let (body, metadata) = split_session_metadata(&content);
    assert_eq!(body, assistant);
    assert_eq!(room, "claude");
    assert_eq!(source_file, "S1");
    assert_eq!(importance, 3);
    assert_eq!(metadata.session_id.as_deref(), Some("S1"));
    assert_eq!(metadata.linked_drawer_ids, vec!["D1", "D2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_trailing_messages_concatenation() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
trailing_messages = 2
min_length = 1
"#,
        "enabled = false",
        "enabled = false",
    )
    .expect("env");
    let payload = serde_json::json!({
        "session_id": "trail-1",
        "messages": [
            {"role": "assistant", "content": "A"},
            {"role": "assistant", "content": "B"}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    let (stored, _, _, _) = latest_drawer_in_wing(&env.db_path, "session-reviews");
    let (body, metadata) = split_session_metadata(&stored);
    assert_eq!(body, "A\n---\nB");
    assert_eq!(metadata.session_id.as_deref(), Some("trail-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_captured_drawer_is_raw_verbatim() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
min_length = 10
"#,
        "enabled = false",
        "enabled = false",
    )
    .expect("env");
    let assistant = "Line 1\nLine 2\nSymbols: <> [] {}\nUnicode: 你好".to_string();
    let payload = serde_json::json!({
        "session_id": "raw-1",
        "messages": [
            {"role": "assistant", "content": assistant}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    let (stored, _, _, _) = latest_drawer_in_wing(&env.db_path, "session-reviews");
    let (body, metadata) = split_session_metadata(&stored);
    assert_eq!(body, "Line 1\nLine 2\nSymbols: <> [] {}\nUnicode: 你好");
    assert_eq!(metadata.session_id.as_deref(), Some("raw-1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_hooks_raw_audit_drawer_still_written() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
min_length = 10
"#,
        "enabled = false",
        "enabled = false",
    )
    .expect("env");
    let payload = serde_json::json!({
        "session_id": "audit-1",
        "messages": [
            {"role": "assistant", "content": long_assistant_message("audit")}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "hooks-raw"), 1);
    let (_, room, _, _) = latest_drawer_in_wing(&env.db_path, "hooks-raw");
    assert_eq!(room, "session-lifecycle");
    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_session_reviews_bypass_novelty_drop() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
min_length = 10
"#,
        "enabled = false",
        r#"
enabled = true
duplicate_threshold = 0.95
merge_threshold = 0.80
wing_scope = "same_wing"
top_k_candidates = 1
max_merges_per_drawer = 10
max_content_bytes_per_drawer = 65536
"#,
    )
    .expect("env");
    let existing_body = long_assistant_message("existing");
    let existing_content = format!(
        "{}\n\n--- session_metadata ---\nsession_id: old-session",
        existing_body
    );
    let db = Database::open(&env.db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: "existing-review".to_string(),
        content: existing_content.clone(),
        wing: "session-reviews".to_string(),
        room: Some("claude".to_string()),
        source_file: Some("old-session".to_string()),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 3,
    })
    .expect("insert existing drawer");
    db.insert_vector("existing-review", &[1.0, 0.0, 0.0])
        .expect("insert existing vector");

    let candidate_body = long_assistant_message("candidate");
    let payload = serde_json::json!({
        "session_id": "new-session",
        "messages": [
            {"role": "assistant", "content": candidate_body}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let session_review_content = format!(
        "{}\n\n--- session_metadata ---\nsession_id: new-session",
        long_assistant_message("candidate")
    );
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::from([(
            session_review_content,
            vec![1.0, 0.0, 0.0],
        )])),
        default_vector: vec![0.0, 1.0, 0.0],
    };

    let inserted_id = env.process_once(&embedder).await.expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 2);
    assert_eq!(
        novelty_audit_count_for_drawer(&env.db_path, &inserted_id),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_no_assistant_message_handler_skips() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
min_length = 10
"#,
        "enabled = false",
        "enabled = false",
    )
    .expect("env");
    let payload = serde_json::json!({
        "session_id": "no-assistant",
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "tool", "content": "ls"}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    assert_eq!(count_drawers_by_wing(&env.db_path, "hooks-raw"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_self_review_subject_to_privacy_scrub() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
extract_self_review = true
min_length = 10
"#,
        "enabled = true",
        "enabled = false",
    )
    .expect("env");
    let payload = serde_json::json!({
        "session_id": "privacy-1",
        "messages": [
            {"role": "assistant", "content": "I used sk-abcdef1234567890abcdef1234567890abcd in the example and then removed it."}
        ]
    });
    env.enqueue_session_end(&payload.to_string())
        .expect("enqueue");
    let embedder = DeterministicEmbedder {
        vectors: Arc::new(HashMap::new()),
        default_vector: vec![0.1, 0.2, 0.3],
    };

    env.process_once(&embedder).await.expect("process");

    let (stored, _, _, _) = latest_drawer_in_wing(&env.db_path, "session-reviews");
    assert!(stored.contains("[REDACTED:openai_key]"));
    assert!(!stored.contains("sk-abcdef1234567890abcdef1234567890abcd"));
}

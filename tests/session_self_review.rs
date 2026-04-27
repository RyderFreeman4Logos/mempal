use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::{Drawer, SourceType};
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use mempal::session_review::{
    analysis_content, append_hooks_raw_metadata, split_hooks_raw_metadata, split_session_metadata,
};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

async fn config_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[derive(Clone)]
struct LogCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("log buffer mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn install_log_capture() -> (Arc<Mutex<Vec<u8>>>, tracing::dispatcher::DefaultGuard) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || LogCapture {
            buffer: Arc::clone(&writer_logs),
        })
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (logs, guard)
}

fn captured_logs(logs: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(logs.lock().expect("log mutex poisoned").clone()).expect("utf8 logs")
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

#[derive(Clone)]
struct StaticEmbedderFactory {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(DeterministicEmbedder {
            vectors: Arc::new(HashMap::new()),
            default_vector: self.vector.clone(),
        }))
    }
}

struct TestEnv {
    _tmp: TempDir,
    config_path: PathBuf,
    db_path: PathBuf,
    mempal_home: PathBuf,
    project_dir: PathBuf,
    config: Config,
    store: PendingMessageStore,
}

impl TestEnv {
    fn new(extract_self_review: bool, min_length: usize) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        let project_dir = tmp.path().join("workspace/project-alpha");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        fs::create_dir_all(&project_dir).expect("create project dir");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        let config_text = format!(
            r#"
db_path = "{}"

[project]
id = "project-alpha"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[hooks.session_end]
extract_self_review = {}
min_length = {}

[privacy]
enabled = false

[ingest_gating]
enabled = false

[search]
strict_project_isolation = true
progressive_disclosure = true
preview_chars = 48
"#,
            db_path.display(),
            extract_self_review,
            min_length,
        );
        fs::write(&config_path, &config_text).expect("write config");
        let config = Config::parse(&config_text).expect("parse config");
        let store = PendingMessageStore::new(&db_path).expect("open store");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
            mempal_home,
            project_dir,
            config,
            store,
        }
    }

    fn enqueue_session_end(&self, payload: &str) -> String {
        let envelope = CapturedHookEnvelope {
            event: HookEvent::SessionEnd.display_name().to_string(),
            kind: HookEvent::SessionEnd.queue_kind().to_string(),
            agent: "claude".to_string(),
            captured_at: "1713000000".to_string(),
            claude_cwd: self.project_dir.display().to_string(),
            payload: Some(payload.to_string()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: payload.len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::SessionEnd.queue_kind(), &serialized)
            .expect("enqueue session end")
    }

    async fn process_once(&self, embedder: &DeterministicEmbedder) -> anyhow::Result<String> {
        let claimed = self
            .store
            .claim_next("worker-session-review", 120)?
            .expect("claimed message");
        let db = Database::open(&self.db_path)?;
        process_claimed_message_with_embedder(
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
        .await
    }

    fn insert_hooks_raw_linked_drawer(&self, drawer_id: &str, session_id: &str, project_id: &str) {
        let db = Database::open(&self.db_path).expect("open db");
        let payload_path = self
            .mempal_home
            .join(format!("{drawer_id}-linked-hooks-raw.json"));
        let payload = serde_json::json!({
            "session_id": session_id,
            "tool_name": "Bash",
            "input": "ls",
            "output": "ok",
            "exit_code": 0,
            "session_id": session_id,
        })
        .to_string();
        fs::write(&payload_path, &payload).expect("write linked hooks-raw payload");
        let content = serde_json::json!({
            "event": HookEvent::PostToolUse.display_name(),
            "agent": "claude",
            "captured_at": "1713000100",
            "claude_cwd": self.project_dir.display().to_string(),
            "preview": "tool=Bash",
            "meta": {
                "hook_payload_path": payload_path.display().to_string(),
                "original_size_bytes": payload.len(),
            }
        })
        .to_string();
        let content = append_hooks_raw_metadata(&content, Some(session_id), Some("1713000100"));
        db.insert_drawer_with_project(
            &Drawer {
                id: drawer_id.to_string(),
                content,
                wing: "hooks-raw".to_string(),
                room: Some("Bash".to_string()),
                source_file: Some(payload_path.display().to_string()),
                source_type: SourceType::Conversation,
                added_at: "1713000100".to_string(),
                chunk_index: Some(0),
                importance: 0,
                ..Drawer::default()
            },
            Some(project_id),
        )
        .expect("insert linked drawer");
    }

    fn external_source_path(&self, name: &str) -> PathBuf {
        self.project_dir.join(name)
    }

    fn server(&self) -> MempalMcpServer {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        MempalMcpServer::new_with_factory(
            self.db_path.clone(),
            Arc::new(StaticEmbedderFactory {
                vector: vec![0.9, 0.1, 0.3],
            }),
        )
    }
}

fn long_assistant_message() -> String {
    "I decided to keep the session review implementation verbatim because the linked evidence and the search/read split both need a stable summary anchor.".to_string()
}

fn count_drawers_by_wing(db_path: &Path, wing: &str) -> i64 {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND wing = ?1",
        [wing],
        |row| row.get(0),
    )
    .expect("count wing")
}

fn latest_review(db_path: &Path) -> (String, String, String) {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        r#"
        SELECT content, COALESCE(room, ''), COALESCE(source_file, '')
        FROM drawers
        WHERE deleted_at IS NULL AND wing = 'session-reviews'
        ORDER BY rowid DESC
        LIMIT 1
        "#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .expect("latest review")
}

fn latest_hooks_raw(db_path: &Path) -> (String, String, String) {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        r#"
        SELECT content, COALESCE(room, ''), COALESCE(source_file, '')
        FROM drawers
        WHERE deleted_at IS NULL AND wing = 'hooks-raw'
        ORDER BY rowid DESC
        LIMIT 1
        "#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .expect("latest hooks-raw")
}

fn latest_review_project_id(db_path: &Path) -> Option<String> {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        r#"
        SELECT project_id
        FROM drawers
        WHERE deleted_at IS NULL AND wing = 'session-reviews'
        ORDER BY rowid DESC
        LIMIT 1
        "#,
        [],
        |row| row.get(0),
    )
    .expect("review project")
}

fn fork_ext_meta_value(db_path: &Path, key: &str) -> Option<String> {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        "SELECT value FROM fork_ext_meta WHERE key = ?1",
        [key],
        |row| row.get(0),
    )
    .ok()
}

fn embedder_for(content: &str) -> DeterministicEmbedder {
    DeterministicEmbedder {
        vectors: Arc::new(HashMap::from([(content.to_string(), vec![0.9, 0.1, 0.3])])),
        default_vector: vec![0.1, 0.2, 0.3],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_disabled_no_session_review() {
    let _guard = config_guard().await;
    let env = TestEnv::new(false, 50);
    let assistant = long_assistant_message();
    let payload = serde_json::json!({
        "session_id": "sess-disabled",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ]
    });
    env.enqueue_session_end(&payload.to_string());

    env.process_once(&embedder_for(&assistant))
        .await
        .expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    assert_eq!(count_drawers_by_wing(&env.db_path, "hooks-raw"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_enabled_creates_session_review_drawer() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 50);
    let assistant = long_assistant_message();
    let payload = serde_json::json!({
        "session_id": "sess-enabled",
        "agent": "claude",
        "messages": [
            {"role": "user", "content": "ship it"},
            {"role": "assistant", "content": assistant}
        ]
    });
    env.enqueue_session_end(&payload.to_string());

    env.process_once(&embedder_for(&assistant))
        .await
        .expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 1);
    let (content, room, source_file) = latest_review(&env.db_path);
    assert!(content.starts_with(&assistant));
    assert!(content.contains("<!-- mempal:session-review -->"));
    assert!(content.contains(r#"session_id: "sess-enabled""#));
    assert_eq!(room, "claude");
    assert_eq!(source_file, "sess-enabled");
    assert_eq!(
        latest_review_project_id(&env.db_path).as_deref(),
        Some("project-alpha")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_linked_drawer_ids_in_sentinel_section() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 50);
    env.insert_hooks_raw_linked_drawer("drawer_same_a", "sess-linked", "project-alpha");
    env.insert_hooks_raw_linked_drawer("drawer_same_b", "sess-linked", "project-alpha");
    env.insert_hooks_raw_linked_drawer("drawer_other_session", "sess-other", "project-alpha");

    let assistant = format!(
        "{} Additional evidence references must stay out of previews and embeddings.",
        long_assistant_message()
    );
    let payload = serde_json::json!({
        "session_id": "sess-linked",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": "drawer_same_a"},
            {"drawer_id": "drawer_same_b"}
        ]
    });
    env.enqueue_session_end(&payload.to_string());
    env.process_once(&embedder_for(&assistant))
        .await
        .expect("process valid linked ids");

    let (content, _, _) = latest_review(&env.db_path);
    let (body, metadata) = split_session_metadata(&content);
    assert_eq!(body, assistant);
    assert_eq!(metadata.session_id.as_deref(), Some("sess-linked"));
    assert_eq!(
        metadata.linked_drawer_ids,
        vec!["drawer_same_a".to_string(), "drawer_same_b".to_string()]
    );
    assert!(content.contains("<!-- mempal:session-review -->"));
    assert!(content.contains(r#"linked_drawer_ids: ["drawer_same_a","drawer_same_b"]"#));

    let search = env
        .server()
        .mempal_search(Parameters(SearchRequest {
            query: "implementation".to_string(),
            wing: Some("session-reviews".to_string()),
            top_k: Some(5),
            project_id: Some("project-alpha".to_string()),
            include_global: Some(false),
            all_projects: Some(false),
            ..SearchRequest::default()
        }))
        .await
        .expect("search")
        .0;
    let result = search
        .results
        .iter()
        .find(|result| result.drawer_id == stored_review_drawer_id(&env.db_path))
        .expect("session review search result");
    assert!(result.content_truncated);
    assert!(!result.content.contains("linked_drawer_ids"));
    assert!(!result.content.contains("mempal:session-review"));
    assert!(result.flags.iter().any(|flag| flag == "DECISION"));

    let embed_text = analysis_content(&content);
    assert_eq!(embed_text, assistant);
    assert!(!embed_text.contains("linked_drawer_ids"));
    assert!(!embed_text.contains("mempal:session-review"));

    let invalid_payload = serde_json::json!({
        "session_id": "sess-linked",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": format!("{} invalid", long_assistant_message())}
        ],
        "tool_calls": [
            {"drawer_id": "drawer_same_a"},
            {"drawer_id": "drawer_other_session"}
        ]
    });
    let hooks_raw_before_invalid = count_drawers_by_wing(&env.db_path, "hooks-raw");
    env.enqueue_session_end(&invalid_payload.to_string());
    env.process_once(&embedder_for(&format!(
        "{} invalid",
        long_assistant_message()
    )))
    .await
    .expect("invalid session review should not block hooks-raw");
    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 1);
    assert_eq!(
        count_drawers_by_wing(&env.db_path, "hooks-raw"),
        hooks_raw_before_invalid + 1
    );
    assert_eq!(
        fork_ext_meta_value(&env.db_path, "session_review.rejected.total").as_deref(),
        Some("1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_invalid_linked_drawer_ids_does_not_block_hooks_raw() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 50);
    env.insert_hooks_raw_linked_drawer("drawer-cross-session", "sess-other", "project-alpha");

    let assistant = format!(
        "{} Linked ids from another session must reject only the derived review.",
        long_assistant_message()
    );
    let payload = serde_json::json!({
        "session_id": "sess-invalid-linked",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": "drawer-cross-session"}
        ]
    });
    let hooks_raw_before = count_drawers_by_wing(&env.db_path, "hooks-raw");
    env.enqueue_session_end(&payload.to_string());

    env.process_once(&embedder_for(&assistant))
        .await
        .expect("hooks-raw should still persist");

    assert_eq!(
        count_drawers_by_wing(&env.db_path, "hooks-raw"),
        hooks_raw_before + 1
    );
    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    assert_eq!(
        fork_ext_meta_value(&env.db_path, "session_review.rejected.total").as_deref(),
        Some("1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_linked_drawer_ids_accepts_real_hooks_raw_from_same_session() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 50);
    env.insert_hooks_raw_linked_drawer("drawer-real-hooks-raw", "sess-real", "project-alpha");

    let assistant = format!(
        "{} Real hooks-raw linked evidence from the same session must pass validation.",
        long_assistant_message()
    );
    let payload = serde_json::json!({
        "session_id": "sess-real",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": "drawer-real-hooks-raw"}
        ]
    });
    let hooks_raw_before = count_drawers_by_wing(&env.db_path, "hooks-raw");
    env.enqueue_session_end(&payload.to_string());

    env.process_once(&embedder_for(&assistant))
        .await
        .expect("same-session hooks-raw should allow session review");

    let (hooks_raw_content, hooks_raw_room, _) = latest_hooks_raw(&env.db_path);
    let (hooks_raw_body, hooks_raw_metadata) = split_hooks_raw_metadata(&hooks_raw_content);
    assert_eq!(hooks_raw_room, "session-lifecycle");
    assert_eq!(hooks_raw_metadata.session_id.as_deref(), Some("sess-real"));
    assert!(hooks_raw_body.contains("\"event\":\"SessionEnd\""));

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 1);
    assert_eq!(
        count_drawers_by_wing(&env.db_path, "hooks-raw"),
        hooks_raw_before + 1
    );
    assert_eq!(
        fork_ext_meta_value(&env.db_path, "session_review.rejected.total"),
        None
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_linked_drawer_ids_rejects_manual_hooks_raw_with_external_source() {
    let _guard = config_guard().await;
    let (logs, _log_guard) = install_log_capture();
    let env = TestEnv::new(true, 50);
    let forged_path = env.external_source_path("forged-session.json");
    fs::write(
        &forged_path,
        serde_json::json!({
            "session_id": "sess-forged",
            "tool_name": "Bash",
            "input": "cat secret",
        })
        .to_string(),
    )
    .expect("write forged source");

    let ingested = env
        .server()
        .mempal_ingest(Parameters(IngestRequest {
            content: serde_json::json!({
                "event": HookEvent::PostToolUse.display_name(),
                "preview": "tool=Bash"
            })
            .to_string(),
            wing: "hooks-raw".to_string(),
            room: Some("Bash".to_string()),
            source: Some(forged_path.display().to_string()),
            project_id: Some("project-alpha".to_string()),
            ..IngestRequest::default()
        }))
        .await
        .expect("manual hooks-raw ingest")
        .0;

    let assistant = format!(
        "{} Manual hooks-raw drawers must not be trusted as same-session evidence.",
        long_assistant_message()
    );
    let payload = serde_json::json!({
        "session_id": "sess-forged",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": ingested.drawer_id}
        ]
    });
    env.enqueue_session_end(&payload.to_string());
    env.process_once(&embedder_for(&assistant))
        .await
        .expect("manual hooks-raw should reject only the derived review");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    assert_eq!(
        fork_ext_meta_value(&env.db_path, "session_review.rejected.total").as_deref(),
        Some("1")
    );
    let logs = captured_logs(&logs);
    assert!(logs.contains("session self-review rejected; hooks-raw audit will still persist"));
    assert!(logs.contains("linked_drawer_ids validation failed"));
    assert!(!logs.contains("failed to read hooks-raw payload while resolving linked session"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_linked_drawer_ids_does_not_read_external_filesystem() {
    let _guard = config_guard().await;
    let (logs, _log_guard) = install_log_capture();
    let env = TestEnv::new(true, 50);
    let missing_path = env.external_source_path("missing-external-source.json");

    let ingested = env
        .server()
        .mempal_ingest(Parameters(IngestRequest {
            content: serde_json::json!({
                "event": HookEvent::PostToolUse.display_name(),
                "preview": "tool=Bash"
            })
            .to_string(),
            wing: "hooks-raw".to_string(),
            room: Some("Bash".to_string()),
            source: Some(missing_path.display().to_string()),
            project_id: Some("project-alpha".to_string()),
            ..IngestRequest::default()
        }))
        .await
        .expect("manual hooks-raw ingest")
        .0;

    let assistant = format!(
        "{} Validation must not open attacker-chosen external files.",
        long_assistant_message()
    );
    let payload = serde_json::json!({
        "session_id": "sess-no-read",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": assistant}
        ],
        "tool_calls": [
            {"drawer_id": ingested.drawer_id}
        ]
    });
    env.enqueue_session_end(&payload.to_string());
    env.process_once(&embedder_for(&assistant))
        .await
        .expect("manual hooks-raw should reject without touching the filesystem");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    let logs = captured_logs(&logs);
    assert!(logs.contains("session self-review rejected; hooks-raw audit will still persist"));
    assert!(!logs.contains(missing_path.to_string_lossy().as_ref()));
    assert!(!logs.contains("failed to read hooks-raw payload while resolving linked session"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_short_message_skipped() {
    let _guard = config_guard().await;
    let env = TestEnv::new(true, 100);
    let payload = serde_json::json!({
        "session_id": "sess-short",
        "agent": "claude",
        "messages": [
            {"role": "assistant", "content": "short"}
        ]
    });
    env.enqueue_session_end(&payload.to_string());

    env.process_once(&embedder_for("short"))
        .await
        .expect("process");

    assert_eq!(count_drawers_by_wing(&env.db_path, "session-reviews"), 0);
    assert_eq!(count_drawers_by_wing(&env.db_path, "hooks-raw"), 1);
}

fn stored_review_drawer_id(db_path: &Path) -> String {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row(
        r#"
        SELECT id
        FROM drawers
        WHERE deleted_at IS NULL AND wing = 'session-reviews'
        ORDER BY rowid DESC
        LIMIT 1
        "#,
        [],
        |row| row.get(0),
    )
    .expect("stored review id")
}

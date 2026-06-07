use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use mempal::core::AsyncDb;
use mempal::core::config::{Config, ConfigHandle};
use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::TaxonomyEntry;
use mempal::daemon::{DaemonIngestContext, process_claimed_message_with_embedder};
use mempal::embed::{EmbedError, Embedder};
use mempal::hook::{CapturedHookEnvelope, HookEvent};
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

struct StaticEmbedder;

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "static-test"
    }
}

struct TestEnv {
    _tmp: TempDir,
    config: Config,
    db_path: PathBuf,
    mempal_home: PathBuf,
    project_dir: PathBuf,
    store: PendingMessageStore,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        let project_dir = tmp.path().join("warifu-ce");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        fs::create_dir_all(&project_dir).expect("create project dir");

        let db_path = mempal_home.join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        db.upsert_taxonomy_entry(&TaxonomyEntry {
            wing: "warifu-ce".to_string(),
            room: "decision".to_string(),
            display_name: Some("Decision".to_string()),
            keywords: vec!["decision".to_string(), "decided".to_string()],
        })
        .expect("seed taxonomy");

        let config_path = mempal_home.join("config.toml");
        let config_text = format!(
            r#"
db_path = "{}"

[project]
id = "warifu-ce"

[hooks]
enabled = true
wing = "warifu-ce"

[privacy]
enabled = false

[ingest_gating]
enabled = true

[patterns]
enabled = false

[repair]
enabled = false
"#,
            db_path.display()
        );
        fs::write(&config_path, &config_text).expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        let config = Config::parse(&config_text).expect("parse config");
        let store = PendingMessageStore::new(&db_path).expect("open queue");

        Self {
            _tmp: tmp,
            config,
            db_path,
            mempal_home,
            project_dir,
            store,
        }
    }

    fn enqueue_user_prompt(&self, agent: &str, prompt: &str) -> String {
        let payload = serde_json::json!({
            "agent": agent,
            "session_id": "codex-session-issue-150",
            "prompt": prompt,
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::UserPromptSubmit.display_name().to_string(),
            kind: HookEvent::UserPromptSubmit.queue_kind().to_string(),
            agent: agent.to_string(),
            captured_at: "2026-05-05T12:00:00Z".to_string(),
            claude_cwd: self.project_dir.display().to_string(),
            payload: Some(payload),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: prompt.len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::UserPromptSubmit.queue_kind(), &serialized)
            .expect("enqueue prompt")
    }

    async fn process_once(&self) {
        let claimed = self
            .store
            .claim_next("hook-project-promotion-test", 120)
            .expect("claim next")
            .expect("claimed message");
        let async_db = AsyncDb::open(&self.db_path, 4).expect("open async db");
        process_claimed_message_with_embedder(
            &async_db,
            &self.store,
            "hook-project-promotion-test",
            &claimed,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &self.config,
                mempal_home: &self.mempal_home,
            },
        )
        .await
        .expect("process hook message");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_codex_user_prompt_promotes_to_project_wing() {
    let _guard = config_guard().await;
    let env = TestEnv::new();
    let prompt = "Decision: promote Codex user-prompt hook captures into project memory.";

    env.enqueue_user_prompt("codex", prompt);
    env.process_once().await;

    assert_eq!(
        count_drawers(&env.db_path, "hooks-raw", "user-prompt"),
        1,
        "raw hook audit drawer must still be stored"
    );
    assert_eq!(
        count_drawers(&env.db_path, "warifu-ce", "decision"),
        1,
        "codex user prompt must be promoted to the configured project wing"
    );

    let promoted = promoted_drawer(&env.db_path);
    assert!(promoted.content.contains(prompt), "{}", promoted.content);
    assert_eq!(promoted.project_id.as_deref(), Some("warifu-ce"));
    assert!(
        Path::new(&promoted.source_file).exists(),
        "promoted drawer must cite the persisted raw hook payload"
    );
    assert_eq!(
        gating_audit_count(&env.db_path, "warifu-ce", "decision"),
        1,
        "promoted project drawer must pass through the normal gating audit"
    );
}

struct PromotedDrawer {
    content: String,
    source_file: String,
    project_id: Option<String>,
}

fn promoted_drawer(db_path: &Path) -> PromotedDrawer {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            r#"
            SELECT content, source_file, project_id
            FROM drawers
            WHERE deleted_at IS NULL AND wing = 'warifu-ce' AND room = 'decision'
            LIMIT 1
            "#,
            [],
            |row| {
                Ok(PromotedDrawer {
                    content: row.get(0)?,
                    source_file: row.get(1)?,
                    project_id: row.get(2)?,
                })
            },
        )
        .expect("promoted drawer")
}

fn count_drawers(db_path: &Path, wing: &str, room: &str) -> i64 {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND wing = ?1 AND room = ?2",
            (wing, room),
            |row| row.get(0),
        )
        .expect("count drawers")
}

fn gating_audit_count(db_path: &Path, wing: &str, room: &str) -> i64 {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM gating_audit audit
            JOIN drawers drawer ON drawer.id = audit.drawer_id
            WHERE drawer.deleted_at IS NULL
              AND drawer.wing = ?1
              AND drawer.room = ?2
            "#,
            (wing, room),
            |row| row.get(0),
        )
        .expect("count gating audits")
}

fn count_drawers_by_wing_only(db_path: &Path, wing: &str) -> i64 {
    Connection::open(db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND wing = ?1",
            [wing],
            |row| row.get(0),
        )
        .expect("count drawers by wing")
}

struct MinimalEnv {
    _tmp: TempDir,
    config: Config,
    db_path: PathBuf,
    mempal_home: PathBuf,
    store: PendingMessageStore,
}

impl MinimalEnv {
    fn new(hooks_wing: &str, project_wing: Option<&str>, project_dir_name: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        if !project_dir_name.is_empty() && project_dir_name != "/" {
            let _ = fs::create_dir_all(tmp.path().join(project_dir_name));
        }

        let db_path = mempal_home.join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        db.upsert_taxonomy_entry(&TaxonomyEntry {
            wing: project_wing.unwrap_or(project_dir_name).to_string(),
            room: "decision".to_string(),
            display_name: Some("Decision".to_string()),
            keywords: vec!["decision".to_string()],
        })
        .expect("seed taxonomy");

        let project_id_line = project_wing
            .map(|id| format!("\n[project]\nid = \"{id}\"\n"))
            .unwrap_or_default();
        let config_text = format!(
            r#"
db_path = "{}"
{}
[hooks]
enabled = true
wing = "{}"

[privacy]
enabled = false

[ingest_gating]
enabled = true

[patterns]
enabled = false

[repair]
enabled = false
"#,
            db_path.display(),
            project_id_line,
            hooks_wing
        );
        let config_path = mempal_home.join("config.toml");
        fs::write(&config_path, &config_text).expect("write config");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        let config = Config::parse(&config_text).expect("parse config");
        let store = PendingMessageStore::new(&db_path).expect("open queue");

        Self {
            _tmp: tmp,
            config,
            db_path,
            mempal_home,
            store,
        }
    }

    fn enqueue_user_prompt_with_cwd(&self, agent: &str, prompt: &str, cwd: &str) {
        let payload = serde_json::json!({
            "agent": agent,
            "session_id": "test-session-wing-routing",
            "prompt": prompt,
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::UserPromptSubmit.display_name().to_string(),
            kind: HookEvent::UserPromptSubmit.queue_kind().to_string(),
            agent: agent.to_string(),
            captured_at: "2026-05-10T12:00:00Z".to_string(),
            claude_cwd: cwd.to_string(),
            payload: Some(payload),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: prompt.len(),
            truncated: false,
        };
        let serialized = serde_json::to_string(&envelope).expect("serialize envelope");
        self.store
            .enqueue(HookEvent::UserPromptSubmit.queue_kind(), &serialized)
            .expect("enqueue prompt");
    }

    async fn process_once(&self) {
        let claimed = self
            .store
            .claim_next("wing-routing-test", 120)
            .expect("claim next")
            .expect("claimed message");
        let async_db = AsyncDb::open(&self.db_path, 4).expect("open async db");
        process_claimed_message_with_embedder(
            &async_db,
            &self.store,
            "wing-routing-test",
            &claimed,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                config: &self.config,
                mempal_home: &self.mempal_home,
            },
        )
        .await
        .expect("process hook message");
    }
}

/// Wing comes from the basename of claude_cwd, not from config.hooks.wing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_wing_derived_from_cwd_basename_not_config() {
    let _guard = config_guard().await;
    // config.hooks.wing = "agent-diary", but claude_cwd points to a "mempal" directory.
    let env = MinimalEnv::new("agent-diary", None, "mempal");
    let prompt = "Decision: wing routing must use cwd project name.";
    let cwd = env._tmp.path().join("mempal").display().to_string();

    env.enqueue_user_prompt_with_cwd("claude", prompt, &cwd);
    env.process_once().await;

    assert_eq!(
        count_drawers(&env.db_path, "mempal", "decision"),
        1,
        "promoted drawer wing must be the cwd basename, not config.hooks.wing"
    );
    assert_eq!(
        count_drawers_by_wing_only(&env.db_path, "agent-diary"),
        0,
        "no drawers must land in the configured hooks.wing when cwd is available"
    );
}

/// When claude_cwd is empty, wing falls back to config.hooks.wing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_wing_falls_back_to_config_when_cwd_empty() {
    let _guard = config_guard().await;
    let env = MinimalEnv::new("warifu-ce", Some("warifu-ce"), "warifu-ce");
    let prompt = "Decision: fallback to config wing when cwd is empty.";

    env.enqueue_user_prompt_with_cwd("claude", prompt, "");
    env.process_once().await;

    assert_eq!(
        count_drawers(&env.db_path, "warifu-ce", "decision"),
        1,
        "wing must fall back to config.hooks.wing when claude_cwd is empty"
    );
}

/// When claude_cwd is root "/", file_name() returns None, so wing falls back to config.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_wing_falls_back_to_config_when_cwd_root() {
    let _guard = config_guard().await;
    let env = MinimalEnv::new("warifu-ce", Some("warifu-ce"), "warifu-ce");
    let prompt = "Decision: fallback to config wing when cwd is root path.";

    env.enqueue_user_prompt_with_cwd("claude", prompt, "/");
    env.process_once().await;

    assert_eq!(
        count_drawers(&env.db_path, "warifu-ce", "decision"),
        1,
        "wing must fall back to config.hooks.wing when claude_cwd has no basename"
    );
}

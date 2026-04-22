mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use common::harness::McpStdio;
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::project::infer_project_id_from_path;
use mempal::core::protocol::MEMORY_PROTOCOL;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory, global_embed_status};
use mempal::mcp::{MempalMcpServer, TimelineRequest};
use rmcp::ServerHandler;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};
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

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_secs() as i64
}

#[derive(Clone)]
struct PanicEmbedderFactory;

#[async_trait]
impl EmbedderFactory for PanicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        panic!("mempal_timeline must not build an embedder");
    }
}

struct TimelineEnv {
    _tmp: TempDir,
    db_path: PathBuf,
}

impl TimelineEnv {
    fn new(project_id: Option<&str>, degrade_after_failures: u64) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config_atomic(
            &config_path,
            &timeline_config(&db_path, project_id, degrade_after_failures),
        );
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self { _tmp: tmp, db_path }
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(self.db_path.clone(), Arc::new(PanicEmbedderFactory))
    }
}

fn timeline_config(
    db_path: &Path,
    project_id: Option<&str>,
    degrade_after_failures: u64,
) -> String {
    let project_section = project_id
        .map(|project_id| format!("\n[project]\nid = \"{project_id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"
{}
[embed]
backend = "model2vec"

[embed.degradation]
degrade_after_n_failures = {}
block_writes_when_degraded = true

[search]
strict_project_isolation = false
progressive_disclosure = true
preview_chars = 200
"#,
        db_path.display(),
        project_section,
        degrade_after_failures
    )
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, contents).expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

struct DrawerSeed<'a> {
    id: &'a str,
    content: &'a str,
    wing: &'a str,
    room: Option<&'a str>,
    added_at: i64,
    importance: i32,
    project_id: Option<&'a str>,
}

fn insert_drawer(db_path: &Path, seed: DrawerSeed<'_>) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer_with_project(
        &Drawer {
            id: seed.id.to_string(),
            content: seed.content.to_string(),
            wing: seed.wing.to_string(),
            room: seed.room.map(ToOwned::to_owned),
            source_file: Some(format!("/tmp/{}.md", seed.id)),
            source_type: SourceType::Manual,
            added_at: seed.added_at.to_string(),
            chunk_index: Some(0),
            importance: seed.importance,
        },
        seed.project_id,
    )
    .expect("insert drawer");
}

fn expected_project_id(path: &Path) -> String {
    infer_project_id_from_path(path)
        .expect("infer project id")
        .expect("project id present")
}

async fn call_mcp_timeline(client: &mut McpStdio, arguments: Value) -> Value {
    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        client.call(
            "tools/call",
            json!({
                "name": "mempal_timeline",
                "arguments": arguments,
            }),
        ),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_timeline failed: {error}\nstderr:\n{stderr}");
        }
        Err(_) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_timeline timed out\nstderr:\n{stderr}");
        }
    };
    result["structuredContent"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_default_ordering() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    let foo_project = tmp.path().join("workspace").join("foo");
    fs::create_dir_all(&foo_project).expect("create foo project");
    Database::open(&db_path).expect("open db");

    let foo_project_id = expected_project_id(&foo_project);
    let now = now_secs();
    for index in 0..10usize {
        let importance = match index {
            0..=2 => 5,
            3..=5 => 4,
            6..=7 => 3,
            8 => 2,
            _ => 1,
        };
        let drawer_id = format!("drawer-{index}");
        let content = format!("timeline entry {index}");
        insert_drawer(
            &db_path,
            DrawerSeed {
                id: &drawer_id,
                content: &content,
                wing: "decisions",
                room: Some("core"),
                added_at: now - ((index as i64 + 1) * 60),
                importance,
                project_id: Some(&foo_project_id),
            },
        );
    }

    let mut client = McpStdio::start(&db_path, HashMap::new())
        .await
        .expect("start mcp stdio");
    let root_uri = format!("file://{}", foo_project.display());
    client
        .initialize_with_roots(&[&root_uri])
        .await
        .expect("initialize with roots");

    let response = call_mcp_timeline(&mut client, json!({})).await;
    let entries = response["entries"].as_array().expect("entries array");

    assert_eq!(entries.len(), 10);
    assert_eq!(
        entries[0]["drawer_id"].as_str().expect("drawer_id"),
        "drawer-0"
    );
    assert_eq!(
        entries[0]["importance_stars"]
            .as_u64()
            .expect("importance stars"),
        5
    );
    assert_eq!(
        response["stats"]["returned"].as_u64().expect("returned"),
        10
    );

    client.shutdown().await.expect("shutdown client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_enforces_project_scope() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    let foo_project = tmp.path().join("workspace").join("foo");
    let bar_project = tmp.path().join("workspace").join("bar");
    fs::create_dir_all(&foo_project).expect("create foo project");
    fs::create_dir_all(&bar_project).expect("create bar project");
    Database::open(&db_path).expect("open db");

    let foo_project_id = expected_project_id(&foo_project);
    let bar_project_id = expected_project_id(&bar_project);
    let now = now_secs();
    insert_drawer(
        &db_path,
        DrawerSeed {
            id: "drawer-foo",
            content: "foo-only timeline entry",
            wing: "notes",
            room: Some("scope"),
            added_at: now - 60,
            importance: 4,
            project_id: Some(&foo_project_id),
        },
    );
    insert_drawer(
        &db_path,
        DrawerSeed {
            id: "drawer-bar",
            content: "bar-only timeline entry",
            wing: "notes",
            room: Some("scope"),
            added_at: now - 30,
            importance: 5,
            project_id: Some(&bar_project_id),
        },
    );

    let mut client = McpStdio::start(&db_path, HashMap::new())
        .await
        .expect("start mcp stdio");
    let root_uri = format!("file://{}", foo_project.display());
    client
        .initialize_with_roots(&[&root_uri])
        .await
        .expect("initialize with roots");

    let response = call_mcp_timeline(&mut client, json!({})).await;
    let entries = response["entries"].as_array().expect("entries array");

    assert_eq!(
        response["project_id"].as_str(),
        Some(foo_project_id.as_str())
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["drawer_id"].as_str(), Some("drawer-foo"));

    client.shutdown().await.expect("shutdown client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_since_filter_7d() {
    let _guard = config_guard().await;
    let env = TimelineEnv::new(Some("foo"), 10);
    let now = now_secs();
    for index in 0..5usize {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: &format!("recent-{index}"),
                content: &format!("recent timeline entry {index}"),
                wing: "timeline",
                room: Some("recent"),
                added_at: now - ((index as i64 + 1) * 60),
                importance: 3,
                project_id: Some("foo"),
            },
        );
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: &format!("old-{index}"),
                content: &format!("old timeline entry {index}"),
                wing: "timeline",
                room: Some("archive"),
                added_at: now - (30 * 24 * 60 * 60) - index as i64,
                importance: 5,
                project_id: Some("foo"),
            },
        );
    }

    let response = env
        .server()
        .mempal_timeline(Parameters(TimelineRequest {
            project_id: Some("foo".to_string()),
            since: Some("7d".to_string()),
            until: None,
            top_k: None,
            min_importance: None,
            wing: None,
            room: None,
        }))
        .await
        .expect("timeline should succeed")
        .0;

    assert_eq!(response.entries.len(), 5);
    assert_eq!(response.stats.total_in_window, 5);
    assert_eq!(response.stats.returned, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_preview_truncation_signal() {
    let _guard = config_guard().await;
    let env = TimelineEnv::new(Some("foo"), 10);
    let content = "signal ".repeat(71) + "tail";
    assert_eq!(content.len(), 501);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-long",
            content: &content,
            wing: "notes",
            room: Some("preview"),
            added_at: now_secs() - 60,
            importance: 4,
            project_id: Some("foo"),
        },
    );

    let response = env
        .server()
        .mempal_timeline(Parameters(TimelineRequest {
            project_id: Some("foo".to_string()),
            since: None,
            until: None,
            top_k: Some(1),
            min_importance: None,
            wing: None,
            room: None,
        }))
        .await
        .expect("timeline should succeed")
        .0;

    let entry = &response.entries[0];
    assert!(entry.preview_truncated);
    assert!(entry.preview.chars().count() <= 200, "{}", entry.preview);
    assert_eq!(entry.original_content_bytes, 501);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_degraded_embedder_still_works() {
    let _guard = config_guard().await;
    let env = TimelineEnv::new(Some("foo"), 1);
    global_embed_status().reset_for_tests();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-degraded",
            content: "timeline still works when embedder is degraded",
            wing: "notes",
            room: Some("degraded"),
            added_at: now_secs() - 60,
            importance: 4,
            project_id: Some("foo"),
        },
    );
    global_embed_status().record_failure(&"synthetic degraded failure");

    let response = env
        .server()
        .mempal_timeline(Parameters(TimelineRequest {
            project_id: Some("foo".to_string()),
            since: None,
            until: None,
            top_k: None,
            min_importance: None,
            wing: None,
            room: None,
        }))
        .await
        .expect("timeline should succeed while degraded")
        .0;

    assert_eq!(response.entries.len(), 1);
    assert!(response.system_warnings.iter().any(|warning| {
        warning.source == "embed" && warning.message.contains("embed backend degraded")
    }));

    global_embed_status().reset_for_tests();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_top_k_upper_bound() {
    let _guard = config_guard().await;
    let env = TimelineEnv::new(Some("foo"), 10);

    let error = match env
        .server()
        .mempal_timeline(Parameters(TimelineRequest {
            project_id: Some("foo".to_string()),
            since: None,
            until: None,
            top_k: Some(500),
            min_importance: None,
            wing: None,
            room: None,
        }))
        .await
    {
        Ok(_) => panic!("oversized top_k must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    assert!(error.message.contains("top_k exceeds max 100"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_timeline_rejects_all_projects() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    let foo_project = tmp.path().join("workspace").join("foo");
    let bar_project = tmp.path().join("workspace").join("bar");
    fs::create_dir_all(&foo_project).expect("create foo project");
    fs::create_dir_all(&bar_project).expect("create bar project");
    Database::open(&db_path).expect("open db");

    let foo_project_id = expected_project_id(&foo_project);
    let bar_project_id = expected_project_id(&bar_project);
    let now = now_secs();
    insert_drawer(
        &db_path,
        DrawerSeed {
            id: "drawer-foo-only",
            content: "foo timeline entry",
            wing: "notes",
            room: Some("overview"),
            added_at: now - 60,
            importance: 4,
            project_id: Some(&foo_project_id),
        },
    );
    insert_drawer(
        &db_path,
        DrawerSeed {
            id: "drawer-bar-only",
            content: "bar timeline entry",
            wing: "notes",
            room: Some("overview"),
            added_at: now - 30,
            importance: 5,
            project_id: Some(&bar_project_id),
        },
    );

    let mut client = McpStdio::start(&db_path, HashMap::new())
        .await
        .expect("start mcp stdio");
    let root_uri = format!("file://{}", foo_project.display());
    client
        .initialize_with_roots(&[&root_uri])
        .await
        .expect("initialize with roots");

    let response = call_mcp_timeline(&mut client, json!({ "all_projects": true })).await;
    let entries = response["entries"].as_array().expect("entries array");

    assert_eq!(
        response["project_id"].as_str(),
        Some(foo_project_id.as_str())
    );
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["drawer_id"].as_str(), Some("drawer-foo-only"));

    client.shutdown().await.expect("shutdown client");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_memory_protocol_mentions_timeline() {
    let _guard = config_guard().await;
    let env = TimelineEnv::new(Some("foo"), 10);
    let info = <MempalMcpServer as ServerHandler>::get_info(&env.server());
    let instructions = info.instructions.expect("instructions");

    assert!(MEMORY_PROTOCOL.contains("mempal_timeline"));
    assert!(MEMORY_PROTOCOL.contains("project state overview without a specific question"));
    assert!(instructions.contains("mempal_timeline"));
    assert!(instructions.contains("project state overview without a specific question"));
}

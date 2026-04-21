use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
#[cfg(feature = "rest")]
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
#[cfg(feature = "rest")]
use mempal::api::{ApiState, router as api_router};
use mempal::core::config::ConfigHandle;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::core::utils::build_drawer_id;
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{IngestRequest, MempalMcpServer, SearchRequest};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::{Connection, OptionalExtension, params};
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
#[cfg(feature = "rest")]
use tower::ServiceExt;

#[path = "../src/core/db_fork_ext.rs"]
mod db_fork_ext;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

async fn config_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn home_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("home mutex poisoned")
}

#[derive(Clone)]
struct StaticEmbedderFactory {
    vector: Vec<f32>,
}

struct StaticEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder {
            vector: self.vector.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "static"
    }
}

struct SearchEnv {
    _tmp: TempDir,
    config_path: PathBuf,
    db_path: PathBuf,
}

impl SearchEnv {
    fn new(project_id: Option<&str>, strict_project_isolation: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        let config = search_config(&db_path, project_id, strict_project_isolation);
        write_config_atomic(&config_path, &config);
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self {
            _tmp: tmp,
            config_path,
            db_path,
        }
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(
            self.db_path.clone(),
            Arc::new(StaticEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
    }

    #[cfg(feature = "rest")]
    fn api_state(&self) -> ApiState {
        ApiState::new(
            self.db_path.clone(),
            Arc::new(StaticEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
    }
}

fn search_config(
    db_path: &Path,
    project_id: Option<&str>,
    strict_project_isolation: bool,
) -> String {
    let project_section = project_id
        .map(|project_id| format!("\n[project]\nid = \"{project_id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"
{}
[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[config_hot_reload]
enabled = true
debounce_ms = 250
poll_fallback_secs = 1

[search]
strict_project_isolation = {}
"#,
        db_path.display(),
        project_section,
        strict_project_isolation
    )
}

fn cli_config(db_path: &Path) -> String {
    format!(
        r#"
db_path = "{}"

[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[search]
strict_project_isolation = false
"#,
        db_path.display()
    )
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).expect("write temp config");
    fs::rename(&tmp, path).expect("rename config");
}

fn wait_for_config_version_change(previous: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let current = ConfigHandle::version();
        if current != previous {
            return current;
        }
        assert!(Instant::now() < deadline, "config version did not change");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql).expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns")
}

fn sqlite_master_sql(conn: &Connection, table: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type IN ('table', 'index') AND name = ?1",
        [table],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .expect("sqlite_master sql")
}

fn insert_projected_drawer(
    db_path: &Path,
    id: &str,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(str::to_string),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 0,
    })
    .expect("insert drawer");
    db.insert_vector(id, &[0.1, 0.2, 0.3])
        .expect("insert vector");
    db.conn()
        .execute(
            "UPDATE drawers SET project_id = ?2 WHERE id = ?1",
            params![id, project_id],
        )
        .expect("update drawer project");
    db.conn()
        .execute(
            "UPDATE drawer_vectors SET project_id = ?2 WHERE id = ?1",
            params![id, project_id],
        )
        .expect("update vector project");
}

async fn search_response_json(server: &MempalMcpServer, query: &str) -> serde_json::Value {
    search_response_json_with_request(
        server,
        SearchRequest {
            query: query.to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
        },
    )
    .await
}

async fn search_response_json_with_request(
    server: &MempalMcpServer,
    request: SearchRequest,
) -> serde_json::Value {
    let response = server
        .mempal_search(Parameters(request))
        .await
        .expect("search should succeed")
        .0;
    serde_json::to_value(response).expect("serialize search response")
}

#[cfg(feature = "rest")]
async fn rest_json_response(
    env: &SearchEnv,
    request: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let response = api_router(env.api_state())
        .oneshot(request)
        .await
        .expect("router request");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json = serde_json::from_slice(&body).expect("parse json response");
    (status, json)
}

fn install_cli_home(tmp: &TempDir) -> PathBuf {
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create cli mempal home");
    home
}

fn run_mempal(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal")
}

#[test]
fn test_ext_v5_migration_adds_project_id_column() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read version");
    assert_eq!(version, 5);

    let drawer_columns = column_names(db.conn(), "drawers");
    assert!(
        drawer_columns.iter().any(|name| name == "project_id"),
        "drawers columns missing project_id: {drawer_columns:?}"
    );
    let triple_columns = column_names(db.conn(), "triples");
    assert!(
        triple_columns.iter().any(|name| name == "project_id"),
        "triples columns missing project_id: {triple_columns:?}"
    );

    let index_sql =
        sqlite_master_sql(db.conn(), "idx_drawers_project_id").expect("project index sql");
    assert!(
        index_sql.contains("project_id"),
        "project_id index missing expected SQL: {index_sql}"
    );
}

#[test]
fn test_ext_v5_migration_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let before_drawers = column_names(db.conn(), "drawers");
    let before_vectors = sqlite_master_sql(db.conn(), "drawer_vectors");

    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("reapply once");
    db_fork_ext::apply_fork_ext_migrations(db.conn()).expect("reapply twice");

    let version = db_fork_ext::read_fork_ext_version(db.conn()).expect("read version");
    assert_eq!(version, 5);
    assert_eq!(before_drawers, column_names(db.conn(), "drawers"));
    assert_eq!(
        before_vectors,
        sqlite_master_sql(db.conn(), "drawer_vectors")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_excludes_other_projects_by_default() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "state lives in proj A",
        "code",
        Some("room-a"),
        Some("proj-A"),
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "state lives in proj B",
        "docs",
        Some("room-b"),
        Some("proj-B"),
    );

    let json = search_response_json(&env.server(), "state").await;
    let results = json["results"].as_array().expect("results array");
    let ids = results
        .iter()
        .map(|value| {
            value["drawer_id"]
                .as_str()
                .expect("drawer_id string")
                .to_string()
        })
        .collect::<Vec<_>>();

    assert!(
        ids.iter().any(|id| id == "drawer-a"),
        "missing project-A hit: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| id != "drawer-b"),
        "project-B hit leaked into scoped search: {ids:?}"
    );
    let source = results
        .iter()
        .find(|value| value["drawer_id"] == "drawer-a")
        .and_then(|value| value["source"].as_str())
        .expect("project result source");
    assert_eq!(source, "project");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tunnel_resolver_bypasses_project_filter() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "anchor query text stays in project A",
        "code",
        Some("shared-room"),
        Some("proj-A"),
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "cross project docs drawer",
        "docs",
        Some("shared-room"),
        Some("proj-B"),
    );

    let json = search_response_json(&env.server(), "anchor").await;
    let results = json["results"].as_array().expect("results array");
    let ids = results
        .iter()
        .map(|value| {
            value["drawer_id"]
                .as_str()
                .expect("drawer_id string")
                .to_string()
        })
        .collect::<Vec<_>>();

    assert!(
        ids.iter().any(|id| id == "drawer-a"),
        "missing anchor result: {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id == "drawer-b"),
        "tunnel did not surface cross-project drawer: {ids:?}"
    );
    let tunnel_source = results
        .iter()
        .find(|value| value["drawer_id"] == "drawer-b")
        .and_then(|value| value["source"].as_str())
        .expect("tunnel result source");
    assert_eq!(tunnel_source, "tunnel_cross_project");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_strict_project_isolation_config_hot_reload() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "reload query from project A",
        "code",
        Some("reload"),
        Some("proj-A"),
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-global",
        "reload query from global drawer",
        "code",
        Some("reload"),
        None,
    );

    let before = search_response_json(&env.server(), "reload").await;
    let before_ids = before["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer_id").to_string())
        .collect::<Vec<_>>();
    assert!(
        before_ids.iter().any(|id| id == "drawer-a")
            && before_ids.iter().any(|id| id == "drawer-global"),
        "strict=false should see project + global drawers: {before_ids:?}"
    );

    let previous = ConfigHandle::version();
    write_config_atomic(&env.config_path, &search_config(&env.db_path, None, true));
    wait_for_config_version_change(&previous);

    let after = search_response_json(&env.server(), "reload").await;
    let after_ids = after["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer_id").to_string())
        .collect::<Vec<_>>();
    assert_eq!(after_ids, vec!["drawer-global".to_string()]);
}

#[cfg(feature = "rest")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rest_search_uses_configured_project_scope() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "state lives in proj A",
        "code",
        Some("room-a"),
        Some("proj-A"),
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "state lives in proj B",
        "docs",
        Some("room-b"),
        Some("proj-B"),
    );

    let request = Request::builder()
        .method("GET")
        .uri("/api/search?q=state")
        .body(Body::empty())
        .expect("build search request");
    let (status, json) = rest_json_response(&env, request).await;

    assert_eq!(status, StatusCode::OK);
    let results = json.as_array().expect("results array");
    let ids = results
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer id").to_string())
        .collect::<Vec<_>>();
    assert!(
        ids.iter().any(|id| id == "drawer-a"),
        "missing proj-A hit: {ids:?}"
    );
    assert!(
        ids.iter().all(|id| id != "drawer-b"),
        "proj-B hit leaked through REST scope: {ids:?}"
    );
}

#[cfg(feature = "rest")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_rest_ingest_uses_configured_project_scope() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);

    let request = Request::builder()
        .method("POST")
        .uri("/api/ingest")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "content": "rest scoped insert",
                "wing": "code",
                "room": "api",
            })
            .to_string(),
        ))
        .expect("build ingest request");
    let (status, json) = rest_json_response(&env, request).await;

    assert_eq!(status, StatusCode::CREATED);
    let drawer_id = json["drawer_id"]
        .as_str()
        .expect("drawer id in REST ingest response");
    let db = Database::open(&env.db_path).expect("open db");
    let stored_project = db
        .drawer_project_id(drawer_id)
        .expect("read drawer project id");
    assert_eq!(stored_project.as_deref(), Some("proj-A"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_allows_same_content_in_different_projects() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);
    let shared_content = "shared memory across repos";
    let original_id = build_drawer_id("code", Some("shared"), shared_content);
    insert_projected_drawer(
        &env.db_path,
        &original_id,
        shared_content,
        "code",
        Some("shared"),
        Some("proj-A"),
    );

    let response = env
        .server()
        .mempal_ingest(Parameters(IngestRequest {
            content: shared_content.to_string(),
            wing: "code".to_string(),
            room: Some("shared".to_string()),
            source: None,
            project_id: Some("proj-B".to_string()),
            dry_run: None,
            importance: None,
        }))
        .await
        .expect("cross-project ingest should succeed")
        .0;

    assert_ne!(
        response.drawer_id, original_id,
        "cross-project ingest must allocate a distinct drawer id"
    );

    let conn = Connection::open(&env.db_path).expect("open sqlite");
    let rows = conn
        .prepare(
            r#"
            SELECT project_id
            FROM drawers
            WHERE content = ?1 AND wing = 'code' AND room = 'shared' AND deleted_at IS NULL
            ORDER BY project_id
            "#,
        )
        .expect("prepare drawer query")
        .query_map([shared_content], |row| row.get::<_, Option<String>>(0))
        .expect("query drawers")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect drawers");
    assert_eq!(
        rows,
        vec![Some("proj-A".to_string()), Some("proj-B".to_string())]
    );

    let proj_b_results = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "shared memory".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: Some("proj-B".to_string()),
            include_global: None,
            all_projects: None,
        },
    )
    .await;
    let ids = proj_b_results["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer id").to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![response.drawer_id]);
}

#[test]
fn test_project_migrate_batched_does_not_block_ingest() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_config(&db_path),
    );
    Database::open(&db_path).expect("open db");

    for index in 0..5_000 {
        let id = format!("code-{index}");
        insert_projected_drawer(
            &db_path,
            &id,
            &format!("code memory drawer {index}"),
            "code-memory",
            Some("migration"),
            None,
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let latencies = Arc::new(Mutex::new(Vec::<Duration>::new()));
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    let db_for_writer = db_path.clone();
    let stop_for_writer = Arc::clone(&stop);
    let latencies_for_writer = Arc::clone(&latencies);
    let errors_for_writer = Arc::clone(&errors);
    let writer = thread::spawn(move || {
        let mut index = 0usize;
        while !stop_for_writer.load(Ordering::SeqCst) {
            let started = Instant::now();
            loop {
                let db = Database::open(&db_for_writer).expect("open writer db");
                let result = db.insert_drawer(&Drawer {
                    id: format!("writer-{index}"),
                    content: format!("writer content {index}"),
                    wing: "logs".to_string(),
                    room: Some("writer".to_string()),
                    source_file: Some(format!("writer-{index}.md")),
                    source_type: SourceType::Manual,
                    added_at: "1713000000".to_string(),
                    chunk_index: Some(0),
                    importance: 0,
                });
                match result {
                    Ok(()) => {
                        latencies_for_writer
                            .lock()
                            .expect("latencies mutex poisoned")
                            .push(started.elapsed());
                        break;
                    }
                    Err(error) if error.to_string().contains("database is locked") => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        errors_for_writer
                            .lock()
                            .expect("errors mutex poisoned")
                            .push(error.to_string());
                        return;
                    }
                }
            }
            index += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let started = Instant::now();
    let output = run_mempal(
        &home,
        &[
            "project",
            "migrate",
            "--project",
            "proj-A",
            "--wing",
            "code-memory",
        ],
    );
    let elapsed = started.elapsed();
    stop.store(true, Ordering::SeqCst);
    writer.join().expect("join writer");

    assert!(
        output.status.success(),
        "project migrate failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "migration took too long: {elapsed:?}"
    );
    assert!(
        errors.lock().expect("errors mutex poisoned").is_empty(),
        "writer saw errors: {:?}",
        errors.lock().expect("errors mutex poisoned")
    );
    let latencies = latencies.lock().expect("latencies mutex poisoned");
    assert!(!latencies.is_empty(), "writer never made progress");
    let mut millis = latencies
        .iter()
        .map(|duration| duration.as_millis() as u64)
        .collect::<Vec<_>>();
    millis.sort_unstable();
    let p99 = millis[((millis.len() - 1) * 99) / 100];
    assert!(p99 < 200, "writer latency p99 too high: {p99}ms");

    let conn = Connection::open(&db_path).expect("open sqlite");
    let updated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE wing = 'code-memory' AND project_id = 'proj-A'",
            [],
            |row| row.get(0),
        )
        .expect("count migrated drawers");
    assert_eq!(updated, 5_000);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let progress_lines = stdout
        .lines()
        .filter(|line| line.starts_with("batch "))
        .count();
    assert!(
        progress_lines >= 5,
        "expected at least 5 batch progress lines, got {progress_lines}:\n{stdout}"
    );
}

#[test]
fn test_project_migrate_begin_immediate_fails_fast() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_config(&db_path),
    );
    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "locked-drawer",
        "locked content",
        "code-memory",
        Some("migration"),
        None,
    );

    let (ready_tx, ready_rx) = mpsc::channel();
    let db_for_lock = db_path.clone();
    let lock_thread = thread::spawn(move || {
        let conn = Connection::open(&db_for_lock).expect("open lock connection");
        conn.execute_batch("BEGIN IMMEDIATE")
            .expect("begin immediate");
        conn.execute(
            "UPDATE drawers SET content = content WHERE id = 'locked-drawer'",
            [],
        )
        .expect("touch locked row");
        ready_tx.send(()).expect("signal ready");
        std::thread::sleep(Duration::from_secs(1));
        conn.execute_batch("COMMIT").expect("commit");
    });
    ready_rx.recv().expect("lock holder ready");

    let mut child = Command::new(mempal_bin())
        .args([
            "project",
            "migrate",
            "--project",
            "proj-A",
            "--wing",
            "code-memory",
        ])
        .env("HOME", &home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal project migrate");
    let stdout = child.stdout.take().expect("stdout pipe");
    let mut reader = BufReader::new(stdout);

    let started = Instant::now();
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).expect("read line");
        if read == 0 {
            break;
        }
        let trimmed = line.trim().to_string();
        lines.push(trimmed.clone());
        if trimmed.contains("batch busy") {
            break;
        }
    }

    let busy_elapsed = started.elapsed();
    let output = child.wait_with_output().expect("wait with output");
    lock_thread.join().expect("join lock thread");

    assert!(
        busy_elapsed < Duration::from_millis(100),
        "busy retry was not fail-fast: {busy_elapsed:?}"
    );
    assert!(
        output.status.success(),
        "project migrate failed:\nstdout={:?}\nstderr={}",
        lines,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        lines.iter().any(|line| line.contains("batch busy")),
        "missing busy retry output: {lines:?}"
    );
}

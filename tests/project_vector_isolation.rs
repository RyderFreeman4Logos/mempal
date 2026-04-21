mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

use async_trait::async_trait;
use common::harness::{McpStdio, dump as dump_vec0, start as start_embed_mock};
use mempal::core::config::ConfigHandle;
use mempal::core::db::{
    Database, apply_fork_ext_migrations_to, read_fork_ext_version, set_fork_ext_version,
};
use mempal::core::project::{
    MAX_PROJECT_ID_BYTES, ProjectError, escape_project_id_for_display, infer_project_id_from_path,
    infer_project_id_from_root_uri, migrate_null_project_ids, validate_project_id,
};
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::mcp::{MempalMcpServer, SearchRequest};
use mempal::search::filter::{build_fts_runtime_sql, build_vector_search_sql};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

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
    db_path: PathBuf,
}

impl SearchEnv {
    fn new(project_id: Option<&str>, strict_project_isolation: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let mempal_home = tmp.path().join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let config_path = mempal_home.join("config.toml");
        let db_path = mempal_home.join("palace.db");
        write_config_atomic(
            &config_path,
            &search_config(&db_path, project_id, strict_project_isolation),
        );
        Database::open(&db_path).expect("open db");
        ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
        Self { _tmp: tmp, db_path }
    }

    fn server(&self) -> MempalMcpServer {
        MempalMcpServer::new_with_factory(
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

fn cli_embed_config(db_path: &Path, base_url: &str) -> String {
    format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"
base_url = "{}"
api_model = "test-embed"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2

[search]
strict_project_isolation = false
"#,
        db_path.display(),
        base_url,
        base_url
    )
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).expect("write temp config");
    fs::rename(&tmp, path).expect("rename config");
}

fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql).expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns")
}

fn sqlite_master_sql(conn: &Connection, object: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type IN ('table', 'index') AND name = ?1",
        [object],
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
    vector: &[f32],
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
    db.insert_vector(id, vector).expect("insert vector");
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

async fn search_response_json_with_request(
    server: &MempalMcpServer,
    request: SearchRequest,
) -> Value {
    let response = server
        .mempal_search(Parameters(request))
        .await
        .expect("search should succeed")
        .0;
    serde_json::to_value(response).expect("serialize response")
}

fn install_cli_home(tmp: &TempDir) -> PathBuf {
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".mempal")).expect("create cli mempal home");
    home
}

fn run_mempal(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal")
}

fn run_mempal_in_dir(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .current_dir(cwd)
        .output()
        .expect("run mempal")
}

fn expected_project_id(path: &Path) -> String {
    expected_project_id_for_path(path).expect("expected project id from path")
}

fn expected_project_id_for_path(path: &Path) -> Option<String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn drawer_project_ids(conn: &Connection, table: &str) -> Vec<Option<String>> {
    let sql = format!("SELECT project_id FROM {table} ORDER BY id");
    let mut stmt = conn.prepare(&sql).expect("prepare project id query");
    stmt.query_map([], |row| row.get::<_, Option<String>>(0))
        .expect("query project ids")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect project ids")
}

fn downgrade_to_v4(conn: &Connection, drop_vector_table: bool) {
    conn.execute_batch("DROP INDEX IF EXISTS idx_drawers_project_id;")
        .expect("drop project index");
    if column_names(conn, "drawers")
        .iter()
        .any(|column| column == "project_id")
    {
        conn.execute_batch("ALTER TABLE drawers DROP COLUMN project_id;")
            .expect("drop drawers project_id");
    }
    if column_names(conn, "triples")
        .iter()
        .any(|column| column == "project_id")
    {
        conn.execute_batch("ALTER TABLE triples DROP COLUMN project_id;")
            .expect("drop triples project_id");
    }

    if drop_vector_table {
        conn.execute_batch("DROP TABLE IF EXISTS drawer_vectors;")
            .expect("drop drawer_vectors");
    } else if sqlite_master_sql(conn, "drawer_vectors")
        .as_deref()
        .is_some_and(|sql| sql.contains("project_id"))
    {
        let snapshot = dump_vec0(conn).expect("dump vec0");
        conn.execute_batch("DROP TABLE drawer_vectors;")
            .expect("drop vec0 with project_id");
        if !snapshot.is_empty() {
            let dim = snapshot[0].dim;
            conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE drawer_vectors USING vec0(id TEXT PRIMARY KEY, embedding FLOAT[{dim}]);"
            ))
            .expect("create legacy vec0");
            for row in snapshot {
                conn.execute(
                    "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, ?2)",
                    params![row.drawer_id, row.raw_blob],
                )
                .expect("restore legacy vec0 row");
            }
        }
    }

    set_fork_ext_version(conn, 4).expect("set fork_ext_version=4");
}

fn parse_search_ids(json: &Value) -> Vec<String> {
    json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer id").to_string())
        .collect()
}

fn parse_cli_search_ids(output: &Output) -> Vec<String> {
    assert!(
        output.status.success(),
        "command failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("parse CLI search JSON");
    json.as_array()
        .expect("search array")
        .iter()
        .map(|value| value["drawer_id"].as_str().expect("drawer id").to_string())
        .collect()
}

async fn call_mcp_search(client: &mut McpStdio, query: &str) -> Value {
    let result = match tokio::time::timeout(
        Duration::from_secs(5),
        client.call(
            "tools/call",
            json!({
                "name": "mempal_search",
                "arguments": {
                    "query": query,
                    "top_k": 10
                }
            }),
        ),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_search failed: {error}\nstderr:\n{stderr}");
        }
        Err(_) => {
            let stderr = client.stderr_lines().await.join("\n");
            panic!("call mempal_search timed out\nstderr:\n{stderr}");
        }
    };
    result["structuredContent"].clone()
}

#[test]
fn test_schema_fork_ext_v5_applies_project_id_column() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    insert_projected_drawer(
        db.path(),
        "legacy-drawer",
        "legacy content",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    downgrade_to_v4(db.conn(), false);

    assert_eq!(read_fork_ext_version(db.conn()).expect("read version"), 4);

    apply_fork_ext_migrations_to(db.conn(), 5).expect("apply ext v5");

    assert_eq!(read_fork_ext_version(db.conn()).expect("read version"), 5);
    assert!(
        column_names(db.conn(), "drawers")
            .iter()
            .any(|name| name == "project_id")
    );
    assert!(
        column_names(db.conn(), "triples")
            .iter()
            .any(|name| name == "project_id")
    );
    let vector_sql = sqlite_master_sql(db.conn(), "drawer_vectors").expect("drawer_vectors sql");
    assert!(
        vector_sql.contains("project_id"),
        "drawer_vectors missing project_id: {vector_sql}"
    );
    let drawer_table_sql = sqlite_master_sql(db.conn(), "drawers").expect("drawers sql");
    assert!(
        drawer_table_sql.contains("project_id TEXT"),
        "drawers project_id column missing: {drawer_table_sql}"
    );
    assert!(
        !drawer_table_sql.contains("DEFAULT 'default'"),
        "drawers project_id must stay nullable for historical rows: {drawer_table_sql}"
    );
    assert!(
        sqlite_master_sql(db.conn(), "idx_drawers_project_id").is_some(),
        "idx_drawers_project_id missing from sqlite_master"
    );
}

#[test]
fn test_migration_v4_to_v5_preserves_null_project_ids() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    insert_projected_drawer(
        db.path(),
        "legacy-a",
        "legacy content a",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        db.path(),
        "legacy-b",
        "legacy content b",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    downgrade_to_v4(db.conn(), false);

    apply_fork_ext_migrations_to(db.conn(), 5).expect("apply ext v5");

    assert_eq!(drawer_project_ids(db.conn(), "drawers"), vec![None, None]);
    assert_eq!(
        drawer_project_ids(db.conn(), "drawer_vectors"),
        vec![None, None]
    );
}

#[test]
fn test_project_migrate_backfills_null_project_ids_after_v5_migration() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "legacy-a",
        "legacy content a",
        "code-memory",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &db_path,
        "legacy-b",
        "legacy content b",
        "code-memory",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    downgrade_to_v4(db.conn(), false);
    apply_fork_ext_migrations_to(db.conn(), 5).expect("apply ext v5");

    migrate_null_project_ids(&db_path, "proj-migrated", Some("code-memory"), |_| {})
        .expect("project migrate after v5");

    assert_eq!(
        drawer_project_ids(db.conn(), "drawers"),
        vec![
            Some("proj-migrated".to_string()),
            Some("proj-migrated".to_string())
        ]
    );
    assert_eq!(
        drawer_project_ids(db.conn(), "drawer_vectors"),
        vec![
            Some("proj-migrated".to_string()),
            Some("proj-migrated".to_string())
        ]
    );
}

#[test]
fn test_status_reports_null_project_backfill_pending_after_v5_migration() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &search_config(&db_path, None, false),
    );

    let db = Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "legacy-status",
        "legacy status content",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    downgrade_to_v4(db.conn(), false);
    apply_fork_ext_migrations_to(db.conn(), 5).expect("apply ext v5");

    let output = run_mempal(&home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(
        stdout.contains("null_project_backfill_pending: true"),
        "missing pending flag in status output:\n{stdout}"
    );
    assert!(
        stdout.contains("null_project_backfill_count: 1"),
        "missing pending count in status output:\n{stdout}"
    );
}

#[test]
fn test_status_shows_project_breakdown() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &search_config(&db_path, None, false),
    );

    Database::open(&db_path).expect("open db");
    for index in 0..3 {
        insert_projected_drawer(
            &db_path,
            &format!("proj-a-{index}"),
            &format!("proj-A drawer {index}"),
            "code",
            Some("room"),
            Some("proj-A"),
            &[0.1, 0.2, 0.3],
        );
    }
    for index in 0..2 {
        insert_projected_drawer(
            &db_path,
            &format!("proj-b-{index}"),
            &format!("proj-B drawer {index}"),
            "code",
            Some("room"),
            Some("proj-B"),
            &[0.1, 0.2, 0.3],
        );
    }
    insert_projected_drawer(
        &db_path,
        "global-0",
        "global drawer",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );

    let output = run_mempal(&home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    let lines = stdout.lines().collect::<Vec<_>>();
    let heading = lines
        .iter()
        .position(|line| *line == "drawers per project:")
        .expect("project breakdown heading");
    assert_eq!(lines.get(heading + 1), Some(&"proj-A=3"));
    assert_eq!(lines.get(heading + 2), Some(&"proj-B=2"));
    assert_eq!(lines.get(heading + 3), Some(&"NULL=1"));
}

#[test]
fn test_status_escapes_project_id_with_control_chars() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &search_config(&db_path, None, false),
    );

    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "newline-drawer",
        "newline drawer",
        "code",
        Some("room"),
        Some("safe-newline"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &db_path,
        "ansi-drawer",
        "ansi drawer",
        "code",
        Some("room"),
        Some("safe-ansi"),
        &[0.1, 0.2, 0.3],
    );

    let newline_project_id = "foo\nWarnings:\n[WARN] fake";
    let ansi_project_id = "ansi\u{1b}[31mred";
    let conn = Connection::open(&db_path).expect("open raw db connection");
    conn.execute(
        "UPDATE drawers SET project_id = ?1 WHERE id = 'newline-drawer'",
        params![newline_project_id],
    )
    .expect("inject newline project id");
    conn.execute(
        "UPDATE drawers SET project_id = ?1 WHERE id = 'ansi-drawer'",
        params![ansi_project_id],
    )
    .expect("inject ansi project id");

    let output = run_mempal(&home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert!(
        lines
            .contains(&format!("{}=1", escape_project_id_for_display(newline_project_id)).as_str()),
        "escaped newline project id missing from status output:\n{stdout}"
    );
    assert!(
        lines.contains(&format!("{}=1", escape_project_id_for_display(ansi_project_id)).as_str()),
        "escaped ansi project id missing from status output:\n{stdout}"
    );
    assert!(
        !lines.contains(&"Warnings:"),
        "status output rendered forged line from raw newline:\n{stdout}"
    );
    assert!(
        !stdout.contains("\u{1b}[31m"),
        "status output rendered raw ANSI escape:\n{stdout}"
    );
}

#[test]
fn test_project_migrate_batched_does_not_block_ingest() {
    let _guard = home_guard();
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &search_config(&db_path, None, false),
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
            &[0.1, 0.2, 0.3],
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
        &search_config(&db_path, None, false),
    );
    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "locked-drawer",
        "locked content",
        "code-memory",
        Some("migration"),
        None,
        &[0.1, 0.2, 0.3],
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
        .expect("spawn project migrate");
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

#[test]
fn test_root_uri_project_id_decodes_percent_encoded_paths() {
    let tmp = TempDir::new().expect("tempdir");
    let path_with_space = tmp.path().join("My Project");
    let path_with_unicode = tmp.path().join("中文");
    let path_with_newline = tmp.path().join("line\nbreak");
    fs::create_dir_all(&path_with_space).expect("create spaced dir");
    fs::create_dir_all(&path_with_unicode).expect("create unicode dir");
    fs::create_dir_all(&path_with_newline).expect("create newline dir");

    let spaced_uri = format!("file://{}", path_with_space.display()).replace(' ', "%20");
    let unicode_uri = format!("file://{}", path_with_unicode.display());
    let newline_uri = format!("file://{}", path_with_newline.display()).replace('\n', "%0A");

    assert_eq!(
        infer_project_id_from_root_uri(&spaced_uri).expect("infer from spaced uri"),
        infer_project_id_from_path(&path_with_space).expect("infer from spaced path")
    );
    assert_eq!(
        infer_project_id_from_root_uri(&unicode_uri).expect("infer from unicode uri"),
        infer_project_id_from_path(&path_with_unicode).expect("infer from unicode path")
    );
    assert_eq!(
        infer_project_id_from_root_uri(&newline_uri).expect("infer from newline uri"),
        None
    );
}

#[test]
fn test_project_id_basename_matches_spec_example() {
    assert_eq!(
        infer_project_id_from_path(Path::new("/path/to/my-awesome-proj"))
            .expect("infer project id"),
        Some("my-awesome-proj".to_string())
    );
}

#[test]
fn test_project_id_inference_returns_none_for_root_path() {
    assert_eq!(
        infer_project_id_from_path(Path::new("/")).expect("infer project id"),
        None
    );
}

#[cfg(unix)]
#[test]
fn test_infer_project_id_rejects_invalid_utf8_basename() {
    let path = PathBuf::from(OsString::from_vec(b"/tmp/mempal-\xFF".to_vec()));
    assert_eq!(
        infer_project_id_from_path(&path).expect("invalid utf8 basename should not error"),
        None
    );
}

#[test]
fn test_infer_project_id_trims_whitespace_and_rejects_empty() {
    assert_eq!(
        infer_project_id_from_path(Path::new("/path/to/foo bar/"))
            .expect("infer project id with internal space"),
        Some("foo bar".to_string())
    );
    assert_eq!(
        infer_project_id_from_path(Path::new("/path/to/  /"))
            .expect("blank basename should be rejected"),
        None
    );
}

#[cfg(unix)]
#[test]
fn test_infer_project_id_rejects_basename_with_slash_or_null() {
    assert!(matches!(
        validate_project_id("foo/bar"),
        Err(ProjectError::Slash)
    ));
    assert!(matches!(
        validate_project_id("foo\0bar"),
        Err(ProjectError::ControlCharacter)
    ));

    let path = PathBuf::from(OsString::from_vec(b"/tmp/foo\0bar".to_vec()));
    assert_eq!(
        infer_project_id_from_path(&path).expect("nul basename should not error"),
        None
    );
}

#[test]
fn test_validate_project_id_rejects_newline() {
    assert!(matches!(
        validate_project_id("foo\nbar"),
        Err(ProjectError::ControlCharacter)
    ));
}

#[test]
fn test_validate_project_id_rejects_ansi_escape() {
    assert!(matches!(
        validate_project_id("foo\u{1b}[31mbar"),
        Err(ProjectError::ControlCharacter)
    ));
}

#[test]
fn test_validate_project_id_rejects_cr_tab_and_other_controls() {
    for candidate in [
        "foo\rbar".to_string(),
        "foo\tbar".to_string(),
        format!("foo{}bar", '\u{7f}'),
        format!("foo{}bar", '\u{85}'),
    ] {
        assert!(
            matches!(
                validate_project_id(&candidate),
                Err(ProjectError::ControlCharacter)
            ),
            "expected control-character rejection for {candidate:?}"
        );
    }
}

#[test]
fn test_validate_project_id_rejects_length_over_limit() {
    let within_limit = "a".repeat(MAX_PROJECT_ID_BYTES);
    assert_eq!(
        validate_project_id(&within_limit).expect("length limit should be inclusive"),
        within_limit
    );

    let over_limit = "a".repeat(MAX_PROJECT_ID_BYTES + 1);
    assert!(matches!(
        validate_project_id(&over_limit),
        Err(ProjectError::TooLong {
            max_bytes: MAX_PROJECT_ID_BYTES
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_excludes_other_project_vectors() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "state is scoped to project A",
        "code",
        Some("room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "state is scoped to project B",
        "code",
        Some("room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let json = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "state".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: Some("proj-A".to_string()),
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        },
    )
    .await;
    assert_eq!(parse_search_ids(&json), vec!["drawer-a".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_search_excludes_other_project_fts() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "fts query token",
        "code",
        Some("room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "fts query token",
        "code",
        Some("room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let db = Database::open(&env.db_path).expect("open db");
    let mut stmt = db
        .conn()
        .prepare(&build_fts_runtime_sql())
        .expect("prepare fts sql");
    let ids = stmt
        .query_map(
            params![
                "fts",
                Option::<String>::None,
                Option::<String>::None,
                "project",
                "proj-A",
                10_i64
            ],
            |row| row.get::<_, String>(0),
        )
        .expect("query fts")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect fts ids");

    assert_eq!(ids, vec!["drawer-a".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_vec_search_excludes_other_project() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "vector query token",
        "code",
        Some("room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "vector query token",
        "code",
        Some("room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let query_json = serde_json::to_string(&vec![0.1_f32, 0.2, 0.3]).expect("serialize query");
    let db = Database::open(&env.db_path).expect("open db");
    let mut stmt = db
        .conn()
        .prepare(&build_vector_search_sql())
        .expect("prepare vector sql");
    let ids = stmt
        .query_map(
            params![
                query_json,
                10_i64,
                "project",
                "proj-A",
                Option::<String>::None,
                Option::<String>::None,
                10_i64
            ],
            |row| row.get::<_, String>(0),
        )
        .expect("query vector sql")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect vector ids");

    assert_eq!(ids, vec!["drawer-a".to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_default_project_matches_current_dir() {
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    let project_dir = tmp.path().join("workspace").join("alpha");
    fs::create_dir_all(&project_dir).expect("create project dir");
    let expected = expected_project_id(&project_dir);

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_embed_config(&db_path, &format!("http://{addr}/v1")),
    );
    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "drawer-cwd",
        "cwd scoped query",
        "code",
        Some("room"),
        Some(&expected),
        &[0.1, 0.2, 0.3, 0.4],
    );
    insert_projected_drawer(
        &db_path,
        "drawer-other",
        "cwd scoped query",
        "code",
        Some("room"),
        Some("other-project"),
        &[0.1, 0.2, 0.3, 0.4],
    );

    let output = run_mempal_in_dir(&home, &project_dir, &["search", "cwd", "--json"]);
    assert_eq!(
        parse_cli_search_ids(&output),
        vec!["drawer-cwd".to_string()]
    );
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_explicit_project_override_wins_over_default() {
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    let project_dir = tmp.path().join("workspace").join("alpha");
    fs::create_dir_all(&project_dir).expect("create project dir");
    let expected = expected_project_id(&project_dir);

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_embed_config(&db_path, &format!("http://{addr}/v1")),
    );
    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "drawer-default",
        "override query",
        "code",
        Some("room"),
        Some(&expected),
        &[0.1, 0.2, 0.3, 0.4],
    );
    insert_projected_drawer(
        &db_path,
        "drawer-foo",
        "override query",
        "code",
        Some("room"),
        Some("foo"),
        &[0.1, 0.2, 0.3, 0.4],
    );

    let output = run_mempal_in_dir(
        &home,
        &project_dir,
        &["search", "override", "--project", "foo", "--json"],
    );
    assert_eq!(
        parse_cli_search_ids(&output),
        vec!["drawer-foo".to_string()]
    );
    handle.shutdown().await;
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
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "cross project docs drawer",
        "docs",
        Some("shared-room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let json = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "anchor".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        },
    )
    .await;
    let ids = parse_search_ids(&json);
    assert!(ids.iter().any(|id| id == "drawer-a"));
    assert!(ids.iter().any(|id| id == "drawer-b"));
}

#[test]
fn test_tunnel_hint_records_target_project_id() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    insert_projected_drawer(
        db.path(),
        "drawer-a",
        "anchor",
        "code",
        Some("shared-room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        db.path(),
        "drawer-b",
        "target",
        "docs",
        Some("shared-room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let tunnels = db
        .tunnel_drawers_for_room("shared-room", "drawer-a", Some("proj-A"))
        .expect("load tunnel drawers");
    assert_eq!(tunnels.len(), 1);
    assert_eq!(tunnels[0].target_project_id.as_deref(), Some("proj-B"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tunnel_resolved_drawer_marks_source_cross_project() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(Some("proj-A"), false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-a",
        "anchor query text stays in project A",
        "code",
        Some("shared-room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-b",
        "cross project docs drawer",
        "docs",
        Some("shared-room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let json = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "anchor".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        },
    )
    .await;

    let tunnel_source = json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .find(|value| value["drawer_id"] == "drawer-b")
        .and_then(|value| value["source"].as_str())
        .expect("tunnel source");
    assert_eq!(tunnel_source, "tunnel_cross_project");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_stores_project_id_from_cwd() {
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    let project_dir = tmp.path().join("workspace").join("beta");
    fs::create_dir_all(&project_dir).expect("create project dir");
    fs::write(project_dir.join("note.md"), "ingest stores cwd project id").expect("write note");
    let expected = expected_project_id(&project_dir);

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_embed_config(&db_path, &format!("http://{addr}/v1")),
    );

    let output = run_mempal_in_dir(
        &home,
        &project_dir,
        &[
            "ingest",
            project_dir.to_str().expect("project dir str"),
            "--wing",
            "code",
        ],
    );
    assert!(
        output.status.success(),
        "ingest failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let db = Database::open(&db_path).expect("open db");
    let stored = db
        .conn()
        .query_row(
            "SELECT project_id FROM drawers ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read stored project");
    assert_eq!(stored.as_deref(), Some(expected.as_str()));
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_ingest_project_id_override_flag() {
    let tmp = TempDir::new().expect("tempdir");
    let home = install_cli_home(&tmp);
    let db_path = home.join(".mempal").join("palace.db");
    let project_dir = tmp.path().join("workspace").join("gamma");
    fs::create_dir_all(&project_dir).expect("create project dir");
    fs::write(
        project_dir.join("note.md"),
        "ingest stores explicit project id",
    )
    .expect("write note");

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    write_config_atomic(
        &home.join(".mempal").join("config.toml"),
        &cli_embed_config(&db_path, &format!("http://{addr}/v1")),
    );

    let output = run_mempal_in_dir(
        &home,
        &project_dir,
        &[
            "ingest",
            project_dir.to_str().expect("project dir str"),
            "--wing",
            "code",
            "--project",
            "foo",
        ],
    );
    assert!(
        output.status.success(),
        "ingest failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let db = Database::open(&db_path).expect("open db");
    let stored = db
        .conn()
        .query_row(
            "SELECT project_id FROM drawers ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("read stored project");
    assert_eq!(stored.as_deref(), Some("foo"));
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_search_threads_project_id_from_mcp_root() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let project_dir = tmp.path().join("workspace").join("mcp-root");
    fs::create_dir_all(&project_dir).expect("create project dir");
    let project_id = expected_project_id(&project_dir);

    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "drawer-root",
        "mcp scoped query",
        "code",
        Some("room"),
        Some(&project_id),
        &[0.1, 0.2, 0.3, 0.4],
    );
    insert_projected_drawer(
        &db_path,
        "drawer-other",
        "mcp scoped query",
        "code",
        Some("room"),
        Some("other-project"),
        &[0.1, 0.2, 0.3, 0.4],
    );

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let mut client = McpStdio::start(
        &db_path,
        std::collections::HashMap::from([(
            "MEMPAL_TEST_EMBED_BASE_URL".to_string(),
            format!("http://{addr}/v1"),
        )]),
    )
    .await
    .expect("start mcp stdio");
    tokio::time::timeout(
        Duration::from_secs(5),
        client.initialize_with_roots(&[&format!("file://{}", project_dir.display())]),
    )
    .await
    .expect("initialize_with_roots timed out")
    .expect("initialize with roots");

    let structured = call_mcp_search(&mut client, "mcp").await;
    assert_eq!(
        parse_search_ids(&structured),
        vec!["drawer-root".to_string()]
    );

    client.shutdown().await.expect("shutdown client");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_roots_list_changed_invalidates_cached_project_id() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let project_a = tmp.path().join("workspace").join("project a");
    let project_b = tmp.path().join("workspace").join("project-b");
    fs::create_dir_all(&project_a).expect("create project a");
    fs::create_dir_all(&project_b).expect("create project b");
    let project_a_id = expected_project_id(&project_a);
    let project_b_id = expected_project_id(&project_b);

    Database::open(&db_path).expect("open db");
    insert_projected_drawer(
        &db_path,
        "drawer-project-a",
        "root change query",
        "code",
        Some("room"),
        Some(&project_a_id),
        &[0.1, 0.2, 0.3, 0.4],
    );
    insert_projected_drawer(
        &db_path,
        "drawer-project-b",
        "root change query",
        "code",
        Some("room"),
        Some(&project_b_id),
        &[0.1, 0.2, 0.3, 0.4],
    );

    let (addr, handle) = start_embed_mock(0).await.expect("start embed mock");
    let mut client = McpStdio::start(
        &db_path,
        std::collections::HashMap::from([(
            "MEMPAL_TEST_EMBED_BASE_URL".to_string(),
            format!("http://{addr}/v1"),
        )]),
    )
    .await
    .expect("start mcp stdio");
    let root_a_uri = format!("file://{}", project_a.display()).replace(' ', "%20");
    let root_b_uri = format!("file://{}", project_b.display());
    tokio::time::timeout(
        Duration::from_secs(5),
        client.initialize_with_roots(&[&root_a_uri]),
    )
    .await
    .expect("initialize_with_roots timed out")
    .expect("initialize with roots");

    let first = call_mcp_search(&mut client, "root change").await;
    assert_eq!(
        parse_search_ids(&first),
        vec!["drawer-project-a".to_string()]
    );

    client.set_roots(&[&root_b_uri]);
    client
        .notify_roots_list_changed()
        .await
        .expect("notify roots changed");

    let second = call_mcp_search(&mut client, "root change").await;
    assert_eq!(
        parse_search_ids(&second),
        vec!["drawer-project-b".to_string()]
    );

    client.shutdown().await.expect("shutdown client");
    handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_search_without_project_runs_unscoped_when_not_strict() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, false);
    insert_projected_drawer(
        &env.db_path,
        "drawer-null",
        "rootless project query",
        "code",
        Some("room"),
        None,
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-other",
        "rootless project query",
        "code",
        Some("room"),
        Some("other-project"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-default",
        "rootless project query",
        "code",
        Some("room"),
        Some("default"),
        &[0.1, 0.2, 0.3],
    );

    let structured = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "rootless".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        },
    )
    .await;
    let mut ids = parse_search_ids(&structured);
    ids.sort();
    assert_eq!(
        ids,
        vec![
            "drawer-default".to_string(),
            "drawer-null".to_string(),
            "drawer-other".to_string()
        ]
    );
    assert!(
        !structured
            .get("system_warnings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|warning| warning["message"] == "no project scope resolved, isolation strict"),
        "unexpected warnings: {structured}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_mcp_search_without_project_warns_and_returns_empty_when_strict() {
    let _guard = config_guard().await;
    let env = SearchEnv::new(None, true);
    insert_projected_drawer(
        &env.db_path,
        "drawer-proj-a",
        "strict rootless query",
        "code",
        Some("room"),
        Some("proj-A"),
        &[0.1, 0.2, 0.3],
    );
    insert_projected_drawer(
        &env.db_path,
        "drawer-proj-b",
        "strict rootless query",
        "code",
        Some("room"),
        Some("proj-B"),
        &[0.1, 0.2, 0.3],
    );

    let structured = search_response_json_with_request(
        &env.server(),
        SearchRequest {
            query: "strict".to_string(),
            wing: None,
            room: None,
            top_k: Some(10),
            project_id: None,
            include_global: None,
            all_projects: None,
            disable_progressive: None,
        },
    )
    .await;
    assert_eq!(parse_search_ids(&structured), Vec::<String>::new());
    let warning_messages = structured["system_warnings"]
        .as_array()
        .expect("system warnings array")
        .iter()
        .filter_map(|warning| warning["message"].as_str())
        .collect::<Vec<_>>();
    assert!(
        warning_messages.contains(&"no project scope resolved, isolation strict"),
        "missing strict isolation warning: {structured}"
    );
}

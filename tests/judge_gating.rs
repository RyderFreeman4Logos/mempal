mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use common::harness::AlwaysFailMigrationHook;
use mempal::core::config::{Config, ConfigHandle, GatingRuleConfig};
use mempal::core::db::{
    Database, apply_fork_ext_migrations_to, apply_fork_ext_migrations_with_hook,
    read_fork_ext_version, set_fork_ext_version,
};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::ingest::gating::{
    GatingRuntime, IngestCandidate, compile_classifier_from_config,
    compile_classifier_from_embedder, evaluate_tier1, evaluate_tier2,
};
use mempal::mcp::{IngestRequest, MempalMcpServer};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/*
test_embedder_error_fail_open
test_fork_ext_migration_v2_to_v3_creates_gating_audit
test_gating_audit_records_decisions
test_gating_disabled_short_circuits
test_gating_preserves_vector_dim_consistency
test_gating_stats_cli_output
test_llm_judge_section_warns_and_ignores
test_tier1_skips_read_tool
test_tier1_skips_short_content
test_tier2_keeps_above_threshold
test_tier2_skips_below_threshold
*/

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

async fn test_guard() -> OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn test_guard_blocking() -> OwnedMutexGuard<()> {
    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(test_guard())
}

#[derive(Clone)]
struct DeterministicEmbedderFactory {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
    fail_on: Arc<Vec<String>>,
}

struct DeterministicEmbedder {
    vectors: Arc<HashMap<String, Vec<f32>>>,
    default_vector: Vec<f32>,
    fail_on: Arc<Vec<String>>,
}

#[async_trait]
impl EmbedderFactory for DeterministicEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(DeterministicEmbedder {
            vectors: Arc::clone(&self.vectors),
            default_vector: self.default_vector.clone(),
            fail_on: Arc::clone(&self.fail_on),
        }))
    }
}

#[async_trait]
impl Embedder for DeterministicEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        if let Some(text) = texts
            .iter()
            .find(|text| self.fail_on.iter().any(|candidate| candidate == *text))
        {
            return Err(EmbedError::Runtime(format!("forced failure for {text}")));
        }

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
    home: PathBuf,
    db_path: PathBuf,
    config_path: PathBuf,
}

impl TestEnv {
    fn new(config_body: &str) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().to_path_buf();
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        let config_path = mempal_home.join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
db_path = "{}"

[config_hot_reload]
enabled = false

{}
"#,
                db_path.display(),
                config_body
            ),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            home,
            db_path,
            config_path,
        }
    }

    fn config(&self) -> Config {
        ConfigHandle::bootstrap(&self.config_path).expect("bootstrap config");
        Config::load_from(&self.config_path).expect("load config")
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }
}

#[derive(Debug)]
struct GatingAuditRow {
    drawer_id: Option<String>,
    decision: String,
    tier: u8,
    label: Option<String>,
    reason: Option<String>,
    score: Option<f32>,
}

struct GatingAuditSeed {
    candidate_hash: String,
    drawer_id: Option<String>,
    decision: &'static str,
    tier: u8,
    label: Option<String>,
    reason: Option<String>,
    score: Option<f32>,
    created_at: i64,
}

fn deterministic_factory(
    vectors: &[(&str, Vec<f32>)],
    default_vector: Vec<f32>,
    fail_on: &[&str],
) -> Arc<dyn EmbedderFactory> {
    Arc::new(DeterministicEmbedderFactory {
        vectors: Arc::new(
            vectors
                .iter()
                .map(|(text, vector)| ((*text).to_string(), vector.clone()))
                .collect(),
        ),
        default_vector,
        fail_on: Arc::new(fail_on.iter().map(|text| (*text).to_string()).collect()),
    })
}

fn run_mempal(home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .expect("run mempal")
}

async fn ingest_mcp(server: &MempalMcpServer, content: &str) -> mempal::mcp::IngestResponse {
    server
        .mempal_ingest(Parameters(IngestRequest {
            content: content.to_string(),
            wing: "code-memory".to_string(),
            room: Some("gating".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
        .expect("mcp ingest")
        .0
}

fn gating_rows(db: &Database) -> Vec<GatingAuditRow> {
    let mut stmt = db
        .conn()
        .prepare(
            r#"
            SELECT candidate_hash, drawer_id, decision, tier, label, reason, score
            FROM gating_audit
            ORDER BY created_at ASC, id ASC
            "#,
        )
        .expect("prepare gating rows");
    stmt.query_map([], |row| {
        Ok(GatingAuditRow {
            drawer_id: row.get::<_, Option<String>>(1)?,
            decision: row.get::<_, String>(2)?,
            tier: row.get::<_, i64>(3)?.clamp(0, i64::from(u8::MAX)) as u8,
            label: row.get::<_, Option<String>>(4)?,
            reason: row.get::<_, Option<String>>(5)?,
            score: row.get::<_, Option<f32>>(6)?,
        })
    })
    .expect("query gating rows")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect gating rows")
}

fn fork_ext_meta_value(db: &Database, key: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

fn gating_row_count(db: &Database) -> i64 {
    db.conn()
        .query_row("SELECT COUNT(*) FROM gating_audit", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count gating rows")
}

fn gating_decision_counts(db: &Database) -> HashMap<String, i64> {
    let mut stmt = db
        .conn()
        .prepare("SELECT decision, COUNT(*) FROM gating_audit GROUP BY decision")
        .expect("prepare gating decision counts");
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })
    .expect("query decision counts")
    .collect::<Result<HashMap<_, _>, _>>()
    .expect("collect decision counts")
}

fn insert_gating_audit_row(db: &Database, seed: GatingAuditSeed) {
    let explain_json = serde_json::json!({
        "decision": if seed.decision == "keep" { "accepted" } else { "rejected" },
        "tier": seed.tier,
        "label": seed.label,
        "gating_reason": seed.reason,
        "score": seed.score,
    })
    .to_string();
    let id = format!(
        "seed-{}-{}-{}-{}",
        seed.candidate_hash, seed.decision, seed.tier, seed.created_at
    );
    db.conn()
        .execute(
            r#"
            INSERT INTO gating_audit (
                id, candidate_hash, drawer_id, decision, tier, label, reason, score,
                explain_json, retained_until, created_at, project_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)
            "#,
            rusqlite::params![
                id,
                seed.candidate_hash,
                seed.drawer_id,
                seed.decision,
                i64::from(seed.tier),
                seed.label,
                seed.reason,
                seed.score,
                explain_json,
                seed.created_at + 604800,
                seed.created_at,
            ],
        )
        .expect("insert gating audit row");
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix time")
        .as_secs() as i64
}

fn gating_columns(db: &Database) -> Vec<String> {
    let mut stmt = db
        .conn()
        .prepare("PRAGMA table_info(gating_audit)")
        .expect("prepare table info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table info")
}

fn prepare_v2_database() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    db.conn()
        .execute_batch("DROP TABLE IF EXISTS gating_audit;")
        .expect("drop gating_audit");
    set_fork_ext_version(db.conn(), 2).expect("set fork_ext_version");
    assert_eq!(
        read_fork_ext_version(db.conn()).expect("read fork_ext_version"),
        2
    );
    (tmp, db)
}

fn recreate_legacy_v6_gating_audit_table(db: &Database) {
    db.conn()
        .execute_batch(
            r#"
            DROP TABLE IF EXISTS gating_audit;
            CREATE TABLE gating_audit (
                id TEXT PRIMARY KEY,
                candidate_hash TEXT NOT NULL,
                decision TEXT NOT NULL,
                explain_json TEXT NOT NULL,
                retained_until INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                project_id TEXT
            );
            CREATE INDEX idx_gating_audit_created_at
                ON gating_audit(created_at);
            CREATE INDEX idx_gating_audit_candidate_hash
                ON gating_audit(candidate_hash);
            "#,
        )
        .expect("recreate legacy gating_audit");
    set_fork_ext_version(db.conn(), 6).expect("set fork_ext_version");
}

fn insert_legacy_gating_audit_row(
    db: &Database,
    id: &str,
    candidate_hash: &str,
    decision: &str,
    explain_json: serde_json::Value,
    created_at: i64,
) {
    db.conn()
        .execute(
            r#"
            INSERT INTO gating_audit (
                id, candidate_hash, decision, explain_json, retained_until, created_at, project_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)
            "#,
            rusqlite::params![
                id,
                candidate_hash,
                decision,
                explain_json.to_string(),
                created_at + 604800,
                created_at,
            ],
        )
        .expect("insert legacy gating audit row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embedder_error_fail_open() {
    let _guard = test_guard().await;
    let config = Config::parse(
        r#"
[config_hot_reload]
enabled = false

[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.4
prototypes = ["valuable"]
"#,
    )
    .expect("parse config");
    let factory = deterministic_factory(
        &[("valuable", vec![1.0, 0.0])],
        vec![0.2, 0.2],
        &["candidate triggers embedder failure"],
    );
    let embedder = factory.build().await.expect("build embedder");
    let classifier = compile_classifier_from_embedder(embedder.as_ref(), &config.ingest_gating)
        .await
        .expect("compile classifier")
        .expect("classifier");
    let outcome = evaluate_tier2(
        &IngestCandidate {
            content: "candidate triggers embedder failure".to_string(),
            tool_name: None,
            exit_code: None,
        },
        &classifier,
        embedder.as_ref(),
        config.ingest_gating.embedding_classifier.threshold,
    )
    .await;

    assert_eq!(outcome.decision.decision, "accepted");
    assert_eq!(outcome.decision.tier, 0);
    assert_eq!(outcome.decision.label.as_deref(), Some("embedder_error"));
    assert!(outcome.vector.is_none());
}

#[test]
fn test_fork_ext_migration_v2_to_v3_creates_gating_audit() {
    let _guard = test_guard_blocking();
    let (_tmp, db) = prepare_v2_database();

    apply_fork_ext_migrations_to(db.conn(), 3).expect("apply migrations to v3");

    assert!(
        db.conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='gating_audit'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("load gating ddl")
            .contains("gating_audit"),
    );
    assert_eq!(
        read_fork_ext_version(db.conn()).expect("read fork_ext_version"),
        3
    );

    let columns = gating_columns(&db);
    for required in [
        "id",
        "candidate_hash",
        "drawer_id",
        "decision",
        "tier",
        "label",
        "reason",
        "created_at",
        "retained_until",
    ] {
        assert!(
            columns.iter().any(|column| column == required),
            "missing column {required}: {columns:?}"
        );
    }
}

#[test]
fn test_fork_ext_migration_v2_to_v3_rollback_via_migration_hook() {
    let _guard = test_guard_blocking();
    let (_tmp, db) = prepare_v2_database();

    let error = apply_fork_ext_migrations_with_hook(db.conn(), Some(&AlwaysFailMigrationHook))
        .expect_err("migration must roll back");
    assert!(error.to_string().contains("simulated crash"), "{error}");
    assert_eq!(
        read_fork_ext_version(db.conn()).expect("read fork_ext_version"),
        2
    );
    assert_eq!(
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gating_audit'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("query sqlite_master"),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_gating_audit_records_decisions() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.6
prototypes = ["valuable", "noise"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(
            &[
                ("valuable", vec![1.0, 0.0]),
                ("noise", vec![0.0, 1.0]),
                ("valuable candidate one", vec![0.95, 0.05]),
                ("valuable candidate two", vec![0.9, 0.1]),
                ("valuable candidate three", vec![0.85, 0.15]),
                ("valuable candidate four", vec![0.8, 0.2]),
                ("noise candidate one", vec![0.1, 0.9]),
                ("noise candidate two", vec![0.05, 0.95]),
            ],
            vec![0.2, 0.2],
            &[],
        ),
    );

    for content in [
        "valuable candidate one",
        "valuable candidate two",
        "valuable candidate three",
        "valuable candidate four",
        "noise candidate one",
        "noise candidate two",
        "tiny-01",
        "tiny-02",
        "tiny-03",
        "tiny-04",
    ] {
        let _ = ingest_mcp(&server, content).await;
    }

    let counts = gating_decision_counts(&env.db());
    assert_eq!(counts.get("keep"), Some(&4));
    assert_eq!(counts.get("skip"), Some(&6));
    assert_eq!(gating_row_count(&env.db()), 10);
    let drop_counts = env.db().gating_drop_counts().expect("gating drop counts");
    assert_eq!(drop_counts.total, Some(6));
    assert_eq!(drop_counts.by_reason.get("too_short"), Some(&4));
    assert_eq!(drop_counts.by_reason.get("prototype.noise"), Some(&2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_gating_disabled_short_circuits() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[gating]
enabled = false

[gating.embedding_classifier]
enabled = true
threshold = 0.4
prototypes = ["valuable"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(&[], vec![0.3, 0.3], &["valuable"]),
    );

    let response = ingest_mcp(&server, "content that bypasses gating entirely").await;

    assert!(!response.dropped);
    assert!(response.gating_decision.is_none());
    assert_eq!(env.db().drawer_count().expect("drawer count"), 1);
    assert_eq!(gating_row_count(&env.db()), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_gating_preserves_vector_dim_consistency() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.5
prototypes = ["valuable"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(
            &[
                ("valuable", vec![1.0, 0.0, 0.0]),
                ("vector dim stays consistent", vec![0.95, 0.05, 0.0]),
            ],
            vec![0.2, 0.2, 0.2],
            &[],
        ),
    );

    let response = ingest_mcp(&server, "vector dim stays consistent").await;
    let rows = gating_rows(&env.db());

    assert!(!response.dropped);
    assert_eq!(env.db().embedding_dim().expect("embedding dim"), Some(3));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].decision, "keep");
    assert_eq!(rows[0].tier, 2);
}

#[test]
fn test_gating_stats_cli_output() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();

    for index in 0..4 {
        insert_gating_audit_row(
            &db,
            GatingAuditSeed {
                candidate_hash: format!("tier1-keep-{index}"),
                drawer_id: Some(format!("tier1-keep-{index}")),
                decision: "keep",
                tier: 1,
                label: Some("rule_accept".to_string()),
                reason: None,
                score: Some(0.91),
                created_at: now,
            },
        );
    }
    for index in 0..6 {
        insert_gating_audit_row(
            &db,
            GatingAuditSeed {
                candidate_hash: format!("tier2-keep-{index}"),
                drawer_id: Some(format!("tier2-keep-{index}")),
                decision: "keep",
                tier: 2,
                label: Some("architectural-decision".to_string()),
                reason: None,
                score: Some(0.87),
                created_at: now,
            },
        );
    }
    for index in 0..12 {
        insert_gating_audit_row(
            &db,
            GatingAuditSeed {
                candidate_hash: format!("tier1-skip-{index}"),
                drawer_id: None,
                decision: "skip",
                tier: 1,
                label: None,
                reason: Some("too_short".to_string()),
                score: None,
                created_at: now,
            },
        );
    }
    for index in 0..8 {
        insert_gating_audit_row(
            &db,
            GatingAuditSeed {
                candidate_hash: format!("tier2-skip-{index}"),
                drawer_id: None,
                decision: "skip",
                tier: 2,
                label: None,
                reason: Some("prototype.noise".to_string()),
                score: Some(0.31),
                created_at: now,
            },
        );
    }
    insert_gating_audit_row(
        &db,
        GatingAuditSeed {
            candidate_hash: "old-window-row".to_string(),
            drawer_id: Some("old-window-row".to_string()),
            decision: "keep",
            tier: 2,
            label: Some("architectural-decision".to_string()),
            reason: None,
            score: Some(0.95),
            created_at: now - 9 * 24 * 60 * 60,
        },
    );

    let output = run_mempal(&env.home, &["gating", "stats", "--since", "7d"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("kept: 10"), "{stdout}");
    assert!(stdout.contains("skipped: 20"), "{stdout}");
    assert!(stdout.contains("tier1_kept: 4"), "{stdout}");
    assert!(stdout.contains("tier1_skipped: 12"), "{stdout}");
    assert!(stdout.contains("tier2_kept: 6"), "{stdout}");
    assert!(stdout.contains("tier2_skipped: 8"), "{stdout}");
    assert!(stdout.contains("architectural-decision=6"), "{stdout}");
    assert!(stdout.contains("prototype.noise=8"), "{stdout}");
    assert!(!stdout.contains("kept: 11"), "{stdout}");
}

#[test]
fn test_v7_migration_backfills_pre_v7_gating_audit_rows() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();
    recreate_legacy_v6_gating_audit_table(&db);
    insert_legacy_gating_audit_row(
        &db,
        "legacy-keep",
        "legacy-keep",
        "keep",
        serde_json::json!({
            "tier": 1,
            "label": "read_tool",
            "score": 0.0,
        }),
        now,
    );
    insert_legacy_gating_audit_row(
        &db,
        "legacy-skip",
        "legacy-skip",
        "skip",
        serde_json::json!({
            "tier": 1,
            "label": "read_tool",
            "reason": "skipped_low_signal",
            "score": 0.0,
        }),
        now,
    );

    apply_fork_ext_migrations_to(db.conn(), 7).expect("apply migrations to v7");

    let mut stmt = db
        .conn()
        .prepare(
            r#"
            SELECT id, drawer_id, tier, label, reason, score
            FROM gating_audit
            ORDER BY id ASC
            "#,
        )
        .expect("prepare migrated gating rows");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<f64>>(5)?,
            ))
        })
        .expect("query migrated gating rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect migrated gating rows");

    assert_eq!(
        rows,
        vec![
            (
                "legacy-keep".to_string(),
                Some("legacy-keep".to_string()),
                1,
                Some("read_tool".to_string()),
                None,
                Some(0.0),
            ),
            (
                "legacy-skip".to_string(),
                None,
                1,
                Some("read_tool".to_string()),
                Some("skipped_low_signal".to_string()),
                Some(0.0),
            ),
        ]
    );

    let output = run_mempal(&env.home, &["gating", "stats", "--since", "7d"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("kept: 1"), "{stdout}");
    assert!(stdout.contains("skipped: 1"), "{stdout}");
    assert!(stdout.contains("tier1_kept: 1"), "{stdout}");
    assert!(stdout.contains("tier1_skipped: 1"), "{stdout}");
    assert!(stdout.contains("unclassified: 0"), "{stdout}");
    assert!(stdout.contains("read_tool=1"), "{stdout}");
    assert!(stdout.contains("skipped_low_signal=1"), "{stdout}");
    assert!(!stdout.contains("unknown=1"), "{stdout}");
    assert!(!stdout.contains("unlabeled=1"), "{stdout}");
}

#[test]
fn test_v7_migration_backfills_legacy_accepted_rejected_decisions() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();
    recreate_legacy_v6_gating_audit_table(&db);
    insert_legacy_gating_audit_row(
        &db,
        "legacy-accepted",
        "abc123",
        "accepted",
        serde_json::json!({
            "tier": 1,
            "label": "read_tool",
            "reason": "rule_accept",
        }),
        now,
    );
    insert_legacy_gating_audit_row(
        &db,
        "legacy-rejected",
        "def456",
        "rejected",
        serde_json::json!({
            "tier": 1,
            "label": "short_content",
            "reason": "rule_reject",
        }),
        now,
    );

    apply_fork_ext_migrations_to(db.conn(), 7).expect("apply migrations to v7");

    let mut stmt = db
        .conn()
        .prepare(
            r#"
            SELECT id, decision, drawer_id, tier, label, reason
            FROM gating_audit
            ORDER BY id ASC
            "#,
        )
        .expect("prepare migrated legacy decisions");
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .expect("query migrated legacy decisions")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect migrated legacy decisions");

    assert_eq!(
        rows,
        vec![
            (
                "legacy-accepted".to_string(),
                "keep".to_string(),
                Some("abc123".to_string()),
                1,
                Some("read_tool".to_string()),
                Some("rule_accept".to_string()),
            ),
            (
                "legacy-rejected".to_string(),
                "skip".to_string(),
                None,
                1,
                Some("short_content".to_string()),
                Some("rule_reject".to_string()),
            ),
        ]
    );
}

#[test]
fn test_v7_migration_backfills_dropped_counters_from_legacy_audit() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();
    recreate_legacy_v6_gating_audit_table(&db);

    for index in 0..3 {
        insert_legacy_gating_audit_row(
            &db,
            &format!("rejected-short-{index}"),
            &format!("rejected-short-{index}"),
            "rejected",
            serde_json::json!({
                "tier": 1,
                "label": "short_content",
                "reason": "rule_reject",
            }),
            now,
        );
    }
    for index in 0..2 {
        insert_legacy_gating_audit_row(
            &db,
            &format!("rejected-read-{index}"),
            &format!("rejected-read-{index}"),
            "rejected",
            serde_json::json!({
                "tier": 1,
                "label": "read_tool",
                "reason": "rule_reject",
            }),
            now,
        );
    }
    for index in 0..5 {
        insert_legacy_gating_audit_row(
            &db,
            &format!("accepted-{index}"),
            &format!("accepted-{index}"),
            "accepted",
            serde_json::json!({
                "tier": 1,
                "label": "read_tool",
                "reason": "rule_accept",
            }),
            now,
        );
    }

    apply_fork_ext_migrations_to(db.conn(), 7).expect("apply migrations to v7");

    assert_eq!(
        fork_ext_meta_value(&db, "gating.dropped.total"),
        Some("5".to_string())
    );
    assert_eq!(
        fork_ext_meta_value(&db, "gating.dropped.by_reason.short_content"),
        Some("3".to_string())
    );
    assert_eq!(
        fork_ext_meta_value(&db, "gating.dropped.by_reason.read_tool"),
        Some("2".to_string())
    );

    let output = run_mempal(&env.home, &["status"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("  dropped_total: 5"), "{stdout}");
    assert!(stdout.contains("read_tool=2"), "{stdout}");
    assert!(stdout.contains("short_content=3"), "{stdout}");
}

#[test]
fn test_status_uses_dedicated_dropped_total_key() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();
    recreate_legacy_v6_gating_audit_table(&db);

    for index in 0..5 {
        insert_legacy_gating_audit_row(
            &db,
            &format!("rejected-unlabeled-{index}"),
            &format!("rejected-unlabeled-{index}"),
            "rejected",
            serde_json::json!({
                "tier": 1,
                "reason": "rule_reject",
            }),
            now,
        );
    }

    apply_fork_ext_migrations_to(db.conn(), 7).expect("apply migrations to v7");

    assert_eq!(
        fork_ext_meta_value(&db, "gating.dropped.total"),
        Some("5".to_string())
    );
    let drop_counts = db.gating_drop_counts().expect("gating drop counts");
    assert_eq!(drop_counts.total, Some(5));
    assert!(drop_counts.by_reason.is_empty(), "{drop_counts:?}");

    let output = run_mempal(&env.home, &["status"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("  dropped_total: 5"), "{stdout}");
    assert!(stdout.contains("  dropped_by_reason: none"), "{stdout}");
}

#[test]
fn test_load_gating_audit_falls_back_to_explain_json_for_default_row() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let now = now_unix_secs();
    let db = env.db();

    db.conn()
        .execute(
            r#"
            INSERT INTO gating_audit (
                id, candidate_hash, drawer_id, decision, tier, label, reason, score,
                explain_json, retained_until, created_at, project_id
            )
            VALUES (?1, ?2, NULL, 'keep', 0, NULL, NULL, NULL, ?3, ?4, ?5, NULL)
            "#,
            rusqlite::params![
                "defaulted-modern-row",
                "defaulted-modern-row",
                serde_json::json!({
                    "tier": 2,
                    "label": "architectural-decision",
                    "gating_reason": "prototype_below_threshold",
                    "score": 0.88,
                })
                .to_string(),
                now + 604800,
                now,
            ],
        )
        .expect("insert defaulted modern row");

    let output = run_mempal(&env.home, &["gating", "stats", "--since", "7d"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("kept: 1"), "{stdout}");
    assert!(stdout.contains("tier2_kept: 1"), "{stdout}");
    assert!(stdout.contains("unclassified: 0"), "{stdout}");
    assert!(stdout.contains("architectural-decision=1"), "{stdout}");
    assert!(!stdout.contains("unlabeled=1"), "{stdout}");
}

#[test]
fn test_llm_judge_section_warns_and_ignores() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new(
        r#"
[ingest_gating.llm_judge]
enabled = true
backend = "api"
"#,
    );

    let config = env.config();
    assert!(!config.ingest_gating.enabled);

    let output = run_mempal(&env.home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("llm_judge tier ignored: external LLM API disabled by design"),
        "{stderr}"
    );
}

#[test]
fn test_tier1_skips_read_tool() {
    let _guard = test_guard_blocking();
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let mut config = env.config();
    config.ingest_gating.enabled = true;
    config.ingest_gating.rules = vec![GatingRuleConfig {
        action: "reject".to_string(),
        tool: Some("Read".to_string()),
        tool_in: None,
        content_bytes_lt: None,
        content_bytes_gt: None,
        exit_code_eq: None,
    }];

    let decision = evaluate_tier1(
        &IngestCandidate {
            content: "long enough content to hit tool rule".to_string(),
            tool_name: Some("Read".to_string()),
            exit_code: None,
        },
        &config.ingest_gating,
    )
    .expect("tier1 decision");
    env.db()
        .record_gating_audit("read-tool-candidate", &decision, None)
        .expect("record gating audit");
    let rows = gating_rows(&env.db());

    assert_eq!(decision.tier, 1);
    assert_eq!(decision.gating_reason.as_deref(), Some("read_tool"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].decision, "skip");
    assert_eq!(rows[0].tier, 1);
    assert_eq!(rows[0].reason.as_deref(), Some("read_tool"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cli_ingest_tier1_only_does_not_init_gating_embedder() {
    let _guard = test_guard().await;
    let mut config = Config::parse(
        r#"
db_path = "/tmp/mempal-cli-tier1-only.db"

[gating]
enabled = true

[gating.embedding_classifier]
enabled = false
threshold = 0.4
prototypes = []

[embed]
backend = "unsupported-backend"
"#,
    )
    .expect("parse config");
    config.ingest_gating.rules = vec![GatingRuleConfig {
        action: "accept".to_string(),
        tool: None,
        tool_in: None,
        content_bytes_lt: None,
        content_bytes_gt: Some(1),
        exit_code_eq: None,
    }];

    let build_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&build_attempts);
    let classifier = if config.ingest_gating.enabled {
        if config.ingest_gating.embedding_classifier.enabled
            && !config
                .ingest_gating
                .embedding_classifier
                .prototypes
                .is_empty()
        {
            attempts.fetch_add(1, Ordering::SeqCst);
        }
        compile_classifier_from_config(&config)
            .await
            .map_err(|error| error.to_string())
            .expect("tier1-only CLI path should skip gating embedder init")
    } else {
        None
    };

    let decision = evaluate_tier1(
        &IngestCandidate {
            content: "content that should pass tier1 without tier2".to_string(),
            tool_name: None,
            exit_code: None,
        },
        &config.ingest_gating,
    )
    .expect("tier1 decision");

    assert!(classifier.is_none());
    assert_eq!(build_attempts.load(Ordering::SeqCst), 0);
    assert!(!decision.is_rejected());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tier1_skips_short_content() {
    let _guard = test_guard().await;
    let env = TestEnv::new("[gating]\nenabled = true\n");
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(&[], vec![0.2, 0.2], &[]),
    );

    let response = ingest_mcp(&server, "tiny").await;
    let rows = gating_rows(&env.db());

    assert!(response.dropped);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].decision, "skip");
    assert_eq!(rows[0].tier, 1);
    assert_eq!(rows[0].reason.as_deref(), Some("too_short"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tier2_keeps_above_threshold() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.8
prototypes = ["valuable", "noise"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(
            &[
                ("valuable", vec![1.0, 0.0]),
                ("noise", vec![0.0, 1.0]),
                ("above threshold candidate", vec![0.98, 0.02]),
            ],
            vec![0.2, 0.2],
            &[],
        ),
    );

    let response = ingest_mcp(&server, "above threshold candidate").await;
    let rows = gating_rows(&env.db());

    assert!(!response.dropped);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].decision, "keep");
    assert_eq!(rows[0].tier, 2);
    assert_eq!(rows[0].label.as_deref(), Some("valuable"));
    assert!(rows[0].score.expect("score") >= 0.8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_tier2_skips_below_threshold() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.9
prototypes = ["valuable", "noise"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(
            &[
                ("valuable", vec![1.0, 0.0]),
                ("noise", vec![0.0, 1.0]),
                ("below threshold candidate", vec![0.5, 0.5]),
            ],
            vec![0.2, 0.2],
            &[],
        ),
    );

    let response = ingest_mcp(&server, "below threshold candidate").await;
    let rows = gating_rows(&env.db());

    assert!(response.dropped);
    assert_eq!(env.db().drawer_count().expect("drawer count"), 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].decision, "skip");
    assert_eq!(rows[0].tier, 2);
    assert_eq!(rows[0].reason.as_deref(), Some("prototype_below_threshold"));
    assert!(rows[0].drawer_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_prototype_count_limit_enforced() {
    let _guard = test_guard().await;
    let mut prototypes = Vec::new();
    for index in 0..65 {
        prototypes.push(format!("prototype-{index}"));
    }
    let config = Config::parse(&format!(
        r#"
db_path = "/tmp/mempal-gating-runtime.db"

[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.4
prototypes = [{}]
"#,
        prototypes
            .iter()
            .map(|label| format!(r#""{label}""#))
            .collect::<Vec<_>>()
            .join(", ")
    ))
    .expect("parse config");
    let runtime = GatingRuntime::new(config, deterministic_factory(&[], vec![0.1, 0.1], &[]));

    let error = runtime
        .initialize()
        .await
        .expect_err("prototype count limit must trip");
    assert!(error.to_string().contains("prototype_count=65"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_prototype_init_error_redacts_text() {
    let _guard = test_guard().await;
    let sensitive = "sensitive prototype text with spaces";
    let config = Config::parse(&format!(
        r#"
db_path = "/tmp/mempal-gating-runtime.db"

[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.4
prototypes = ["{sensitive}"]
"#
    ))
    .expect("parse config");
    let runtime = GatingRuntime::new(
        config,
        deterministic_factory(&[], vec![0.1, 0.1], &[sensitive]),
    );

    let error = runtime
        .initialize()
        .await
        .expect_err("prototype init must fail");
    let rendered = error.to_string();
    assert!(rendered.contains("prototype#1"), "{rendered}");
    assert!(!rendered.contains(sensitive), "{rendered}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_warns_when_hooks_on_and_gating_off() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[privacy]
enabled = true

[hooks]
enabled = true

[gating]
enabled = false
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(&[], vec![0.2, 0.2], &[]),
    );

    let mcp_status = server.mempal_status().await.expect("mcp status").0;
    let output = run_mempal(&env.home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let warning_messages = mcp_status
        .system_warnings
        .iter()
        .map(|warning| warning.message.clone())
        .collect::<Vec<_>>();

    assert!(
        stdout.contains("hooks capture is enabled while local gating is disabled"),
        "{stdout}"
    );
    assert!(warning_messages.iter().any(|message| {
        message.contains("hooks capture is enabled while local gating is disabled")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_status_warns_when_gating_fail_open() {
    let _guard = test_guard().await;
    let env = TestEnv::new(
        r#"
[privacy]
enabled = true

[hooks]
enabled = true

[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.4
prototypes = ["valuable"]
"#,
    );
    let config = env.config();
    let server = MempalMcpServer::new_with_factory_and_config(
        env.db_path.clone(),
        config,
        deterministic_factory(&[("valuable", vec![1.0, 0.0])], vec![0.2, 0.2], &[]),
    );

    let mcp_status = server.mempal_status().await.expect("mcp status").0;
    let output = run_mempal(&env.home, &["status"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let warning_messages = mcp_status
        .system_warnings
        .iter()
        .map(|warning| warning.message.clone())
        .collect::<Vec<_>>();

    assert!(
        stdout.contains("tier-2 gating is fail-open on embedder errors"),
        "{stdout}"
    );
    assert!(
        warning_messages
            .iter()
            .any(|message| message.contains("tier-2 gating is fail-open on embedder errors"))
    );
}

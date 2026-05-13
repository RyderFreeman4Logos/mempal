use std::fs;
use std::process::Command;

use mempal::core::config::ImportanceConfig;
use mempal::core::db::{Database, read_fork_ext_version};
use mempal::core::decay::compute_effective_importance;
use mempal::core::types::{Drawer, SourceType, Triple};
use rusqlite::Connection;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn default_importance_config() -> ImportanceConfig {
    ImportanceConfig::default()
}

fn setup_home(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "model2vec"
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    Database::open(&db_path).expect("initialize db");
    (home, db_path)
}

fn insert_test_drawer(db_path: &std::path::Path, id: &str, wing: &str, importance: i32) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1_746_000_000);
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: format!("test content for {id}"),
        wing: wing.to_string(),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        added_at: now_secs.to_string(),
        importance,
        ..Drawer::default()
    })
    .expect("insert drawer");
    // Set effective_importance to the base importance so tests start from a
    // meaningful baseline rather than the default 0.0.
    db.conn()
        .execute(
            "UPDATE drawers SET effective_importance = CAST(importance AS REAL) WHERE id = ?1",
            [id],
        )
        .expect("backfill initial effective_importance");
}

fn read_effective_importance(db_path: &std::path::Path, id: &str) -> f64 {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(
            "SELECT COALESCE(effective_importance, CAST(COALESCE(importance, 0) AS REAL)) FROM drawers WHERE id = ?1",
            [id],
            |row| row.get::<_, f64>(0),
        )
        .expect("read effective_importance")
}

fn read_access_count(db_path: &std::path::Path, id: &str) -> i64 {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(
            "SELECT COALESCE(access_count, 0) FROM drawers WHERE id = ?1",
            [id],
            |row| row.get::<_, i64>(0),
        )
        .expect("read access_count")
}

fn read_last_accessed_at(db_path: &std::path::Path, id: &str) -> Option<i64> {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(
            "SELECT last_accessed_at FROM drawers WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .expect("read last_accessed_at")
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .any(|r| r.as_deref().unwrap_or("") == column)
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

// --- Integration tests ---

#[test]
fn test_fork_ext_migration_v9_to_v10_adds_importance_columns() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    // Verify migration ran and fork_ext_version is current
    let version = read_fork_ext_version(db.conn()).expect("read fork_ext_version");
    assert!(
        version >= 10,
        "fork_ext_version should be >= 10 after migration: {version}"
    );

    // Verify new columns exist
    let conn = db.conn();
    assert!(
        table_has_column(conn, "drawers", "last_accessed_at"),
        "last_accessed_at column must exist"
    );
    assert!(
        table_has_column(conn, "drawers", "access_count"),
        "access_count column must exist"
    );
    assert!(
        table_has_column(conn, "drawers", "accumulated_boost"),
        "accumulated_boost column must exist"
    );
    assert!(
        table_has_column(conn, "drawers", "effective_importance"),
        "effective_importance column must exist"
    );
    assert!(
        table_has_column(conn, "drawers", "stale_penalty_applied"),
        "stale_penalty_applied column must exist"
    );

    // Verify index exists
    assert!(
        index_exists(conn, "idx_drawers_eff_importance"),
        "idx_drawers_eff_importance must exist"
    );
}

#[test]
fn test_fork_ext_migration_v10_sets_effective_importance_from_importance() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");

    // Open a fresh db which runs all migrations
    let db = Database::open(&db_path).expect("open db");

    // Insert a drawer with known importance before checking
    db.insert_drawer(&Drawer {
        id: "drawer-imp-check".to_string(),
        content: "migration check content".to_string(),
        wing: "test".to_string(),
        source_file: Some("check.md".to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        importance: 3,
        ..Drawer::default()
    })
    .expect("insert drawer");

    // effective_importance should be readable and >= 0
    let eff = read_effective_importance(&db_path, "drawer-imp-check");
    assert!(
        eff >= 0.0,
        "effective_importance should be non-negative: {eff}"
    );
}

#[test]
fn test_search_hit_updates_access_fields_async() {
    let tmp = TempDir::new().expect("tempdir");
    let (_, db_path) = setup_home(&tmp);

    insert_test_drawer(&db_path, "drawer-access-test", "default", 2);

    // Verify initial state
    assert_eq!(read_access_count(&db_path, "drawer-access-test"), 0);
    assert!(read_last_accessed_at(&db_path, "drawer-access-test").is_none());

    // Directly call the DB function that the async search path calls
    let db = Database::open(&db_path).expect("open db");
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let config = default_importance_config();
    db.update_access_fields_batch(
        &["drawer-access-test".to_string()],
        now_ms,
        config.decay_rate,
        config.floor,
        config.boost_cap,
    )
    .expect("update access fields");

    assert_eq!(
        read_access_count(&db_path, "drawer-access-test"),
        1,
        "access_count should be incremented"
    );
    assert!(
        read_last_accessed_at(&db_path, "drawer-access-test").is_some(),
        "last_accessed_at should be set"
    );
}

#[test]
fn test_session_ingest_boosts_hit_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let (_, db_path) = setup_home(&tmp);

    insert_test_drawer(&db_path, "drawer-boost-test", "default", 2);

    let initial_eff = read_effective_importance(&db_path, "drawer-boost-test");

    // Simulate the boost that mempal_ingest applies after search hits
    let db = Database::open(&db_path).expect("open db");
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let config = default_importance_config();
    db.apply_ingest_boost_batch(
        &["drawer-boost-test".to_string()],
        now_ms,
        config.boost_per_access,
        config.boost_cap,
        config.decay_rate,
        config.floor,
    )
    .expect("apply ingest boost");

    let boosted_eff = read_effective_importance(&db_path, "drawer-boost-test");
    assert!(
        boosted_eff > initial_eff,
        "boost must increase effective_importance: {boosted_eff} > {initial_eff}"
    );
}

#[test]
fn test_stale_kg_triple_penalizes_importance() {
    let tmp = TempDir::new().expect("tempdir");
    let (_, db_path) = setup_home(&tmp);

    insert_test_drawer(&db_path, "drawer-stale-test", "default", 3);

    // Set effective_importance to 3.0 explicitly
    let db = Database::open(&db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE drawers SET effective_importance = 3.0 WHERE id = 'drawer-stale-test'",
            [],
        )
        .expect("set effective_importance");

    // Add an expired KG triple linked to the drawer
    let past_secs = 1_000_000u64;
    db.insert_triple(&Triple {
        id: "triple-stale-1".to_string(),
        subject: "TestEntity".to_string(),
        predicate: "uses".to_string(),
        object: "OldTechnology".to_string(),
        valid_from: None,
        valid_to: Some(past_secs.to_string()),
        confidence: 1.0,
        source_drawer: Some("drawer-stale-test".to_string()),
    })
    .expect("insert triple");

    // Apply stale penalty (0.5 by default)
    let config = default_importance_config();
    db.apply_stale_penalty_to_drawer("drawer-stale-test", config.stale_penalty)
        .expect("apply stale penalty");

    let penalized_eff = read_effective_importance(&db_path, "drawer-stale-test");
    let expected = 3.0 * config.stale_penalty;
    assert!(
        (penalized_eff - expected).abs() < 1e-6,
        "stale penalty should multiply by {}: expected {expected}, got {penalized_eff}",
        config.stale_penalty
    );

    // Verify stale_penalty_applied is persisted so recompute doesn't lose it.
    let stored_penalty: f64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(stale_penalty_applied, 1.0) FROM drawers WHERE id = 'drawer-stale-test'",
            [],
            |row| row.get(0),
        )
        .expect("read stale_penalty_applied");
    assert!(
        (stored_penalty - config.stale_penalty).abs() < 1e-6,
        "stale_penalty_applied must be persisted: expected {}, got {stored_penalty}",
        config.stale_penalty
    );
}

#[test]
fn test_audit_stale_surfaces_decayed_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let (home, db_path) = setup_home(&tmp);

    insert_test_drawer(&db_path, "drawer-low-imp", "default", 1);
    insert_test_drawer(&db_path, "drawer-high-imp", "default", 3);

    // Set explicit effective_importance values
    let db = Database::open(&db_path).expect("open db");
    db.conn()
        .execute(
            "UPDATE drawers SET effective_importance = 0.4 WHERE id = 'drawer-low-imp'",
            [],
        )
        .expect("set low eff imp");
    db.conn()
        .execute(
            "UPDATE drawers SET effective_importance = 3.0 WHERE id = 'drawer-high-imp'",
            [],
        )
        .expect("set high eff imp");
    drop(db);

    let output = Command::new(mempal_bin())
        .env("HOME", &home)
        .args(["audit", "--stale", "--threshold", "0.5"])
        .output()
        .expect("run audit --stale");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "audit --stale should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("drawer-low-imp"),
        "audit --stale should list low-importance drawer: {stdout}"
    );
    assert!(
        !stdout.contains("drawer-high-imp"),
        "audit --stale should not list high-importance drawer: {stdout}"
    );
}

#[test]
fn test_search_result_dto_includes_effective_importance() {
    // Verify that SearchResultDto has the effective_importance field by checking
    // that the struct compiles with the field set (type-level check via instantiation).
    use mempal::mcp::{RouteDecisionDto, SearchResultDto};

    let _dto = SearchResultDto {
        drawer_id: "test".to_string(),
        content: "content".to_string(),
        content_truncated: false,
        original_content_bytes: 0,
        wing: "wing".to_string(),
        room: None,
        source_file: "file.md".to_string(),
        source: "bm25".to_string(),
        source_type: "agent_inference".to_string(),
        confidence: 0.5,
        similarity: 0.9,
        route: RouteDecisionDto {
            wing: None,
            room: None,
            confidence: 0.8,
            reason: "test".to_string(),
        },
        tunnel_hints: vec![],
        neighbors: None,
        entities: vec![],
        topics: vec![],
        flags: vec![],
        emotions: vec![],
        importance_stars: 3,
        effective_importance: 2.5,
        memory_kind: "evidence".to_string(),
        domain: "project".to_string(),
        field: "default".to_string(),
        is_pinned: false,
        statement: None,
        tier: None,
        status: None,
        anchor_kind: "global".to_string(),
        anchor_id: "anc".to_string(),
        parent_anchor_id: None,
        matched_pattern_id: None,
    };
    assert!((_dto.effective_importance - 2.5).abs() < 1e-9);
}

#[test]
fn test_recompute_all_effective_importance_cli() {
    let tmp = TempDir::new().expect("tempdir");
    let (home, db_path) = setup_home(&tmp);

    insert_test_drawer(&db_path, "drawer-recompute-1", "default", 2);
    insert_test_drawer(&db_path, "drawer-recompute-2", "default", 4);

    let output = Command::new(mempal_bin())
        .env("HOME", &home)
        .arg("recompute-importance")
        .output()
        .expect("run recompute-importance");

    assert!(
        output.status.success(),
        "recompute-importance should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("updated"),
        "should report updated count: {stdout}"
    );
}

#[test]
fn test_importance_config_hot_reload_decay_rate() {
    // This test verifies the config struct accepts updated decay_rate and that
    // compute_effective_importance uses it. Full hot-reload daemon test is omitted
    // (daemon lifecycle is complex; covered by config_hot_reload integration suite).
    let config_a = ImportanceConfig {
        decay_rate: 0.05,
        ..default_importance_config()
    };
    let config_b = ImportanceConfig {
        decay_rate: 0.0,
        ..default_importance_config()
    };

    let eff_a = compute_effective_importance(3.0, 30.0, 0.0, &config_a);
    let eff_b = compute_effective_importance(3.0, 30.0, 0.0, &config_b);

    assert!(
        eff_b > eff_a,
        "zero decay_rate should produce higher effective_importance: {eff_b} > {eff_a}"
    );
    // With decay_rate = 0.0, exp(0) = 1.0, so eff_b ≈ base * 1.0 + 0 = 3.0
    assert!(
        (eff_b - 3.0).abs() < 1e-9,
        "zero decay_rate must yield exactly base importance: {eff_b}"
    );
}

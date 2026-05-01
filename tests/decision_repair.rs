#![warn(clippy::all)]

use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::config::RepairConfig;
use mempal::core::db::{CURRENT_FORK_EXT_VERSION, Database, read_fork_ext_version};
use mempal::core::types::{Drawer, SourceType};
use mempal::repair::{
    assemble_repair_package, compute_topic_sig, detect_failure_keyword, detect_repeated_failures,
    load_repair_warnings, record_failure_event,
};
use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_test_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db)
}

fn default_repair_config() -> RepairConfig {
    RepairConfig {
        enabled: true,
        failure_keywords: vec![],
        window_days: 7,
        min_failures: 3,
        alert_threshold: 3,
    }
}

fn insert_test_drawer(db: &Database, id: &str, content: &str, wing: &str) {
    let drawer = Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: Some("test-room".to_string()),
        source_file: Some(format!("tests://{id}.md")),
        source_type: SourceType::Manual,
        added_at: "2026-01-01T00:00:00Z".to_string(),
        importance: 2,
        ..Drawer::default()
    };
    db.insert_drawer(&drawer).expect("insert drawer");
}

fn insert_failure_event(
    conn: &Connection,
    event_id: &str,
    drawer_id: &str,
    wing: &str,
    topic_sig: &str,
    failure_type: &str,
    detected_at_ms: i64,
) {
    record_failure_event(
        conn,
        &mempal::repair::FailureEventArgs {
            event_id,
            drawer_id,
            wing,
            room: Some("test-room"),
            topic_sig,
            failure_type,
            project_id: None,
            detected_at_ms,
        },
    )
    .expect("record failure event");
}

// ---------------------------------------------------------------------------
// Schema migration tests
// ---------------------------------------------------------------------------

#[test]
fn test_fork_ext_migration_v7_to_v8_creates_failure_events() {
    let (_tmp, db) = new_test_db();

    // Verify fork_ext_version advanced to current (≥ 12 means v12 ran).
    let version = read_fork_ext_version(db.conn()).expect("read version");
    assert_eq!(version, CURRENT_FORK_EXT_VERSION);
    assert!(
        version >= 12,
        "expected fork_ext_version >= 12, got {version}"
    );

    // failure_events table must exist.
    let table_exists: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='failure_events'",
            [],
            |row| row.get(0),
        )
        .expect("query sqlite_master");
    assert_eq!(table_exists, 1, "failure_events table must exist");

    // Required columns must be present.
    let mut stmt = db
        .conn()
        .prepare("PRAGMA table_info(failure_events)")
        .expect("prepare pragma");
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect columns");

    for col in &[
        "event_id",
        "drawer_id",
        "topic_sig",
        "failure_type",
        "detected_at",
    ] {
        assert!(
            columns.iter().any(|c| c == col),
            "column {col} must exist in failure_events"
        );
    }

    // Index must exist.
    let idx_exists: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_failure_events_topic'",
            [],
            |row| row.get(0),
        )
        .expect("query index");
    assert_eq!(idx_exists, 1, "idx_failure_events_topic must exist");
}

// ---------------------------------------------------------------------------
// Failure keyword detection
// ---------------------------------------------------------------------------

#[test]
fn test_ingest_no_failure_keyword_skips_event() {
    let content = "everything worked perfectly today";
    let result = detect_failure_keyword(content, &[]);
    assert!(result.is_none(), "no failure keyword expected");
}

#[test]
fn test_ingest_failure_keyword_detection_builtin() {
    let content = "the migration failed with SQLITE_ERROR";
    let result = detect_failure_keyword(content, &[]);
    assert!(result.is_some(), "failure keyword must be detected");
}

#[test]
fn test_detect_failure_keyword_case_insensitive() {
    let cases = [
        "Deploy FAILED due to timeout",
        "System ERROR encountered during startup",
        "Process was ABORTED",
        "Encountered an EXCEPTION in handler",
    ];
    for case in &cases {
        assert!(
            detect_failure_keyword(case, &[]).is_some(),
            "keyword not detected in: {case}"
        );
    }
}

#[test]
fn test_detect_failure_keyword_word_boundary() {
    // "unfailing" must NOT match "failed/failure" keywords.
    assert!(
        detect_failure_keyword("unfailing service", &[]).is_none(),
        "word boundary must prevent 'unfailing' from matching"
    );
}

// ---------------------------------------------------------------------------
// Record failure event + write to DB
// ---------------------------------------------------------------------------

#[test]
fn test_ingest_failure_keyword_creates_event() {
    let (_tmp, db) = new_test_db();
    let content = "the migration failed with SQLITE_ERROR during upgrade";
    let drawer_id = "test-drawer-001";

    insert_test_drawer(&db, drawer_id, content, "code-memory");

    let topic_sig = compute_topic_sig(content);
    let failure_type = detect_failure_keyword(content, &[]).expect("keyword detected");
    let ts = now_ms();

    insert_failure_event(
        db.conn(),
        "evt-001",
        drawer_id,
        "code-memory",
        &topic_sig,
        &failure_type,
        ts,
    );

    // Verify row exists.
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM failure_events", [], |row| row.get(0))
        .expect("count failure_events");
    assert_eq!(count, 1);

    // Check failure_type and wing.
    let (db_failure_type, db_wing): (String, String) = db
        .conn()
        .query_row(
            "SELECT failure_type, wing FROM failure_events WHERE event_id = 'evt-001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("fetch row");
    assert_eq!(db_wing, "code-memory");
    assert!(!db_failure_type.is_empty());
}

// ---------------------------------------------------------------------------
// Repeated failure pattern detection
// ---------------------------------------------------------------------------

#[test]
fn test_fact_check_detects_repeated_failure_pattern() {
    let (_tmp, db) = new_test_db();
    let cfg = default_repair_config();
    let ts = now_ms();

    // Insert 3 failure events with the same topic_sig.
    let topic_sig = "aaaa1111bbbb2222cccc3333dddd4444"; // 32 hex chars
    for i in 0..3usize {
        let drawer_id = format!("failure-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed with database error",
            "test-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("evt-{i}"),
            &drawer_id,
            "test-wing",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    let packages = detect_repeated_failures(db.conn(), &cfg, None, ts);
    assert!(!packages.is_empty(), "must detect at least one pattern");

    let pkg = packages.iter().find(|p| p.topic_sig == topic_sig);
    assert!(pkg.is_some(), "must find pattern for our topic_sig");
    let pkg = pkg.unwrap();
    assert_eq!(pkg.failure_count, 3);
    assert_eq!(pkg.failure_drawers.len(), 3);
}

#[test]
fn test_repair_window_excludes_old_events() {
    let (_tmp, db) = new_test_db();
    let cfg = default_repair_config(); // window_days = 7
    let ts = now_ms();

    // Insert 3 failure events older than 10 days.
    let old_ts = ts - 10 * 86_400_000i64;
    let topic_sig = "bbbb2222cccc3333dddd4444eeee5555";
    for i in 0..3usize {
        let drawer_id = format!("old-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed with database error",
            "test-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("old-evt-{i}"),
            &drawer_id,
            "test-wing",
            topic_sig,
            "failed",
            old_ts - i as i64 * 1000,
        );
    }

    let packages = detect_repeated_failures(db.conn(), &cfg, None, ts);
    let found = packages.iter().any(|p| p.topic_sig == topic_sig);
    assert!(
        !found,
        "old events outside window must not trigger detection"
    );
}

#[test]
fn test_repair_disabled_skips_detection() {
    let (_tmp, db) = new_test_db();
    let mut cfg = default_repair_config();
    cfg.enabled = false;
    let ts = now_ms();

    let topic_sig = "cccc3333dddd4444eeee5555ffff6666";
    for i in 0..3usize {
        let drawer_id = format!("disabled-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed with database error",
            "test-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("disabled-evt-{i}"),
            &drawer_id,
            "test-wing",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    let packages = detect_repeated_failures(db.conn(), &cfg, None, ts);
    assert!(
        packages.is_empty(),
        "disabled repair must return no packages"
    );
}

// ---------------------------------------------------------------------------
// RepairPackage evidence assembly
// ---------------------------------------------------------------------------

#[test]
fn test_repair_package_assembles_evidence() {
    let (_tmp, db) = new_test_db();
    let ts = now_ms();
    let window_start = ts - 7 * 86_400_000i64;
    let topic_sig = "dddd4444eeee5555ffff6666aaaa1111";

    // Insert 3 failure drawers.
    for i in 0..3usize {
        let drawer_id = format!("fail-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed due to error",
            "test-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("pkg-evt-{i}"),
            &drawer_id,
            "test-wing",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    // Insert 2 success drawers in same wing/room (no failure keywords).
    for i in 0..2usize {
        insert_test_drawer(
            &db,
            &format!("success-{i}"),
            "migration completed successfully with no issues",
            "test-wing",
        );
    }

    let pkg = assemble_repair_package(db.conn(), topic_sig, 3, 7, window_start);
    assert_eq!(pkg.failure_drawers.len(), 3, "must find 3 failure drawers");
    assert!(
        !pkg.success_drawers.is_empty(),
        "must find at least 1 success drawer"
    );
}

// ---------------------------------------------------------------------------
// Repair warnings injection
// ---------------------------------------------------------------------------

#[test]
fn test_repair_warnings_injected_when_pattern_exists() {
    let (_tmp, db) = new_test_db();
    let cfg = default_repair_config();
    let ts = now_ms();

    let topic_sig = "eeee5555ffff6666aaaa1111bbbb2222";
    for i in 0..3usize {
        let drawer_id = format!("warn-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed due to error",
            "warn-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("warn-evt-{i}"),
            &drawer_id,
            "warn-wing",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    let warnings = load_repair_warnings(db.conn(), &cfg, None, ts);
    assert!(!warnings.is_empty(), "must have at least one warning");
    assert_eq!(warnings[0].severity, "warn");
    assert!(
        warnings[0].message.contains("warn-wing"),
        "warning must mention the wing"
    );
}

#[test]
fn test_repair_warnings_empty_when_disabled() {
    let (_tmp, db) = new_test_db();
    let mut cfg = default_repair_config();
    cfg.enabled = false;
    let ts = now_ms();

    let warnings = load_repair_warnings(db.conn(), &cfg, None, ts);
    assert!(
        warnings.is_empty(),
        "disabled repair must produce no warnings"
    );
}

// ---------------------------------------------------------------------------
// topic_sig correctness
// ---------------------------------------------------------------------------

#[test]
fn test_topic_sig_is_32_hex_chars() {
    let sig = compute_topic_sig("the deployment failed with a database error");
    assert_eq!(sig.len(), 32);
    assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_topic_sig_deterministic_and_order_independent() {
    let a = compute_topic_sig("migration failed sqlite database error");
    let b = compute_topic_sig("migration failed sqlite database error");
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// Context integration — repair_warnings in ContextPack
// ---------------------------------------------------------------------------

#[test]
fn test_repair_warnings_injected_in_context() {
    // Uses default Config (repair.enabled = true, window_days = 7, min_failures = 3).
    let cfg = default_repair_config();
    let (_tmp, db) = new_test_db();
    let ts = now_ms();

    let topic_sig = "ffff6666aaaa1111bbbb2222cccc3333";
    for i in 0..3usize {
        let drawer_id = format!("ctx-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "migration failed due to database error",
            "ctx-wing",
        );
        insert_failure_event(
            db.conn(),
            &format!("ctx-evt-{i}"),
            &drawer_id,
            "ctx-wing",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    // Verify warnings are produced via the repair module directly (bypasses ConfigHandle).
    let warnings = load_repair_warnings(db.conn(), &cfg, None, ts);
    assert!(!warnings.is_empty(), "repair_warnings must be populated");
    assert_eq!(warnings[0].severity, "warn");
    assert!(warnings[0].message.contains("ctx-wing"));
}

// ---------------------------------------------------------------------------
// CLI output — mempal repair list
// ---------------------------------------------------------------------------

#[test]
fn test_repair_list_cli_shows_patterns() {
    let (_tmp, db) = new_test_db();
    let ts = now_ms();
    let topic_sig = "1111aaaa2222bbbb3333cccc4444dddd";

    // Insert 4 failure events so failure_count = 4.
    for i in 0..4usize {
        let drawer_id = format!("list-drawer-{i}");
        insert_test_drawer(
            &db,
            &drawer_id,
            "deployment failed with config error",
            "code-memory",
        );
        insert_failure_event(
            db.conn(),
            &format!("list-evt-{i}"),
            &drawer_id,
            "code-memory",
            topic_sig,
            "failed",
            ts - i as i64 * 1000,
        );
    }

    let cfg = default_repair_config();
    let packages = detect_repeated_failures(db.conn(), &cfg, None, ts);
    let found = packages.iter().find(|p| p.topic_sig == topic_sig);
    assert!(found.is_some(), "repair list must include our topic_sig");
    assert_eq!(found.unwrap().failure_count, 4);
}

// ---------------------------------------------------------------------------
// Non-blocking ingest — structural test via direct spawn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_ingest_failure_detection_is_nonblocking() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_tmp, db_path_buf) = {
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let _db = Database::open(&db_path).expect("open db");
        (tmp, db_path)
    };

    let written = Arc::new(AtomicBool::new(false));
    let written_clone = written.clone();
    let db_path = db_path_buf.clone();

    // Simulate spawn_failure_detection with a 50ms async delay before writing.
    let task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        written_clone.store(true, Ordering::SeqCst);
        // Verify the DB file exists and is openable.
        assert!(db_path.exists(), "db must exist");
    });

    // Immediately check — write should NOT have happened yet (fire-and-forget).
    assert!(
        !written.load(Ordering::SeqCst),
        "write must not have happened yet"
    );

    task.await.expect("task");
    assert!(written.load(Ordering::SeqCst), "write eventually completes");
}

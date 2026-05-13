use mempal::core::db::{CURRENT_FORK_EXT_VERSION, Database, read_fork_ext_version};
use mempal::embed::EmbedError;
use mempal::embed::status::{EmbedStatus, RetryConfigSnapshot};
use mempal::ingest::gating::GatingDecision;
use rusqlite::params;
use tempfile::TempDir;

fn new_test_db() -> (TempDir, std::path::PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db_path, db)
}

fn table_columns(db: &Database, table: &str) -> Vec<(String, String)> {
    let mut statement = db
        .conn()
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table info");
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .expect("query table info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect table info")
}

fn has_column(columns: &[(String, String)], name: &str, ty: &str) -> bool {
    columns
        .iter()
        .any(|(column_name, column_ty)| column_name == name && column_ty.eq_ignore_ascii_case(ty))
}

#[test]
fn test_audit_observability_schema_exists_after_current_migration() {
    let (_tmp, _db_path, db) = new_test_db();

    assert_eq!(
        read_fork_ext_version(db.conn()).expect("read fork_ext_version"),
        CURRENT_FORK_EXT_VERSION
    );

    let embed_columns = table_columns(&db, "embed_failure_log");
    assert!(has_column(&embed_columns, "id", "TEXT"));
    assert!(has_column(&embed_columns, "timestamp", "INTEGER"));
    assert!(has_column(&embed_columns, "error_message", "TEXT"));
    assert!(has_column(&embed_columns, "endpoint", "TEXT"));
    assert!(has_column(
        &embed_columns,
        "consecutive_failures",
        "INTEGER"
    ));
    assert!(has_column(&embed_columns, "duration_ms", "INTEGER"));
    assert!(has_column(&embed_columns, "retained_until", "INTEGER"));

    let gating_columns = table_columns(&db, "gating_audit");
    assert!(has_column(&gating_columns, "content_preview", "TEXT"));
}

#[test]
fn test_record_gating_audit_stores_preview_for_skip_decision() {
    let (_tmp, _db_path, db) = new_test_db();
    let content = format!("{}b", "a".repeat(500));
    let decision = GatingDecision::rejected(1, Some("low_signal".to_string()), None, None);

    db.record_gating_audit("candidate-hash", &decision, None, Some(&content))
        .expect("record gating audit");

    let preview = db
        .conn()
        .query_row(
            "SELECT content_preview FROM gating_audit WHERE candidate_hash = ?1",
            params!["candidate-hash"],
            |row| row.get::<_, String>(0),
        )
        .expect("read content preview");

    assert_eq!(preview, format!("{}…", "a".repeat(500)));
}

#[test]
fn test_embed_status_records_failure_with_snapshot_to_db() {
    let (_tmp, db_path, db) = new_test_db();
    let status = EmbedStatus::new();
    status.set_audit_db_path(Some(db_path));
    let snapshot = RetryConfigSnapshot {
        retry_interval_secs: 2,
        alert_threshold: 100,
        degrade_threshold: 10,
        alert_script: None,
        alert_enabled: false,
    };
    let error = EmbedError::Runtime("backend unavailable".to_string());

    status.record_failure_with_snapshot(&error, &snapshot, Some(123));

    let (message, consecutive_failures, duration_ms, timestamp, retained_until) = db
        .conn()
        .query_row(
            "SELECT error_message, consecutive_failures, duration_ms, timestamp, retained_until FROM embed_failure_log LIMIT 1",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        ).expect("read embed failure log");

    assert_eq!(message, "embedding runtime error: backend unavailable");
    assert_eq!(consecutive_failures, 1);
    assert_eq!(duration_ms, Some(123));
    assert!(timestamp > 0);
    assert!(retained_until > timestamp);
}

#[test]
fn test_audit_cleanup_dry_run_does_not_delete() {
    let (_tmp, _db_path, db) = new_test_db();
    let project_id = "test-project";

    // 1. Insert a drawer
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d1",
                "content 1",
                "hooks-raw",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                project_id
            ],
        )
        .expect("insert drawer 1");

    // 2. Insert corresponding gating audit entry (decision=keep, score=0.5)
    let decision = GatingDecision::accepted(1, Some("test".to_string()), Some(0.5));
    db.record_gating_audit("h1", &decision, Some(project_id), Some("content 1"))
        .expect("record gating audit");

    // Fixup: record_gating_audit might not set drawer_id if it's not a merge.
    // Let's manually set drawer_id to match the drawer we just inserted.
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd1' WHERE candidate_hash = 'h1'",
            [],
        )
        .expect("update drawer_id");

    // 3. Run cleanup with dry_run=true
    let options = mempal::observability::AuditCleanupOptions {
        dry_run: true,
        score_threshold: 0.55,
        wing_filter: "hooks-raw",
    };
    let mut config = mempal::core::config::Config::default();
    config.project.id = Some(project_id.to_string());
    mempal::observability::audit_cleanup_command(&db, &config, options).expect("cleanup command");

    // 4. Verify drawer still exists (not deleted)
    let deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d1"],
            |row| row.get(0),
        )
        .expect("query deleted_at");
    assert!(deleted_at.is_none());
}

#[test]
fn test_audit_cleanup_soft_deletes_low_score_drawers() {
    let (_tmp, _db_path, db) = new_test_db();
    let project_id = "test-project";

    // 1. Insert some drawers
    // d1: hooks-raw, score 0.5 (should be deleted)
    // d2: hooks-raw, score 0.6 (should be kept)
    // d3: other-wing, score 0.5 (should be kept)
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d1",
                "content 1",
                "hooks-raw",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                project_id
            ],
        )
        .expect("insert drawer 1");
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d2",
                "content 2",
                "hooks-raw",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                project_id
            ],
        )
        .expect("insert drawer 2");
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d3",
                "content 3",
                "other-wing",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                project_id
            ],
        )
        .expect("insert drawer 3");

    // 2. Insert corresponding gating audit entries
    let d1_audit = GatingDecision::accepted(1, Some("test".to_string()), Some(0.5));
    db.record_gating_audit("h1", &d1_audit, Some(project_id), Some("content 1"))
        .expect("record audit 1");
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd1' WHERE candidate_hash = 'h1'",
            [],
        )
        .expect("update audit 1");

    let d2_audit = GatingDecision::accepted(1, Some("test".to_string()), Some(0.6));
    db.record_gating_audit("h2", &d2_audit, Some(project_id), Some("content 2"))
        .expect("record audit 2");
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd2' WHERE candidate_hash = 'h2'",
            [],
        )
        .expect("update audit 2");

    let d3_audit = GatingDecision::accepted(1, Some("test".to_string()), Some(0.5));
    db.record_gating_audit("h3", &d3_audit, Some(project_id), Some("content 3"))
        .expect("record audit 3");
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd3' WHERE candidate_hash = 'h3'",
            [],
        )
        .expect("update audit 3");

    // 3. Run cleanup with dry_run=false
    let options = mempal::observability::AuditCleanupOptions {
        dry_run: false,
        score_threshold: 0.55,
        wing_filter: "hooks-raw",
    };
    let mut config = mempal::core::config::Config::default();
    config.project.id = Some(project_id.to_string());
    mempal::observability::audit_cleanup_command(&db, &config, options).expect("cleanup command");

    // 4. Verify d1 is deleted
    let d1_deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d1"],
            |row| row.get(0),
        )
        .expect("query d1 deleted_at");
    assert!(d1_deleted_at.is_some());

    // 5. Verify d2 is NOT deleted
    let d2_deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d2"],
            |row| row.get(0),
        )
        .expect("query d2 deleted_at");
    assert!(d2_deleted_at.is_none());

    // 6. Verify d3 is NOT deleted
    let d3_deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d3"],
            |row| row.get(0),
        )
        .expect("query d3 deleted_at");
    assert!(d3_deleted_at.is_none());
}

#[test]
fn test_audit_cleanup_respects_project_isolation() {
    let (_tmp, _db_path, db) = new_test_db();

    // 1. Insert two drawers with different project_ids
    // d1: project_id = 'A', wing = 'hooks-raw', score = 0.5
    // d2: project_id = 'B', wing = 'hooks-raw', score = 0.5
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d1",
                "content 1",
                "hooks-raw",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                "A"
            ],
        )
        .expect("insert drawer 1");
    db.conn()
        .execute(
            "INSERT INTO drawers (id, content, wing, source_type, added_at, project_id) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                "d2",
                "content 2",
                "hooks-raw",
                "agent_inference",
                "2024-05-05T10:00:00Z",
                "B"
            ],
        )
        .expect("insert drawer 2");

    // 2. Insert corresponding gating audit entries
    let d1_audit = GatingDecision::accepted(1, Some("test".to_string()), Some(0.5));
    db.record_gating_audit("h1", &d1_audit, None, Some("content 1"))
        .expect("record audit 1");
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd1', project_id = 'A' WHERE candidate_hash = 'h1'",
            [],
        )
        .expect("update audit 1");

    let d2_audit = GatingDecision::accepted(1, Some("test".to_string()), Some(0.5));
    db.record_gating_audit("h2", &d2_audit, None, Some("content 2"))
        .expect("record audit 2");
    db.conn()
        .execute(
            "UPDATE gating_audit SET drawer_id = 'd2', project_id = 'B' WHERE candidate_hash = 'h2'",
            [],
        )
        .expect("update audit 2");

    // 3. Create a config with project_id = 'A'
    let mut config = mempal::core::config::Config::default();
    config.project.id = Some("A".to_string());

    // 4. Run cleanup scoped to project A
    let options = mempal::observability::AuditCleanupOptions {
        dry_run: false,
        score_threshold: 0.55,
        wing_filter: "hooks-raw",
    };
    mempal::observability::audit_cleanup_command(&db, &config, options).expect("cleanup command");

    // 5. Verify d1 is deleted
    let d1_deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d1"],
            |row| row.get(0),
        )
        .expect("query d1 deleted_at");
    assert!(d1_deleted_at.is_some());

    // 6. Verify d2 is NOT deleted
    let d2_deleted_at: Option<String> = db
        .conn()
        .query_row(
            "SELECT deleted_at FROM drawers WHERE id = ?",
            params!["d2"],
            |row| row.get(0),
        )
        .expect("query d2 deleted_at");
    assert!(
        d2_deleted_at.is_none(),
        "d2 should not be deleted as it belongs to project B"
    );
}

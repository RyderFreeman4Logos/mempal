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
            r#"
            SELECT error_message, consecutive_failures, duration_ms, timestamp, retained_until
            FROM embed_failure_log
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .expect("read embed failure log");

    assert!(message.contains("backend unavailable"));
    assert_eq!(consecutive_failures, 1);
    assert_eq!(duration_ms, 123);
    assert_eq!(retained_until, timestamp + 7 * 24 * 60 * 60);
}

#[test]
fn test_prune_expired_audit_logs_removes_expired_rows() {
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
    status.record_failure_with_snapshot(&error, &snapshot, Some(1));

    let decision = GatingDecision::rejected(1, Some("low_signal".to_string()), None, None);
    db.record_gating_audit("candidate-hash", &decision, None, Some("candidate"))
        .expect("record gating audit");
    db.conn()
        .execute("UPDATE embed_failure_log SET retained_until = 0", [])
        .expect("expire embed failure");
    db.conn()
        .execute("UPDATE gating_audit SET retained_until = 0", [])
        .expect("expire gating audit");

    db.prune_expired_audit_logs().expect("prune audit logs");

    let embed_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM embed_failure_log", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count embed failures");
    let gating_count = db
        .conn()
        .query_row("SELECT COUNT(*) FROM gating_audit", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count gating audit");

    assert_eq!(embed_count, 0);
    assert_eq!(gating_count, 0);
}

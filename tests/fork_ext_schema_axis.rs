use mempal::core::db::{
    CURRENT_FORK_EXT_VERSION, Database, apply_fork_ext_migrations, apply_fork_ext_migrations_to,
    read_fork_ext_version, set_fork_ext_version,
};
use rusqlite::Connection;
use tempfile::TempDir;

fn new_test_db() -> (TempDir, std::path::PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db_path, db)
}

fn current_schema_version() -> u32 {
    let source = include_str!("../src/core/db.rs");
    let version_line = source
        .lines()
        .find(|line| line.contains("const CURRENT_SCHEMA_VERSION"))
        .expect("CURRENT_SCHEMA_VERSION line");
    let version = version_line
        .split('=')
        .nth(1)
        .expect("version assignment")
        .trim()
        .trim_end_matches(';');

    version.parse().expect("CURRENT_SCHEMA_VERSION value")
}

fn table_columns(conn: &Connection, table: &str) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table info");
    stmt.query_map([], |row| {
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
fn test_fork_ext_version_is_current_after_audit_project_scope_phase() {
    let (_tmp, _db_path, db) = new_test_db();

    let version = read_fork_ext_version(db.conn()).expect("read fork-ext version");

    assert_eq!(version, CURRENT_FORK_EXT_VERSION);
}

#[test]
fn test_fork_ext_migrations_idempotent() {
    let (_tmp, _db_path, db) = new_test_db();

    apply_fork_ext_migrations(db.conn()).expect("first apply");
    apply_fork_ext_migrations(db.conn()).expect("second apply");

    let version = read_fork_ext_version(db.conn()).expect("read fork-ext version");
    assert_eq!(version, CURRENT_FORK_EXT_VERSION);
}

#[test]
fn test_upstream_user_version_preserved_after_fork_ext_init() {
    let (_tmp, db_path, _db) = new_test_db();
    let conn = Connection::open(db_path).expect("open sqlite connection");

    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version");

    assert_eq!(user_version, current_schema_version());
}

#[test]
fn test_fork_ext_v16_to_v17_adds_drawers_source_root_idempotently() {
    let conn = Connection::open_in_memory().expect("open sqlite connection");
    let schema_version = current_schema_version();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE fork_ext_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO fork_ext_meta (key, value) VALUES ('fork_ext_version', '16');
        CREATE TABLE drawers (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            source_file TEXT,
            wing TEXT NOT NULL,
            room TEXT,
            normalize_version INTEGER NOT NULL DEFAULT 1,
            deleted_at TEXT
        );
        PRAGMA user_version = {schema_version};
        "#
    ))
    .expect("create v16 schema");

    apply_fork_ext_migrations_to(&conn, 17).expect("first v17 migration");
    apply_fork_ext_migrations_to(&conn, 17).expect("second v17 migration");

    let source_root = table_columns(&conn, "drawers")
        .into_iter()
        .find(|(name, _)| name == "source_root")
        .expect("source_root column exists");
    assert_eq!(source_root.1, "TEXT");
    let not_null: i64 = conn
        .query_row(
            "SELECT \"notnull\" FROM pragma_table_info('drawers') WHERE name = 'source_root'",
            [],
            |row| row.get(0),
        )
        .expect("read source_root notnull");
    assert_eq!(not_null, 0);
    let index_exists: i64 = conn
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM sqlite_master
            WHERE type = 'index'
              AND name = 'idx_drawers_reindex_source_identity_active'
            "#,
            [],
            |row| row.get(0),
        )
        .expect("read index");
    assert_eq!(index_exists, 1);
    assert_eq!(read_fork_ext_version(&conn).expect("read version"), 17);
    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version");
    assert_eq!(user_version, schema_version);
    assert_eq!(schema_version, current_schema_version());
}

#[test]
fn test_fork_ext_v17_to_v18_adds_xurl_vector_metadata_idempotently() {
    let conn = Connection::open_in_memory().expect("open sqlite connection");
    let schema_version = current_schema_version();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE fork_ext_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO fork_ext_meta (key, value) VALUES ('fork_ext_version', '17');
        CREATE TABLE conversation_turns (
            id TEXT PRIMARY KEY
        );
        CREATE TABLE conversation_turn_vectors (
            turn_id     TEXT NOT NULL REFERENCES conversation_turns(id),
            chunk_index INTEGER NOT NULL DEFAULT 0,
            vector      BLOB NOT NULL,
            PRIMARY KEY (turn_id, chunk_index)
        );
        PRAGMA user_version = {schema_version};
        "#
    ))
    .expect("create v17 xurl vector schema");

    apply_fork_ext_migrations_to(&conn, 18).expect("first v18 migration");
    apply_fork_ext_migrations_to(&conn, 18).expect("second v18 migration");

    let columns = table_columns(&conn, "conversation_turn_vectors");
    assert!(has_column(&columns, "embedder_fingerprint", "TEXT"));
    assert!(has_column(&columns, "dim", "INTEGER"));
    assert!(has_column(&columns, "index_version", "TEXT"));
    assert_eq!(
        columns
            .iter()
            .filter(|(name, _)| name == "embedder_fingerprint")
            .count(),
        1
    );
    assert_eq!(columns.iter().filter(|(name, _)| name == "dim").count(), 1);
    assert_eq!(
        columns
            .iter()
            .filter(|(name, _)| name == "index_version")
            .count(),
        1
    );
    let version = read_fork_ext_version(&conn).expect("read fork-ext version");
    assert_eq!(version, 18);
}

#[test]
fn test_new_database_has_async_ingest_columns_after_v19() {
    let (_tmp, _db_path, db) = new_test_db();

    let pending_columns = table_columns(db.conn(), "pending_messages");
    assert!(has_column(&pending_columns, "result_drawer_id", "TEXT"));
    assert!(has_column(&pending_columns, "op_state", "TEXT"));
    assert!(has_column(&pending_columns, "rejected_reason", "TEXT"));
    assert!(has_column(&pending_columns, "failure_detail", "TEXT"));
    assert!(has_column(&pending_columns, "result_json", "TEXT"));

    let completion_columns = table_columns(db.conn(), "pending_message_completions");
    assert!(has_column(&completion_columns, "result_drawer_id", "TEXT"));
    assert!(has_column(&completion_columns, "op_state", "TEXT"));
    assert!(has_column(&completion_columns, "rejected_reason", "TEXT"));
    assert!(has_column(&completion_columns, "failure_detail", "TEXT"));
    assert!(has_column(&completion_columns, "result_json", "TEXT"));

    let version = read_fork_ext_version(db.conn()).expect("read fork-ext version");
    assert_eq!(version, CURRENT_FORK_EXT_VERSION);
}

#[test]
fn test_fork_ext_v18_to_v19_adds_async_ingest_columns_idempotently() {
    let conn = Connection::open_in_memory().expect("open sqlite connection");
    let schema_version = current_schema_version();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE fork_ext_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO fork_ext_meta (key, value) VALUES ('fork_ext_version', '18');

        CREATE TABLE pending_messages (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            source_hash TEXT NOT NULL,
            claim_token TEXT,
            claimed_at INTEGER,
            heartbeat_at INTEGER,
            retry_count INTEGER NOT NULL DEFAULT 0,
            retry_backoff_ms INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL CHECK(status IN ('pending', 'claimed', 'done', 'failed')),
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_error TEXT
        );

        CREATE TABLE pending_message_completions (
            message_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            claimed_at INTEGER,
            completed_at INTEGER NOT NULL,
            processing_ms INTEGER
        );

        PRAGMA user_version = {schema_version};
        "#
    ))
    .expect("create v18 schema");

    apply_fork_ext_migrations_to(&conn, 19).expect("first v19 migration");
    apply_fork_ext_migrations_to(&conn, 19).expect("second v19 migration");

    let pending_columns = table_columns(&conn, "pending_messages");
    assert!(has_column(&pending_columns, "result_drawer_id", "TEXT"));
    assert!(has_column(&pending_columns, "op_state", "TEXT"));
    assert!(has_column(&pending_columns, "rejected_reason", "TEXT"));
    assert!(has_column(&pending_columns, "failure_detail", "TEXT"));
    assert!(has_column(&pending_columns, "result_json", "TEXT"));

    let completion_columns = table_columns(&conn, "pending_message_completions");
    assert!(has_column(&completion_columns, "result_drawer_id", "TEXT"));
    assert!(has_column(&completion_columns, "op_state", "TEXT"));
    assert!(has_column(&completion_columns, "rejected_reason", "TEXT"));
    assert!(has_column(&completion_columns, "failure_detail", "TEXT"));
    assert!(has_column(&completion_columns, "result_json", "TEXT"));

    assert_eq!(read_fork_ext_version(&conn).expect("read version"), 19);
    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version");
    assert_eq!(user_version, schema_version);
}

#[test]
fn test_fork_ext_v19_to_v20_backfills_op_state_and_normalizes_completion_created_at() {
    let conn = Connection::open_in_memory().expect("open sqlite connection");
    let schema_version = current_schema_version();
    conn.execute_batch(&format!(
        r#"
        CREATE TABLE fork_ext_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO fork_ext_meta (key, value) VALUES ('fork_ext_version', '19');

        CREATE TABLE pending_messages (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            source_hash TEXT NOT NULL,
            claim_token TEXT,
            claimed_at INTEGER,
            heartbeat_at INTEGER,
            retry_count INTEGER NOT NULL DEFAULT 0,
            retry_backoff_ms INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL CHECK(status IN ('pending', 'claimed', 'done', 'failed')),
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_error TEXT,
            result_drawer_id TEXT,
            op_state TEXT NOT NULL DEFAULT 'queued' CHECK(op_state IN ('queued', 'running', 'completed', 'rejected', 'failed')),
            rejected_reason TEXT,
            failure_detail TEXT,
            result_json TEXT
        );

        CREATE TABLE pending_message_completions (
            message_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            claimed_at INTEGER,
            completed_at INTEGER NOT NULL,
            processing_ms INTEGER,
            result_drawer_id TEXT,
            op_state TEXT NOT NULL DEFAULT 'queued' CHECK(op_state IN ('queued', 'running', 'completed', 'rejected', 'failed')),
            rejected_reason TEXT,
            failure_detail TEXT,
            result_json TEXT
        );

        INSERT INTO pending_messages (
            id,
            kind,
            source_hash,
            claim_token,
            claimed_at,
            heartbeat_at,
            retry_count,
            retry_backoff_ms,
            next_attempt_at,
            status,
            payload,
            created_at,
            last_error,
            result_drawer_id,
            op_state,
            rejected_reason,
            failure_detail,
            result_json
        )
        VALUES
            ('pm-pending', 'hook_event', 'hash-pending', NULL, NULL, NULL, 0, 0, 0, 'pending', 'payload-pending', 1700000000, NULL, NULL, 'queued', NULL, NULL, NULL),
            ('pm-claimed', 'hook_event', 'hash-claimed', 'worker:claim', 1700000001, 1700000001, 0, 0, 1700000001, 'claimed', 'payload-claimed', 1700000001, NULL, NULL, 'queued', NULL, NULL, NULL),
            ('pm-done', 'hook_event', 'hash-done', NULL, NULL, NULL, 0, 0, 0, 'done', 'payload-done', 1700000002, NULL, NULL, 'queued', NULL, NULL, NULL),
            ('pm-failed', 'hook_event', 'hash-failed', NULL, NULL, NULL, 0, 0, 0, 'failed', 'payload-failed', 1700000003, 'boom', NULL, 'queued', NULL, NULL, NULL);

        INSERT INTO pending_message_completions (
            message_id,
            kind,
            created_at,
            claimed_at,
            completed_at,
            processing_ms,
            result_drawer_id,
            op_state,
            rejected_reason,
            failure_detail,
            result_json
        )
        VALUES
            ('comp-completed', 'hook_event', 1700000000000001, 1700000001, 1700000001234, 1234, 'drawer-1', 'queued', NULL, NULL, '{{"state":"completed"}}'),
            ('comp-running-stale', 'hook_event', 1700000000000004, 1700000004, 1700000004234, 1234, 'drawer-2', 'running', NULL, NULL, '{{"state":"completed"}}'),
            ('comp-rejected', 'hook_event', 1700000000000002, 1700000002, 1700000002234, 1234, NULL, 'queued', 'policy', NULL, '{{"state":"rejected"}}'),
            ('comp-failed', 'hook_event', 1700000000000003, 1700000003, 1700000003234, 1234, NULL, 'queued', NULL, 'boom', '{{"state":"failed"}}');

        PRAGMA user_version = {schema_version};
        "#
    ))
    .expect("create v19 schema");

    apply_fork_ext_migrations_to(&conn, 20).expect("first v20 migration");
    apply_fork_ext_migrations_to(&conn, 20).expect("second v20 migration");

    for (id, expected_status, expected_op_state) in [
        ("pm-pending", "pending", "queued"),
        ("pm-claimed", "claimed", "running"),
        ("pm-done", "done", "completed"),
        ("pm-failed", "failed", "failed"),
    ] {
        let (status, op_state): (String, String) = conn
            .query_row(
                "SELECT status, op_state FROM pending_messages WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read pending row");
        assert_eq!(status, expected_status, "{id}");
        assert_eq!(op_state, expected_op_state, "{id}");
    }

    for (id, expected_op_state) in [
        ("comp-completed", "completed"),
        ("comp-running-stale", "completed"),
        ("comp-rejected", "rejected"),
        ("comp-failed", "failed"),
    ] {
        let op_state: String = conn
            .query_row(
                "SELECT op_state FROM pending_message_completions WHERE message_id = ?1",
                [id],
                |row| row.get(0),
            )
            .expect("read completion row");
        assert_eq!(op_state, expected_op_state, "{id}");
    }

    let queued_completions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE op_state = 'queued'",
            [],
            |row| row.get(0),
        )
        .expect("count queued completions");
    assert_eq!(queued_completions, 0);

    let running_completions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE op_state = 'running'",
            [],
            |row| row.get(0),
        )
        .expect("count running completions");
    assert_eq!(running_completions, 0);

    let (min_created_len, max_created_len): (i64, i64) = conn
        .query_row(
            r#"
            SELECT
                MIN(LENGTH(CAST(created_at AS TEXT))),
                MAX(LENGTH(CAST(created_at AS TEXT)))
            FROM pending_message_completions
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read created_at lengths");
    assert_eq!((min_created_len, max_created_len), (13, 13));

    assert_eq!(read_fork_ext_version(&conn).expect("read version"), 20);
    let user_version = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .expect("read user_version");
    assert_eq!(user_version, schema_version);
}

#[test]
fn test_fork_ext_meta_table_exists_after_init() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fork_ext_meta'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_gating_audit_table_exists_after_ext_v3() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='gating_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_novelty_audit_table_exists_after_ext_v4() {
    let (_tmp, _db_path, db) = new_test_db();

    let exists = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='novelty_audit'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master");

    assert_eq!(exists, 1);
}

#[test]
fn test_audit_tables_store_project_scope_after_ext_v6() {
    let (_tmp, _db_path, db) = new_test_db();

    let gating_columns = db
        .conn()
        .prepare("PRAGMA table_info(gating_audit)")
        .expect("prepare gating pragma")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query gating pragma")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect gating columns");
    assert!(gating_columns.iter().any(|name| name == "project_id"));

    let novelty_columns = db
        .conn()
        .prepare("PRAGMA table_info(novelty_audit)")
        .expect("prepare novelty pragma")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query novelty pragma")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect novelty columns");
    assert!(novelty_columns.iter().any(|name| name == "project_id"));
}

#[test]
fn test_new_database_has_gating_audit_llm_columns_after_ext_v8() {
    let (_tmp, _db_path, db) = new_test_db();

    let columns = table_columns(db.conn(), "gating_audit");

    assert!(
        has_column(&columns, "llm_verdict", "TEXT"),
        "missing llm_verdict TEXT: {columns:?}"
    );
    assert!(
        has_column(&columns, "llm_score", "REAL"),
        "missing llm_score REAL: {columns:?}"
    );
}

#[test]
fn test_fork_ext_migration_v7_to_v8_adds_llm_audit_columns_idempotently() {
    let (_tmp, _db_path, db) = new_test_db();
    db.conn()
        .execute_batch(
            r#"
            DROP TABLE IF EXISTS gating_audit;
            CREATE TABLE gating_audit (
                id TEXT PRIMARY KEY,
                candidate_hash TEXT NOT NULL,
                drawer_id TEXT,
                decision TEXT NOT NULL CHECK(decision IN ('keep', 'skip')),
                tier INTEGER NOT NULL,
                label TEXT,
                reason TEXT,
                score REAL,
                explain_json TEXT NOT NULL,
                retained_until INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                project_id TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_gating_audit_created_at
                ON gating_audit(created_at);
            CREATE INDEX IF NOT EXISTS idx_gating_audit_candidate_hash
                ON gating_audit(candidate_hash);
            CREATE INDEX IF NOT EXISTS idx_gating_audit_project_created_at
                ON gating_audit(project_id, created_at);
            "#,
        )
        .expect("recreate v7 gating_audit");
    set_fork_ext_version(db.conn(), 7).expect("set fork_ext_version");

    let before_columns = table_columns(db.conn(), "gating_audit");
    assert!(!has_column(&before_columns, "llm_verdict", "TEXT"));
    assert!(!has_column(&before_columns, "llm_score", "REAL"));

    apply_fork_ext_migrations_to(db.conn(), 8).expect("apply v8 migration");

    let columns = table_columns(db.conn(), "gating_audit");
    assert!(
        has_column(&columns, "llm_verdict", "TEXT"),
        "missing llm_verdict TEXT after upgrade: {columns:?}"
    );
    assert!(
        has_column(&columns, "llm_score", "REAL"),
        "missing llm_score REAL after upgrade: {columns:?}"
    );
    assert_eq!(read_fork_ext_version(db.conn()).expect("read version"), 8);

    set_fork_ext_version(db.conn(), 7).expect("simulate stale fork_ext_version");
    apply_fork_ext_migrations_to(db.conn(), 8).expect("reapply v8 migration");

    assert_eq!(read_fork_ext_version(db.conn()).expect("read version"), 8);
}

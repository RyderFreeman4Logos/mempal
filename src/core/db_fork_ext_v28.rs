use rusqlite::Connection;

const SCHEMA_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN creation_operation_id TEXT;
"#;

pub(super) fn apply_v28(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)
}

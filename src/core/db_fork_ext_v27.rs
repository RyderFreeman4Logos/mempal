use rusqlite::Connection;

const SCHEMA_SQL: &str = r#"
ALTER TABLE pending_message_completions ADD COLUMN source_hash TEXT;
"#;

pub(super) fn apply_v27(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)
}

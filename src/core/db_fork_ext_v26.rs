//! Fork extension v26: generation fencing for runtime writer leases.

use rusqlite::Connection;

const V24_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_pending_status_heartbeat_at
    ON pending_messages(status, heartbeat_at);
"#;

const V25_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_pending_completions_op_state_rejected_reason
    ON pending_message_completions(op_state, rejected_reason);
"#;

const SCHEMA_SQL: &str = r#"
ALTER TABLE runtime_writer_leases
    ADD COLUMN generation INTEGER NOT NULL DEFAULT 1;

CREATE TABLE IF NOT EXISTS runtime_writer_lease_generations (
    name TEXT PRIMARY KEY,
    last_generation INTEGER NOT NULL CHECK(last_generation > 0)
);

INSERT INTO runtime_writer_lease_generations (name, last_generation)
    SELECT name, MAX(generation)
    FROM runtime_writer_leases
    GROUP BY name
ON CONFLICT(name) DO UPDATE SET
    last_generation = MAX(last_generation, excluded.last_generation);
"#;

pub(super) fn apply_v26(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA_SQL)
}

pub(super) fn apply_v24(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(V24_SCHEMA_SQL)
}

pub(super) fn apply_v25(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(V25_SCHEMA_SQL)
}

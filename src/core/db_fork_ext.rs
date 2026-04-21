use rusqlite::{Connection, OptionalExtension, params};

/// Hook injected into the migration runner between `up()` and `COMMIT`.
/// Returning Err triggers a ROLLBACK, leaving `fork_ext_version` unchanged.
/// // harness-point: PR0
pub trait MigrationHook: Send + Sync {
    fn pre_commit(&self) -> anyhow::Result<()>;
}

pub const FORK_EXT_META_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS fork_ext_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

pub const FORK_EXT_V1_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS pending_messages (
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

CREATE INDEX IF NOT EXISTS idx_pending_status_claimed_at
    ON pending_messages(status, claimed_at);
CREATE INDEX IF NOT EXISTS idx_pending_next_attempt
    ON pending_messages(status, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_pending_source_hash
    ON pending_messages(source_hash);
"#;

pub const FORK_EXT_V2_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS reindex_progress (
    source_path TEXT PRIMARY KEY,
    last_processed_chunk_id INTEGER,
    embedder_name TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('running', 'paused', 'done', 'failed'))
);
"#;

pub const FORK_EXT_V3_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS gating_audit (
    id TEXT PRIMARY KEY,
    candidate_hash TEXT NOT NULL,
    decision TEXT NOT NULL,
    explain_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_gating_audit_created_at
    ON gating_audit(created_at);
CREATE INDEX IF NOT EXISTS idx_gating_audit_candidate_hash
    ON gating_audit(candidate_hash);
"#;

pub const FORK_EXT_V4_SCHEMA_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN merge_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE drawers ADD COLUMN updated_at TEXT;

CREATE TABLE IF NOT EXISTS novelty_audit (
    id TEXT PRIMARY KEY,
    candidate_hash TEXT NOT NULL,
    decision TEXT NOT NULL,
    near_drawer_id TEXT,
    cosine REAL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_novelty_audit_created_at
    ON novelty_audit(created_at);
CREATE INDEX IF NOT EXISTS idx_novelty_audit_candidate_hash
    ON novelty_audit(candidate_hash);

-- TODO(spec ambiguity): earlier spec drafts referred to this trigger as
-- `drawers_au_fts`, while the task prompt provided the concrete
-- `drawers_fts_after_update` name. Standardize on the prompt name here and
-- drop the older draft name if it exists.
DROP TRIGGER IF EXISTS drawers_au_fts;
DROP TRIGGER IF EXISTS drawers_fts_after_update;
CREATE TRIGGER drawers_fts_after_update
AFTER UPDATE OF content ON drawers BEGIN
    INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
    INSERT INTO drawers_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

struct Migration {
    version: u32,
    up: fn(&Connection) -> rusqlite::Result<()>,
}

pub fn read_fork_ext_version(conn: &Connection) -> rusqlite::Result<u32> {
    let table_exists = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='fork_ext_meta'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if table_exists == 0 {
        return Ok(0);
    }

    let value = conn
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'fork_ext_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    match value {
        Some(value) => Ok(value.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?),
        None => Ok(0),
    }
}

pub fn set_fork_ext_version(conn: &Connection, v: u32) -> rusqlite::Result<()> {
    conn.execute(
        r#"
        INSERT INTO fork_ext_meta (key, value)
        VALUES ('fork_ext_version', ?1)
        ON CONFLICT(key) DO UPDATE SET value = excluded.value
        "#,
        params![v.to_string()],
    )?;
    Ok(())
}

fn fork_ext_migrations() -> &'static [Migration] {
    &[
        Migration {
            version: 1,
            up: apply_v1,
        },
        Migration {
            version: 2,
            up: apply_v2,
        },
        Migration {
            version: 3,
            up: apply_v3,
        },
        Migration {
            version: 4,
            up: apply_v4,
        },
        Migration {
            version: 5,
            up: apply_v5,
        },
        Migration {
            version: 6,
            up: apply_v6,
        },
    ]
}

fn apply_v1(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_V1_SCHEMA_SQL)
}

fn apply_v2(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_V2_SCHEMA_SQL)
}

fn apply_v3(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_V3_SCHEMA_SQL)
}

fn apply_v4(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_V4_SCHEMA_SQL)
}

fn apply_v5(conn: &Connection) -> rusqlite::Result<()> {
    ensure_project_column(conn, "drawers", "TEXT")?;
    ensure_project_column(conn, "triples", "TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_drawers_project_id ON drawers(project_id);",
    )?;
    recreate_drawer_vectors_with_project_id(conn)?;
    Ok(())
}

fn apply_v6(conn: &Connection) -> rusqlite::Result<()> {
    ensure_project_column(conn, "gating_audit", "TEXT")?;
    ensure_project_column(conn, "novelty_audit", "TEXT")?;
    conn.execute_batch(
        r#"
        UPDATE gating_audit
        SET project_id = (
            SELECT project_id FROM drawers WHERE drawers.id = gating_audit.candidate_hash
        )
        WHERE project_id IS NULL;

        UPDATE novelty_audit
        SET project_id = (
            SELECT project_id FROM drawers WHERE drawers.id = novelty_audit.candidate_hash
        )
        WHERE project_id IS NULL;

        CREATE INDEX IF NOT EXISTS idx_gating_audit_project_created_at
            ON gating_audit(project_id, created_at);
        CREATE INDEX IF NOT EXISTS idx_novelty_audit_project_created_at
            ON novelty_audit(project_id, created_at);
        "#,
    )?;
    Ok(())
}

fn ensure_project_column(conn: &Connection, table: &str, ty: &str) -> rusqlite::Result<()> {
    if table_has_column(conn, table, "project_id")? {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN project_id {ty};"))
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn recreate_drawer_vectors_with_project_id(conn: &Connection) -> rusqlite::Result<()> {
    let table_sql = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawer_vectors'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(table_sql) = table_sql else {
        return Ok(());
    };
    if table_sql.contains("project_id") {
        return Ok(());
    }

    let dimension = vector_dimension(conn, &table_sql)?;
    let expected_count = conn.query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
        row.get::<_, i64>(0)
    })?;

    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS _drawer_vectors_backup;
        CREATE TEMP TABLE _drawer_vectors_backup (
            id TEXT PRIMARY KEY,
            embedding BLOB NOT NULL
        );
        INSERT INTO _drawer_vectors_backup (id, embedding)
            SELECT id, embedding FROM drawer_vectors;
        DROP TABLE drawer_vectors;
        "#,
    )?;
    conn.execute_batch(&format!(
        r#"
        CREATE VIRTUAL TABLE drawer_vectors USING vec0(
            id TEXT PRIMARY KEY,
            embedding FLOAT[{dimension}],
            +project_id TEXT
        );
        "#
    ))?;
    conn.execute_batch(
        r#"
        INSERT INTO drawer_vectors (id, embedding, project_id)
            SELECT id, embedding, NULL FROM _drawer_vectors_backup;
        "#,
    )?;

    let actual_count = conn.query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
        row.get::<_, i64>(0)
    })?;
    if actual_count != expected_count {
        return Err(rusqlite::Error::ExecuteReturnedResults);
    }

    conn.execute_batch("DROP TABLE _drawer_vectors_backup;")?;
    Ok(())
}

fn vector_dimension(conn: &Connection, table_sql: &str) -> rusqlite::Result<usize> {
    let by_row = conn
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as usize);
    if let Some(by_row) = by_row {
        return Ok(by_row);
    }

    let marker = "FLOAT[";
    let Some(start) = table_sql.find(marker) else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    let rest = &table_sql[start + marker.len()..];
    let Some(end) = rest.find(']') else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    rest[..end]
        .trim()
        .parse::<usize>()
        .map_err(|_| rusqlite::Error::InvalidQuery)
}

pub fn apply_fork_ext_migrations(conn: &Connection) -> rusqlite::Result<()> {
    apply_fork_ext_migrations_with_hook(conn, None)
}

/// Run fork-ext migrations with an optional test hook injected between `up()`
/// and `COMMIT`. If the hook returns Err the transaction is rolled back.
// harness-point: PR0
pub fn apply_fork_ext_migrations_with_hook(
    conn: &Connection,
    hook: Option<&dyn MigrationHook>,
) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_META_DDL)?;

    let current_version = read_fork_ext_version(conn)?;
    for migration in fork_ext_migrations()
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (|| {
            (migration.up)(conn)?;
            // harness-point: PR0 — test hook injection between up() and COMMIT
            if let Some(hook) = hook {
                hook.pre_commit().map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::other(error.to_string())),
                    )
                })?;
            }
            set_fork_ext_version(conn, migration.version)?;
            conn.execute_batch("COMMIT")?;
            Ok(())
        })();

        if let Err(error) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error);
        }
    }

    Ok(())
}

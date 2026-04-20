use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const FORK_EXT_META_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS fork_ext_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

struct Migration {
    version: u32,
    up: fn(&Connection) -> rusqlite::Result<()>,
}

pub(crate) fn read_fork_ext_version(conn: &Connection) -> rusqlite::Result<u32> {
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

pub(crate) fn set_fork_ext_version(conn: &Connection, v: u32) -> rusqlite::Result<()> {
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
    &[]
}

pub(crate) fn apply_fork_ext_migrations(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORK_EXT_META_DDL)?;

    let current_version = read_fork_ext_version(conn)?;
    for migration in fork_ext_migrations()
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        conn.execute_batch("BEGIN IMMEDIATE")?;

        let result = (|| {
            (migration.up)(conn)?;
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

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexProgressRow {
    pub source_path: String,
    pub last_processed_chunk_id: Option<i64>,
    pub embedder_name: String,
    pub started_at: i64,
    pub updated_at: i64,
    pub status: String,
}

#[derive(Debug, Error)]
pub enum ReindexProgressError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

pub type Result<T> = std::result::Result<T, ReindexProgressError>;

#[derive(Debug, Clone)]
pub struct ReindexProgressStore {
    db_path: PathBuf,
}

impl ReindexProgressStore {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            db_path: path.as_ref().to_path_buf(),
        }
    }

    pub fn upsert_running(
        &self,
        source_path: &str,
        last_processed_chunk_id: Option<i64>,
        embedder_name: &str,
    ) -> Result<()> {
        self.upsert(
            source_path,
            last_processed_chunk_id,
            embedder_name,
            "running",
        )
    }

    pub fn mark_paused(
        &self,
        source_path: &str,
        last_processed_chunk_id: Option<i64>,
        embedder_name: &str,
    ) -> Result<()> {
        self.upsert(
            source_path,
            last_processed_chunk_id,
            embedder_name,
            "paused",
        )
    }

    pub fn mark_done(
        &self,
        source_path: &str,
        last_processed_chunk_id: Option<i64>,
        embedder_name: &str,
    ) -> Result<()> {
        self.upsert(source_path, last_processed_chunk_id, embedder_name, "done")
    }

    pub fn latest_resumable(
        &self,
        embedder_name: Option<&str>,
    ) -> Result<Option<ReindexProgressRow>> {
        let conn = self.open_connection()?;
        let sql = match embedder_name {
            Some(_) => {
                r#"
                SELECT source_path, last_processed_chunk_id, embedder_name, started_at, updated_at, status
                FROM reindex_progress
                WHERE status IN ('running', 'paused') AND embedder_name = ?1
                ORDER BY updated_at DESC, source_path ASC
                LIMIT 1
                "#
            }
            None => {
                r#"
                SELECT source_path, last_processed_chunk_id, embedder_name, started_at, updated_at, status
                FROM reindex_progress
                WHERE status IN ('running', 'paused')
                ORDER BY updated_at DESC, source_path ASC
                LIMIT 1
                "#
            }
        };

        let row = match embedder_name {
            Some(name) => conn.query_row(sql, [name], map_row).optional()?,
            None => conn.query_row(sql, [], map_row).optional()?,
        };
        Ok(row)
    }

    fn upsert(
        &self,
        source_path: &str,
        last_processed_chunk_id: Option<i64>,
        embedder_name: &str,
        status: &str,
    ) -> Result<()> {
        let now = now_secs();
        let conn = self.open_connection()?;
        conn.execute(
            r#"
            INSERT INTO reindex_progress (
                source_path,
                last_processed_chunk_id,
                embedder_name,
                started_at,
                updated_at,
                status
            )
            VALUES (?1, ?2, ?3, ?4, ?4, ?5)
            ON CONFLICT(source_path) DO UPDATE SET
                last_processed_chunk_id = excluded.last_processed_chunk_id,
                embedder_name = excluded.embedder_name,
                updated_at = excluded.updated_at,
                status = excluded.status
            "#,
            params![
                source_path,
                last_processed_chunk_id,
                embedder_name,
                now,
                status
            ],
        )?;
        Ok(())
    }

    fn open_connection(&self) -> Result<Connection> {
        Ok(Connection::open(&self.db_path)?)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReindexProgressRow> {
    Ok(ReindexProgressRow {
        source_path: row.get(0)?,
        last_processed_chunk_id: row.get(1)?,
        embedder_name: row.get(2)?,
        started_at: row.get(3)?,
        updated_at: row.get(4)?,
        status: row.get(5)?,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

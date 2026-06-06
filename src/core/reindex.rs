use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use super::db::SQLITE_CACHE_SIZE_KIB_256_MIB;

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

    pub fn mark_failed(
        &self,
        source_path: &str,
        last_processed_chunk_id: Option<i64>,
        embedder_name: &str,
    ) -> Result<()> {
        self.upsert(
            source_path,
            last_processed_chunk_id,
            embedder_name,
            "failed",
        )
    }

    /// Finalize orphan `running` rows whose source drawers are already fully
    /// current for the active vector index.
    ///
    /// This is intentionally conservative: only sources with at least one
    /// drawer and zero stale drawers are promoted to `done`, so a partially
    /// reindexed source remains resumable.
    pub fn finalize_completed_running_rows(
        &self,
        current_index_version: &str,
        target_fingerprint: &str,
    ) -> Result<usize> {
        let now = now_secs();
        let conn = self.open_connection()?;
        // Aggregate each source once, then join the summary back to running rows.
        let updated = conn.execute(
            r#"
            WITH source_summary AS (
                SELECT
                    COALESCE(d.source_file, d.id) AS source_path,
                    COUNT(*) AS drawer_count,
                    MAX(COALESCE(d.chunk_index, 0)) AS last_processed_chunk_id,
                    SUM(
                        CASE
                            WHEN v.id IS NULL
                              OR COALESCE(idx.value, legacy_idx.value, '') != ?1
                              OR COALESCE(fp.value, '') != ?2
                            THEN 1
                            ELSE 0
                        END
                    ) AS stale_drawer_count
                FROM drawers AS d
                LEFT JOIN drawer_vectors AS v ON v.id = d.id
                LEFT JOIN fork_ext_meta AS idx
                  ON idx.key = 'reindex:' || d.id || ':index_version'
                LEFT JOIN fork_ext_meta AS legacy_idx
                  ON legacy_idx.key = 'reindex:' || d.id || ':normalize_version'
                LEFT JOIN fork_ext_meta AS fp
                  ON fp.key = 'reindex:' || d.id || ':embedder_fingerprint'
                WHERE d.deleted_at IS NULL
                GROUP BY COALESCE(d.source_file, d.id)
            )
            UPDATE reindex_progress
            SET last_processed_chunk_id = source_summary.last_processed_chunk_id,
                updated_at = ?3,
                status = 'done'
            FROM source_summary
            WHERE reindex_progress.status = 'running'
              AND reindex_progress.source_path = source_summary.source_path
              AND source_summary.drawer_count > 0
              AND source_summary.stale_drawer_count = 0
            "#,
            params![current_index_version, target_fingerprint, now],
        )?;
        Ok(updated)
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
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB_256_MIB)?;
        Ok(conn)
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

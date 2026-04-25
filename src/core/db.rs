#[rustfmt::skip] #[path = "db_fork_ext.rs"] mod db_fork_ext;
// harness-point: PR0 — re-export MigrationHook trait + hooked migration runner for tests
pub use db_fork_ext::{
    CURRENT_FORK_EXT_VERSION, FORK_EXT_META_DDL, FORK_EXT_V1_SCHEMA_SQL, FORK_EXT_V2_SCHEMA_SQL,
    FORK_EXT_V3_SCHEMA_SQL, MigrationHook, apply_fork_ext_migrations, apply_fork_ext_migrations_to,
    apply_fork_ext_migrations_with_hook, read_fork_ext_version, set_fork_ext_version,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params, params_from_iter};
use serde_json::Value;
use thiserror::Error;

use super::{
    types::{Drawer, DrawerDetails, SourceType, TaxonomyEntry, Triple, TripleStats, TunnelDrawer},
    utils::{build_drawer_id, build_scoped_drawer_id},
};
use crate::ingest::gating::GatingDecision;
use crate::ingest::novelty::NoveltyAction;

const CURRENT_SCHEMA_VERSION: u32 = 5;
const GATING_DROP_TOTAL_KEY: &str = "gating.dropped.total";

const CONTENT_HASH_BACKFILL_BATCH: usize = 1_000;

fn content_hash_hex(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

const V1_SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS drawers (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    wing TEXT NOT NULL,
    room TEXT,
    source_file TEXT,
    source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
    added_at TEXT NOT NULL,
    chunk_index INTEGER
);

-- drawer_vectors is created lazily by insert_vector() with the actual
-- embedding dimension from the configured embedder. This avoids hardcoding
-- a dimension that may not match the model in use.

CREATE TABLE IF NOT EXISTS triples (
    id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL,
    valid_from TEXT,
    valid_to TEXT,
    confidence REAL DEFAULT 1.0,
    source_drawer TEXT REFERENCES drawers(id)
);

CREATE TABLE IF NOT EXISTS taxonomy (
    wing TEXT NOT NULL,
    room TEXT NOT NULL DEFAULT '',
    display_name TEXT,
    keywords TEXT,
    PRIMARY KEY (wing, room)
);

CREATE INDEX IF NOT EXISTS idx_drawers_wing ON drawers(wing);
CREATE INDEX IF NOT EXISTS idx_drawers_wing_room ON drawers(wing, room);
CREATE INDEX IF NOT EXISTS idx_triples_subject ON triples(subject);
CREATE INDEX IF NOT EXISTS idx_triples_object ON triples(object);
"#;

static SQLITE_VEC_AUTO_EXTENSION: OnceLock<Result<(), String>> = OnceLock::new();

#[derive(Debug, Error)]
pub enum DbError {
    #[error("failed to create database directory for {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read database metadata for {path}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to parse taxonomy keywords JSON")]
    Json(#[from] serde_json::Error),
    #[error("invalid source_type stored in database: {0}")]
    InvalidSourceType(String),
    #[error("failed to register sqlite-vec auto extension: {0}")]
    RegisterVec(String),
    #[error("database schema version {current} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { current: u32, supported: u32 },
}

pub struct Database {
    conn: Connection,
    path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatingDropCounts {
    pub total: Option<u64>,
    pub by_reason: std::collections::BTreeMap<String, u64>,
}

#[derive(Clone, Copy)]
enum OpenMode {
    ReadOnly,
    ReadWrite,
}

impl OpenMode {
    fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadWrite)
    }

    pub fn open_read_only(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadOnly)
    }

    fn open_with_mode(path: &Path, mode: OpenMode) -> Result<Self, DbError> {
        if mode.allows_write() {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }

        register_sqlite_vec()?;

        let conn = match mode {
            OpenMode::ReadOnly => {
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
            }
            OpenMode::ReadWrite => Connection::open(path)?,
        };
        if mode.allows_write() {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            apply_migrations(&conn)?;
            db_fork_ext::apply_fork_ext_migrations(&conn)?;
        }

        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert_drawer(&self, drawer: &Drawer) -> Result<(), DbError> {
        self.insert_drawer_with_project(drawer, None)
    }

    pub fn insert_drawer_with_project(
        &self,
        drawer: &Drawer,
        project_id: Option<&str>,
    ) -> Result<(), DbError> {
        let content_hash = content_hash_hex(&drawer.content);
        self.conn.execute(
            r#"
            INSERT OR IGNORE INTO drawers (
                id,
                content,
                wing,
                room,
                source_file,
                source_type,
                added_at,
                chunk_index,
                importance,
                project_id,
                content_hash
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                drawer.id,
                drawer.content,
                drawer.wing,
                drawer.room,
                drawer.source_file,
                source_type_as_str(&drawer.source_type),
                drawer.added_at,
                drawer.chunk_index,
                drawer.importance,
                project_id,
                content_hash,
            ],
        )?;

        Ok(())
    }

    pub fn record_gating_audit(
        &self,
        candidate_hash: &str,
        decision: &GatingDecision,
        project_id: Option<&str>,
    ) -> Result<(), DbError> {
        let explain_json = serde_json::to_string(decision)?;
        let created_at = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let retained_until = created_at + 7 * 24 * 60 * 60;
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let id_seed = format!(
            "{candidate_hash}:{created_at}:{unique_nanos}:{}",
            explain_json
        );
        let id = format!("gating_{}", blake3::hash(id_seed.as_bytes()).to_hex());
        let audit_decision = if decision.is_rejected() {
            "skip"
        } else {
            "keep"
        };
        let drawer_id = (!decision.is_rejected()).then_some(candidate_hash);
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), DbError> {
            self.conn.execute(
                r#"
                INSERT INTO gating_audit (
                    id,
                    candidate_hash,
                    drawer_id,
                    decision,
                    tier,
                    label,
                    reason,
                    score,
                    explain_json,
                    retained_until,
                    created_at,
                    project_id
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                "#,
                params![
                    id,
                    candidate_hash,
                    drawer_id,
                    audit_decision,
                    i64::from(decision.tier),
                    decision.label.as_deref(),
                    decision.gating_reason.as_deref(),
                    decision.score,
                    explain_json,
                    retained_until,
                    created_at,
                    project_id,
                ],
            )?;
            if let Some(reason) = decision.drop_reason() {
                self.increment_meta_counter(GATING_DROP_TOTAL_KEY)?;
                self.increment_meta_counter(&format!("gating.dropped.{reason}"))?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn gating_drop_counts(&self) -> Result<GatingDropCounts, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT key, value
            FROM fork_ext_meta
            WHERE key LIKE 'gating.dropped.%'
            ORDER BY key
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                let key = row.get::<_, String>(0)?;
                let value = row.get::<_, String>(1)?;
                Ok((key, value))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut total = None;
        let mut counts = std::collections::BTreeMap::new();
        for (key, value) in rows {
            let count = value.parse::<u64>().unwrap_or_default();
            if key == GATING_DROP_TOTAL_KEY {
                total = Some(count);
                continue;
            }
            if count == 0 {
                continue;
            }
            if let Some(reason) = key.strip_prefix("gating.dropped.by_reason.") {
                *counts.entry(reason.to_string()).or_default() += count;
                continue;
            }

            let Some(reason) = key.strip_prefix("gating.dropped.") else {
                continue;
            };
            if reason == "total" || reason.starts_with("by_reason.") {
                continue;
            }
            *counts.entry(reason.to_string()).or_default() += count;
        }
        Ok(GatingDropCounts {
            total,
            by_reason: counts,
        })
    }

    fn increment_meta_counter(&self, key: &str) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT INTO fork_ext_meta (key, value)
            VALUES (?1, '1')
            ON CONFLICT(key) DO UPDATE
            SET value = CAST(CAST(COALESCE(fork_ext_meta.value, '0') AS INTEGER) + 1 AS TEXT)
            "#,
            [key],
        )?;
        Ok(())
    }

    pub fn drawer_merge_state(&self, drawer_id: &str) -> Result<Option<(String, u32)>, DbError> {
        let mut statement = self.conn.prepare(
            "SELECT content, COALESCE(merge_count, 0) FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
        )?;
        let mut rows = statement.query_map([drawer_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn update_drawer_after_merge(
        &mut self,
        drawer_id: &str,
        merged_content: &str,
        updated_at: &str,
        vector: &[f32],
    ) -> Result<(), DbError> {
        self.ensure_vectors_table(vector.len())?;
        let vector_json = serde_json::to_string(vector)?;
        let transaction = self.conn.transaction()?;
        transaction.execute(
            r#"
            UPDATE drawers
            SET content = ?2,
                updated_at = ?3,
                merge_count = COALESCE(merge_count, 0) + 1
            WHERE id = ?1
            "#,
            params![drawer_id, merged_content, updated_at],
        )?;
        transaction.execute("DELETE FROM drawer_vectors WHERE id = ?1", [drawer_id])?;
        let project_id = transaction.query_row(
            "SELECT project_id FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, Option<String>>(0),
        )?;
        transaction.execute(
            "INSERT INTO drawer_vectors (id, embedding, project_id) VALUES (?1, vec_f32(?2), ?3)",
            params![drawer_id, vector_json, project_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_novelty_audit(
        &self,
        candidate_hash: &str,
        action: NoveltyAction,
        near_drawer_id: Option<&str>,
        cosine: Option<f32>,
        audit_decision: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<(), DbError> {
        let created_at = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let decision = audit_decision
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| action.as_str().to_string());
        let id_seed = format!(
            "{candidate_hash}:{created_at}:{unique_nanos}:{decision}:{}:{}",
            near_drawer_id.unwrap_or_default(),
            cosine.unwrap_or_default()
        );
        let id = format!("novelty_{}", blake3::hash(id_seed.as_bytes()).to_hex());
        self.conn.execute(
            r#"
            INSERT INTO novelty_audit (id, candidate_hash, decision, near_drawer_id, cosine, created_at, project_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                id,
                candidate_hash,
                decision,
                near_drawer_id,
                cosine,
                created_at,
                project_id
            ],
        )?;
        Ok(())
    }

    pub fn taxonomy_entries(&self) -> Result<Vec<TaxonomyEntry>, DbError> {
        let mut statement = self.conn.prepare(
            "SELECT wing, room, display_name, keywords FROM taxonomy ORDER BY wing, room",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;

        let mut entries = Vec::new();
        for row in rows {
            let (wing, room, display_name, keywords_json) = row?;
            let keywords = parse_keywords(keywords_json.as_deref())?;
            entries.push(TaxonomyEntry {
                wing,
                room,
                display_name,
                keywords,
            });
        }

        Ok(entries)
    }

    pub fn upsert_taxonomy_entry(&self, entry: &TaxonomyEntry) -> Result<(), DbError> {
        let keywords = serde_json::to_string(&entry.keywords)?;
        self.conn.execute(
            r#"
            INSERT INTO taxonomy (wing, room, display_name, keywords)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(wing, room) DO UPDATE SET
                display_name = excluded.display_name,
                keywords = excluded.keywords
            "#,
            (
                entry.wing.as_str(),
                entry.room.as_str(),
                entry.display_name.as_deref(),
                keywords.as_str(),
            ),
        )?;

        Ok(())
    }

    /// Returns top drawers sorted by importance (descending), then recency.
    pub fn top_drawers(&self, limit: usize) -> Result<Vec<Drawer>, DbError> {
        let limit = i64::try_from(limit)
            .map_err(|_| rusqlite::Error::InvalidParameterName("limit".to_string()))?;
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, content, wing, room, source_file, source_type, added_at, chunk_index,
                   COALESCE(importance, 0) as importance
            FROM drawers
            WHERE deleted_at IS NULL
            ORDER BY importance DESC, CAST(added_at AS INTEGER) DESC, id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, i32>(8)?,
            ))
        })?;

        let mut drawers = Vec::new();
        for row in rows {
            let (
                id,
                content,
                wing,
                room,
                source_file,
                source_type,
                added_at,
                chunk_index,
                importance,
            ) = row?;
            drawers.push(Drawer {
                id,
                content,
                wing,
                room,
                source_file,
                source_type: source_type_from_str(&source_type)?,
                added_at,
                chunk_index,
                importance,
            });
        }

        Ok(drawers)
    }

    pub fn drawer_exists(&self, drawer_id: &str) -> Result<bool, DbError> {
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM drawers WHERE id = ?1 AND deleted_at IS NULL)",
            [drawer_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists == 1)
    }

    pub fn resolve_ingest_drawer_id(
        &self,
        wing: &str,
        room: Option<&str>,
        content: &str,
        project_id: Option<&str>,
    ) -> Result<(String, bool), DbError> {
        if let Some(existing_id) =
            self.find_active_drawer_id_by_identity(wing, room, content, project_id)?
        {
            return Ok((existing_id, true));
        }

        let base_id = build_drawer_id(wing, room, content);
        if !self.drawer_id_in_use(&base_id)? {
            return Ok((base_id, false));
        }

        let scoped_seed = project_id.unwrap_or("__global_collision__");
        let scoped_id = build_scoped_drawer_id(wing, room, content, Some(scoped_seed));
        if scoped_id != base_id && !self.drawer_id_in_use(&scoped_id)? {
            return Ok((scoped_id, false));
        }

        let mut suffix = 2usize;
        loop {
            let candidate = format!("{scoped_id}_{suffix}");
            if !self.drawer_id_in_use(&candidate)? {
                return Ok((candidate, false));
            }
            suffix += 1;
        }
    }

    pub fn insert_vector(&self, drawer_id: &str, vector: &[f32]) -> Result<(), DbError> {
        self.insert_vector_with_project(drawer_id, vector, None)
    }

    pub fn insert_vector_with_project(
        &self,
        drawer_id: &str,
        vector: &[f32],
        project_id: Option<&str>,
    ) -> Result<(), DbError> {
        self.ensure_vectors_table(vector.len())?;
        let vector_json = serde_json::to_string(vector)?;
        match self.conn.execute(
            "INSERT INTO drawer_vectors (id, embedding, project_id) VALUES (?1, vec_f32(?2), ?3)",
            params![drawer_id, vector_json.as_str(), project_id],
        ) {
            Ok(_) => Ok(()),
            // sqlite-vec's vec0 virtual table does not honor INSERT OR IGNORE
            // or INSERT OR REPLACE — it always raises a UNIQUE primary key
            // violation on duplicate id, regardless of conflict clause. Match
            // on the message text (extended_code is SQLITE_ERROR=1, not 1555)
            // and swallow to preserve first-writer-wins semantics consistent
            // with drawers table's INSERT OR IGNORE behavior.
            Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                if msg.contains("UNIQUE constraint failed on drawer_vectors") =>
            {
                Ok(())
            }
            Err(e) => Err(DbError::Sqlite(e)),
        }
    }

    pub fn novelty_candidates(
        &self,
        query_vector: &[f32],
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, f32)>, DbError> {
        let vectors_exist: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get(0),
        )?;
        if !vectors_exist || limit == 0 {
            return Ok(Vec::new());
        }

        let query_json = serde_json::to_string(query_vector)?;
        let limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let fork_ext_version = db_fork_ext::read_fork_ext_version(&self.conn)?;
        let rows = if fork_ext_version >= 5 {
            let mut statement = self.conn.prepare(
                r#"
                WITH matches AS (
                    SELECT id
                    FROM drawer_vectors
                    WHERE embedding MATCH vec_f32(?1)
                      AND k = ?2
                      AND (?5 IS NULL OR project_id = ?5)
                )
                SELECT d.id,
                       CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
                FROM matches
                JOIN drawer_vectors v ON v.id = matches.id
                JOIN drawers d ON d.id = matches.id
                WHERE d.deleted_at IS NULL
                  AND (?3 IS NULL OR d.wing = ?3)
                  AND (?4 IS NULL OR d.room = ?4)
                  AND (?5 IS NULL OR d.project_id = ?5)
                ORDER BY similarity DESC
                LIMIT ?2
                "#,
            )?;
            statement
                .query_map(
                    (query_json.as_str(), limit, wing, room, project_id),
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?)),
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut statement = self.conn.prepare(
                r#"
                WITH matches AS (
                    SELECT id
                    FROM drawer_vectors
                    WHERE embedding MATCH vec_f32(?1)
                      AND k = ?2
                )
                SELECT d.id,
                       CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
                FROM matches
                JOIN drawer_vectors v ON v.id = matches.id
                JOIN drawers d ON d.id = matches.id
                WHERE d.deleted_at IS NULL
                  AND (?3 IS NULL OR d.wing = ?3)
                  AND (?4 IS NULL OR d.room = ?4)
                ORDER BY similarity DESC
                LIMIT ?2
                "#,
            )?;
            statement
                .query_map((query_json.as_str(), limit, wing, room), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows)
    }

    /// Ensure drawer_vectors table exists with the right dimension.
    /// Creates it on first call; errors on dimension mismatch.
    fn ensure_vectors_table(&self, dim: usize) -> Result<(), DbError> {
        let fork_ext_version = db_fork_ext::read_fork_ext_version(&self.conn)?;
        let project_column = if fork_ext_version >= 5 {
            ", +project_id TEXT"
        } else {
            ""
        };
        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS drawer_vectors USING vec0(id TEXT PRIMARY KEY, embedding FLOAT[{dim}]{project_column});"
        ))?;
        Ok(())
    }

    pub fn drawer_count(&self) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn taxonomy_count(&self) -> Result<i64, DbError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM taxonomy", [], |row| row.get(0))?)
    }

    pub fn scope_counts(&self) -> Result<Vec<(String, Option<String>, i64)>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT wing, room, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY wing, room
            ORDER BY wing, room
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn get_drawer(&self, drawer_id: &str) -> Result<Option<Drawer>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, content, wing, room, source_file, source_type, added_at, chunk_index,
                   COALESCE(importance, 0) as importance
            FROM drawers
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
        )?;
        let mut rows = statement.query_map([drawer_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, i32>(8)?,
            ))
        })?;

        match rows.next() {
            Some(row) => {
                let (
                    id,
                    content,
                    wing,
                    room,
                    source_file,
                    source_type,
                    added_at,
                    chunk_index,
                    importance,
                ) = row?;
                Ok(Some(Drawer {
                    id,
                    content,
                    wing,
                    room,
                    source_file,
                    source_type: source_type_from_str(&source_type)?,
                    added_at,
                    chunk_index,
                    importance,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn get_drawer_details(&self, drawer_id: &str) -> Result<Option<DrawerDetails>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, content, wing, room, source_file, source_type, added_at, chunk_index,
                   COALESCE(importance, 0) as importance,
                   updated_at,
                   COALESCE(merge_count, 0) as merge_count,
                   project_id
            FROM drawers
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
        )?;
        let mut rows = statement.query_map([drawer_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, i32>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, u32>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })?;

        match rows.next() {
            Some(row) => {
                let (
                    id,
                    content,
                    wing,
                    room,
                    source_file,
                    source_type,
                    added_at,
                    chunk_index,
                    importance,
                    updated_at,
                    merge_count,
                    project_id,
                ) = row?;
                Ok(Some(DrawerDetails {
                    drawer: Drawer {
                        id,
                        content,
                        wing,
                        room,
                        source_file,
                        source_type: source_type_from_str(&source_type)?,
                        added_at,
                        chunk_index,
                        importance,
                    },
                    updated_at,
                    merge_count,
                    project_id,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn get_drawer_details_batch(
        &self,
        drawer_ids: &[String],
    ) -> Result<Vec<DrawerDetails>, DbError> {
        const SQLITE_VARIABLE_LIMIT: usize = 900;

        let mut seen = HashSet::new();
        let mut ordered_ids = Vec::new();
        for drawer_id in drawer_ids {
            if seen.insert(drawer_id.clone()) {
                ordered_ids.push(drawer_id.clone());
            }
        }

        if ordered_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut found_by_id = HashMap::new();
        for chunk in ordered_ids.chunks(SQLITE_VARIABLE_LIMIT) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                r#"
                SELECT id, content, wing, room, source_file, source_type, added_at, chunk_index,
                       COALESCE(importance, 0) as importance,
                       updated_at,
                       COALESCE(merge_count, 0) as merge_count,
                       project_id
                FROM drawers
                WHERE deleted_at IS NULL
                  AND id IN ({placeholders})
                "#
            );
            let mut statement = self.conn.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(chunk.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i32>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, u32>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            })?;

            for row in rows {
                let (
                    id,
                    content,
                    wing,
                    room,
                    source_file,
                    source_type,
                    added_at,
                    chunk_index,
                    importance,
                    updated_at,
                    merge_count,
                    project_id,
                ) = row?;
                found_by_id.insert(
                    id.clone(),
                    DrawerDetails {
                        drawer: Drawer {
                            id,
                            content,
                            wing,
                            room,
                            source_file,
                            source_type: source_type_from_str(&source_type)?,
                            added_at,
                            chunk_index,
                            importance,
                        },
                        updated_at,
                        merge_count,
                        project_id,
                    },
                );
            }
        }

        Ok(ordered_ids
            .into_iter()
            .filter_map(|drawer_id| found_by_id.remove(&drawer_id))
            .collect())
    }

    pub fn drawer_project_id(&self, drawer_id: &str) -> Result<Option<String>, DbError> {
        let value = self
            .conn
            .query_row(
                "SELECT project_id FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                [drawer_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    fn drawer_id_in_use(&self, drawer_id: &str) -> Result<bool, DbError> {
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM drawers WHERE id = ?1)",
            [drawer_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists == 1)
    }

    fn find_active_drawer_id_by_identity(
        &self,
        wing: &str,
        room: Option<&str>,
        content: &str,
        project_id: Option<&str>,
    ) -> Result<Option<String>, DbError> {
        // Indexed lookup via idx_drawers_content_hash(wing, content_hash).
        // blake3 collisions are cryptographically negligible, so the hash
        // alone determines content-identity; room/project_id/deleted_at are
        // post-filtered against the (typically single-row) hash bucket.
        let hash = content_hash_hex(content);
        let value = self
            .conn
            .query_row(
                r#"
                SELECT id
                FROM drawers
                WHERE deleted_at IS NULL
                  AND wing = ?1
                  AND content_hash = ?2
                  AND ((room IS NULL AND ?3 IS NULL) OR room = ?3)
                  AND ((project_id IS NULL AND ?4 IS NULL) OR project_id = ?4)
                ORDER BY id
                LIMIT 1
                "#,
                params![wing, hash, room, project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(value)
    }

    pub fn soft_delete_drawer(&self, drawer_id: &str) -> Result<bool, DbError> {
        let timestamp = super::utils::current_timestamp();
        let affected = self.conn.execute(
            "UPDATE drawers SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![timestamp, drawer_id],
        )?;
        Ok(affected > 0)
    }

    pub fn purge_deleted(&self, before: Option<&str>) -> Result<u64, DbError> {
        // First collect IDs to purge, then delete from both tables
        let ids: Vec<String> = if let Some(before) = before {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM drawers WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            )?;
            stmt.query_map([before], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM drawers WHERE deleted_at IS NOT NULL")?;
            stmt.query_map([], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        if ids.is_empty() {
            return Ok(0);
        }

        // Check if drawer_vectors table exists (lazy-created)
        let vectors_exist: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get(0),
        )?;

        for id in &ids {
            if vectors_exist {
                self.conn
                    .execute("DELETE FROM drawer_vectors WHERE id = ?1", [id])?;
            }
            self.conn
                .execute("DELETE FROM drawers WHERE id = ?1", [id])?;
        }

        Ok(ids.len() as u64)
    }

    pub fn deleted_drawer_count(&self) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    // --- FTS5 BM25 search ---

    pub fn search_fts(
        &self,
        query: &str,
        wing: Option<&str>,
        room: Option<&str>,
        project_mode: &str,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, f64)>, DbError> {
        let Some(match_query) = build_fts_match_query(query) else {
            return Ok(Vec::new());
        };
        let limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let mut stmt = self
            .conn
            .prepare(&crate::search::filter::build_fts_runtime_sql())?;
        let rows = stmt
            .query_map(
                (
                    match_query.as_str(),
                    wing,
                    room,
                    project_mode,
                    project_id,
                    limit,
                ),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn project_breakdown(&self) -> Result<Vec<(Option<String>, i64)>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT project_id, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY project_id
            ORDER BY project_id NULLS LAST
            "#,
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn null_project_backfill_pending_count(&self) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND project_id IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn tunnel_drawers_for_room(
        &self,
        room: &str,
        exclude_drawer_id: &str,
        current_project_id: Option<&str>,
    ) -> Result<Vec<TunnelDrawer>, DbError> {
        let Some(current_project_id) = current_project_id else {
            return Ok(Vec::new());
        };

        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, content, wing, room, source_file, source_type, added_at, chunk_index,
                   COALESCE(importance, 0) as importance, project_id
            FROM drawers
            WHERE deleted_at IS NULL
              AND room = ?1
              AND id != ?2
              AND project_id IS NOT NULL
              AND project_id != ?3
            ORDER BY CAST(added_at AS INTEGER) DESC, id DESC
            "#,
        )?;
        let rows = stmt
            .query_map([room, exclude_drawer_id, current_project_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i32>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(
                    id,
                    content,
                    wing,
                    room,
                    source_file,
                    source_type,
                    added_at,
                    chunk_index,
                    importance,
                    project_id,
                )| {
                    Ok(TunnelDrawer {
                        drawer: Drawer {
                            id,
                            content,
                            wing,
                            room,
                            source_file,
                            source_type: source_type_from_str(&source_type)?,
                            added_at,
                            chunk_index,
                            importance,
                        },
                        target_project_id: project_id,
                    })
                },
            )
            .collect()
    }

    // --- Triples (Knowledge Graph) ---

    pub fn insert_triple(&self, triple: &Triple) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT OR REPLACE INTO triples (id, subject, predicate, object, valid_from, valid_to, confidence, source_drawer)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                triple.id,
                triple.subject,
                triple.predicate,
                triple.object,
                triple.valid_from,
                triple.valid_to,
                triple.confidence,
                triple.source_drawer,
            ],
        )?;
        Ok(())
    }

    pub fn query_triples(
        &self,
        subject: Option<&str>,
        predicate: Option<&str>,
        object: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<Triple>, DbError> {
        let active_clause = if active_only {
            "AND (valid_to IS NULL OR valid_to > strftime('%s', 'now'))"
        } else {
            ""
        };
        let sql = format!(
            r#"
            SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_drawer
            FROM triples
            WHERE (?1 IS NULL OR subject = ?1)
              AND (?2 IS NULL OR predicate = ?2)
              AND (?3 IS NULL OR object = ?3)
              {active_clause}
            ORDER BY confidence DESC, id
            "#
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map((subject, predicate, object), |row| {
                Ok(Triple {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    predicate: row.get(2)?,
                    object: row.get(3)?,
                    valid_from: row.get(4)?,
                    valid_to: row.get(5)?,
                    confidence: row.get(6)?,
                    source_drawer: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn invalidate_triple(&self, triple_id: &str) -> Result<bool, DbError> {
        let timestamp = super::utils::current_timestamp();
        let affected = self.conn.execute(
            "UPDATE triples SET valid_to = ?1 WHERE id = ?2 AND valid_to IS NULL",
            params![timestamp, triple_id],
        )?;
        Ok(affected > 0)
    }

    pub fn triple_count(&self) -> Result<i64, DbError> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM triples", [], |row| row.get(0))?)
    }

    pub fn timeline_for_entity(&self, entity: &str) -> Result<Vec<Triple>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, subject, predicate, object, valid_from, valid_to, confidence, source_drawer
            FROM triples
            WHERE subject = ?1 OR object = ?1
            ORDER BY COALESCE(valid_from, '0') ASC, id ASC
            "#,
        )?;
        let rows = stmt
            .query_map([entity], |row| {
                Ok(Triple {
                    id: row.get(0)?,
                    subject: row.get(1)?,
                    predicate: row.get(2)?,
                    object: row.get(3)?,
                    valid_from: row.get(4)?,
                    valid_to: row.get(5)?,
                    confidence: row.get(6)?,
                    source_drawer: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn triple_stats(&self) -> Result<TripleStats, DbError> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM triples", [], |row| row.get(0))?;
        let active: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM triples WHERE valid_to IS NULL",
            [],
            |row| row.get(0),
        )?;
        let expired = total - active;
        let entities: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(DISTINCT entity) FROM (
                SELECT subject AS entity FROM triples
                UNION
                SELECT object AS entity FROM triples
            )
            "#,
            [],
            |row| row.get(0),
        )?;
        let mut top_predicates_stmt = self.conn.prepare(
            "SELECT predicate, COUNT(*) as cnt FROM triples GROUP BY predicate ORDER BY cnt DESC LIMIT 5",
        )?;
        let top_predicates = top_predicates_stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(TripleStats {
            total,
            active,
            expired,
            entities,
            top_predicates,
        })
    }

    // --- Tunnels (cross-Wing discovery) ---

    pub fn find_tunnels(&self) -> Result<Vec<(String, Vec<String>)>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT room, GROUP_CONCAT(DISTINCT wing) as wings
            FROM drawers
            WHERE deleted_at IS NULL AND room IS NOT NULL AND room != ''
            GROUP BY room
            HAVING COUNT(DISTINCT wing) > 1
            ORDER BY room
            "#,
        )?;
        let rows = stmt
            .query_map([], |row| {
                let room: String = row.get(0)?;
                let wings_csv: String = row.get(1)?;
                Ok((room, wings_csv))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(room, wings_csv)| {
                let wings = wings_csv.split(',').map(ToOwned::to_owned).collect();
                (room, wings)
            })
            .collect())
    }

    // --- Embedding dimension management ---

    /// Returns the current embedding dimension from the vec0 table, or None if the table is empty.
    pub fn embedding_dim(&self) -> Result<Option<usize>, DbError> {
        // sqlite-vec stores dimension in table schema; probe by checking a row
        let result: std::result::Result<i64, _> = self.conn.query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(dim) => Ok(Some(dim as usize)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(DbError::Sqlite(e)),
        }
    }

    /// Drop and recreate the drawer_vectors table with the specified dimension.
    /// All existing vectors are lost — caller must re-embed after this.
    pub fn recreate_vectors_table(&self, dim: usize) -> Result<(), DbError> {
        let fork_ext_version = db_fork_ext::read_fork_ext_version(&self.conn)?;
        let project_column = if fork_ext_version >= 5 {
            ", +project_id TEXT"
        } else {
            ""
        };
        self.conn.execute_batch(&format!(
            r#"
            DROP TABLE IF EXISTS drawer_vectors;
            CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                id TEXT PRIMARY KEY,
                embedding FLOAT[{dim}]{project_column}
            );
            "#
        ))?;
        Ok(())
    }

    /// Returns all active (non-deleted) drawer IDs and their content for re-embedding.
    pub fn all_active_drawers(&self) -> Result<Vec<(String, String)>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, content FROM drawers WHERE deleted_at IS NULL ORDER BY id")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn database_size_bytes(&self) -> Result<u64, DbError> {
        fs::metadata(&self.path)
            .map(|metadata| metadata.len())
            .map_err(|source| DbError::Metadata {
                path: self.path.clone(),
                source,
            })
    }

    pub fn schema_version(&self) -> Result<u32, DbError> {
        read_user_version(&self.conn)
    }
}

fn apply_migrations(conn: &Connection) -> Result<(), DbError> {
    let current_version = read_user_version(conn)?;
    if current_version > CURRENT_SCHEMA_VERSION {
        return Err(DbError::UnsupportedSchemaVersion {
            current: current_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    for migration in migrations()
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        if migration.version == 5 {
            // ALTER TABLE ADD COLUMN is not idempotent in SQLite, so guard
            // it explicitly. The index in the migration SQL already uses
            // CREATE INDEX IF NOT EXISTS.
            ensure_drawers_content_hash_column(conn)?;
        }
        conn.execute_batch(migration.sql)?;
        if migration.version == 5 {
            // V5 introduces content_hash for indexed dedup; backfill existing
            // rows so the new query path can rely on the column being populated
            // for all live drawers. Batched commits keep the WAL bounded on
            // installs with hundreds of thousands of drawers.
            backfill_content_hash(conn)?;
        }
        set_user_version(conn, migration.version)?;
    }

    Ok(())
}

fn ensure_drawers_content_hash_column(conn: &Connection) -> Result<(), DbError> {
    let exists = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('drawers') WHERE name = 'content_hash'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if exists == 0 {
        conn.execute_batch("ALTER TABLE drawers ADD COLUMN content_hash TEXT;")?;
    }
    Ok(())
}

fn backfill_content_hash(conn: &Connection) -> Result<(), DbError> {
    loop {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let updated = (|| -> Result<usize, DbError> {
            let mut select = conn
                .prepare("SELECT id, content FROM drawers WHERE content_hash IS NULL LIMIT ?1")?;
            let rows = select
                .query_map([CONTENT_HASH_BACKFILL_BATCH as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(select);

            let mut update = conn.prepare("UPDATE drawers SET content_hash = ?1 WHERE id = ?2")?;
            for (id, content) in &rows {
                update.execute(params![content_hash_hex(content), id])?;
            }
            Ok(rows.len())
        })();
        match updated {
            Ok(count) => {
                conn.execute_batch("COMMIT")?;
                if count == 0 {
                    break;
                }
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(err);
            }
        }
    }
    Ok(())
}

fn read_user_version(conn: &Connection) -> Result<u32, DbError> {
    let version = conn.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
    Ok(version)
}

fn set_user_version(conn: &Connection, version: u32) -> Result<(), DbError> {
    conn.execute_batch(&format!("PRAGMA user_version = {version};"))?;
    Ok(())
}

const V2_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN deleted_at TEXT;
CREATE INDEX IF NOT EXISTS idx_drawers_deleted_at ON drawers(deleted_at);
"#;

const V3_MIGRATION_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS drawers_fts USING fts5(
    content,
    content='drawers',
    content_rowid='rowid'
);

-- Populate FTS from existing drawers (excluding soft-deleted)
INSERT INTO drawers_fts(rowid, content)
    SELECT rowid, content FROM drawers WHERE deleted_at IS NULL;

-- Keep FTS in sync: INSERT trigger
CREATE TRIGGER IF NOT EXISTS drawers_ai AFTER INSERT ON drawers BEGIN
    INSERT INTO drawers_fts(rowid, content) VALUES (new.rowid, new.content);
END;

-- Keep FTS in sync: soft-delete (UPDATE deleted_at) removes from FTS
CREATE TRIGGER IF NOT EXISTS drawers_au_softdelete AFTER UPDATE OF deleted_at ON drawers
    WHEN new.deleted_at IS NOT NULL AND old.deleted_at IS NULL BEGIN
    INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', old.rowid, old.content);
END;

-- No DELETE trigger on drawers — soft-deleted rows are already removed from FTS
-- by the UPDATE trigger above. Physical DELETE (purge) skips FTS because the
-- entry is already gone.
"#;

fn migrations() -> &'static [Migration] {
    static MIGRATIONS: &[Migration] = &[
        Migration {
            version: 1,
            sql: V1_SCHEMA_SQL,
        },
        Migration {
            version: 2,
            sql: V2_MIGRATION_SQL,
        },
        Migration {
            version: 3,
            sql: V3_MIGRATION_SQL,
        },
        Migration {
            version: 4,
            sql: V4_MIGRATION_SQL,
        },
        Migration {
            version: 5,
            sql: V5_MIGRATION_SQL,
        },
    ];
    MIGRATIONS
}

const V4_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN importance INTEGER DEFAULT 0;
"#;

const V5_MIGRATION_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_content_hash ON drawers(wing, content_hash);
"#;

struct Migration {
    version: u32,
    sql: &'static str,
}

fn register_sqlite_vec() -> Result<(), DbError> {
    SQLITE_VEC_AUTO_EXTENSION
        .get_or_init(|| unsafe {
            // sqlite-vec exposes a standard SQLite extension init symbol; auto-registration
            // makes vec0 available on every subsequently opened connection in this process.
            let init: rusqlite::auto_extension::RawAutoExtension =
                std::mem::transmute::<*const (), rusqlite::auto_extension::RawAutoExtension>(
                    sqlite_vec::sqlite3_vec_init as *const (),
                );

            rusqlite::auto_extension::register_auto_extension(init)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map(|_| ())
        .map_err(|message| DbError::RegisterVec(message.clone()))
}

fn source_type_as_str(source_type: &SourceType) -> &'static str {
    match source_type {
        SourceType::Project => "project",
        SourceType::Conversation => "conversation",
        SourceType::Manual => "manual",
    }
}

fn source_type_from_str(source_type: &str) -> Result<SourceType, DbError> {
    match source_type {
        "project" => Ok(SourceType::Project),
        "conversation" => Ok(SourceType::Conversation),
        "manual" => Ok(SourceType::Manual),
        other => Err(DbError::InvalidSourceType(other.to_string())),
    }
}

fn parse_keywords(raw: Option<&str>) -> Result<Vec<String>, DbError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let value: Value = serde_json::from_str(raw)?;
    let keywords = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str())
        .map(ToOwned::to_owned)
        .collect();

    Ok(keywords)
}

fn build_fts_match_query(query: &str) -> Option<String> {
    let terms = query
        .split_whitespace()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

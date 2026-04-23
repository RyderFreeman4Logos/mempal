use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{Connection, Row, params};
use serde_json::Value;
use thiserror::Error;

use super::anchor;
use super::types::{
    AnchorKind, Drawer, ExplicitTunnel, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind,
    Provenance, SourceType, TaxonomyEntry, Triple, TripleStats, TunnelEndpoint, TunnelFollowResult,
};
use super::utils::{build_tunnel_id, current_timestamp, format_tunnel_endpoint};

const CURRENT_SCHEMA_VERSION: u32 = 6;
const DRAWER_SELECT_COLUMNS: &str = r#"
    id,
    content,
    wing,
    room,
    source_file,
    source_type,
    added_at,
    chunk_index,
    COALESCE(importance, 0) as importance,
    memory_kind,
    domain,
    field,
    anchor_kind,
    anchor_id,
    parent_anchor_id,
    provenance,
    statement,
    tier,
    status,
    supporting_refs,
    counterexample_refs,
    teaching_refs,
    verification_refs,
    scope_constraints,
    trigger_hints
"#;

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
    #[error("invalid {kind} stored in database: {value}")]
    InvalidEnumValue { kind: &'static str, value: String },
    #[error("invalid drawer metadata: {0}")]
    InvalidDrawerMetadata(String),
    #[error("invalid tunnel: {0}")]
    InvalidTunnel(String),
    #[error("failed to register sqlite-vec auto extension: {0}")]
    RegisterVec(String),
    #[error("database schema version {current} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { current: u32, supported: u32 },
}

pub struct Database {
    conn: Connection,
    path: PathBuf,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        register_sqlite_vec()?;

        let conn = Connection::open(path)?;
        apply_migrations(&conn)?;

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
        anchor::validate_anchor_domain(&drawer.domain, &drawer.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

        self.conn.execute(
            r#"
            INSERT INTO drawers (
                id,
                content,
                wing,
                room,
                source_file,
                source_type,
                added_at,
                chunk_index,
                importance,
                memory_kind,
                domain,
                field,
                anchor_kind,
                anchor_id,
                parent_anchor_id,
                provenance,
                statement,
                tier,
                status,
                supporting_refs,
                counterexample_refs,
                teaching_refs,
                verification_refs,
                scope_constraints,
                trigger_hints
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
            "#,
            params![
                drawer.id.as_str(),
                drawer.content.as_str(),
                drawer.wing.as_str(),
                drawer.room.as_deref(),
                drawer.source_file.as_deref(),
                source_type_as_str(&drawer.source_type),
                drawer.added_at.as_str(),
                drawer.chunk_index,
                drawer.importance,
                memory_kind_as_str(&drawer.memory_kind),
                memory_domain_as_str(&drawer.domain),
                drawer.field.as_str(),
                anchor_kind_as_str(&drawer.anchor_kind),
                drawer.anchor_id.as_str(),
                drawer.parent_anchor_id.as_deref(),
                drawer.provenance.as_ref().map(provenance_as_str),
                drawer.statement.as_deref(),
                drawer.tier.as_ref().map(knowledge_tier_as_str),
                drawer.status.as_ref().map(knowledge_status_as_str),
                encode_json(&drawer.supporting_refs)?,
                encode_json(&drawer.counterexample_refs)?,
                encode_json(&drawer.teaching_refs)?,
                encode_json(&drawer.verification_refs)?,
                drawer.scope_constraints.as_deref(),
                encode_optional_json(drawer.trigger_hints.as_ref())?,
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
        let mut statement = self.conn.prepare(&format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}
            FROM drawers
            WHERE deleted_at IS NULL
            ORDER BY importance DESC, CAST(added_at AS INTEGER) DESC, id DESC
            LIMIT ?1
            "#,
        ))?;
        let rows = statement.query_map([limit], |row| {
            drawer_from_row(row).map_err(row_decode_error)
        })?;

        let mut drawers = Vec::new();
        for row in rows {
            drawers.push(row?);
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

    pub fn insert_vector(&self, drawer_id: &str, vector: &[f32]) -> Result<(), DbError> {
        self.ensure_vectors_table(vector.len())?;
        let vector_json = serde_json::to_string(vector)?;
        self.conn.execute(
            "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, vec_f32(?2))",
            (drawer_id, vector_json.as_str()),
        )?;
        Ok(())
    }

    /// Ensure drawer_vectors table exists with the right dimension.
    /// Creates it on first call; errors on dimension mismatch.
    fn ensure_vectors_table(&self, dim: usize) -> Result<(), DbError> {
        // Check if table exists
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
                [],
                |row| row.get(0),
            )?;

        if !exists {
            self.conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE drawer_vectors USING vec0(id TEXT PRIMARY KEY, embedding FLOAT[{dim}]);"
            ))?;
        }
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
        let mut statement = self.conn.prepare(&format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}
            FROM drawers
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
        ))?;
        let mut rows = statement.query_map([drawer_id], |row| {
            drawer_from_row(row).map_err(row_decode_error)
        })?;

        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
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
        limit: usize,
    ) -> Result<Vec<(String, f64)>, DbError> {
        let Some(match_query) = build_fts_match_query(query) else {
            return Ok(Vec::new());
        };
        let limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let mut stmt = self.conn.prepare(
            r#"
            SELECT d.id, fts.rank
            FROM drawers_fts fts
            JOIN drawers d ON d.rowid = fts.rowid
            WHERE drawers_fts MATCH ?1
              AND d.deleted_at IS NULL
              AND (?2 IS NULL OR d.wing = ?2)
              AND (?3 IS NULL OR d.room = ?3)
            ORDER BY fts.rank
            LIMIT ?4
            "#,
        )?;
        let rows = stmt
            .query_map((match_query.as_str(), wing, room, limit), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
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

    pub fn create_tunnel(
        &self,
        left: &TunnelEndpoint,
        right: &TunnelEndpoint,
        label: &str,
        created_by: Option<&str>,
    ) -> Result<ExplicitTunnel, DbError> {
        let left = normalize_tunnel_endpoint(left)?;
        let right = normalize_tunnel_endpoint(right)?;
        let label = label.trim();
        if label.is_empty() {
            return Err(DbError::InvalidTunnel("label is required".to_string()));
        }
        if left == right {
            return Err(DbError::InvalidTunnel(
                "self-link is not allowed".to_string(),
            ));
        }

        let id = build_tunnel_id(&left, &right);
        let created_at = current_timestamp();
        let created_by = created_by
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        self.conn.execute(
            r#"
            INSERT INTO tunnels (
                id, left_wing, left_room, right_wing, right_room,
                label, created_at, created_by, deleted_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
            ON CONFLICT(id) DO UPDATE SET
                label = CASE
                    WHEN tunnels.deleted_at IS NOT NULL THEN excluded.label
                    ELSE tunnels.label
                END,
                created_at = CASE
                    WHEN tunnels.deleted_at IS NOT NULL THEN excluded.created_at
                    ELSE tunnels.created_at
                END,
                created_by = CASE
                    WHEN tunnels.deleted_at IS NOT NULL THEN excluded.created_by
                    ELSE tunnels.created_by
                END,
                deleted_at = NULL
            "#,
            params![
                id, left.wing, left.room, right.wing, right.room, label, created_at, created_by,
            ],
        )?;

        self.get_explicit_tunnel(&id)?
            .ok_or_else(|| DbError::InvalidTunnel(format!("failed to create tunnel {id}")))
    }

    pub fn list_explicit_tunnels(
        &self,
        wing: Option<&str>,
    ) -> Result<Vec<ExplicitTunnel>, DbError> {
        let wing = wing.map(str::trim).filter(|value| !value.is_empty());
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, left_wing, left_room, right_wing, right_room,
                   label, created_at, created_by, deleted_at
            FROM tunnels
            WHERE deleted_at IS NULL
              AND (?1 IS NULL OR left_wing = ?1 OR right_wing = ?1)
            ORDER BY left_wing, left_room, right_wing, right_room, id
            "#,
        )?;
        let rows = statement
            .query_map([wing], explicit_tunnel_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_explicit_tunnel(&self, tunnel_id: &str) -> Result<bool, DbError> {
        let timestamp = current_timestamp();
        let affected = self.conn.execute(
            "UPDATE tunnels SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![timestamp, tunnel_id],
        )?;
        Ok(affected > 0)
    }

    pub fn follow_explicit_tunnels(
        &self,
        from: &TunnelEndpoint,
        max_hops: u8,
    ) -> Result<Vec<TunnelFollowResult>, DbError> {
        if !(1..=2).contains(&max_hops) {
            return Err(DbError::InvalidTunnel(
                "max_hops must be 1 or 2".to_string(),
            ));
        }

        let from = normalize_tunnel_endpoint(from)?;
        let tunnels = self.list_explicit_tunnels(None)?;
        let mut visited = BTreeSet::from([from.clone()]);
        let mut queue = VecDeque::from([(from, 0_u8)]);
        let mut results = Vec::new();

        while let Some((current, hop)) = queue.pop_front() {
            if hop >= max_hops {
                continue;
            }
            let next_hop = hop + 1;
            for tunnel in &tunnels {
                let neighbor = if tunnel.left == current {
                    Some(tunnel.right.clone())
                } else if tunnel.right == current {
                    Some(tunnel.left.clone())
                } else {
                    None
                };
                let Some(neighbor) = neighbor else {
                    continue;
                };
                if !visited.insert(neighbor.clone()) {
                    continue;
                }
                results.push(TunnelFollowResult {
                    endpoint: neighbor.clone(),
                    via_tunnel_id: tunnel.id.clone(),
                    hop: next_hop,
                });
                queue.push_back((neighbor, next_hop));
            }
        }

        results.sort_by(|left, right| {
            left.hop
                .cmp(&right.hop)
                .then_with(|| left.endpoint.cmp(&right.endpoint))
                .then_with(|| left.via_tunnel_id.cmp(&right.via_tunnel_id))
        });
        Ok(results)
    }

    pub fn explicit_tunnel_hints(
        &self,
        wing: &str,
        room: Option<&str>,
    ) -> Result<Vec<String>, DbError> {
        let endpoint = TunnelEndpoint {
            wing: wing.to_string(),
            room: room.map(ToOwned::to_owned),
        };
        let hints = self
            .follow_explicit_tunnels(&endpoint, 1)?
            .into_iter()
            .map(|result| format_tunnel_endpoint(&result.endpoint))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Ok(hints)
    }

    fn get_explicit_tunnel(&self, tunnel_id: &str) -> Result<Option<ExplicitTunnel>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, left_wing, left_room, right_wing, right_room,
                   label, created_at, created_by, deleted_at
            FROM tunnels
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
        )?;
        let mut rows = statement.query_map([tunnel_id], explicit_tunnel_from_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
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
        self.conn.execute_batch(&format!(
            r#"
            DROP TABLE IF EXISTS drawer_vectors;
            CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                id TEXT PRIMARY KEY,
                embedding FLOAT[{dim}]
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
        apply_migration_atomic(conn, migration)?;
    }

    Ok(())
}

fn apply_migration_atomic(conn: &Connection, migration: &Migration) -> Result<(), DbError> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        conn.execute_batch(migration.sql)?;
        set_user_version(conn, migration.version)?;
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
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

fn normalize_tunnel_endpoint(endpoint: &TunnelEndpoint) -> Result<TunnelEndpoint, DbError> {
    let wing = endpoint.wing.trim();
    if wing.is_empty() {
        return Err(DbError::InvalidTunnel(
            "endpoint wing is required".to_string(),
        ));
    }
    let room = endpoint
        .room
        .as_deref()
        .map(str::trim)
        .filter(|room| !room.is_empty())
        .map(ToOwned::to_owned);
    Ok(TunnelEndpoint {
        wing: wing.to_string(),
        room,
    })
}

fn explicit_tunnel_from_row(row: &Row<'_>) -> rusqlite::Result<ExplicitTunnel> {
    Ok(ExplicitTunnel {
        id: row.get(0)?,
        left: TunnelEndpoint {
            wing: row.get(1)?,
            room: row.get(2)?,
        },
        right: TunnelEndpoint {
            wing: row.get(3)?,
            room: row.get(4)?,
        },
        label: row.get(5)?,
        created_at: row.get(6)?,
        created_by: row.get(7)?,
        deleted_at: row.get(8)?,
    })
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

const V4_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN importance INTEGER DEFAULT 0;
"#;

const V5_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge')) DEFAULT 'evidence';
ALTER TABLE drawers ADD COLUMN domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')) DEFAULT 'project';
ALTER TABLE drawers ADD COLUMN field TEXT NOT NULL DEFAULT 'general';
ALTER TABLE drawers ADD COLUMN anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('global', 'repo', 'worktree')) DEFAULT 'repo';
ALTER TABLE drawers ADD COLUMN anchor_id TEXT NOT NULL DEFAULT 'repo://legacy';
ALTER TABLE drawers ADD COLUMN parent_anchor_id TEXT;
ALTER TABLE drawers ADD COLUMN provenance TEXT CHECK(provenance IN ('runtime', 'research', 'human'));
ALTER TABLE drawers ADD COLUMN statement TEXT;
ALTER TABLE drawers ADD COLUMN tier TEXT CHECK(tier IN ('qi', 'shu', 'dao_ren', 'dao_tian'));
ALTER TABLE drawers ADD COLUMN status TEXT CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired'));
ALTER TABLE drawers ADD COLUMN supporting_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN counterexample_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN teaching_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN verification_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN scope_constraints TEXT;
ALTER TABLE drawers ADD COLUMN trigger_hints TEXT;

UPDATE drawers
SET memory_kind = 'evidence',
    domain = 'project',
    field = 'general',
    anchor_kind = 'repo',
    anchor_id = 'repo://legacy',
    parent_anchor_id = NULL,
    provenance = CASE source_type
        WHEN 'project' THEN 'research'
        WHEN 'conversation' THEN 'human'
        WHEN 'manual' THEN 'human'
        ELSE NULL
    END
WHERE memory_kind = 'evidence'
  AND domain = 'project'
  AND field = 'general'
  AND anchor_kind = 'repo'
  AND anchor_id = 'repo://legacy'
  AND parent_anchor_id IS NULL
  AND provenance IS NULL;
"#;

const V6_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS tunnels (
    id TEXT PRIMARY KEY,
    left_wing TEXT NOT NULL,
    left_room TEXT,
    right_wing TEXT NOT NULL,
    right_room TEXT,
    label TEXT NOT NULL,
    created_at TEXT NOT NULL,
    created_by TEXT,
    deleted_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_tunnels_left
    ON tunnels(left_wing, left_room)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_tunnels_right
    ON tunnels(right_wing, right_room)
    WHERE deleted_at IS NULL;
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
        Migration {
            version: 6,
            sql: V6_MIGRATION_SQL,
        },
    ];
    MIGRATIONS
}

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

fn memory_kind_as_str(memory_kind: &MemoryKind) -> &'static str {
    match memory_kind {
        MemoryKind::Evidence => "evidence",
        MemoryKind::Knowledge => "knowledge",
    }
}

fn memory_kind_from_str(memory_kind: &str) -> Result<MemoryKind, DbError> {
    match memory_kind {
        "evidence" => Ok(MemoryKind::Evidence),
        "knowledge" => Ok(MemoryKind::Knowledge),
        other => Err(DbError::InvalidEnumValue {
            kind: "memory_kind",
            value: other.to_string(),
        }),
    }
}

fn memory_domain_as_str(domain: &MemoryDomain) -> &'static str {
    match domain {
        MemoryDomain::Project => "project",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn memory_domain_from_str(domain: &str) -> Result<MemoryDomain, DbError> {
    match domain {
        "project" => Ok(MemoryDomain::Project),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => Err(DbError::InvalidEnumValue {
            kind: "domain",
            value: other.to_string(),
        }),
    }
}

fn anchor_kind_as_str(anchor_kind: &AnchorKind) -> &'static str {
    match anchor_kind {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}

fn anchor_kind_from_str(anchor_kind: &str) -> Result<AnchorKind, DbError> {
    match anchor_kind {
        "global" => Ok(AnchorKind::Global),
        "repo" => Ok(AnchorKind::Repo),
        "worktree" => Ok(AnchorKind::Worktree),
        other => Err(DbError::InvalidEnumValue {
            kind: "anchor_kind",
            value: other.to_string(),
        }),
    }
}

fn provenance_as_str(provenance: &Provenance) -> &'static str {
    match provenance {
        Provenance::Runtime => "runtime",
        Provenance::Research => "research",
        Provenance::Human => "human",
    }
}

fn provenance_from_str(provenance: &str) -> Result<Provenance, DbError> {
    match provenance {
        "runtime" => Ok(Provenance::Runtime),
        "research" => Ok(Provenance::Research),
        "human" => Ok(Provenance::Human),
        other => Err(DbError::InvalidEnumValue {
            kind: "provenance",
            value: other.to_string(),
        }),
    }
}

fn knowledge_tier_as_str(tier: &KnowledgeTier) -> &'static str {
    match tier {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

fn knowledge_tier_from_str(tier: &str) -> Result<KnowledgeTier, DbError> {
    match tier {
        "qi" => Ok(KnowledgeTier::Qi),
        "shu" => Ok(KnowledgeTier::Shu),
        "dao_ren" => Ok(KnowledgeTier::DaoRen),
        "dao_tian" => Ok(KnowledgeTier::DaoTian),
        other => Err(DbError::InvalidEnumValue {
            kind: "tier",
            value: other.to_string(),
        }),
    }
}

fn knowledge_status_as_str(status: &KnowledgeStatus) -> &'static str {
    match status {
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

fn knowledge_status_from_str(status: &str) -> Result<KnowledgeStatus, DbError> {
    match status {
        "candidate" => Ok(KnowledgeStatus::Candidate),
        "promoted" => Ok(KnowledgeStatus::Promoted),
        "canonical" => Ok(KnowledgeStatus::Canonical),
        "demoted" => Ok(KnowledgeStatus::Demoted),
        "retired" => Ok(KnowledgeStatus::Retired),
        other => Err(DbError::InvalidEnumValue {
            kind: "status",
            value: other.to_string(),
        }),
    }
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, DbError> {
    Ok(serde_json::to_string(value)?)
}

fn encode_optional_json<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>, DbError> {
    value.map(encode_json).transpose()
}

fn parse_string_list(raw: Option<&str>) -> Result<Vec<String>, DbError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    Ok(serde_json::from_str::<Vec<String>>(raw)?)
}

fn parse_optional_json<T>(raw: Option<&str>) -> Result<Option<T>, DbError>
where
    T: serde::de::DeserializeOwned,
{
    raw.map(serde_json::from_str)
        .transpose()
        .map_err(DbError::from)
}

fn row_decode_error(error: DbError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn drawer_from_row(row: &Row<'_>) -> Result<Drawer, DbError> {
    let source_type = source_type_from_str(&row.get::<_, String>(5)?)?;
    let memory_kind = memory_kind_from_str(&row.get::<_, String>(9)?)?;
    let domain = memory_domain_from_str(&row.get::<_, String>(10)?)?;
    let field = row.get::<_, String>(11)?;
    let anchor_kind = anchor_kind_from_str(&row.get::<_, String>(12)?)?;
    let anchor_id = row.get::<_, String>(13)?;
    let parent_anchor_id = row.get::<_, Option<String>>(14)?;
    let provenance = row
        .get::<_, Option<String>>(15)?
        .as_deref()
        .map(provenance_from_str)
        .transpose()?;
    let statement = row.get::<_, Option<String>>(16)?;
    let tier = row
        .get::<_, Option<String>>(17)?
        .as_deref()
        .map(knowledge_tier_from_str)
        .transpose()?;
    let status = row
        .get::<_, Option<String>>(18)?
        .as_deref()
        .map(knowledge_status_from_str)
        .transpose()?;
    let supporting_refs = parse_string_list(row.get::<_, Option<String>>(19)?.as_deref())?;
    let counterexample_refs = parse_string_list(row.get::<_, Option<String>>(20)?.as_deref())?;
    let teaching_refs = parse_string_list(row.get::<_, Option<String>>(21)?.as_deref())?;
    let verification_refs = parse_string_list(row.get::<_, Option<String>>(22)?.as_deref())?;
    let scope_constraints = row.get::<_, Option<String>>(23)?;
    let trigger_hints = parse_optional_json(row.get::<_, Option<String>>(24)?.as_deref())?;

    anchor::validate_anchor_domain(&domain, &anchor_kind)
        .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

    Ok(Drawer {
        id: row.get(0)?,
        content: row.get(1)?,
        wing: row.get(2)?,
        room: row.get(3)?,
        source_file: row.get(4)?,
        source_type,
        added_at: row.get(6)?,
        chunk_index: row.get(7)?,
        importance: row.get(8)?,
        memory_kind,
        domain,
        field,
        anchor_kind,
        anchor_id,
        parent_anchor_id,
        provenance,
        statement,
        tier,
        status,
        supporting_refs,
        counterexample_refs,
        teaching_refs,
        verification_refs,
        scope_constraints,
        trigger_hints,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_migration_rolls_back_partial_schema_changes() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            r#"
            CREATE TABLE drawers (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL
            );
            PRAGMA user_version = 4;
            "#,
        )
        .expect("create base schema");

        let migration = Migration {
            version: 5,
            sql: r#"
            ALTER TABLE drawers ADD COLUMN memory_kind TEXT;
            ALTER TABLE missing_table ADD COLUMN nope TEXT;
            "#,
        };

        let error = apply_migration_atomic(&conn, &migration).expect_err("migration should fail");
        assert!(
            matches!(error, DbError::Sqlite(_)),
            "unexpected error: {error:?}"
        );
        assert_eq!(read_user_version(&conn).expect("user_version"), 4);

        let mut stmt = conn
            .prepare("PRAGMA table_info(drawers)")
            .expect("table_info");
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect columns");

        assert!(
            !columns.iter().any(|column| column == "memory_kind"),
            "failed migration must not leave partial columns behind"
        );
    }
}

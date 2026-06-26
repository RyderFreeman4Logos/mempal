#[rustfmt::skip] #[path = "db_fork_ext.rs"] mod db_fork_ext;
// harness-point: PR0 — re-export MigrationHook trait + hooked migration runner for tests
pub use db_fork_ext::{
    CURRENT_FORK_EXT_VERSION, FORK_EXT_META_DDL, FORK_EXT_V1_SCHEMA_SQL, FORK_EXT_V2_SCHEMA_SQL,
    FORK_EXT_V3_SCHEMA_SQL, FORK_EXT_V13_SCHEMA_SQL, FORK_EXT_V14_SCHEMA_SQL, MigrationHook,
    apply_fork_ext_migrations, apply_fork_ext_migrations_to, apply_fork_ext_migrations_with_hook,
    read_fork_ext_version, set_fork_ext_version,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, functions::FunctionFlags, params,
    params_from_iter, types::Value as SqlValue,
};
use serde_json::Value;
use thiserror::Error;

use super::anchor;
use super::{
    project::{ProjectFilterMode, ProjectSearchScope},
    types::{
        AnchorKind, ChunkNeighbors, CompactionStrategy, ConsolidationStats, Drawer, DrawerDetails,
        DrawerSummary, DrawerVectorDetails, ExplicitTunnel, KnowledgeCard, KnowledgeCardEvent,
        KnowledgeCardFilter, KnowledgeEventType, KnowledgeEvidenceLink, KnowledgeEvidenceRole,
        KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, NeighborChunk, Provenance,
        ReindexSource, RuntimeAdoptionEvent, RuntimeAdoptionFilter, RuntimeAdoptionSignal,
        RuntimeAdoptionTrack, RuntimeWriterLease, SleepStats, SourceType, TaxonomyEntry, Triple,
        TripleStats, TunnelDrawer, TunnelEndpoint, TunnelFollowResult,
    },
    utils::{
        build_drawer_id, build_scoped_drawer_id, build_tunnel_id, current_timestamp,
        format_tunnel_endpoint,
    },
};
use crate::ingest::gating::GatingDecision;
use crate::ingest::novelty::NoveltyAction;

pub const CURRENT_SCHEMA_VERSION: u32 = 20;
pub const CURRENT_VECTOR_INDEX_VERSION: &str = "v2";
pub const VECTOR_DISTANCE_METRIC: &str = "cosine";
/// Default SQLite page cache budget for normal CLI/daemon/MCP connections.
///
/// Negative `PRAGMA cache_size` values are KiB. Keep the default small because
/// read-only status/search probes can leave page-cache memory resident in
/// long-lived daemon and MCP processes (#525). High-throughput maintenance
/// paths that need a larger cache must opt in explicitly.
pub(crate) const SQLITE_CACHE_SIZE_KIB_DEFAULT: i64 = -16_384;
/// SQLite page cache budget for issue #311's large-DB stale reindex path.
///
/// Negative `PRAGMA cache_size` values are KiB, so `-262144` is 256 MiB.
/// That is enough to avoid the default 2 MiB cache thrash on ~10 GiB stores,
/// while staying far below the 4 GiB peak-memory cap. The reindex speedup is
/// still the O(n) snapshot work-list; do not replace it with multi-GiB cache or
/// `mmap_size`.
pub(crate) const SQLITE_CACHE_SIZE_KIB_256_MIB: i64 = -262_144;
const GATING_DROP_TOTAL_KEY: &str = "gating.dropped.total";
const AUDIT_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

const CONTENT_HASH_BACKFILL_BATCH: usize = 1_000;

fn content_hash_hex(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

fn runtime_writer_session_id(name: &str, owner: &str) -> String {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(name.as_bytes());
    hasher.update(b"\0");
    hasher.update(owner.as_bytes());
    hasher.update(b"\0");
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(now_nanos.to_string().as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

#[cfg(target_os = "linux")]
fn runtime_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "linux"))]
fn runtime_boot_id() -> Option<String> {
    None
}

#[cfg(target_os = "linux")]
fn runtime_writer_process_is_live_holder(pid: u32, boot_id: Option<&str>) -> bool {
    if pid == 0 {
        return false;
    }
    let Some(expected_boot_id) = boot_id else {
        return false;
    };
    if runtime_boot_id().as_deref() != Some(expected_boot_id) {
        return false;
    }

    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    if !proc_dir.exists() {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }

    let exe_matches = fs::read_link(proc_dir.join("exe"))
        .ok()
        .as_deref()
        .is_some_and(runtime_writer_path_is_mempal_holder);
    if exe_matches {
        return true;
    }

    fs::read(proc_dir.join("cmdline"))
        .ok()
        .and_then(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .find(|part| !part.is_empty())
                .map(|part| PathBuf::from(String::from_utf8_lossy(part).into_owned()))
        })
        .as_deref()
        .is_some_and(runtime_writer_path_is_mempal_holder)
}

#[cfg(not(target_os = "linux"))]
fn runtime_writer_process_is_live_holder(_pid: u32, _boot_id: Option<&str>) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn runtime_writer_path_is_mempal_holder(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "mempal")
}

fn truncate_preview(content: &str, max_chars: usize) -> String {
    let mut chars = content.chars();
    let mut preview = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn truncate_to_char_budget(content: &str, max_chars: usize) -> String {
    content.chars().take(max_chars).collect()
}

const DRAWER_SELECT_COLUMNS: &str = r#"
    id,
    content,
    wing,
    room,
    source_file,
    source_type,
    confidence,
    added_at,
    chunk_index,
    normalize_version,
    COALESCE(importance, 0) as importance,
    memory_kind,
    domain,
    COALESCE(field, 'general') as field,
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
    COALESCE(is_pinned, 0) as is_pinned,
    pin_order,
    supersedes,
    -- Treat a persisted 0.0 as "not computed" and fall back to base importance.
    -- COALESCE alone substitutes only on NULL, but ingest can land a literal 0.0
    -- that would otherwise sink the drawer below every ei>0 row in the importance
    -- rerank, making it unrecallable (GitHub #309). NULLIF(...,0.0) maps the 0.0
    -- sentinel to NULL so the fallback fires. The fallback term carries the persisted
    -- stale penalty (COALESCE(stale_penalty_applied, 1.0); default 1.0 for a
    -- never-penalized row) so a legacy 0.0 row that fact-check down-ranked still ranks
    -- at importance*penalty, not full importance — preserving the P13 stale-fact
    -- contract without re-burying never-penalized rows. A drawer that legitimately
    -- decays to exactly 0.0 also falls back here — an acceptable safe floor.
    COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)) as effective_importance,
    compacted_into
"#;

const V1_SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS drawers (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    wing TEXT NOT NULL,
    room TEXT,
    source_file TEXT,
    source_type TEXT NOT NULL DEFAULT 'system_generated' CHECK(source_type IN ('user_explicit', 'agent_observation', 'agent_inference', 'system_generated')),
    confidence REAL NOT NULL DEFAULT 0.5,
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

pub(crate) fn ensure_wal_journal_mode(conn: &Connection) -> rusqlite::Result<()> {
    let mode = conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    Ok(())
}

pub fn rusqlite_error_is_lock(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite, _)
            if matches!(
                sqlite.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
                || matches!(
                    sqlite.extended_code & 0xff,
                    rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED
                )
    )
}

pub fn db_error_is_sqlite_lock(error: &DbError) -> bool {
    matches!(error, DbError::Sqlite(sqlite) if rusqlite_error_is_lock(sqlite))
}

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
    #[error("failed to open audit log {path}")]
    AuditOpen {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write audit log {path}")]
    AuditWrite {
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
    #[error(
        "database schema version {current} is newer than supported version {supported}; update the mempal binary that opens this database (for example, run `cargo install mempal` or reinstall from this source checkout). If this error comes from an MCP server, check the MCP client configuration and ensure its command/path points at the updated mempal binary."
    )]
    UnsupportedSchemaVersion { current: u32, supported: u32 },
    #[error("supersedes and replace_text are mutually exclusive")]
    ReplacementTargetConflict,
    #[error("superseded drawer {drawer_id} was not found or is already deleted")]
    SupersededDrawerNotFound { drawer_id: String },
    #[error(
        "superseded drawer {drawer_id} belongs to project scope {actual:?}, expected {expected:?}"
    )]
    SupersededDrawerProjectMismatch {
        drawer_id: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    #[error("no matching active fact found for replace_text")]
    ReplacementTextNotFound,
    #[error("multiple matching active facts found for replace_text; candidates: {candidate_ids:?}")]
    ReplacementTextAmbiguous { candidate_ids: Vec<String> },
    #[error("compaction cluster is empty")]
    CompactionClusterEmpty,
    #[error("compaction drawer {drawer_id} was not found, inactive, or already compacted")]
    CompactionDrawerNotFound { drawer_id: String },
    #[error(
        "drawer {drawer_id} changed during novelty merge: expected merge_count {expected_merge_count}"
    )]
    DrawerMergeConflict {
        drawer_id: String,
        expected_merge_count: u32,
    },
    #[error("LLM compaction not yet implemented")]
    LlmCompactionNotImplemented,
    #[error("off-runtime database task failed to complete: {0}")]
    BlockingTaskFailed(String),
    #[error(
        "read-pool cache budget exceeded: {conns} connections request {requested_mib} MiB of page cache, over the {budget_mib} MiB cap (issue #311)"
    )]
    PoolCacheBudgetExceeded {
        conns: usize,
        requested_mib: i64,
        budget_mib: i64,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FtsMetadataFilters<'a> {
    pub memory_kind: Option<&'a str>,
    pub domain: Option<&'a str>,
    pub field: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub status: Option<&'a str>,
    pub anchor_kind: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct FtsSearchScope<'a> {
    pub wing: Option<&'a str>,
    pub room: Option<&'a str>,
    pub project_mode: &'a str,
    pub project_id: Option<&'a str>,
    pub filters: FtsMetadataFilters<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ReindexSourceScopeSummary {
    pub(crate) drawer_count: u64,
    pub(crate) source_count: u64,
}

pub struct Database {
    conn: Connection,
    path: PathBuf,
}

/// Novelty audit row to insert with a database-side mutation.
#[derive(Debug, Clone, Copy)]
pub struct NoveltyAuditInsert<'a> {
    pub candidate_hash: &'a str,
    pub action: NoveltyAction,
    pub near_drawer_id: Option<&'a str>,
    pub cosine: Option<f32>,
    pub audit_decision: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

struct DrawerMergeUpdate<'a> {
    drawer_id: &'a str,
    merged_content: &'a str,
    updated_at: &'a str,
    content_hash: &'a str,
    vector_json: &'a str,
    vector_len: usize,
    expected_merge_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatingDropCounts {
    pub total: Option<u64>,
    pub by_reason: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalRejudgeAudit<'a> {
    pub drawer_id: &'a str,
    pub decision: &'a str,
    pub tier: u8,
    pub reason: Option<&'a str>,
    pub label: Option<&'a str>,
    pub score: Option<f64>,
    pub project_id: Option<&'a str>,
    pub content_preview: Option<&'a str>,
    pub judge_model: Option<&'a str>,
    pub config_version: &'a str,
    pub mutation: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoricalRejudgeCandidate {
    pub rowid: i64,
    pub drawer: Drawer,
    pub project_id: Option<String>,
}

#[derive(Clone, Copy)]
enum OpenMode {
    ReadOnly,
    QueryOnly,
    ReadWrite,
}

impl OpenMode {
    fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

fn register_math_functions(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "EXP",
        1,
        FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx: &rusqlite::functions::Context<'_>| {
            let x: f64 = ctx.get(0)?;
            Ok(x.exp())
        },
    )?;
    Ok(())
}

/// Name of the transient table that holds the pre-recreate `drawer_vectors`
/// rows during a metric/dim-change reindex (issue #302). It is a connection
/// `TEMP` table, so a process crash mid-reindex drops it automatically and
/// leaves no leftover artifact in the database file.
const REINDEX_VECTOR_STASH_TABLE: &str = "drawer_vectors_reindex_stash";

/// A snapshot of the `drawer_vectors` rows captured immediately before a
/// destructive reindex recreate, used to roll the table back if the reindex
/// embeds ZERO vectors (a total embedder outage) instead of leaving an empty
/// index behind (issue #302).
///
/// sqlite-vec 0.1.9's `vec0` virtual table does not implement `xRename`, so
/// `ALTER TABLE ... RENAME` is rejected (upstream asg017/sqlite-vec#43). The
/// textbook strategy-A atomic swap (embed into a staging table, then rename it
/// over the original) is therefore not achievable directly; instead the same
/// temp-table copy primitive `db_fork_ext` already uses for the `project_id`
/// migration stages the OLD rows aside so they can be restored on failure.
#[derive(Debug, Clone)]
pub struct ReindexVectorStash {
    old_dim: usize,
    old_metric: String,
    had_project_id: bool,
    row_count: i64,
}

impl ReindexVectorStash {
    /// Number of rows preserved in the stash (the pre-recreate vector count).
    pub fn row_count(&self) -> i64 {
        self.row_count
    }
}

/// Whitelist a vec0 distance metric before interpolating it into DDL. The value
/// provably originates from [`Database::vector_table_distance_metric`] (only
/// ever `cosine` or `l2`), so this is defense-in-depth against a corrupted
/// stored schema rather than untrusted input.
fn validate_vector_metric(metric: &str) -> Result<&str, DbError> {
    match metric {
        "cosine" | "l2" => Ok(metric),
        other => Err(DbError::InvalidDrawerMetadata(format!(
            "unexpected vec0 distance metric: {other}"
        ))),
    }
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadWrite)
    }

    /// Open a read-write database connection with a caller-selected SQLite busy timeout.
    pub fn open_with_busy_timeout(path: &Path, busy_timeout: Duration) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, OpenMode::ReadWrite, busy_timeout)
    }

    pub fn open_read_only(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::ReadOnly)
    }

    /// Open a non-mutating connection for read paths that must not run startup
    /// writes such as WAL mode changes or migrations.
    pub fn open_query_only(path: &Path) -> Result<Self, DbError> {
        Self::open_with_mode(path, OpenMode::QueryOnly)
    }

    fn open_with_mode(path: &Path, mode: OpenMode) -> Result<Self, DbError> {
        Self::open_with_mode_and_busy_timeout(path, mode, Duration::from_secs(5))
    }

    fn open_with_mode_and_busy_timeout(
        path: &Path,
        mode: OpenMode,
        busy_timeout: Duration,
    ) -> Result<Self, DbError> {
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
            OpenMode::QueryOnly => {
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?
            }
            OpenMode::ReadWrite => Connection::open(path)?,
        };
        conn.busy_timeout(busy_timeout)?;
        conn.pragma_update(None, "cache_size", SQLITE_CACHE_SIZE_KIB_DEFAULT)?;
        register_math_functions(&conn)?;
        if matches!(mode, OpenMode::QueryOnly) {
            conn.pragma_update(None, "query_only", "ON")?;
        }
        if mode.allows_write() {
            ensure_wal_journal_mode(&conn)?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.execute_batch("PRAGMA foreign_keys = ON;")?;
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
        self.insert_drawer_with_project_validity(drawer, project_id, None, None, None)
    }

    pub fn insert_drawer_with_project_validity(
        &self,
        drawer: &Drawer,
        project_id: Option<&str>,
        source_root: Option<&str>,
        valid_from: Option<&str>,
        valid_until: Option<&str>,
    ) -> Result<(), DbError> {
        let content_hash = content_hash_hex(&drawer.content);
        anchor::validate_anchor_domain(&drawer.domain, &drawer.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

        // Persist effective_importance explicitly so a new drawer is never
        // stranded at the column DEFAULT 0.0 — a literal 0.0 sinks below every
        // ei>0 row in the importance rerank and is effectively unrecallable
        // (GitHub #309). Seed from base importance when the constructor left the
        // field at the 0.0 sentinel, mirroring the read-time NULLIF(...,0.0) guard.
        let seeded_effective_importance = if drawer.effective_importance == 0.0 {
            f64::from(drawer.importance)
        } else {
            drawer.effective_importance
        };

        self.conn.execute(
            r#"
            INSERT OR IGNORE INTO drawers (
                id,
                content,
                wing,
                room,
                source_file,
                source_root,
                source_type,
                confidence,
                added_at,
                chunk_index,
                normalize_version,
                importance,
                project_id,
                content_hash,
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
                is_pinned,
                pin_order,
                supersedes,
                valid_from,
                valid_until,
                effective_importance
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)
            "#,
            params![
                drawer.id.as_str(),
                drawer.content.as_str(),
                drawer.wing.as_str(),
                drawer.room.as_deref(),
                drawer.source_file.as_deref(),
                source_root,
                source_type_as_str(&drawer.source_type),
                drawer.confidence,
                drawer.added_at.as_str(),
                drawer.chunk_index,
                i64::from(drawer.normalize_version),
                drawer.importance,
                project_id,
                content_hash,
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
                drawer.is_pinned,
                drawer.pin_order,
                drawer.supersedes.as_deref(),
                valid_from.unwrap_or(drawer.added_at.as_str()),
                valid_until,
                seeded_effective_importance,
            ],
        )?;

        if self.table_exists("drawers_fts")? {
            let rowid: i64 = self.conn.query_row(
                "SELECT rowid FROM drawers WHERE id = ?1",
                [drawer.id.as_str()],
                |row| row.get(0),
            )?;
            let tokenized = fts_tokenize_content(&drawer.content);
            self.conn.execute(
                "INSERT INTO drawers_fts(rowid, content) VALUES (?1, ?2)",
                params![rowid, tokenized],
            )?;
        }

        Ok(())
    }

    pub fn record_gating_audit(
        &self,
        candidate_hash: &str,
        decision: &GatingDecision,
        project_id: Option<&str>,
        content: Option<&str>,
    ) -> Result<(), DbError> {
        let explain_json = serde_json::to_string(decision)?;
        let created_at = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let retained_until = created_at + AUDIT_RETENTION_SECS;
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
        let content_preview = decision
            .is_rejected()
            .then(|| content.map(|text| truncate_preview(text, 500)))
            .flatten();
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
                    project_id,
                    content_preview
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
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
                    content_preview.as_deref(),
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

    pub fn record_embed_failure(
        &self,
        error_message: &str,
        endpoint: Option<&str>,
        consecutive_failures: u64,
        duration_ms: Option<u64>,
    ) -> Result<(), DbError> {
        let timestamp = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let retained_until = timestamp + AUDIT_RETENTION_SECS;
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let id_seed = format!("{timestamp}:{unique_nanos}:{consecutive_failures}:{error_message}");
        let id = format!(
            "embed_failure_{}",
            blake3::hash(id_seed.as_bytes()).to_hex()
        );
        self.conn.execute(
            r#"
            INSERT INTO embed_failure_log (
                id,
                timestamp,
                error_message,
                endpoint,
                consecutive_failures,
                duration_ms,
                retained_until
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                id,
                timestamp,
                error_message,
                endpoint,
                i64::try_from(consecutive_failures).unwrap_or(i64::MAX),
                duration_ms.map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
                retained_until,
            ],
        )?;
        Ok(())
    }

    pub fn prune_expired_audit_logs(&self) -> Result<(), DbError> {
        let now = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        self.conn.execute(
            "DELETE FROM gating_audit WHERE retained_until < ?1",
            params![now],
        )?;
        self.conn.execute(
            "DELETE FROM embed_failure_log WHERE retained_until < ?1",
            params![now],
        )?;
        Ok(())
    }

    pub fn upsert_llm_verdict(
        &self,
        drawer_id: &str,
        verdict: &str,
        score: Option<f64>,
    ) -> Result<(), DbError> {
        // Patch the LLM verdict columns on the existing gating_audit row for this drawer.
        // The row may carry label='llm_pending' (MCP path) or the original Tier 2 label
        // (daemon path), so we match only on drawer_id.
        self.conn.execute(
            r#"
            UPDATE gating_audit
            SET llm_verdict = ?1, llm_score = ?2
            WHERE drawer_id = ?3
            "#,
            params![verdict, score, drawer_id],
        )?;
        Ok(())
    }

    pub fn upsert_llm_verdict_by_candidate_hash(
        &self,
        candidate_hash: &str,
        verdict: &str,
        score: Option<f64>,
    ) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            UPDATE gating_audit
            SET llm_verdict = ?1, llm_score = ?2
            WHERE candidate_hash = ?3
            "#,
            params![verdict, score, candidate_hash],
        )?;
        Ok(())
    }

    pub fn record_historical_rejudge_audit(
        &self,
        audit: HistoricalRejudgeAudit<'_>,
    ) -> Result<(), DbError> {
        let created_at = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let retained_until = created_at + AUDIT_RETENTION_SECS;
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let explain_json = serde_json::json!({
            "workflow": "historical_rejudge",
            "decision": audit.decision,
            "reason": audit.reason,
            "label": audit.label,
            "score": audit.score,
            "judge_model": audit.judge_model,
            "config_version": audit.config_version,
            "mutation": audit.mutation,
        });
        let explain_json = serde_json::to_string(&explain_json)?;
        let id_seed = format!(
            "historical_rejudge:{}:{created_at}:{unique_nanos}:{}",
            audit.drawer_id, explain_json
        );
        let id = format!("gating_{}", blake3::hash(id_seed.as_bytes()).to_hex());
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
                project_id,
                content_preview,
                llm_verdict,
                llm_score
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            "#,
            params![
                id,
                audit.drawer_id,
                audit.drawer_id,
                audit.decision,
                i64::from(audit.tier),
                audit.label,
                audit.reason,
                audit.score,
                explain_json,
                retained_until,
                created_at,
                audit.project_id,
                audit.content_preview,
                audit
                    .judge_model
                    .filter(|model| *model != "deterministic")
                    .map(|_| audit.decision),
                audit
                    .judge_model
                    .filter(|model| *model != "deterministic")
                    .and(audit.score),
            ],
        )?;
        Ok(())
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
        &self,
        drawer_id: &str,
        merged_content: &str,
        updated_at: &str,
        vector: &[f32],
        expected_merge_count: u32,
    ) -> Result<(), DbError> {
        self.ensure_vectors_table(vector.len())?;
        let vector_json = serde_json::to_string(vector)?;
        let content_hash = content_hash_hex(merged_content);
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), DbError> {
            self.apply_drawer_merge_update(DrawerMergeUpdate {
                drawer_id,
                merged_content,
                updated_at,
                content_hash: &content_hash,
                vector_json: &vector_json,
                vector_len: vector.len(),
                expected_merge_count,
            })?;
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

    pub fn update_drawer_after_merge_and_record_novelty_audit(
        &self,
        drawer_id: &str,
        merged_content: &str,
        updated_at: &str,
        vector: &[f32],
        expected_merge_count: u32,
        audit: NoveltyAuditInsert<'_>,
    ) -> Result<(), DbError> {
        self.ensure_vectors_table(vector.len())?;
        let vector_json = serde_json::to_string(vector)?;
        let content_hash = content_hash_hex(merged_content);
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), DbError> {
            self.apply_drawer_merge_update(DrawerMergeUpdate {
                drawer_id,
                merged_content,
                updated_at,
                content_hash: &content_hash,
                vector_json: &vector_json,
                vector_len: vector.len(),
                expected_merge_count,
            })?;
            self.insert_novelty_audit_row(audit)?;
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

    fn apply_drawer_merge_update(&self, update: DrawerMergeUpdate<'_>) -> Result<(), DbError> {
        let updated = self.conn.execute(
            r#"
            UPDATE drawers
            SET content = ?2,
                updated_at = ?3,
                content_hash = ?4,
                merge_count = COALESCE(merge_count, 0) + 1
            WHERE id = ?1
              AND deleted_at IS NULL
              AND COALESCE(merge_count, 0) = ?5
            "#,
            params![
                update.drawer_id,
                update.merged_content,
                update.updated_at,
                update.content_hash,
                update.expected_merge_count
            ],
        )?;
        if updated == 0 {
            return Err(DbError::DrawerMergeConflict {
                drawer_id: update.drawer_id.to_string(),
                expected_merge_count: update.expected_merge_count,
            });
        }
        self.conn.execute(
            "DELETE FROM drawer_vectors WHERE id = ?1",
            [update.drawer_id],
        )?;
        let project_id = self.conn.query_row(
            "SELECT project_id FROM drawers WHERE id = ?1",
            [update.drawer_id],
            |row| row.get::<_, Option<String>>(0),
        )?;
        self.conn.execute(
            "INSERT INTO drawer_vectors (id, embedding, project_id) VALUES (?1, vec_f32(?2), ?3)",
            params![update.drawer_id, update.vector_json, project_id],
        )?;
        self.record_current_vector_metadata(update.drawer_id, update.vector_len)?;
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
        self.insert_novelty_audit_row(NoveltyAuditInsert {
            candidate_hash,
            action,
            near_drawer_id,
            cosine,
            audit_decision,
            project_id,
        })
    }

    fn insert_novelty_audit_row(&self, audit: NoveltyAuditInsert<'_>) -> Result<(), DbError> {
        let created_at = super::utils::current_timestamp()
            .parse::<i64>()
            .unwrap_or_default();
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let decision = audit
            .audit_decision
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| audit.action.as_str().to_string());
        let id_seed = format!(
            "{}:{created_at}:{unique_nanos}:{decision}:{}:{}",
            audit.candidate_hash,
            audit.near_drawer_id.unwrap_or_default(),
            audit.cosine.unwrap_or_default()
        );
        let id = format!("novelty_{}", blake3::hash(id_seed.as_bytes()).to_hex());
        self.conn.execute(
            r#"
            INSERT INTO novelty_audit (id, candidate_hash, decision, near_drawer_id, cosine, created_at, project_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                id,
                audit.candidate_hash,
                decision,
                audit.near_drawer_id,
                audit.cosine,
                created_at,
                audit.project_id
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

    pub fn drawer_exists_exact(
        &self,
        content: &str,
        wing: &str,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<bool, DbError> {
        Ok(!self
            .find_active_drawers_by_content(content, wing, room, project_id)?
            .is_empty())
    }

    pub fn find_active_drawers_by_content(
        &self,
        text: &str,
        wing: &str,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<Vec<DrawerSummary>, DbError> {
        let content_hash = content_hash_hex(text);
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, wing, room, source_file, project_id, added_at
            FROM drawers
            WHERE deleted_at IS NULL
              AND content_hash = ?1
              AND content = ?2
              AND wing = ?3
              AND ((room IS NULL AND ?4 IS NULL) OR room = ?4)
              AND ((project_id IS NULL AND ?5 IS NULL) OR project_id = ?5)
            ORDER BY added_at DESC, id
            "#,
        )?;
        let rows =
            statement.query_map(params![content_hash, text, wing, room, project_id], |row| {
                Ok(DrawerSummary {
                    id: row.get(0)?,
                    wing: row.get(1)?,
                    room: row.get(2)?,
                    source_file: row.get(3)?,
                    project_id: row.get(4)?,
                    added_at: row.get(5)?,
                })
            })?;

        let mut summaries = Vec::new();
        for row in rows {
            summaries.push(row?);
        }
        Ok(summaries)
    }

    pub fn resolve_replacement_target(
        &self,
        supersedes: Option<&str>,
        replace_text: Option<&str>,
        wing: &str,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<Option<DrawerSummary>, DbError> {
        match (supersedes, replace_text) {
            (Some(_), Some(_)) => Err(DbError::ReplacementTargetConflict),
            (Some(drawer_id), None) => {
                let details = self.get_drawer_details(drawer_id)?.ok_or_else(|| {
                    DbError::SupersededDrawerNotFound {
                        drawer_id: drawer_id.to_string(),
                    }
                })?;
                if details.project_id.as_deref() != project_id {
                    return Err(DbError::SupersededDrawerProjectMismatch {
                        drawer_id: drawer_id.to_string(),
                        expected: project_id.map(ToOwned::to_owned),
                        actual: details.project_id,
                    });
                }
                Ok(Some(drawer_summary_from_details(details)))
            }
            (None, Some(text)) => {
                let matches = self.find_active_drawers_by_content(text, wing, room, project_id)?;
                match matches.len() {
                    0 => Err(DbError::ReplacementTextNotFound),
                    1 => Ok(matches.into_iter().next()),
                    _ => Err(DbError::ReplacementTextAmbiguous {
                        candidate_ids: matches.into_iter().map(|summary| summary.id).collect(),
                    }),
                }
            }
            (None, None) => Ok(None),
        }
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
            Ok(_) => {
                self.record_current_vector_metadata(drawer_id, vector.len())?;
                Ok(())
            }
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

    pub fn vector_table_distance_metric(&self) -> Result<Option<String>, DbError> {
        if !self.table_exists("drawer_vectors")? {
            return Ok(None);
        }
        let sql = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawer_vectors'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(sql) = sql else {
            return Ok(None);
        };
        let lower = sql.to_ascii_lowercase();
        if lower.contains("distance_metric=cosine") {
            Ok(Some("cosine".to_string()))
        } else if lower.contains("distance_metric=l2") {
            Ok(Some("l2".to_string()))
        } else {
            // sqlite-vec defaults vec0 float vectors to L2 when no metric is declared.
            Ok(Some("l2".to_string()))
        }
    }

    /// True when the `drawer_vectors` table exists but declares the legacy `l2`
    /// distance metric instead of `cosine`. In that state broad sqlite-vec KNN
    /// ranks in the wrong metric space and semantic recall is silently degraded
    /// until `mempal reindex --from-config --stale` rebuilds the table. False
    /// when the table is absent (empty store) or already `cosine`.
    pub fn vector_index_is_stale(&self) -> Result<bool, DbError> {
        Ok(matches!(
            self.vector_table_distance_metric()?.as_deref(),
            Some("l2")
        ))
    }

    pub fn current_vector_embedder_fingerprint(dim: usize) -> String {
        let config = super::config::ConfigHandle::current();
        config.embed.current_vector_embedder_fingerprint(dim)
    }

    pub fn record_current_vector_metadata(
        &self,
        drawer_id: &str,
        dim: usize,
    ) -> Result<(), DbError> {
        self.record_vector_metadata(
            drawer_id,
            CURRENT_VECTOR_INDEX_VERSION,
            &Self::current_vector_embedder_fingerprint(dim),
        )
    }

    pub fn record_vector_metadata(
        &self,
        drawer_id: &str,
        index_version: &str,
        embedder_fingerprint: &str,
    ) -> Result<(), DbError> {
        if !self.table_exists("fork_ext_meta")? {
            return Ok(());
        }
        self.conn.execute(
            r#"
            INSERT INTO fork_ext_meta (key, value)
            VALUES (?1, ?2), (?3, ?4)
            ON CONFLICT(key) DO UPDATE SET value = excluded.value
            "#,
            params![
                vector_metadata_key(drawer_id, "index_version"),
                index_version,
                vector_metadata_key(drawer_id, "embedder_fingerprint"),
                embedder_fingerprint,
            ],
        )?;
        Ok(())
    }

    pub fn upsert_drawer_and_replace_vector(
        &self,
        drawer: &Drawer,
        vector: &[f32],
    ) -> Result<(), DbError> {
        anchor::validate_anchor_domain(&drawer.domain, &drawer.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;
        self.ensure_vectors_table(vector.len())?;

        let existing = self
            .conn
            .query_row(
                "SELECT 1 FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                [drawer.id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;

        if existing.is_none() {
            self.insert_drawer(drawer)?;
            return self.insert_vector(&drawer.id, vector);
        }

        let vector_json = serde_json::to_string(vector)?;
        let content_hash = content_hash_hex(&drawer.content);

        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<(), DbError> {
            self.conn.execute(
                r#"
                UPDATE drawers
                SET content = ?2,
                    wing = ?3,
                    room = ?4,
                    source_file = ?5,
                    source_type = ?6,
                    confidence = ?7,
                    added_at = ?8,
                    chunk_index = ?9,
                    normalize_version = ?10,
                    importance = ?11,
                    memory_kind = ?12,
                    domain = ?13,
                    field = ?14,
                    anchor_kind = ?15,
                    anchor_id = ?16,
                    parent_anchor_id = ?17,
                    provenance = ?18,
                    statement = ?19,
                    tier = ?20,
                    status = ?21,
                    supporting_refs = ?22,
                    counterexample_refs = ?23,
                    teaching_refs = ?24,
                    verification_refs = ?25,
                    scope_constraints = ?26,
                    trigger_hints = ?27,
                    is_pinned = ?28,
                    pin_order = ?29,
                    supersedes = ?30,
                    content_hash = ?31,
                    valid_from = ?32,
                    valid_until = NULL
                WHERE id = ?1 AND deleted_at IS NULL
                "#,
                params![
                    drawer.id.as_str(),
                    drawer.content.as_str(),
                    drawer.wing.as_str(),
                    drawer.room.as_deref(),
                    drawer.source_file.as_deref(),
                    source_type_as_str(&drawer.source_type),
                    drawer.confidence,
                    drawer.added_at.as_str(),
                    drawer.chunk_index,
                    i64::from(drawer.normalize_version),
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
                    drawer.is_pinned,
                    drawer.pin_order,
                    drawer.supersedes.as_deref(),
                    content_hash,
                    drawer.added_at.as_str(),
                ],
            )?;

            self.conn.execute(
                "DELETE FROM drawer_vectors WHERE id = ?1",
                [drawer.id.as_str()],
            )?;
            self.conn.execute(
                "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, vec_f32(?2))",
                params![drawer.id.as_str(), vector_json.as_str()],
            )?;
            self.record_current_vector_metadata(&drawer.id, vector.len())?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
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

    pub fn count_novelty_candidate_drawers(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<i64, DbError> {
        let vectors_exist: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get(0),
        )?;
        if !vectors_exist {
            return Ok(0);
        }

        self.conn
            .query_row(
                r#"
                SELECT COUNT(*)
                FROM drawer_vectors v
                JOIN drawers d ON d.id = v.id
                WHERE d.deleted_at IS NULL
                  AND (?1 IS NULL OR d.wing = ?1)
                  AND (?2 IS NULL OR d.room = ?2)
                  AND (?3 IS NULL OR d.project_id = ?3)
                "#,
                (wing, room, project_id),
                |row| row.get(0),
            )
            .map_err(DbError::from)
    }

    pub fn novelty_candidates_exact(
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
        let mut statement = self.conn.prepare(
            r#"
            SELECT d.id,
                   CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
            FROM drawer_vectors v
            JOIN drawers d ON d.id = v.id
            WHERE d.deleted_at IS NULL
              AND (?2 IS NULL OR d.wing = ?2)
              AND (?3 IS NULL OR d.room = ?3)
              AND (?4 IS NULL OR d.project_id = ?4)
            ORDER BY similarity DESC
            LIMIT ?5
            "#,
        )?;
        statement
            .query_map(
                (query_json.as_str(), wing, room, project_id, limit),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(DbError::from)
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
            "CREATE VIRTUAL TABLE IF NOT EXISTS drawer_vectors USING vec0(id TEXT PRIMARY KEY, embedding FLOAT[{dim}] distance_metric={VECTOR_DISTANCE_METRIC}{project_column});"
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

    pub fn stale_drawer_count(&self, current_normalize_version: u32) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND normalize_version < ?1",
            [i64::from(current_normalize_version)],
            |row| row.get(0),
        )?)
    }

    pub fn drawer_count_by_normalize_version(&self) -> Result<Vec<(u32, i64)>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT normalize_version, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY normalize_version
            ORDER BY normalize_version
            "#,
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn source_type_counts(&self) -> Result<Vec<(SourceType, i64)>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT source_type, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY source_type
            ORDER BY source_type
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                let source_type = row.get::<_, String>(0)?;
                let source_type = source_type_from_str(&source_type).map_err(row_decode_error)?;
                Ok((source_type, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn pinned_fact_counts_by_project(&self) -> Result<Vec<(Option<String>, i64)>, DbError> {
        if !drawers_column_exists(&self.conn, "project_id")? {
            let count = self.conn.query_row(
                r#"
                SELECT COUNT(*)
                FROM drawers
                WHERE deleted_at IS NULL
                  AND is_pinned = 1
                  AND COALESCE(status, 'active') IN ('active', 'canonical')
                "#,
                [],
                |row| row.get::<_, i64>(0),
            )?;
            return Ok(vec![(None, count)]);
        }

        let mut statement = self.conn.prepare(
            r#"
            SELECT project_id, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
              AND is_pinned = 1
              AND COALESCE(status, 'active') IN ('active', 'canonical')
            GROUP BY project_id
            ORDER BY project_id IS NOT NULL DESC, project_id
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn pin_drawer(&self, drawer_id: &str, pin_order: Option<i64>) -> Result<bool, DbError> {
        if let Some(order) = pin_order {
            return self.pin_drawer_with_order(drawer_id, Some(order));
        }

        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<bool, DbError> {
            let resolved_order = self.conn.query_row(
                "SELECT COALESCE(MAX(pin_order), -1) + 1 FROM drawers WHERE is_pinned = 1",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )?;
            self.pin_drawer_with_order(drawer_id, resolved_order)
        })();

        match result {
            Ok(affected) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(affected)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    fn pin_drawer_with_order(
        &self,
        drawer_id: &str,
        resolved_order: Option<i64>,
    ) -> Result<bool, DbError> {
        let affected = self.conn.execute(
            "UPDATE drawers SET is_pinned = 1, pin_order = COALESCE(?2, pin_order) WHERE id = ?1 AND deleted_at IS NULL",
            params![drawer_id, resolved_order],
        )?;
        Ok(affected > 0)
    }

    pub fn unpin_drawer(&self, drawer_id: &str) -> Result<bool, DbError> {
        let affected = self.conn.execute(
            "UPDATE drawers SET is_pinned = 0, pin_order = NULL WHERE id = ?1 AND deleted_at IS NULL",
            [drawer_id],
        )?;
        Ok(affected > 0)
    }

    pub fn reorder_pinned_facts(&self, drawer_ids: &[String]) -> Result<(), DbError> {
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<(), DbError> {
            for (index, drawer_id) in drawer_ids.iter().enumerate() {
                self.conn.execute(
                    "UPDATE drawers SET is_pinned = 1, pin_order = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                    params![drawer_id, i64::try_from(index).map_err(|_| {
                        DbError::InvalidEnumValue {
                            kind: "pin_order",
                            value: index.to_string(),
                        }
                    })?],
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    pub fn get_pinned_facts(
        &self,
        project_id: Option<&str>,
        budget_chars: usize,
    ) -> Result<Vec<Drawer>, DbError> {
        if budget_chars == 0 {
            return Ok(Vec::new());
        }

        let has_project_id = drawers_column_exists(&self.conn, "project_id")?;
        let updated_order = if drawers_column_exists(&self.conn, "updated_at")? {
            "COALESCE(updated_at, added_at)"
        } else {
            "added_at"
        };
        let mut values = Vec::new();
        let mut project_filter = String::new();
        if has_project_id && let Some(project_id) = project_id {
            values.push(SqlValue::Text(project_id.to_string()));
            project_filter = " AND project_id = ?1".to_string();
        }

        let sql = format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}
            FROM drawers
            WHERE deleted_at IS NULL
              AND is_pinned = 1
              AND COALESCE(status, 'active') IN ('active', 'canonical')
              {project_filter}
            ORDER BY pin_order IS NULL ASC,
                     pin_order ASC,
                     importance DESC,
                     {updated_order} DESC,
                     id ASC
            "#
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                drawer_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut used_chars = 0usize;
        let mut facts = Vec::new();
        for mut drawer in rows {
            if used_chars >= budget_chars {
                break;
            }
            let remaining = budget_chars - used_chars;
            let content_chars = drawer.content.chars().count();
            if content_chars <= remaining {
                used_chars += content_chars;
                facts.push(drawer);
            } else if remaining > 0 {
                drawer.content = truncate_to_char_budget(&drawer.content, remaining);
                facts.push(drawer);
                break;
            }
        }
        Ok(facts)
    }

    pub fn diary_rollup_days(&self) -> Result<u32, DbError> {
        let count = self.conn.query_row(
            r#"
            SELECT COUNT(DISTINCT substr(source_file, length(source_file) - 9, 10))
            FROM drawers
            WHERE deleted_at IS NULL
              AND wing = 'agent-diary'
              AND source_file LIKE 'agent-diary://rollup/%'
            "#,
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(count as u32)
    }

    pub fn reindex_sources_stale(
        &self,
        current_normalize_version: u32,
    ) -> Result<Vec<ReindexSource>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT source_root, source_file, project_id, wing, NULL AS room, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL AND normalize_version < ?1
            GROUP BY project_id, source_root, source_file, wing
            ORDER BY project_id, source_root, source_file, wing
            "#,
        )?;
        let rows = statement
            .query_map(
                [i64::from(current_normalize_version)],
                reindex_source_from_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn project_scoped_reindex_sources_stale(
        &self,
        current_normalize_version: u32,
    ) -> Result<ReindexSourceScopeSummary, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT COALESCE(SUM(drawer_count), 0), COUNT(*)
            FROM (
                SELECT COUNT(*) AS drawer_count
                FROM drawers
                WHERE deleted_at IS NULL
                  AND normalize_version < ?1
                  AND project_id IS NOT NULL
                  AND source_root IS NULL
                GROUP BY project_id, source_root, source_file, wing
            )
            "#,
        )?;
        let summary = statement.query_row(
            [i64::from(current_normalize_version)],
            reindex_source_scope_summary_from_row,
        )?;
        Ok(summary)
    }

    pub fn reindex_sources_force(&self) -> Result<Vec<ReindexSource>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT source_root, source_file, project_id, wing, NULL AS room, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY project_id, source_root, source_file, wing
            ORDER BY project_id, source_root, source_file, wing
            "#,
        )?;
        let rows = statement
            .query_map([], reindex_source_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn project_scoped_reindex_sources_force(
        &self,
    ) -> Result<ReindexSourceScopeSummary, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT COALESCE(SUM(drawer_count), 0), COUNT(*)
            FROM (
                SELECT COUNT(*) AS drawer_count
                FROM drawers
                WHERE deleted_at IS NULL
                  AND project_id IS NOT NULL
                  AND source_root IS NULL
                GROUP BY project_id, source_root, source_file, wing
            )
            "#,
        )?;
        let summary = statement.query_row([], reindex_source_scope_summary_from_row)?;
        Ok(summary)
    }

    /// Hard-delete the active drawers for a source scoped to a specific room
    /// (NULL room matches NULL room), removing their FTS and vector rows too.
    ///
    /// Use this when the caller knows the exact room the drawers live in.
    /// For reindex, prefer [`replace_active_source_drawers_across_rooms`]:
    /// re-ingesting a physical source may re-route it to a different room, and
    /// a room-scoped delete would miss the stale drawers in the old room and
    /// leave duplicates behind.
    pub fn replace_active_source_drawers(
        &self,
        source_file: &str,
        wing: &str,
        room: Option<&str>,
        project_id: Option<&str>,
        source_root: Option<&str>,
    ) -> Result<u64, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT rowid, id, content
            FROM drawers
            WHERE deleted_at IS NULL
              AND source_file = ?1
              AND wing = ?2
              AND ((?3 IS NULL AND room IS NULL) OR room = ?3)
              AND ((?4 IS NULL AND project_id IS NULL) OR project_id = ?4)
              AND ((?5 IS NULL AND source_root IS NULL) OR source_root = ?5)
            ORDER BY rowid
            "#,
        )?;
        let rows = statement
            .query_map((source_file, wing, room, project_id, source_root), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        self.delete_source_drawer_rows(rows, project_id, source_root)
    }

    /// Hard-delete the active drawers for a source across ALL rooms in a scope.
    ///
    /// This is the correct replace semantics for reindex: a physical source
    /// file maps to one logical source, so re-indexing it should keep only the
    /// freshly produced drawers regardless of which room each previous version
    /// was routed into. The fork still scopes the delete by project/source_root,
    /// so cross-room replacement cannot delete another project or source root.
    pub fn replace_active_source_drawers_across_rooms(
        &self,
        source_file: &str,
        wing: &str,
        project_id: Option<&str>,
        source_root: Option<&str>,
    ) -> Result<u64, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT rowid, id, content
            FROM drawers
            WHERE deleted_at IS NULL
              AND source_file = ?1
              AND wing = ?2
              AND ((?3 IS NULL AND project_id IS NULL) OR project_id = ?3)
              AND ((?4 IS NULL AND source_root IS NULL) OR source_root = ?4)
            ORDER BY rowid
            "#,
        )?;
        let rows = statement
            .query_map((source_file, wing, project_id, source_root), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        self.delete_source_drawer_rows(rows, project_id, source_root)
    }

    /// Transactionally hard-delete the given (rowid, id, content) drawer rows
    /// along with their FTS and vector entries. Shared by the room-scoped and
    /// across-rooms source replacement paths.
    fn delete_source_drawer_rows(
        &self,
        rows: Vec<(i64, String, String)>,
        project_id: Option<&str>,
        source_root: Option<&str>,
    ) -> Result<u64, DbError> {
        if rows.is_empty() {
            return Ok(0);
        }

        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<u64, DbError> {
            let fts_exists = self.table_exists("drawers_fts")?;
            let vectors_exist = self.table_exists("drawer_vectors")?;

            for (rowid, id, content) in &rows {
                if fts_exists {
                    self.delete_drawer_fts_row(*rowid, content)?;
                }
                if vectors_exist {
                    self.conn.execute(
                        r#"
                        DELETE FROM drawer_vectors
                        WHERE id = ?1
                          AND ((?2 IS NULL AND project_id IS NULL) OR project_id = ?2)
                          AND EXISTS (
                              SELECT 1
                              FROM drawers
                              WHERE rowid = ?3
                                AND id = ?1
                                AND ((?2 IS NULL AND project_id IS NULL) OR project_id = ?2)
                                AND ((?4 IS NULL AND source_root IS NULL) OR source_root = ?4)
                          )
                        "#,
                        params![id, project_id, rowid, source_root],
                    )?;
                }
                // triples.source_drawer is a FK to drawers(id) (RESTRICT). Drop
                // the dangling provenance link before the hard delete, otherwise
                // deleting a drawer referenced by a KG triple fails with a
                // FOREIGN KEY constraint error. The triple (a KG fact) is kept;
                // only its stale source pointer is cleared.
                self.conn.execute(
                    "UPDATE triples SET source_drawer = NULL WHERE source_drawer = ?1",
                    [id],
                )?;
                self.conn.execute(
                    r#"
                    DELETE FROM drawers
                    WHERE rowid = ?1
                      AND ((?2 IS NULL AND project_id IS NULL) OR project_id = ?2)
                      AND ((?3 IS NULL AND source_root IS NULL) OR source_root = ?3)
                    "#,
                    params![rowid, project_id, source_root],
                )?;
            }

            Ok(rows.len() as u64)
        })();

        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(count)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    fn table_exists(&self, table_name: &str) -> Result<bool, DbError> {
        table_exists_conn(&self.conn, table_name)
    }

    fn delete_drawer_fts_row(&self, rowid: i64, content: &str) -> Result<(), DbError> {
        if !self.drawer_fts_row_indexed(rowid)? {
            return Ok(());
        }

        let tokenized = fts_tokenize_content(content);
        self.conn.execute(
            "INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', ?1, ?2)",
            params![rowid, tokenized],
        )?;
        Ok(())
    }

    fn drawer_fts_row_indexed(&self, rowid: i64) -> Result<bool, DbError> {
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS temp.drawer_fts_vocab \
             USING fts5vocab('main', 'drawers_fts', 'instance')",
        )?;
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM temp.drawer_fts_vocab WHERE doc = ?1)",
            [rowid],
            |row| row.get(0),
        )?)
    }

    pub fn drawer_vector_details(&self, drawer_id: &str) -> Result<DrawerVectorDetails, DbError> {
        let metric = self.vector_table_distance_metric()?;
        let dimension = if metric.is_some() {
            self.conn
                .query_row(
                    "SELECT vec_length(embedding) FROM drawer_vectors WHERE id = ?1",
                    [drawer_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .map(|value| value as usize)
        } else {
            None
        };
        let has_vector = dimension.is_some();
        let index_version = self
            .load_vector_metadata(drawer_id, "index_version")?
            .or(self.load_vector_metadata(drawer_id, "normalize_version")?);
        let embedder_fingerprint = self.load_vector_metadata(drawer_id, "embedder_fingerprint")?;
        let current_embedder_fingerprint = dimension.map(Self::current_vector_embedder_fingerprint);
        let (embedder, model) = embedder_fingerprint
            .as_deref()
            .map(parse_vector_fingerprint)
            .unwrap_or((None, None));
        let stale = !has_vector
            || metric.as_deref() != Some(VECTOR_DISTANCE_METRIC)
            || index_version.as_deref() != Some(CURRENT_VECTOR_INDEX_VERSION)
            || embedder_fingerprint.as_deref() != current_embedder_fingerprint.as_deref();

        Ok(DrawerVectorDetails {
            has_vector,
            dimension,
            embedder,
            model,
            embedder_fingerprint,
            index_version,
            current_embedder_fingerprint,
            current_index_version: CURRENT_VECTOR_INDEX_VERSION.to_string(),
            distance_metric: metric,
            stale,
        })
    }

    fn load_vector_metadata(
        &self,
        drawer_id: &str,
        field: &str,
    ) -> Result<Option<String>, DbError> {
        if !self.table_exists("fork_ext_meta")? {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT value FROM fork_ext_meta WHERE key = ?1",
                [vector_metadata_key(drawer_id, field)],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(DbError::Sqlite)
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

    pub fn scope_counts_for_search_scope(
        &self,
        scope: &ProjectSearchScope,
    ) -> Result<Vec<(String, Option<String>, i64)>, DbError> {
        if scope.mode == ProjectFilterMode::AllProjects
            || !drawers_column_exists(&self.conn, "project_id")?
        {
            return self.scope_counts();
        }

        let mut statement = self.conn.prepare(
            r#"
            SELECT wing, room, COUNT(*)
            FROM drawers
            WHERE deleted_at IS NULL
              AND (
                (?1 = 'project' AND project_id = ?2)
                OR (?1 = 'project_plus_global' AND (project_id = ?2 OR project_id IS NULL))
                OR (?1 = 'null_only' AND project_id IS NULL)
              )
            GROUP BY wing, room
            ORDER BY wing, room
            "#,
        )?;
        let rows = statement
            .query_map(
                params![scope.mode_param(), scope.project_id.as_deref()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-field counts for the P106 distill signal: active evidence drawers and
    /// active promoted-or-canonical knowledge drawers, grouped by `field`.
    /// Read-only; performs no writes.
    pub fn distill_field_counts(&self) -> Result<Vec<(String, i64, i64)>, DbError> {
        self.distill_field_counts_scoped(None)
    }

    /// Project-scoped variant of [`Self::distill_field_counts`].
    ///
    /// When `project_id` is `None`, this preserves the historical all-projects
    /// behavior used before project isolation was threaded into context assembly.
    pub fn distill_field_counts_scoped(
        &self,
        project_id: Option<&str>,
    ) -> Result<Vec<(String, i64, i64)>, DbError> {
        if let Some(project_id) = project_id {
            let mut statement = self.conn.prepare(
                r#"
                SELECT field,
                       SUM(CASE WHEN memory_kind = 'evidence' THEN 1 ELSE 0 END) AS evidence_count,
                       SUM(CASE WHEN memory_kind = 'knowledge'
                                 AND status IN ('promoted', 'canonical') THEN 1 ELSE 0 END) AS promoted_count
                FROM drawers
                WHERE deleted_at IS NULL
                  AND project_id = ?1
                GROUP BY field
                "#,
            )?;
            let rows = statement
                .query_map([project_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            return Ok(rows);
        }

        let mut statement = self.conn.prepare(
            r#"
            SELECT field,
                   SUM(CASE WHEN memory_kind = 'evidence' THEN 1 ELSE 0 END) AS evidence_count,
                   SUM(CASE WHEN memory_kind = 'knowledge'
                             AND status IN ('promoted', 'canonical') THEN 1 ELSE 0 END) AS promoted_count
            FROM drawers
            WHERE deleted_at IS NULL
            GROUP BY field
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Up to `limit` active evidence drawer ids for a `field`, ordered by rowid
    /// for deterministic sampling. Read-only; performs no writes.
    pub fn sample_evidence_drawer_ids(
        &self,
        field: &str,
        limit: usize,
    ) -> Result<Vec<String>, DbError> {
        self.sample_evidence_drawer_ids_scoped(field, limit, None)
    }

    /// Project-scoped variant of [`Self::sample_evidence_drawer_ids`].
    ///
    /// When `project_id` is `None`, this preserves the historical all-projects
    /// behavior used before project isolation was threaded into context assembly.
    pub fn sample_evidence_drawer_ids_scoped(
        &self,
        field: &str,
        limit: usize,
        project_id: Option<&str>,
    ) -> Result<Vec<String>, DbError> {
        if let Some(project_id) = project_id {
            let mut statement = self.conn.prepare(
                r#"
                SELECT id
                FROM drawers
                WHERE deleted_at IS NULL
                  AND memory_kind = 'evidence'
                  AND field = ?1
                  AND project_id = ?3
                ORDER BY rowid
                LIMIT ?2
                "#,
            )?;
            let rows = statement
                .query_map(params![field, limit as i64, project_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            return Ok(rows);
        }

        let mut statement = self.conn.prepare(
            r#"
            SELECT id
            FROM drawers
            WHERE deleted_at IS NULL AND memory_kind = 'evidence' AND field = ?1
            ORDER BY rowid
            LIMIT ?2
            "#,
        )?;
        let rows = statement
            .query_map(params![field, limit as i64], |row| row.get::<_, String>(0))?
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

    pub fn get_drawer_details(&self, drawer_id: &str) -> Result<Option<DrawerDetails>, DbError> {
        let mut statement = self.conn.prepare(&format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS},
                   updated_at,
                   COALESCE(merge_count, 0) as merge_count,
                   project_id
            FROM drawers
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
        ))?;
        let mut rows = statement.query_map([drawer_id], |row| {
            let drawer = drawer_from_row(row).map_err(row_decode_error)?;
            // DRAWER_SELECT_COLUMNS has 32 columns (indices 0-31), so
            // the extra columns appended here start at index 32.
            let updated_at = row.get::<_, Option<String>>(32)?;
            let merge_count = row.get::<_, u32>(33)?;
            let project_id = row.get::<_, Option<String>>(34)?;
            Ok((drawer, updated_at, merge_count, project_id))
        })?;

        let details = match rows.next() {
            Some(row) => {
                let (drawer, updated_at, merge_count, project_id) = row?;
                Some((drawer, updated_at, merge_count, project_id))
            }
            None => None,
        };
        drop(rows);
        drop(statement);
        details
            .map(|(drawer, updated_at, merge_count, project_id)| {
                let vector = self.drawer_vector_details(&drawer.id)?;
                Ok(DrawerDetails {
                    drawer,
                    updated_at,
                    merge_count,
                    project_id,
                    vector,
                })
            })
            .transpose()
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
                SELECT {DRAWER_SELECT_COLUMNS},
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
                let drawer = drawer_from_row(row).map_err(row_decode_error)?;
                // DRAWER_SELECT_COLUMNS is 32 columns (0-31); extra columns start at 32.
                let updated_at = row.get::<_, Option<String>>(32)?;
                let merge_count = row.get::<_, u32>(33)?;
                let project_id = row.get::<_, Option<String>>(34)?;
                Ok((
                    drawer.id.clone(),
                    drawer,
                    updated_at,
                    merge_count,
                    project_id,
                ))
            })?;

            let mut base_rows = Vec::new();
            for row in rows {
                base_rows.push(row?);
            }
            drop(statement);
            for (id, drawer, updated_at, merge_count, project_id) in base_rows {
                let vector = self.drawer_vector_details(&id)?;
                let details = DrawerDetails {
                    drawer,
                    updated_at,
                    merge_count,
                    project_id,
                    vector,
                };
                found_by_id.insert(id, details);
            }
        }

        Ok(ordered_ids
            .into_iter()
            .filter_map(|drawer_id| found_by_id.remove(&drawer_id))
            .collect())
    }

    pub(crate) fn apply_compaction(
        &self,
        target_id: &str,
        source_ids: &[String],
        merged_content: &str,
        strategy: CompactionStrategy,
    ) -> Result<(), DbError> {
        if source_ids.is_empty() {
            return Err(DbError::CompactionClusterEmpty);
        }

        let source_drawer_ids_json = serde_json::to_string(source_ids)?;
        let timestamp = current_timestamp();
        let unique_nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let log_seed = format!("{target_id}:{timestamp}:{unique_nanos}:{source_drawer_ids_json}");
        let log_digest = blake3::hash(log_seed.as_bytes()).to_hex().to_string();
        let log_id = format!("consolidation_{}", &log_digest[..16]);
        let content_hash = content_hash_hex(merged_content);

        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), DbError> {
            let (target_rowid, old_content, wing, room, project_id) = self
                .conn
                .query_row(
                    r#"
                    SELECT rowid, content, wing, room, project_id
                    FROM drawers
                    WHERE id = ?1
                      AND deleted_at IS NULL
                      AND compacted_into IS NULL
                    "#,
                    [target_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or_else(|| DbError::CompactionDrawerNotFound {
                    drawer_id: target_id.to_string(),
                })?;

            let fts_exists = self.table_exists("drawers_fts")?;
            if fts_exists {
                let old_tokenized = fts_tokenize_content(&old_content);
                self.conn.execute(
                    "INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', ?1, ?2)",
                    params![target_rowid, old_tokenized],
                )?;
            }

            let updated = self.conn.execute(
                r#"
                UPDATE drawers
                SET content = ?2,
                    content_hash = ?3,
                    updated_at = ?4,
                    merge_count = COALESCE(merge_count, 0) + ?5
                WHERE id = ?1
                  AND deleted_at IS NULL
                  AND compacted_into IS NULL
                "#,
                params![
                    target_id,
                    merged_content,
                    content_hash,
                    timestamp,
                    i64::try_from(source_ids.len().saturating_sub(1)).unwrap_or(i64::MAX),
                ],
            )?;
            if updated != 1 {
                return Err(DbError::CompactionDrawerNotFound {
                    drawer_id: target_id.to_string(),
                });
            }

            if fts_exists {
                let tokenized = fts_tokenize_content(merged_content);
                self.conn.execute(
                    "INSERT INTO drawers_fts(rowid, content) VALUES (?1, ?2)",
                    params![target_rowid, tokenized],
                )?;
            }

            for source_id in source_ids
                .iter()
                .filter(|source_id| source_id.as_str() != target_id)
            {
                let affected = self.conn.execute(
                    r#"
                    UPDATE drawers
                    SET deleted_at = ?2,
                        valid_until = ?2,
                        compacted_into = ?3
                    WHERE id = ?1
                      AND deleted_at IS NULL
                      AND compacted_into IS NULL
                    "#,
                    params![source_id, timestamp, target_id],
                )?;
                if affected != 1 {
                    return Err(DbError::CompactionDrawerNotFound {
                        drawer_id: source_id.clone(),
                    });
                }
            }

            self.conn.execute(
                r#"
                INSERT INTO consolidation_log (
                    id,
                    wing,
                    room,
                    project_id,
                    cluster_size,
                    strategy,
                    target_drawer_id,
                    source_drawer_ids,
                    dry_run
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
                "#,
                params![
                    log_id,
                    wing,
                    room,
                    project_id,
                    i64::try_from(source_ids.len()).unwrap_or(i64::MAX),
                    strategy.as_str(),
                    target_id,
                    source_drawer_ids_json,
                ],
            )?;

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

    pub fn resolve_available_drawer_id(&self, preferred_id: &str) -> Result<String, DbError> {
        if !self.drawer_id_in_use(preferred_id)? {
            return Ok(preferred_id.to_string());
        }

        let mut suffix = 2usize;
        loop {
            let candidate = format!("{preferred_id}_{suffix}");
            if !self.drawer_id_in_use(&candidate)? {
                return Ok(candidate);
            }
            suffix += 1;
        }
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

    pub fn update_knowledge_lifecycle(
        &self,
        drawer_id: &str,
        status: &KnowledgeStatus,
        verification_refs: &[String],
        counterexample_refs: &[String],
    ) -> Result<bool, DbError> {
        let affected = self.conn.execute(
            r#"
            UPDATE drawers
            SET status = ?2,
                verification_refs = ?3,
                counterexample_refs = ?4
            WHERE id = ?1
              AND deleted_at IS NULL
              AND memory_kind = 'knowledge'
            "#,
            params![
                drawer_id,
                knowledge_status_as_str(status),
                encode_json(verification_refs)?,
                encode_json(counterexample_refs)?,
            ],
        )?;
        Ok(affected > 0)
    }

    pub fn update_knowledge_anchor(
        &self,
        drawer_id: &str,
        anchor_kind: &AnchorKind,
        anchor_id: &str,
        parent_anchor_id: Option<&str>,
    ) -> Result<bool, DbError> {
        let affected = self.conn.execute(
            r#"
            UPDATE drawers
            SET anchor_kind = ?2,
                anchor_id = ?3,
                parent_anchor_id = ?4
            WHERE id = ?1
              AND deleted_at IS NULL
              AND memory_kind = 'knowledge'
            "#,
            params![
                drawer_id,
                anchor_kind_as_str(anchor_kind),
                anchor_id,
                parent_anchor_id,
            ],
        )?;
        Ok(affected > 0)
    }

    pub fn insert_knowledge_card(&self, card: &KnowledgeCard) -> Result<(), DbError> {
        anchor::validate_anchor_domain(&card.domain, &card.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

        self.conn.execute(
            r#"
            INSERT INTO knowledge_cards (
                id,
                statement,
                content,
                tier,
                status,
                domain,
                field,
                anchor_kind,
                anchor_id,
                parent_anchor_id,
                scope_constraints,
                trigger_hints,
                auto_generated,
                crystallization_score,
                source_drawer_ids,
                created_at,
                updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            "#,
            params![
                card.id.as_str(),
                card.statement.as_str(),
                card.content.as_str(),
                knowledge_tier_as_str(&card.tier),
                knowledge_status_as_str(&card.status),
                memory_domain_as_str(&card.domain),
                card.field.as_str(),
                anchor_kind_as_str(&card.anchor_kind),
                card.anchor_id.as_str(),
                card.parent_anchor_id.as_deref(),
                card.scope_constraints.as_deref(),
                encode_optional_json(card.trigger_hints.as_ref())?,
                card.auto_generated,
                card.crystallization_score,
                serde_json::to_string(&card.source_drawer_ids)?,
                card.created_at.as_str(),
                card.updated_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn get_knowledge_card(&self, card_id: &str) -> Result<Option<KnowledgeCard>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT
                id,
                statement,
                content,
                tier,
                status,
                domain,
                field,
                anchor_kind,
                anchor_id,
                parent_anchor_id,
                scope_constraints,
                trigger_hints,
                auto_generated,
                crystallization_score,
                source_drawer_ids,
                created_at,
                updated_at
            FROM knowledge_cards
            WHERE id = ?1
            "#,
        )?;
        let mut rows = statement.query_map([card_id], |row| {
            knowledge_card_from_row(row).map_err(row_decode_error)
        })?;

        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    pub fn list_knowledge_cards(
        &self,
        filter: &KnowledgeCardFilter,
    ) -> Result<Vec<KnowledgeCard>, DbError> {
        let tier = filter.tier.as_ref().map(knowledge_tier_as_str);
        let status = filter.status.as_ref().map(knowledge_status_as_str);
        let domain = filter.domain.as_ref().map(memory_domain_as_str);
        let anchor_kind = filter.anchor_kind.as_ref().map(anchor_kind_as_str);
        let auto_generated = filter
            .auto_generated
            .map(|value| if value { 1_i64 } else { 0_i64 });
        let pending_review = filter
            .pending_review
            .map(|value| if value { 1_i64 } else { 0_i64 });

        let mut statement = self.conn.prepare(
            r#"
            SELECT
                id,
                statement,
                content,
                tier,
                status,
                domain,
                field,
                anchor_kind,
                anchor_id,
                parent_anchor_id,
                scope_constraints,
                trigger_hints,
                auto_generated,
                crystallization_score,
                source_drawer_ids,
                created_at,
                updated_at
            FROM knowledge_cards
            WHERE (?1 IS NULL OR tier = ?1)
              AND (?2 IS NULL OR status = ?2)
              AND (?3 IS NULL OR domain = ?3)
              AND (?4 IS NULL OR field = ?4)
              AND (?5 IS NULL OR anchor_kind = ?5)
              AND (?6 IS NULL OR anchor_id = ?6)
              AND (?7 IS NULL OR auto_generated = ?7)
              AND (?8 IS NULL OR (?8 = 1 AND status = 'pending_review') OR (?8 = 0 AND status != 'pending_review'))
            ORDER BY tier, status, id
            "#,
        )?;
        let rows = statement
            .query_map(
                params![
                    tier,
                    status,
                    domain,
                    filter.field.as_deref(),
                    anchor_kind,
                    filter.anchor_id.as_deref(),
                    auto_generated,
                    pending_review,
                ],
                |row| knowledge_card_from_row(row).map_err(row_decode_error),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn knowledge_card_count(&self) -> Result<i64, DbError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM knowledge_cards", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn pending_auto_generated_knowledge_card_count(&self) -> Result<i64, DbError> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM knowledge_cards WHERE auto_generated = 1 AND status = 'pending_review'",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn last_crystallization_at(&self) -> Result<Option<String>, DbError> {
        self.conn
            .query_row(
                "SELECT MAX(created_at) FROM knowledge_cards WHERE auto_generated = 1",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn insert_runtime_adoption_event(
        &self,
        event: &RuntimeAdoptionEvent,
    ) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT INTO runtime_adoption_events (
                id,
                track,
                signal,
                feature,
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
                created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
            params![
                event.id.as_str(),
                runtime_adoption_track_as_str(&event.track),
                runtime_adoption_signal_as_str(&event.signal),
                event.feature.as_str(),
                event.query.as_deref(),
                event.context_hash.as_deref(),
                event.card_id.as_deref(),
                event.evaluator_id.as_deref(),
                event.research_report_id.as_deref(),
                event.note.as_deref(),
                encode_optional_json(event.metadata.as_ref())?,
                event.created_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn list_runtime_adoption_events(
        &self,
        filter: &RuntimeAdoptionFilter,
        limit: usize,
    ) -> Result<Vec<RuntimeAdoptionEvent>, DbError> {
        let track = filter.track.as_ref().map(runtime_adoption_track_as_str);
        let limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let mut statement = self.conn.prepare(
            r#"
            SELECT
                id,
                track,
                signal,
                feature,
                query,
                context_hash,
                card_id,
                evaluator_id,
                research_report_id,
                note,
                metadata,
                created_at
            FROM runtime_adoption_events
            WHERE (?1 IS NULL OR track = ?1)
              AND (?2 IS NULL OR feature = ?2)
            ORDER BY created_at DESC, id DESC
            LIMIT ?3
            "#,
        )?;
        let rows = statement
            .query_map(params![track, filter.feature.as_deref(), limit], |row| {
                runtime_adoption_event_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_knowledge_drawers_for_card_backfill(
        &self,
        filter: &KnowledgeCardFilter,
    ) -> Result<Vec<Drawer>, DbError> {
        let tier = filter.tier.as_ref().map(knowledge_tier_as_str);
        let status = filter.status.as_ref().map(knowledge_status_as_str);
        let domain = filter.domain.as_ref().map(memory_domain_as_str);
        let anchor_kind = filter.anchor_kind.as_ref().map(anchor_kind_as_str);

        let mut statement = self.conn.prepare(&format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}
            FROM drawers
            WHERE deleted_at IS NULL
              AND memory_kind = 'knowledge'
              AND (?1 IS NULL OR tier = ?1)
              AND (?2 IS NULL OR status = ?2)
              AND (?3 IS NULL OR domain = ?3)
              AND (?4 IS NULL OR field = ?4)
              AND (?5 IS NULL OR anchor_kind = ?5)
              AND (?6 IS NULL OR anchor_id = ?6)
            ORDER BY id
            "#,
        ))?;
        let rows = statement
            .query_map(
                params![
                    tier,
                    status,
                    domain,
                    filter.field.as_deref(),
                    anchor_kind,
                    filter.anchor_id.as_deref(),
                ],
                |row| drawer_from_row(row).map_err(row_decode_error),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn update_knowledge_card(&self, card: &KnowledgeCard) -> Result<bool, DbError> {
        anchor::validate_anchor_domain(&card.domain, &card.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

        let affected = self.conn.execute(
            r#"
            UPDATE knowledge_cards
            SET statement = ?2,
                content = ?3,
                tier = ?4,
                status = ?5,
                domain = ?6,
                field = ?7,
                anchor_kind = ?8,
                anchor_id = ?9,
                parent_anchor_id = ?10,
                scope_constraints = ?11,
                trigger_hints = ?12,
                auto_generated = ?13,
                crystallization_score = ?14,
                source_drawer_ids = ?15,
                updated_at = ?16
            WHERE id = ?1
            "#,
            params![
                card.id.as_str(),
                card.statement.as_str(),
                card.content.as_str(),
                knowledge_tier_as_str(&card.tier),
                knowledge_status_as_str(&card.status),
                memory_domain_as_str(&card.domain),
                card.field.as_str(),
                anchor_kind_as_str(&card.anchor_kind),
                card.anchor_id.as_str(),
                card.parent_anchor_id.as_deref(),
                card.scope_constraints.as_deref(),
                encode_optional_json(card.trigger_hints.as_ref())?,
                card.auto_generated,
                card.crystallization_score,
                serde_json::to_string(&card.source_drawer_ids)?,
                card.updated_at.as_str(),
            ],
        )?;
        Ok(affected > 0)
    }

    pub fn insert_knowledge_evidence_link(
        &self,
        link: &KnowledgeEvidenceLink,
    ) -> Result<(), DbError> {
        let evidence = self.get_drawer(&link.evidence_drawer_id)?.ok_or_else(|| {
            DbError::InvalidDrawerMetadata(format!(
                "evidence drawer {} does not exist",
                link.evidence_drawer_id
            ))
        })?;
        if evidence.memory_kind != MemoryKind::Evidence {
            return Err(DbError::InvalidDrawerMetadata(format!(
                "evidence link target {} must be an evidence drawer",
                link.evidence_drawer_id
            )));
        }

        self.conn.execute(
            r#"
            INSERT INTO knowledge_evidence_links (
                id,
                card_id,
                evidence_drawer_id,
                role,
                note,
                created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                link.id.as_str(),
                link.card_id.as_str(),
                link.evidence_drawer_id.as_str(),
                knowledge_evidence_role_as_str(&link.role),
                link.note.as_deref(),
                link.created_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn knowledge_evidence_links(
        &self,
        card_id: &str,
    ) -> Result<Vec<KnowledgeEvidenceLink>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, card_id, evidence_drawer_id, role, note, created_at
            FROM knowledge_evidence_links
            WHERE card_id = ?1
            ORDER BY created_at, id
            "#,
        )?;
        let rows = statement
            .query_map([card_id], |row| {
                knowledge_evidence_link_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn knowledge_evidence_links_for_drawer(
        &self,
        evidence_drawer_id: &str,
    ) -> Result<Vec<KnowledgeEvidenceLink>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT id, card_id, evidence_drawer_id, role, note, created_at
            FROM knowledge_evidence_links
            WHERE evidence_drawer_id = ?1
            ORDER BY created_at, id
            "#,
        )?;
        let rows = statement
            .query_map([evidence_drawer_id], |row| {
                knowledge_evidence_link_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn append_knowledge_event(&self, event: &KnowledgeCardEvent) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT INTO knowledge_events (
                id,
                card_id,
                event_type,
                from_status,
                to_status,
                reason,
                actor,
                metadata,
                created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                event.id.as_str(),
                event.card_id.as_str(),
                knowledge_event_type_as_str(&event.event_type),
                event.from_status.as_ref().map(knowledge_status_as_str),
                event.to_status.as_ref().map(knowledge_status_as_str),
                event.reason.as_str(),
                event.actor.as_deref(),
                encode_optional_json(event.metadata.as_ref())?,
                event.created_at.as_str(),
            ],
        )?;
        Ok(())
    }

    pub fn knowledge_events(&self, card_id: &str) -> Result<Vec<KnowledgeCardEvent>, DbError> {
        let mut statement = self.conn.prepare(
            r#"
            SELECT
                id,
                card_id,
                event_type,
                from_status,
                to_status,
                reason,
                actor,
                metadata,
                created_at
            FROM knowledge_events
            WHERE card_id = ?1
            ORDER BY created_at, id
            "#,
        )?;
        let rows = statement
            .query_map([card_id], |row| {
                knowledge_event_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn neighbor_chunks(
        &self,
        source_file: &str,
        wing: &str,
        room: Option<&str>,
        chunk_index: i64,
    ) -> Result<ChunkNeighbors, DbError> {
        let prev_index = chunk_index - 1;
        let next_index = chunk_index + 1;
        let sql = r#"
            SELECT id, content, chunk_index
            FROM drawers
            WHERE deleted_at IS NULL
              AND source_file = ?1
              AND wing = ?2
              AND ((?3 IS NULL AND room IS NULL) OR (?3 IS NOT NULL AND room = ?3))
              AND chunk_index IN (?4, ?5)
            ORDER BY chunk_index, id
            "#;
        let mut statement = self.conn.prepare(sql)?;
        let mut rows = statement.query(params![source_file, wing, room, prev_index, next_index])?;
        let mut neighbors = ChunkNeighbors {
            prev: None,
            next: None,
        };

        while let Some(row) = rows.next()? {
            let row_index = row.get::<_, i64>(2)?;
            let Ok(chunk_index) = u32::try_from(row_index) else {
                continue;
            };
            let chunk = NeighborChunk {
                drawer_id: row.get(0)?,
                content: row.get(1)?,
                chunk_index,
            };
            if row_index == prev_index && neighbors.prev.is_none() {
                neighbors.prev = Some(chunk);
            } else if row_index == next_index && neighbors.next.is_none() {
                neighbors.next = Some(chunk);
            }
        }

        Ok(neighbors)
    }

    pub fn soft_delete_drawer(&self, drawer_id: &str) -> Result<bool, DbError> {
        let timestamp = super::utils::current_timestamp();
        let affected = self.conn.execute(
            "UPDATE drawers SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![timestamp, drawer_id],
        )?;
        Ok(affected > 0)
    }

    pub fn soft_delete_drawers_by_ids(&self, drawer_ids: &[String]) -> Result<usize, DbError> {
        let timestamp = super::utils::current_timestamp();
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<usize, DbError> {
            let mut affected_total = 0usize;
            for drawer_id in drawer_ids {
                affected_total += self.conn.execute(
                    "UPDATE drawers SET deleted_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                    params![timestamp, drawer_id],
                )?;
            }
            Ok(affected_total)
        })();
        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(count)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub fn supersede_drawer(&self, old_id: &str, reason: &str) -> Result<bool, DbError> {
        let timestamp = super::utils::current_timestamp();
        let affected = self.conn.execute(
            "UPDATE drawers SET status = 'superseded', deleted_at = ?1, valid_until = ?1 WHERE id = ?2 AND deleted_at IS NULL",
            params![timestamp, old_id],
        )?;
        if affected > 0 {
            self.append_supersede_audit_entry(old_id, reason, &timestamp)?;
        }
        Ok(affected > 0)
    }

    fn append_supersede_audit_entry(
        &self,
        old_id: &str,
        reason: &str,
        timestamp: &str,
    ) -> Result<(), DbError> {
        let audit_path = self
            .path
            .parent()
            .map(|parent| parent.join("audit.jsonl"))
            .unwrap_or_else(|| PathBuf::from("audit.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
            .map_err(|source| DbError::AuditOpen {
                path: audit_path.clone(),
                source,
            })?;
        let entry = serde_json::json!({
            "timestamp": timestamp,
            "command": "supersede",
            "drawer_id": old_id,
            "reason": reason,
        });
        writeln!(file, "{entry}").map_err(|source| DbError::AuditWrite {
            path: audit_path,
            source,
        })?;
        Ok(())
    }

    pub fn soft_delete_drawers_since(
        &self,
        since: &str,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<Vec<String>, DbError> {
        let mut sql = String::from(
            "UPDATE drawers SET deleted_at = ?1 \
             WHERE added_at > ?2 AND deleted_at IS NULL",
        );
        let mut values = vec![
            SqlValue::Text(super::utils::current_timestamp()),
            SqlValue::Text(since.to_string()),
        ];
        append_drawers_since_filters(&mut sql, &mut values, wing, room, project_id);
        sql.push_str(" RETURNING id");

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values), |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn count_drawers_since(
        &self,
        since: &str,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<i64, DbError> {
        let mut sql = String::from(
            "SELECT COUNT(*) FROM drawers \
             WHERE added_at > ?1 AND deleted_at IS NULL",
        );
        let mut values = vec![SqlValue::Text(since.to_string())];
        append_drawers_since_filters(&mut sql, &mut values, wing, room, project_id);

        Ok(self
            .conn
            .query_row(&sql, params_from_iter(values), |row| row.get(0))?)
    }

    pub fn historical_rejudge_candidates(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HistoricalRejudgeCandidate>, DbError> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut sql = format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}, project_id, rowid
            FROM drawers
            WHERE deleted_at IS NULL
            "#
        );
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(wing) = wing {
            values.push(SqlValue::Text(wing.to_string()));
            sql.push_str(&format!("AND wing = ?{} ", values.len()));
        }
        if let Some(room) = room {
            values.push(SqlValue::Text(room.to_string()));
            sql.push_str(&format!("AND room = ?{} ", values.len()));
        }
        if let Some(project_id) = project_id {
            values.push(SqlValue::Text(project_id.to_string()));
            sql.push_str(&format!("AND project_id = ?{} ", values.len()));
        }
        values.push(SqlValue::Integer(limit_i64));
        sql.push_str(&format!("ORDER BY rowid ASC LIMIT ?{}", values.len()));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                Ok(HistoricalRejudgeCandidate {
                    rowid: row.get(33)?,
                    drawer: drawer_from_row(row).map_err(row_decode_error)?,
                    project_id: row.get(32)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn historical_rejudge_scope_max_rowid(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
    ) -> Result<Option<i64>, DbError> {
        let mut sql = String::from("SELECT MAX(rowid) FROM drawers WHERE deleted_at IS NULL");
        let mut values: Vec<SqlValue> = Vec::new();
        append_drawers_since_filters(&mut sql, &mut values, wing, room, project_id);
        Ok(self
            .conn
            .query_row(&sql, params_from_iter(values), |row| row.get(0))?)
    }

    pub fn historical_rejudge_scope_count_until(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
        max_rowid: i64,
    ) -> Result<usize, DbError> {
        let mut sql =
            String::from("SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL AND rowid <= ?1");
        let mut values: Vec<SqlValue> = vec![SqlValue::Integer(max_rowid)];
        append_drawers_since_filters(&mut sql, &mut values, wing, room, project_id);
        let count: i64 = self
            .conn
            .query_row(&sql, params_from_iter(values), |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub fn historical_rejudge_candidates_page(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
        after_rowid: i64,
        max_rowid: i64,
        limit: usize,
    ) -> Result<Vec<HistoricalRejudgeCandidate>, DbError> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut sql = format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}, project_id, rowid
            FROM drawers
            WHERE deleted_at IS NULL
              AND rowid > ?1
              AND rowid <= ?2
            "#
        );
        let mut values: Vec<SqlValue> =
            vec![SqlValue::Integer(after_rowid), SqlValue::Integer(max_rowid)];
        append_drawers_since_filters(&mut sql, &mut values, wing, room, project_id);
        values.push(SqlValue::Integer(limit_i64));
        sql.push_str(&format!(" ORDER BY rowid ASC LIMIT ?{}", values.len()));

        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                Ok(HistoricalRejudgeCandidate {
                    rowid: row.get(33)?,
                    drawer: drawer_from_row(row).map_err(row_decode_error)?,
                    project_id: row.get(32)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn historical_rejudge_candidate_by_rowid(
        &self,
        drawer_rowid: i64,
        drawer_id: &str,
    ) -> Result<Option<HistoricalRejudgeCandidate>, DbError> {
        let sql = format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}, project_id, rowid
            FROM drawers
            WHERE deleted_at IS NULL
              AND rowid = ?1
              AND id = ?2
            "#
        );
        let row = self
            .conn
            .query_row(&sql, params![drawer_rowid, drawer_id], |row| {
                Ok(HistoricalRejudgeCandidate {
                    rowid: row.get(33)?,
                    drawer: drawer_from_row(row).map_err(row_decode_error)?,
                    project_id: row.get(32)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn timeline_drawers(
        &self,
        wing: Option<&str>,
        room: Option<&str>,
        project_id: Option<&str>,
        limit: usize,
        strict_null_only: bool,
    ) -> Result<Vec<Drawer>, DbError> {
        let sql_limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let mut sql = format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}
            FROM drawers
            WHERE deleted_at IS NULL
            "#
        );
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(w) = wing {
            values.push(SqlValue::Text(w.to_string()));
            sql.push_str(&format!("AND wing = ?{} ", values.len()));
        }
        if let Some(r) = room {
            values.push(SqlValue::Text(r.to_string()));
            sql.push_str(&format!("AND room = ?{} ", values.len()));
        }
        if let Some(p) = project_id {
            values.push(SqlValue::Text(p.to_string()));
            sql.push_str(&format!("AND project_id = ?{} ", values.len()));
        } else if strict_null_only {
            sql.push_str("AND project_id IS NULL ");
        }
        values.push(SqlValue::Integer(sql_limit));
        sql.push_str(&format!(
            "ORDER BY CASE WHEN added_at NOT GLOB '20*' THEN strftime('%Y-%m-%dT%H:%M:%SZ', CAST(added_at AS INTEGER), 'unixepoch') ELSE added_at END DESC, id DESC LIMIT ?{}",
            values.len()
        ));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(values), |row| {
                drawer_from_row(row).map_err(row_decode_error)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn hard_delete_drawers_by_ids(&self, drawer_ids: &[String]) -> Result<usize, DbError> {
        if drawer_ids.is_empty() {
            return Ok(0);
        }

        let vectors_exist: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get(0),
        )?;

        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<usize, DbError> {
            let fts_exists = self.table_exists("drawers_fts")?;
            let mut deleted_total = 0usize;
            for id in drawer_ids {
                if vectors_exist {
                    self.conn
                        .execute("DELETE FROM drawer_vectors WHERE id = ?1", [id])?;
                }
                self.conn.execute(
                    "UPDATE triples SET source_drawer = NULL WHERE source_drawer = ?1",
                    [id],
                )?;
                if fts_exists {
                    let fts_row = self
                        .conn
                        .query_row(
                            "SELECT rowid, content FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                            [id],
                            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()?;
                    if let Some((rowid, content)) = fts_row {
                        self.delete_drawer_fts_row(rowid, &content)?;
                    }
                }
                deleted_total += self.conn.execute(
                    "DELETE FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                    [id],
                )?;
            }
            Ok(deleted_total)
        })();

        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(count)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    pub fn purge_deleted(&self, before: Option<&str>) -> Result<u64, DbError> {
        // First collect IDs to purge, then delete from both tables
        let rows: Vec<(i64, String, String)> = if let Some(before) = before {
            let mut stmt = self.conn.prepare(
                "SELECT rowid, id, content FROM drawers WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            )?;
            stmt.query_map([before], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = self
                .conn
                .prepare("SELECT rowid, id, content FROM drawers WHERE deleted_at IS NOT NULL")?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        if rows.is_empty() {
            return Ok(0);
        }

        // Check if drawer_vectors table exists (lazy-created)
        let vectors_exist: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
            [],
            |row| row.get(0),
        )?;
        let fts_exists = self.table_exists("drawers_fts")?;

        // Wrap the purge in a single transaction. Clearing the
        // triples.source_drawer FK and deleting the drawer must be atomic:
        // another RESTRICT FK (e.g. knowledge_evidence_links.evidence_drawer_id)
        // can block `DELETE FROM drawers`, and without a transaction the prior
        // `UPDATE triples SET source_drawer = NULL` would have already committed
        // — silently dropping provenance for a drawer that was not purged.
        self.conn.execute_batch("BEGIN IMMEDIATE;")?;
        let result = (|| -> Result<u64, DbError> {
            for (rowid, id, content) in &rows {
                if vectors_exist {
                    self.conn
                        .execute("DELETE FROM drawer_vectors WHERE id = ?1", [id])?;
                }
                // Clear the triples.source_drawer FK (RESTRICT) before the hard
                // delete so purging a soft-deleted drawer referenced by a KG
                // triple does not fail with a FOREIGN KEY constraint error.
                self.conn.execute(
                    "UPDATE triples SET source_drawer = NULL WHERE source_drawer = ?1",
                    [id],
                )?;
                if fts_exists {
                    self.delete_drawer_fts_row(*rowid, content)?;
                }
                self.conn
                    .execute("DELETE FROM drawers WHERE id = ?1", [id])?;
            }
            Ok(rows.len() as u64)
        })();

        match result {
            Ok(count) => {
                self.conn.execute_batch("COMMIT;")?;
                Ok(count)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK;");
                Err(error)
            }
        }
    }

    pub fn deleted_drawer_count(&self) -> Result<i64, DbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn consolidation_stats(&self) -> Result<ConsolidationStats, DbError> {
        let total_compacted_drawers = self.conn.query_row(
            "SELECT COUNT(*) FROM drawers WHERE compacted_into IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let (consolidation_runs, last_consolidation_at) = self.conn.query_row(
            "SELECT COUNT(*), MAX(created_at) FROM consolidation_log",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )?;
        let sleep_stats = self.sleep_stats()?;
        Ok(ConsolidationStats {
            total_compacted_drawers,
            consolidation_runs,
            last_consolidation_at,
            last_sleep_at: sleep_stats.last_sleep_at,
            sleep_items_pruned: sleep_stats.items_pruned,
            sleep_items_compacted: sleep_stats.items_compacted,
            sleep_conflicts_resolved: sleep_stats.conflicts_resolved,
        })
    }

    pub fn sleep_stats(&self) -> Result<SleepStats, DbError> {
        if !self.table_exists("sleep_log")? {
            return Ok(SleepStats::default());
        }

        let stats = self
            .conn
            .query_row(
                r#"
                SELECT created_at, pruned_count, compacted_count, conflicts_resolved_count
                FROM sleep_log
                WHERE dry_run = 0
                ORDER BY created_at DESC, id DESC
                LIMIT 1
                "#,
                [],
                |row| {
                    Ok(SleepStats {
                        last_sleep_at: row.get::<_, Option<String>>(0)?,
                        items_pruned: row.get::<_, i64>(1)? as u64,
                        items_compacted: row.get::<_, i64>(2)? as u64,
                        conflicts_resolved: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        Ok(stats)
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
        self.search_fts_filtered(
            query,
            FtsSearchScope {
                wing,
                room,
                project_mode,
                project_id,
                filters: FtsMetadataFilters::default(),
            },
            limit,
        )
    }

    pub fn search_fts_filtered(
        &self,
        query: &str,
        scope: FtsSearchScope<'_>,
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
                    scope.wing,
                    scope.room,
                    scope.project_mode,
                    scope.project_id,
                    limit,
                    scope.filters.memory_kind,
                    scope.filters.domain,
                    scope.filters.field,
                    scope.filters.tier,
                    scope.filters.status,
                    scope.filters.anchor_kind,
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
        limit: usize,
    ) -> Result<Vec<TunnelDrawer>, DbError> {
        let Some(current_project_id) = current_project_id else {
            return Ok(Vec::new());
        };

        let sql_limit =
            i64::try_from(limit).map_err(|_| DbError::InvalidSourceType("limit".to_string()))?;
        let mut stmt = self.conn.prepare(&format!(
            r#"
            SELECT {DRAWER_SELECT_COLUMNS}, project_id
            FROM drawers
            WHERE deleted_at IS NULL
              AND room = ?1
              AND id != ?2
              AND project_id IS NOT NULL
              AND project_id != ?3
            ORDER BY CAST(added_at AS INTEGER) DESC, id DESC
            LIMIT ?4
            "#,
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params![room, exclude_drawer_id, current_project_id, sql_limit],
                |row| {
                    let drawer = drawer_from_row(row).map_err(row_decode_error)?;
                    // DRAWER_SELECT_COLUMNS is 32 columns (0-31); project_id appended at 32.
                    let project_id = row.get::<_, Option<String>>(32)?;
                    Ok(TunnelDrawer {
                        drawer,
                        target_project_id: project_id,
                    })
                },
            )?
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

    pub fn triple_exists(&self, triple_id: &str) -> Result<bool, DbError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM triples WHERE id = ?1",
            params![triple_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
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
                embedding FLOAT[{dim}] distance_metric={VECTOR_DISTANCE_METRIC}{project_column}
            );
            "#
        ))?;
        Ok(())
    }

    /// Number of rows currently in the `drawer_vectors` vec0 table, returning 0
    /// when the table is absent. Counting the vec0 virtual table directly is
    /// valid because every `Database` connection registers the sqlite-vec
    /// extension (a plain sqlite connection would fail with `no such module:
    /// vec0`). This is the row-count source of truth for the `mempal status`
    /// empty-index signal and the reindex atomicity checkpoint (issue #302).
    pub fn vector_row_count(&self) -> Result<i64, DbError> {
        if !self.table_exists("drawer_vectors")? {
            return Ok(0);
        }
        let count = self
            .conn
            .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| {
                row.get::<_, i64>(0)
            })?;
        Ok(count)
    }

    /// Detect whether the live `drawer_vectors` table declares the `+project_id`
    /// auxiliary column (present once `fork_ext_version >= 5`). Read from the
    /// stored DDL so the stash restores the table's exact original shape.
    fn vector_table_has_project_id(&self) -> Result<bool, DbError> {
        let sql: Option<String> = self
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawer_vectors'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(sql.is_some_and(|sql| sql.contains("project_id")))
    }

    /// Stage the current `drawer_vectors` rows into a transient `TEMP` table
    /// before a metric/dim-change recreate so they can be restored if the
    /// reindex embeds zero vectors (issue #302). Returns `Ok(None)` when there
    /// is nothing worth protecting (no table, or an already-empty table) — the
    /// caller then proceeds without rollback bookkeeping.
    ///
    /// Strategy note: vec0 has no `ALTER TABLE RENAME` (sqlite-vec 0.1.9
    /// xRename=0), so rather than embedding into a staging table and renaming it
    /// over the original, this stages the OLD rows aside (the proven
    /// `db_fork_ext` copy primitive) while the embed phase keeps writing
    /// `drawer_vectors` in place. On a zero-embedded failure
    /// [`Self::restore_vectors_from_stash`] copies the rows back unchanged.
    pub fn stash_vectors_before_recreate(&self) -> Result<Option<ReindexVectorStash>, DbError> {
        let Some(old_metric) = self.vector_table_distance_metric()? else {
            return Ok(None); // no table -> nothing to protect
        };
        let Some(old_dim) = self.embedding_dim()? else {
            return Ok(None); // table exists but is empty -> nothing to restore
        };
        let row_count = self.vector_row_count()?;
        if row_count == 0 {
            return Ok(None);
        }
        let had_project_id = self.vector_table_has_project_id()?;
        let table = REINDEX_VECTOR_STASH_TABLE;
        let project_col = if had_project_id { ", project_id" } else { "" };
        let project_col_def = if had_project_id {
            ", project_id TEXT"
        } else {
            ""
        };
        self.conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS {table};
             CREATE TEMP TABLE {table} (
                 id TEXT PRIMARY KEY,
                 embedding BLOB NOT NULL{project_col_def}
             );
             INSERT INTO {table} (id, embedding{project_col})
                 SELECT id, embedding{project_col} FROM drawer_vectors;"
        ))?;
        Ok(Some(ReindexVectorStash {
            old_dim,
            old_metric,
            had_project_id,
            row_count,
        }))
    }

    /// Drop the reindex stash after a successful (>= 1 embedded) reindex.
    pub fn discard_reindex_stash(&self) -> Result<(), DbError> {
        let table = REINDEX_VECTOR_STASH_TABLE;
        self.conn
            .execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
        Ok(())
    }

    /// Restore the pre-recreate `drawer_vectors` rows from the stash, recreating
    /// the table with its original dimension, distance metric, and `project_id`
    /// column. Called when a recreate-reindex embeds zero vectors so the store
    /// is left exactly as it started (issue #302) rather than with an empty
    /// index. Consumes the stash table at the end.
    pub fn restore_vectors_from_stash(&self, stash: &ReindexVectorStash) -> Result<(), DbError> {
        let metric = validate_vector_metric(&stash.old_metric)?;
        let dim = stash.old_dim;
        let table = REINDEX_VECTOR_STASH_TABLE;
        let project_col = if stash.had_project_id {
            ", project_id"
        } else {
            ""
        };
        let project_col_def = if stash.had_project_id {
            ", +project_id TEXT"
        } else {
            ""
        };
        self.conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS drawer_vectors;
             CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                 id TEXT PRIMARY KEY,
                 embedding FLOAT[{dim}] distance_metric={metric}{project_col_def}
             );
             INSERT INTO drawer_vectors (id, embedding{project_col})
                 SELECT id, embedding{project_col} FROM {table};
             DROP TABLE IF EXISTS {table};"
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

    /// Load all active (non-deleted) drawers for importance rescoring.
    ///
    /// When `only_zero` is true, returns only drawers where importance is 0 or NULL.
    pub fn drawers_for_rescore(&self, only_zero: bool) -> Result<Vec<Drawer>, DbError> {
        let sql = if only_zero {
            format!(
                r#"
                SELECT {DRAWER_SELECT_COLUMNS}
                FROM drawers
                WHERE deleted_at IS NULL AND COALESCE(importance, 0) = 0
                ORDER BY id ASC
                "#
            )
        } else {
            format!(
                r#"
                SELECT {DRAWER_SELECT_COLUMNS}
                FROM drawers
                WHERE deleted_at IS NULL
                ORDER BY id ASC
                "#
            )
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| drawer_from_row(row).map_err(row_decode_error))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Apply importance scores in batched `BEGIN IMMEDIATE` transactions.
    ///
    /// Each batch of up to 1000 rows is committed independently so that concurrent
    /// readers (WAL mode) are not blocked for the full duration of large rescores.
    /// Returns the total number of rows updated.
    pub fn bulk_update_importance(&self, updates: &[(String, i32)]) -> Result<usize, DbError> {
        const BATCH_SIZE: usize = 1000;
        let mut total = 0usize;
        for chunk in updates.chunks(BATCH_SIZE) {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<usize, DbError> {
                let mut count = 0usize;
                for (id, importance) in chunk {
                    self.conn.execute(
                        "UPDATE drawers SET importance = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                        rusqlite::params![importance, id],
                    )?;
                    count += 1;
                }
                Ok(count)
            })();
            match result {
                Ok(n) => {
                    self.conn.execute_batch("COMMIT")?;
                    total += n;
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
        }
        Ok(total)
    }

    // -----------------------------------------------------------------------
    // P13: importance decay — access tracking + boost + stale penalty
    // -----------------------------------------------------------------------

    /// Atomically update access tracking and recompute `effective_importance`
    /// for a batch of drawer IDs using a single SQL UPDATE per drawer.
    ///
    /// Uses WAL + deferred transaction (NOT `BEGIN IMMEDIATE`) to avoid lock
    /// contention during high-frequency concurrent searches.
    pub fn update_access_fields_batch(
        &self,
        drawer_ids: &[String],
        now_ms: i64,
        decay_rate: f64,
        floor: f64,
        boost_cap: f64,
    ) -> Result<(), DbError> {
        if drawer_ids.is_empty() {
            return Ok(());
        }
        // Single UPDATE per drawer; all arithmetic is done inside SQLite to
        // guarantee atomicity without a read-modify-write round-trip.
        let sql = r#"
            UPDATE drawers SET
                last_accessed_at = ?1,
                access_count = access_count + 1,
                effective_importance = (
                    CAST(COALESCE(importance, 0) AS REAL)
                    * MIN(1.0, MAX(
                        EXP(-?2 * MAX(0, ?1 - COALESCE(last_accessed_at, strftime('%s', added_at) * 1000, CAST(added_at AS INTEGER) * 1000)) / 86400000.0),
                        ?3
                    ))
                    + MIN(COALESCE(accumulated_boost, 0.0), ?4)
                ) * COALESCE(stale_penalty_applied, 1.0)
            WHERE id = ?5 AND deleted_at IS NULL
        "#;
        let mut stmt = self.conn.prepare_cached(sql)?;
        self.conn.execute_batch("BEGIN")?;
        let result: Result<(), DbError> = (|| {
            for id in drawer_ids {
                stmt.execute(rusqlite::params![now_ms, decay_rate, floor, boost_cap, id])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Atomically apply session-ingest boost and recompute `effective_importance`
    /// for a batch of drawer IDs.
    ///
    /// Note: SQLite SET clauses use pre-update column values, so
    /// `effective_importance` must reference `(accumulated_boost + boost_per_access)`
    /// explicitly rather than relying on the already-updated `accumulated_boost`.
    pub fn apply_ingest_boost_batch(
        &self,
        drawer_ids: &[String],
        now_ms: i64,
        boost_per_access: f64,
        boost_cap: f64,
        decay_rate: f64,
        floor: f64,
    ) -> Result<(), DbError> {
        if drawer_ids.is_empty() {
            return Ok(());
        }
        let sql = r#"
            UPDATE drawers SET
                accumulated_boost = COALESCE(accumulated_boost, 0.0) + ?1,
                effective_importance = (
                    CAST(COALESCE(importance, 0) AS REAL)
                    * MIN(1.0, MAX(
                        EXP(-?3 * MAX(0, ?2 - COALESCE(last_accessed_at, strftime('%s', added_at) * 1000, CAST(added_at AS INTEGER) * 1000)) / 86400000.0),
                        ?4
                    ))
                    + MIN(COALESCE(accumulated_boost, 0.0) + ?1, ?5)
                ) * COALESCE(stale_penalty_applied, 1.0)
            WHERE id = ?6 AND deleted_at IS NULL
        "#;
        let mut stmt = self.conn.prepare_cached(sql)?;
        self.conn.execute_batch("BEGIN")?;
        let result: Result<(), DbError> = (|| {
            for id in drawer_ids {
                stmt.execute(rusqlite::params![
                    boost_per_access,
                    now_ms,
                    decay_rate,
                    floor,
                    boost_cap,
                    id
                ])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Apply stale penalty: persist the multiplier in `stale_penalty_applied` and
    /// immediately reduce `effective_importance` for a specific drawer.
    /// `stale_penalty_applied` survives `recompute_all_effective_importance`.
    ///
    /// The `effective_importance` update derives the pre-penalty value from
    /// `NULLIF(effective_importance, 0.0)` falling back to
    /// `importance * COALESCE(stale_penalty_applied, 1.0)` — the same cumulative
    /// expression the read fallback uses. So a legacy row stuck at the 0.0
    /// sentinel (GitHub #309) re-penalizes coherently: a row already carrying
    /// `stale_penalty_applied = 0.5` composes to `importance * 0.5 * new_penalty`
    /// rather than dropping the prior penalty and yielding `importance *
    /// new_penalty`. SQLite resolves every `SET` right-hand side against the
    /// row's pre-UPDATE values, so the `stale_penalty_applied` read here is the
    /// OLD multiplier, not the newly-assigned one on the line above. This keeps
    /// the cumulative stale penalty order-independent and persists a non-zero
    /// value instead of `0.0 * penalty = 0.0` — which would otherwise bypass the
    /// stale down-rank and surface outdated memory at full importance via the
    /// read fallback.
    pub fn apply_stale_penalty_to_drawer(
        &self,
        drawer_id: &str,
        stale_penalty: f64,
    ) -> Result<(), DbError> {
        self.conn.execute(
            r#"UPDATE drawers SET
                stale_penalty_applied = COALESCE(stale_penalty_applied, 1.0) * ?1,
                effective_importance = COALESCE(NULLIF(effective_importance, 0.0), CAST(COALESCE(importance, 0) AS REAL) * COALESCE(stale_penalty_applied, 1.0)) * ?1
            WHERE id = ?2 AND deleted_at IS NULL"#,
            rusqlite::params![stale_penalty, drawer_id],
        )?;
        Ok(())
    }

    /// Batch recompute `effective_importance` for all active drawers using the
    /// provided decay parameters. Used by `mempal recompute-importance --effective`.
    ///
    /// Runs in batches of 1000 using `BEGIN IMMEDIATE` to avoid blocking readers.
    pub fn recompute_all_effective_importance(
        &self,
        now_ms: i64,
        decay_rate: f64,
        floor: f64,
        boost_cap: f64,
    ) -> Result<usize, DbError> {
        const BATCH_SIZE: i64 = 1000;
        let mut total = 0usize;
        let mut last_rowid: i64 = -1;

        loop {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            let result: std::result::Result<(usize, i64), DbError> = (|| {
                let sql = r#"
                    UPDATE drawers SET
                        effective_importance = (
                            CAST(COALESCE(importance, 0) AS REAL)
                            * MIN(1.0, MAX(
                                EXP(-?1 * MAX(0, ?2 - COALESCE(last_accessed_at, strftime('%s', added_at) * 1000, CAST(added_at AS INTEGER) * 1000)) / 86400000.0),
                                ?3
                            ))
                            + MIN(COALESCE(accumulated_boost, 0.0), ?4)
                        ) * COALESCE(stale_penalty_applied, 1.0)
                    WHERE rowid IN (
                        SELECT rowid FROM drawers
                        WHERE deleted_at IS NULL AND rowid > ?5
                        ORDER BY rowid ASC LIMIT ?6
                    )
                "#;
                let updated = self.conn.execute(
                    sql,
                    rusqlite::params![decay_rate, now_ms, floor, boost_cap, last_rowid, BATCH_SIZE],
                )?;
                // Find the last rowid processed in this batch.
                let new_last_rowid: i64 = self
                    .conn
                    .query_row(
                        "SELECT MAX(rowid) FROM (SELECT rowid FROM drawers WHERE deleted_at IS NULL AND rowid > ?1 ORDER BY rowid ASC LIMIT ?2)",
                        rusqlite::params![last_rowid, BATCH_SIZE],
                        |row| row.get::<_, Option<i64>>(0),
                    )?
                    .unwrap_or(last_rowid);
                Ok((updated, new_last_rowid))
            })();
            match result {
                Ok((n, new_last_rowid)) => {
                    self.conn.execute_batch("COMMIT")?;
                    total += n;
                    if n == 0 || new_last_rowid == last_rowid {
                        break;
                    }
                    last_rowid = new_last_rowid;
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
        }
        Ok(total)
    }

    /// List drawers with `effective_importance < threshold`, ordered by
    /// `effective_importance ASC` (most decayed first). Used by `mempal audit --stale`.
    ///
    /// Each tuple: `(id, wing, room, effective_importance, access_count, last_accessed_at_ms)`.
    #[allow(clippy::type_complexity)]
    pub fn drawers_below_importance_threshold(
        &self,
        threshold: f64,
        limit: usize,
    ) -> Result<Vec<(String, String, Option<String>, f64, i64, Option<i64>)>, DbError> {
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, wing, room, effective_importance, access_count, last_accessed_at
            FROM drawers
            WHERE deleted_at IS NULL AND effective_importance < ?1
            ORDER BY effective_importance ASC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt
            .query_map(rusqlite::params![threshold, limit_i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Normalise `added_at` values in batched `BEGIN IMMEDIATE` transactions.
    ///
    /// Each batch of up to 1000 rows is committed independently so concurrent
    /// readers (WAL mode) are not blocked for the full duration.  Returns the
    /// total number of rows updated.
    ///
    /// `updates`: slice of `(drawer_id, new_added_at)` pairs.
    pub fn bulk_update_added_at(&self, updates: &[(String, String)]) -> Result<usize, DbError> {
        const BATCH_SIZE: usize = 1000;
        let mut total = 0usize;
        for chunk in updates.chunks(BATCH_SIZE) {
            self.conn.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<usize, DbError> {
                let mut count = 0usize;
                for (id, new_added_at) in chunk {
                    self.conn.execute(
                        "UPDATE drawers SET added_at = ?1 WHERE id = ?2 AND deleted_at IS NULL",
                        rusqlite::params![new_added_at, id],
                    )?;
                    count += 1;
                }
                Ok(count)
            })();
            match result {
                Ok(n) => {
                    self.conn.execute_batch("COMMIT")?;
                    total += n;
                }
                Err(e) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
        }
        Ok(total)
    }

    fn with_immediate_tx<T, F>(&self, work: F) -> Result<T, DbError>
    where
        F: FnOnce() -> Result<T, DbError>,
    {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match work() {
            Ok(value) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    // --- Lease coordination ---

    pub fn lease_acquire(
        &self,
        resource_path: &str,
        holder_id: &str,
        ttl_secs: u64,
        metadata: Option<&str>,
    ) -> Result<bool, DbError> {
        self.conn.execute(
            "DELETE FROM leases WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
        )?;
        let expires_at = crate::cowork::peek::format_rfc3339(
            std::time::SystemTime::now() + std::time::Duration::from_secs(ttl_secs),
        );
        let rows = self.conn.execute(
            "INSERT OR IGNORE INTO leases (resource_path, holder_id, expires_at, metadata) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![resource_path, holder_id, &expires_at, metadata],
        )?;
        if rows > 0 {
            return Ok(true);
        }
        let existing_holder: Option<String> = self
            .conn
            .query_row(
                "SELECT holder_id FROM leases WHERE resource_path = ?1",
                [resource_path],
                |row| row.get(0),
            )
            .optional()?;
        match existing_holder {
            Some(ref h) if h == holder_id => {
                self.conn.execute(
                    "UPDATE leases SET expires_at = ?1 WHERE resource_path = ?2 AND holder_id = ?3",
                    rusqlite::params![&expires_at, resource_path, holder_id],
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    pub fn lease_release(&self, resource_path: &str, holder_id: &str) -> Result<bool, DbError> {
        let rows = self.conn.execute(
            "DELETE FROM leases WHERE resource_path = ?1 AND holder_id = ?2",
            rusqlite::params![resource_path, holder_id],
        )?;
        Ok(rows > 0)
    }

    pub fn lease_renew(
        &self,
        resource_path: &str,
        holder_id: &str,
        ttl_secs: u64,
    ) -> Result<bool, DbError> {
        let expires_at = crate::cowork::peek::format_rfc3339(
            std::time::SystemTime::now() + std::time::Duration::from_secs(ttl_secs),
        );
        let rows = self.conn.execute(
            "UPDATE leases SET expires_at = ?1 WHERE resource_path = ?2 AND holder_id = ?3",
            rusqlite::params![&expires_at, resource_path, holder_id],
        )?;
        Ok(rows > 0)
    }

    pub fn lease_status(
        &self,
        resource_path: Option<&str>,
    ) -> Result<Vec<crate::core::types::LeaseInfo>, DbError> {
        self.conn.execute(
            "DELETE FROM leases WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
        )?;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut results = Vec::new();
        let collect_row = |row: &rusqlite::Row| -> rusqlite::Result<crate::core::types::LeaseInfo> {
            let exp: String = row.get(3)?;
            let remaining = crate::cowork::peek::parse_rfc3339(&exp)
                .map(|e| (e - now_secs).max(0))
                .unwrap_or(0);
            Ok(crate::core::types::LeaseInfo {
                resource_path: row.get(0)?,
                holder_id: row.get(1)?,
                acquired_at: row.get(2)?,
                expires_at: exp,
                metadata: row.get(4)?,
                remaining_secs: remaining,
            })
        };
        if let Some(path) = resource_path {
            let mut stmt = self.conn.prepare(
                "SELECT resource_path, holder_id, acquired_at, expires_at, metadata \
                 FROM leases WHERE resource_path = ?1",
            )?;
            let rows = stmt.query_map([path], collect_row)?;
            for row in rows {
                results.push(row?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT resource_path, holder_id, acquired_at, expires_at, metadata FROM leases",
            )?;
            let rows = stmt.query_map([], collect_row)?;
            for row in rows {
                results.push(row?);
            }
        }
        Ok(results)
    }

    pub fn lease_cleanup_expired(&self) -> Result<usize, DbError> {
        let rows = self.conn.execute(
            "DELETE FROM leases WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            [],
        )?;
        Ok(rows)
    }

    // --- Runtime writer lease coordination ---

    pub fn runtime_writer_lease_acquire(
        &self,
        name: &str,
        owner: &str,
        mode: &str,
        ttl_secs: u64,
        metadata_json: Option<&str>,
    ) -> Result<Option<RuntimeWriterLease>, DbError> {
        self.runtime_writer_lease_acquire_with_cleanup_policy(
            name,
            owner,
            mode,
            ttl_secs,
            metadata_json,
            false,
        )
    }

    pub fn runtime_writer_lease_acquire_preserving_live_holders(
        &self,
        name: &str,
        owner: &str,
        mode: &str,
        ttl_secs: u64,
        metadata_json: Option<&str>,
    ) -> Result<Option<RuntimeWriterLease>, DbError> {
        self.runtime_writer_lease_acquire_with_cleanup_policy(
            name,
            owner,
            mode,
            ttl_secs,
            metadata_json,
            true,
        )
    }

    fn runtime_writer_lease_acquire_with_cleanup_policy(
        &self,
        name: &str,
        owner: &str,
        mode: &str,
        ttl_secs: u64,
        metadata_json: Option<&str>,
        preserve_live_holders: bool,
    ) -> Result<Option<RuntimeWriterLease>, DbError> {
        let mut session_id = String::new();
        let mut acquired = false;
        let mut acquired_at = String::new();
        let mut expires_at = String::new();
        let pid = std::process::id();
        let boot_id = runtime_boot_id();
        self.with_immediate_tx(|| {
            self.runtime_writer_lease_cleanup_expired_tx(preserve_live_holders)?;
            session_id = runtime_writer_session_id(name, owner);
            let now_time = SystemTime::now();
            acquired_at = crate::cowork::peek::format_rfc3339(now_time);
            expires_at = crate::cowork::peek::format_rfc3339(
                now_time + std::time::Duration::from_secs(ttl_secs),
            );
            let rows = self.conn.execute(
                "INSERT OR IGNORE INTO runtime_writer_leases \
                 (name, owner, pid, boot_id, session_id, acquired_at, expires_at, heartbeat_at, mode, metadata_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, ?9)",
                params![
                    name,
                    owner,
                    pid as i64,
                    &boot_id,
                    &session_id,
                    &acquired_at,
                    &expires_at,
                    mode,
                    metadata_json
                ],
            )?;
            acquired = rows > 0;
            Ok(())
        })?;
        if acquired {
            let remaining_secs = i64::try_from(ttl_secs).unwrap_or(i64::MAX);
            Ok(Some(RuntimeWriterLease {
                name: name.to_string(),
                owner: owner.to_string(),
                pid,
                boot_id,
                session_id,
                acquired_at: acquired_at.clone(),
                expires_at,
                heartbeat_at: acquired_at,
                mode: mode.to_string(),
                metadata_json: metadata_json.map(ToOwned::to_owned),
                remaining_secs,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn runtime_writer_lease_renew(
        &self,
        name: &str,
        owner: &str,
        session_id: &str,
        ttl_secs: u64,
    ) -> Result<bool, DbError> {
        let mut renewed = false;
        self.with_immediate_tx(|| {
            self.runtime_writer_lease_cleanup_expired_tx(true)?;
            let now = crate::cowork::peek::format_rfc3339(SystemTime::now());
            let expires_at = crate::cowork::peek::format_rfc3339(
                SystemTime::now() + std::time::Duration::from_secs(ttl_secs),
            );
            let rows = self.conn.execute(
                "UPDATE runtime_writer_leases \
                 SET expires_at = ?4, heartbeat_at = ?5 \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                params![name, owner, session_id, &expires_at, &now],
            )?;
            renewed = rows > 0;
            Ok(())
        })?;
        Ok(renewed)
    }

    pub fn runtime_writer_lease_is_active(
        &self,
        name: &str,
        owner: &str,
        session_id: &str,
    ) -> Result<bool, DbError> {
        self.with_immediate_tx(|| self.runtime_writer_lease_cleanup_expired_tx(true))?;
        let active = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runtime_writer_leases
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3
             )",
            params![name, owner, session_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(active != 0)
    }

    pub fn runtime_writer_lease_release(
        &self,
        name: &str,
        owner: &str,
        session_id: &str,
    ) -> Result<bool, DbError> {
        self.with_immediate_tx(|| {
            let rows = self.conn.execute(
                "DELETE FROM runtime_writer_leases \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                params![name, owner, session_id],
            )?;
            Ok(rows > 0)
        })
    }

    pub fn runtime_writer_lease_status(
        &self,
        name: Option<&str>,
    ) -> Result<Vec<RuntimeWriterLease>, DbError> {
        self.with_immediate_tx(|| self.runtime_writer_lease_cleanup_expired_tx(true))?;
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);
        let collect_row = |row: &rusqlite::Row| -> rusqlite::Result<RuntimeWriterLease> {
            let expires_at: String = row.get(6)?;
            let remaining_secs = crate::cowork::peek::parse_rfc3339(&expires_at)
                .map(|expires| (expires - now_secs).max(0))
                .unwrap_or(0);
            let pid_i64: i64 = row.get(2)?;
            Ok(RuntimeWriterLease {
                name: row.get(0)?,
                owner: row.get(1)?,
                pid: u32::try_from(pid_i64).unwrap_or(0),
                boot_id: row.get(3)?,
                session_id: row.get(4)?,
                acquired_at: row.get(5)?,
                expires_at,
                heartbeat_at: row.get(7)?,
                mode: row.get(8)?,
                metadata_json: row.get(9)?,
                remaining_secs,
            })
        };
        let mut leases = Vec::new();
        if let Some(name) = name {
            let mut stmt = self.conn.prepare(
                "SELECT name, owner, pid, boot_id, session_id, acquired_at, expires_at, heartbeat_at, mode, metadata_json \
                 FROM runtime_writer_leases WHERE name = ?1",
            )?;
            for row in stmt.query_map([name], collect_row)? {
                leases.push(row?);
            }
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT name, owner, pid, boot_id, session_id, acquired_at, expires_at, heartbeat_at, mode, metadata_json \
                 FROM runtime_writer_leases ORDER BY name ASC",
            )?;
            for row in stmt.query_map([], collect_row)? {
                leases.push(row?);
            }
        }
        Ok(leases)
    }

    pub fn runtime_writer_lease_has_live_daemon(&self, name: &str) -> Result<bool, DbError> {
        let mut stmt = self.conn.prepare(
            "SELECT pid, boot_id \
             FROM runtime_writer_leases \
             WHERE name = ?1 AND mode = 'daemon'",
        )?;
        let rows = stmt.query_map([name], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (pid_i64, boot_id) = row?;
            if u32::try_from(pid_i64)
                .ok()
                .is_some_and(|pid| runtime_writer_process_is_live_holder(pid, boot_id.as_deref()))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn runtime_writer_lease_cleanup_expired(&self) -> Result<usize, DbError> {
        self.with_immediate_tx(|| self.runtime_writer_lease_cleanup_expired_tx(false))
    }

    fn runtime_writer_lease_cleanup_expired_tx(
        &self,
        preserve_live_holders: bool,
    ) -> Result<usize, DbError> {
        let expired = {
            let mut stmt = self.conn.prepare(
                "SELECT name, owner, pid, boot_id, session_id \
                 FROM runtime_writer_leases \
                 WHERE expires_at < strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            )?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        let mut removed = 0;
        for (name, owner, pid, boot_id, session_id) in expired {
            let preserve_live_holder = preserve_live_holders
                && u32::try_from(pid).ok().is_some_and(|pid| {
                    runtime_writer_process_is_live_holder(pid, boot_id.as_deref())
                });
            if preserve_live_holder {
                continue;
            }
            removed += self.conn.execute(
                "DELETE FROM runtime_writer_leases \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                params![name, owner, session_id],
            )?;
        }
        Ok(removed)
    }
}

pub fn find_similar_clusters(
    conn: &Connection,
    wing: Option<&str>,
    room: Option<&str>,
    project_id: Option<&str>,
    threshold: f64,
    min_cluster_size: usize,
) -> Result<Vec<Vec<(String, f64)>>, DbError> {
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        return Err(DbError::InvalidDrawerMetadata(
            "compaction threshold must be finite and between 0.0 and 1.0".to_string(),
        ));
    }
    if min_cluster_size < 2 {
        return Err(DbError::InvalidDrawerMetadata(
            "minimum compaction cluster size must be at least 2".to_string(),
        ));
    }
    if !table_exists_conn(conn, "drawer_vectors")? {
        return Ok(Vec::new());
    }

    let mut sql = String::from(
        r#"
        SELECT da.id,
               db.id,
               CAST(1.0 - vec_distance_cosine(va.embedding, vb.embedding) AS REAL) AS similarity
        FROM drawer_vectors va
        JOIN drawer_vectors vb ON va.id < vb.id
        JOIN drawers da ON da.id = va.id
        JOIN drawers db ON db.id = vb.id
        WHERE da.deleted_at IS NULL
          AND db.deleted_at IS NULL
          AND da.compacted_into IS NULL
          AND db.compacted_into IS NULL
          AND vec_distance_cosine(va.embedding, vb.embedding) < ?1
        "#,
    );
    let mut values = vec![SqlValue::Real(1.0 - threshold)];

    for (column, value) in [("wing", wing), ("room", room)] {
        if let Some(value) = value {
            values.push(SqlValue::Text(value.to_string()));
            let placeholder = values.len();
            sql.push_str(&format!(
                " AND da.{column} = ?{placeholder} AND db.{column} = ?{placeholder}"
            ));
        }
    }
    if let Some(project_id) = project_id {
        values.push(SqlValue::Text(project_id.to_string()));
        let placeholder = values.len();
        sql.push_str(&format!(
            " AND da.project_id = ?{placeholder} AND db.project_id = ?{placeholder}"
        ));
    } else {
        sql.push_str(" AND da.project_id IS NULL AND db.project_id IS NULL");
    }

    let mut statement = conn.prepare(&sql)?;
    let pairs = statement
        .query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut union_find = UnionFind::default();
    let mut similarity_totals: HashMap<String, (f64, usize)> = HashMap::new();
    for (left_id, right_id, similarity) in pairs {
        if similarity <= threshold {
            continue;
        }
        union_find.union(&left_id, &right_id);
        let left_total = similarity_totals.entry(left_id).or_insert((0.0, 0));
        left_total.0 += similarity;
        left_total.1 += 1;
        let right_total = similarity_totals.entry(right_id).or_insert((0.0, 0));
        right_total.0 += similarity;
        right_total.1 += 1;
    }

    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for drawer_id in similarity_totals.keys().cloned().collect::<Vec<_>>() {
        let root = union_find.find(&drawer_id);
        grouped.entry(root).or_default().push(drawer_id);
    }

    let mut clusters = grouped
        .into_values()
        .filter(|component| component.len() >= min_cluster_size)
        .map(|mut component| {
            component.sort();
            let mut cluster = component
                .into_iter()
                .map(|drawer_id| {
                    let (total, count) = similarity_totals
                        .get(&drawer_id)
                        .copied()
                        .unwrap_or((0.0, 1));
                    (drawer_id, total / count as f64)
                })
                .collect::<Vec<_>>();
            cluster.sort_by(|left, right| {
                right
                    .1
                    .total_cmp(&left.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            cluster
        })
        .collect::<Vec<_>>();

    clusters.sort_by(|left, right| {
        let left_avg = cluster_average_similarity(left);
        let right_avg = cluster_average_similarity(right);
        right
            .len()
            .cmp(&left.len())
            .then_with(|| right_avg.total_cmp(&left_avg))
            .then_with(|| left[0].0.cmp(&right[0].0))
    });
    Ok(clusters)
}

fn cluster_average_similarity(cluster: &[(String, f64)]) -> f64 {
    if cluster.is_empty() {
        return 0.0;
    }
    cluster
        .iter()
        .map(|(_, similarity)| similarity)
        .sum::<f64>()
        / cluster.len() as f64
}

#[derive(Default)]
struct UnionFind {
    parents: HashMap<String, String>,
    ranks: HashMap<String, u8>,
}

impl UnionFind {
    fn ensure(&mut self, id: &str) {
        self.parents
            .entry(id.to_string())
            .or_insert_with(|| id.to_string());
        self.ranks.entry(id.to_string()).or_insert(0);
    }

    fn find(&mut self, id: &str) -> String {
        self.ensure(id);
        let parent = self
            .parents
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string());
        if parent == id {
            return parent;
        }
        let root = self.find(&parent);
        self.parents.insert(id.to_string(), root.clone());
        root
    }

    fn union(&mut self, left: &str, right: &str) {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return;
        }

        let left_rank = self.ranks.get(&left_root).copied().unwrap_or(0);
        let right_rank = self.ranks.get(&right_root).copied().unwrap_or(0);
        if left_rank < right_rank {
            self.parents.insert(left_root, right_root);
        } else if left_rank > right_rank {
            self.parents.insert(right_root, left_root);
        } else {
            self.parents.insert(right_root.clone(), left_root.clone());
            self.ranks.insert(left_root, left_rank.saturating_add(1));
        }
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
            // Some fork installs added content_hash early and bumped user_version
            // past V5 without ever applying the upstream V5 drawer columns.
            // Repair V5 by column existence, not only by schema version.
            ensure_v5_drawers_schema(conn, current_version)?;
            continue;
        }
        if migration.version == 7
            && current_version < 7
            && drawers_column_exists(conn, "normalize_version")?
        {
            apply_migration_atomic(conn, &V7_ALREADY_APPLIED_MIGRATION)?;
            continue;
        }
        if migration.version == 10 {
            ensure_v10_drawers_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        if migration.version == 11 {
            ensure_v11_source_confidence_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        if migration.version == 12 {
            ensure_v12_compaction_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        if migration.version == 13 {
            ensure_v13_typed_pinned_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        if migration.version == 14 {
            ensure_v14_sleep_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        if migration.version == 15 {
            ensure_v15_crystallize_schema(conn, read_user_version(conn)?)?;
            continue;
        }
        apply_migration_atomic(conn, migration)?;
        if migration.version == 17 {
            repopulate_fts_contentless(conn)?;
        }
    }

    if current_version >= 5 {
        ensure_v5_drawers_schema(conn, read_user_version(conn)?)?;
    }
    if read_user_version(conn)? >= 11 {
        ensure_v11_source_confidence_schema(conn, read_user_version(conn)?)?;
    }
    if read_user_version(conn)? >= 12 {
        ensure_v12_compaction_schema(conn, read_user_version(conn)?)?;
    }
    if read_user_version(conn)? >= 13 {
        ensure_v13_typed_pinned_schema(conn, read_user_version(conn)?)?;
    }
    if read_user_version(conn)? >= 14 {
        ensure_v14_sleep_schema(conn, read_user_version(conn)?)?;
    }
    if read_user_version(conn)? >= 15 {
        ensure_v15_crystallize_schema(conn, read_user_version(conn)?)?;
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

fn repopulate_fts_contentless(conn: &Connection) -> Result<(), DbError> {
    let mut stmt = conn.prepare("SELECT rowid, content FROM drawers WHERE deleted_at IS NULL")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for (rowid, content) in rows {
        let tokenized = fts_tokenize_content(&content);
        conn.execute(
            "INSERT INTO drawers_fts(rowid, content) VALUES (?1, ?2)",
            params![rowid, tokenized],
        )?;
    }
    Ok(())
}

fn ensure_v5_drawers_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_columns = V5_DRAWER_COLUMN_MIGRATIONS
        .iter()
        .filter(|column| !existing_columns.contains(column.name))
        .copied()
        .collect::<Vec<_>>();

    if missing_columns.is_empty() && current_version >= 5 {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        for column in missing_columns {
            conn.execute_batch(column.sql)?;
        }
        conn.execute_batch(V5_DRAWER_METADATA_BACKFILL_SQL)?;
        conn.execute_batch(V5_CONTENT_HASH_INDEX_SQL)?;
        if current_version < 5 {
            set_user_version(conn, 5)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    // Keep the large content_hash rewrite outside the schema transaction so
    // the WAL stays bounded on legacy installs with many historical rows.
    backfill_content_hash(conn)?;
    Ok(())
}

fn ensure_v10_drawers_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_columns = V10_DRAWER_COLUMN_MIGRATIONS
        .iter()
        .filter(|column| !existing_columns.contains(column.name))
        .copied()
        .collect::<Vec<_>>();

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        for column in missing_columns {
            conn.execute_batch(column.sql)?;
        }
        conn.execute_batch(V10_VALIDITY_BACKFILL_SQL)?;
        if current_version < 10 {
            set_user_version(conn, 10)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn ensure_v11_source_confidence_schema(
    conn: &Connection,
    current_version: u32,
) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_source_type = !existing_columns.contains("source_type");
    let missing_confidence = !existing_columns.contains("confidence");

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        if missing_source_type {
            conn.execute_batch(
                "ALTER TABLE drawers ADD COLUMN source_type TEXT NOT NULL DEFAULT 'system_generated' CHECK(source_type IN ('user_explicit', 'agent_observation', 'agent_inference', 'system_generated'));",
            )?;
        }
        if missing_confidence {
            conn.execute_batch(
                "ALTER TABLE drawers ADD COLUMN confidence REAL NOT NULL DEFAULT 0.5;",
            )?;
        }
        let rewrote_check = rewrite_drawers_source_type_check(conn)?;
        if current_version < 11 || missing_source_type || missing_confidence || rewrote_check {
            conn.execute_batch(V11_SOURCE_CONFIDENCE_BACKFILL_SQL)?;
        }
        if current_version < 11 {
            set_user_version(conn, 11)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn ensure_v12_compaction_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_compacted_into = !existing_columns.contains("compacted_into");

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        if missing_compacted_into {
            conn.execute_batch(
                "ALTER TABLE drawers ADD COLUMN compacted_into TEXT REFERENCES drawers(id);",
            )?;
        }
        conn.execute_batch(V12_COMPACTION_SCHEMA_SQL)?;
        if current_version < 12 {
            set_user_version(conn, 12)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn ensure_v13_typed_pinned_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_is_pinned = !existing_columns.contains("is_pinned");
    let missing_pin_order = !existing_columns.contains("pin_order");
    let missing_supersedes = !existing_columns.contains("supersedes");

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        if missing_is_pinned {
            conn.execute_batch(
                "ALTER TABLE drawers ADD COLUMN is_pinned INTEGER NOT NULL DEFAULT 0 CHECK(is_pinned IN (0, 1));",
            )?;
        }
        if missing_pin_order {
            conn.execute_batch("ALTER TABLE drawers ADD COLUMN pin_order INTEGER;")?;
        }
        if missing_supersedes {
            conn.execute_batch(
                "ALTER TABLE drawers ADD COLUMN supersedes TEXT REFERENCES drawers(id);",
            )?;
        }
        let rewrote_checks = rewrite_drawers_typed_ingest_checks(conn)?;
        conn.execute_batch(V13_TYPED_PINNED_SCHEMA_SQL)?;
        if rewrote_checks {
            bump_sqlite_schema_version(conn)?;
        }
        if current_version < 13 {
            set_user_version(conn, 13)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn ensure_v14_sleep_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    let existing_columns = drawers_column_names(conn)?;
    let missing_priority = !existing_columns.contains("consolidation_priority");
    let missing_last_sleep_at = !existing_columns.contains("last_sleep_at");

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        if missing_priority {
            conn.execute_batch("ALTER TABLE drawers ADD COLUMN consolidation_priority REAL;")?;
        }
        if missing_last_sleep_at {
            conn.execute_batch("ALTER TABLE drawers ADD COLUMN last_sleep_at TEXT;")?;
        }
        conn.execute_batch(V14_SLEEP_SCHEMA_SQL)?;
        if current_version < 14 {
            set_user_version(conn, 14)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn ensure_v15_crystallize_schema(conn: &Connection, current_version: u32) -> Result<(), DbError> {
    if !table_exists_conn(conn, "knowledge_cards")? {
        if current_version < 15 {
            set_user_version(conn, 15)?;
        }
        return Ok(());
    }

    let existing_columns = table_column_names(conn, "knowledge_cards")?;
    let missing_auto_generated = !existing_columns.contains("auto_generated");
    let missing_crystallization_score = !existing_columns.contains("crystallization_score");
    let missing_source_drawer_ids = !existing_columns.contains("source_drawer_ids");

    conn.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = (|| -> Result<(), DbError> {
        if missing_auto_generated {
            conn.execute_batch(
                "ALTER TABLE knowledge_cards ADD COLUMN auto_generated INTEGER NOT NULL DEFAULT 0 CHECK(auto_generated IN (0, 1));",
            )?;
        }
        if missing_crystallization_score {
            conn.execute_batch(
                "ALTER TABLE knowledge_cards ADD COLUMN crystallization_score REAL;",
            )?;
        }
        if missing_source_drawer_ids {
            conn.execute_batch(
                "ALTER TABLE knowledge_cards ADD COLUMN source_drawer_ids TEXT NOT NULL DEFAULT '[]';",
            )?;
        }
        let rewrote_status_check = rewrite_knowledge_cards_status_check(conn)?;
        conn.execute_batch(V15_CRYSTALLIZE_SCHEMA_SQL)?;
        if rewrote_status_check {
            bump_sqlite_schema_version(conn)?;
        }
        if current_version < 15 {
            set_user_version(conn, 15)?;
        }
        conn.execute_batch("COMMIT;")?;
        Ok(())
    })() {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(error);
    }

    Ok(())
}

fn rewrite_drawers_source_type_check(conn: &Connection) -> Result<bool, DbError> {
    let table_sql = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawers'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    if table_sql.contains("user_explicit")
        && table_sql.contains("agent_observation")
        && table_sql.contains("agent_inference")
        && table_sql.contains("system_generated")
    {
        return Ok(false);
    }

    let replacements = [
        (
            "source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual'))",
            "source_type TEXT NOT NULL DEFAULT 'system_generated' CHECK(source_type IN ('user_explicit', 'agent_observation', 'agent_inference', 'system_generated'))",
        ),
        (
            "source_type TEXT NOT NULL CHECK(source_type IN ('project','conversation','manual'))",
            "source_type TEXT NOT NULL DEFAULT 'system_generated' CHECK(source_type IN ('user_explicit', 'agent_observation', 'agent_inference', 'system_generated'))",
        ),
    ];
    let mut new_sql = table_sql.clone();
    for (old, new) in replacements {
        new_sql = new_sql.replace(old, new);
    }
    if new_sql == table_sql {
        return Ok(false);
    }

    conn.execute_batch("PRAGMA writable_schema = ON;")?;
    let update_result = conn.execute(
        "UPDATE sqlite_master SET sql = ?1 WHERE type = 'table' AND name = 'drawers'",
        [new_sql],
    );
    conn.execute_batch("PRAGMA writable_schema = OFF;")?;
    update_result?;
    bump_sqlite_schema_version(conn)?;
    Ok(true)
}

fn rewrite_drawers_typed_ingest_checks(conn: &Connection) -> Result<bool, DbError> {
    let table_sql = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drawers'",
        [],
        |row| row.get::<_, String>(0),
    )?;

    let replacements = [
        (
            "memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge')) DEFAULT 'evidence'",
            "memory_kind TEXT NOT NULL DEFAULT 'evidence' CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait'))",
        ),
        (
            "memory_kind TEXT NOT NULL DEFAULT 'evidence' CHECK(memory_kind IN ('evidence', 'knowledge'))",
            "memory_kind TEXT NOT NULL DEFAULT 'evidence' CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait'))",
        ),
        (
            "memory_kind TEXT NOT NULL DEFAULT 'evidence' CHECK(memory_kind IN ('evidence', 'knowledge', 'profile_fact'))",
            "memory_kind TEXT NOT NULL DEFAULT 'evidence' CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait'))",
        ),
        (
            "domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')) DEFAULT 'project'",
            "domain TEXT NOT NULL DEFAULT 'project' CHECK(domain IN ('user', 'agent', 'project', 'skill', 'global'))",
        ),
        (
            "domain TEXT NOT NULL DEFAULT 'project' CHECK(domain IN ('project', 'agent', 'skill', 'global'))",
            "domain TEXT NOT NULL DEFAULT 'project' CHECK(domain IN ('user', 'agent', 'project', 'skill', 'global'))",
        ),
        ("field TEXT NOT NULL DEFAULT 'general'", "field TEXT"),
        (
            "status TEXT CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
            "status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
        (
            "status TEXT DEFAULT 'active' CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
            "status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
        (
            "status TEXT DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
            "status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
        (
            "status TEXT DEFAULT 'active' CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
            "status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
    ];

    let mut new_sql = table_sql.clone();
    for (old, new) in replacements {
        new_sql = new_sql.replace(old, new);
    }
    if new_sql == table_sql {
        return Ok(false);
    }

    conn.execute_batch("PRAGMA writable_schema = ON;")?;
    let update_result = conn.execute(
        "UPDATE sqlite_master SET sql = ?1 WHERE type = 'table' AND name = 'drawers'",
        [new_sql],
    );
    conn.execute_batch("PRAGMA writable_schema = OFF;")?;
    update_result?;
    Ok(true)
}

fn rewrite_knowledge_cards_status_check(conn: &Connection) -> Result<bool, DbError> {
    let table_sql = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'knowledge_cards'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    if table_sql.contains("pending_review") {
        return Ok(false);
    }

    let replacements = [
        (
            "status TEXT NOT NULL CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
            "status TEXT NOT NULL CHECK(status IN ('pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
        (
            "status TEXT NOT NULL CHECK(status IN ('candidate','promoted','canonical','demoted','retired'))",
            "status TEXT NOT NULL CHECK(status IN ('pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'))",
        ),
    ];
    let mut new_sql = table_sql.clone();
    for (old, new) in replacements {
        new_sql = new_sql.replace(old, new);
    }
    if new_sql == table_sql {
        return Ok(false);
    }

    conn.execute_batch("PRAGMA writable_schema = ON;")?;
    let update_result = conn.execute(
        "UPDATE sqlite_master SET sql = ?1 WHERE type = 'table' AND name = 'knowledge_cards'",
        [new_sql],
    );
    conn.execute_batch("PRAGMA writable_schema = OFF;")?;
    update_result?;
    Ok(true)
}

fn bump_sqlite_schema_version(conn: &Connection) -> Result<(), DbError> {
    let schema_version = conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, u32>(0))?;
    conn.execute_batch(&format!(
        "PRAGMA schema_version = {};",
        schema_version.saturating_add(1)
    ))?;
    Ok(())
}

fn drawers_column_exists(conn: &Connection, column: &str) -> Result<bool, DbError> {
    let exists = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('drawers') WHERE name = ?1",
        [column],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(exists > 0)
}

fn drawers_column_names(conn: &Connection) -> Result<HashSet<String>, DbError> {
    table_column_names(conn, "drawers")
}

fn table_column_names(conn: &Connection, table_name: &str) -> Result<HashSet<String>, DbError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table_name})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(columns)
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

fn table_exists_conn(conn: &Connection, table_name: &str) -> Result<bool, DbError> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1)",
        [table_name],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(exists == 1)
}

fn append_drawers_since_filters(
    sql: &mut String,
    values: &mut Vec<SqlValue>,
    wing: Option<&str>,
    room: Option<&str>,
    project_id: Option<&str>,
) {
    for (column, value) in [("wing", wing), ("room", room), ("project_id", project_id)] {
        if let Some(value) = value {
            values.push(SqlValue::Text(value.to_string()));
            sql.push_str(&format!(" AND {column} = ?{}", values.len()));
        }
    }
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

fn reindex_source_from_row(row: &Row<'_>) -> rusqlite::Result<ReindexSource> {
    let drawer_count = row.get::<_, i64>(5)?;
    Ok(ReindexSource {
        source_root: row.get(0)?,
        source_file: row.get(1)?,
        project_id: row.get(2)?,
        wing: row.get(3)?,
        room: row.get(4)?,
        drawer_count: drawer_count as u64,
    })
}

fn reindex_source_scope_summary_from_row(
    row: &Row<'_>,
) -> rusqlite::Result<ReindexSourceScopeSummary> {
    let drawer_count = row.get::<_, i64>(0)?;
    let source_count = row.get::<_, i64>(1)?;
    Ok(ReindexSourceScopeSummary {
        drawer_count: drawer_count as u64,
        source_count: source_count as u64,
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

#[derive(Clone, Copy)]
struct DrawerColumnMigration {
    name: &'static str,
    sql: &'static str,
}

const V5_DRAWER_COLUMN_MIGRATIONS: &[DrawerColumnMigration] = &[
    DrawerColumnMigration {
        name: "memory_kind",
        sql: "ALTER TABLE drawers ADD COLUMN memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait')) DEFAULT 'evidence';",
    },
    DrawerColumnMigration {
        name: "domain",
        sql: "ALTER TABLE drawers ADD COLUMN domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')) DEFAULT 'project';",
    },
    DrawerColumnMigration {
        name: "field",
        sql: "ALTER TABLE drawers ADD COLUMN field TEXT NOT NULL DEFAULT 'general';",
    },
    DrawerColumnMigration {
        name: "anchor_kind",
        sql: "ALTER TABLE drawers ADD COLUMN anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('global', 'repo', 'worktree')) DEFAULT 'repo';",
    },
    DrawerColumnMigration {
        name: "anchor_id",
        sql: "ALTER TABLE drawers ADD COLUMN anchor_id TEXT NOT NULL DEFAULT 'repo://legacy';",
    },
    DrawerColumnMigration {
        name: "parent_anchor_id",
        sql: "ALTER TABLE drawers ADD COLUMN parent_anchor_id TEXT;",
    },
    DrawerColumnMigration {
        name: "provenance",
        sql: "ALTER TABLE drawers ADD COLUMN provenance TEXT CHECK(provenance IN ('runtime', 'research', 'human'));",
    },
    DrawerColumnMigration {
        name: "statement",
        sql: "ALTER TABLE drawers ADD COLUMN statement TEXT;",
    },
    DrawerColumnMigration {
        name: "tier",
        sql: "ALTER TABLE drawers ADD COLUMN tier TEXT CHECK(tier IN ('qi', 'shu', 'dao_ren', 'dao_tian'));",
    },
    DrawerColumnMigration {
        name: "status",
        sql: "ALTER TABLE drawers ADD COLUMN status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'));",
    },
    DrawerColumnMigration {
        name: "supporting_refs",
        sql: "ALTER TABLE drawers ADD COLUMN supporting_refs TEXT NOT NULL DEFAULT '[]';",
    },
    DrawerColumnMigration {
        name: "counterexample_refs",
        sql: "ALTER TABLE drawers ADD COLUMN counterexample_refs TEXT NOT NULL DEFAULT '[]';",
    },
    DrawerColumnMigration {
        name: "teaching_refs",
        sql: "ALTER TABLE drawers ADD COLUMN teaching_refs TEXT NOT NULL DEFAULT '[]';",
    },
    DrawerColumnMigration {
        name: "verification_refs",
        sql: "ALTER TABLE drawers ADD COLUMN verification_refs TEXT NOT NULL DEFAULT '[]';",
    },
    DrawerColumnMigration {
        name: "scope_constraints",
        sql: "ALTER TABLE drawers ADD COLUMN scope_constraints TEXT;",
    },
    DrawerColumnMigration {
        name: "trigger_hints",
        sql: "ALTER TABLE drawers ADD COLUMN trigger_hints TEXT;",
    },
    DrawerColumnMigration {
        name: "content_hash",
        sql: "ALTER TABLE drawers ADD COLUMN content_hash TEXT;",
    },
];

const V5_DRAWER_METADATA_BACKFILL_SQL: &str = r#"
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
        WHEN 'user_explicit' THEN 'human'
        WHEN 'agent_observation' THEN 'human'
        WHEN 'agent_inference' THEN 'research'
        WHEN 'system_generated' THEN 'runtime'
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

const V5_CONTENT_HASH_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_content_hash ON drawers(wing, content_hash);
"#;

const V5_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN memory_kind TEXT NOT NULL CHECK(memory_kind IN ('evidence', 'knowledge', 'atomic_fact', 'decision', 'case', 'skill', 'foresight', 'profile_fact', 'profile_trait')) DEFAULT 'evidence';
ALTER TABLE drawers ADD COLUMN domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')) DEFAULT 'project';
ALTER TABLE drawers ADD COLUMN field TEXT NOT NULL DEFAULT 'general';
ALTER TABLE drawers ADD COLUMN anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('global', 'repo', 'worktree')) DEFAULT 'repo';
ALTER TABLE drawers ADD COLUMN anchor_id TEXT NOT NULL DEFAULT 'repo://legacy';
ALTER TABLE drawers ADD COLUMN parent_anchor_id TEXT;
ALTER TABLE drawers ADD COLUMN provenance TEXT CHECK(provenance IN ('runtime', 'research', 'human'));
ALTER TABLE drawers ADD COLUMN statement TEXT;
ALTER TABLE drawers ADD COLUMN tier TEXT CHECK(tier IN ('qi', 'shu', 'dao_ren', 'dao_tian'));
ALTER TABLE drawers ADD COLUMN status TEXT CHECK(status IN ('active', 'superseded', 'pending_review', 'candidate', 'promoted', 'canonical', 'demoted', 'retired'));
ALTER TABLE drawers ADD COLUMN supporting_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN counterexample_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN teaching_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN verification_refs TEXT NOT NULL DEFAULT '[]';
ALTER TABLE drawers ADD COLUMN scope_constraints TEXT;
ALTER TABLE drawers ADD COLUMN trigger_hints TEXT;
ALTER TABLE drawers ADD COLUMN content_hash TEXT;

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
        WHEN 'user_explicit' THEN 'human'
        WHEN 'agent_observation' THEN 'human'
        WHEN 'agent_inference' THEN 'research'
        WHEN 'system_generated' THEN 'runtime'
        ELSE NULL
    END
WHERE memory_kind = 'evidence'
  AND domain = 'project'
  AND field = 'general'
  AND anchor_kind = 'repo'
  AND anchor_id = 'repo://legacy'
  AND parent_anchor_id IS NULL
  AND provenance IS NULL;

CREATE INDEX IF NOT EXISTS idx_drawers_content_hash ON drawers(wing, content_hash);
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

const V7_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN normalize_version INTEGER NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_drawers_normalize_version
    ON drawers(normalize_version)
    WHERE deleted_at IS NULL;
"#;

const V7_ALREADY_APPLIED_MIGRATION_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_normalize_version
    ON drawers(normalize_version)
    WHERE deleted_at IS NULL;
"#;

const V8_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS knowledge_cards (
    id TEXT PRIMARY KEY,
    statement TEXT NOT NULL,
    content TEXT NOT NULL,
    tier TEXT NOT NULL CHECK(tier IN ('qi', 'shu', 'dao_ren', 'dao_tian')),
    status TEXT NOT NULL CHECK(status IN ('candidate', 'promoted', 'canonical', 'demoted', 'retired')),
    domain TEXT NOT NULL CHECK(domain IN ('project', 'agent', 'skill', 'global')),
    field TEXT NOT NULL DEFAULT 'general',
    anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('global', 'repo', 'worktree')),
    anchor_id TEXT NOT NULL,
    parent_anchor_id TEXT,
    scope_constraints TEXT,
    trigger_hints TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS knowledge_evidence_links (
    id TEXT PRIMARY KEY,
    card_id TEXT NOT NULL,
    evidence_drawer_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('supporting', 'verification', 'counterexample', 'teaching')),
    note TEXT,
    created_at TEXT NOT NULL,
    UNIQUE(card_id, evidence_drawer_id, role),
    FOREIGN KEY(card_id) REFERENCES knowledge_cards(id) ON DELETE RESTRICT,
    FOREIGN KEY(evidence_drawer_id) REFERENCES drawers(id) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS knowledge_events (
    id TEXT PRIMARY KEY,
    card_id TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK(event_type IN ('created', 'promoted', 'demoted', 'retired', 'linked', 'unlinked', 'updated', 'published_anchor')),
    from_status TEXT,
    to_status TEXT,
    reason TEXT NOT NULL,
    actor TEXT,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY(card_id) REFERENCES knowledge_cards(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_knowledge_cards_tier_status
    ON knowledge_cards(tier, status);

CREATE INDEX IF NOT EXISTS idx_knowledge_cards_domain_field
    ON knowledge_cards(domain, field);

CREATE INDEX IF NOT EXISTS idx_knowledge_cards_anchor
    ON knowledge_cards(anchor_kind, anchor_id);

CREATE INDEX IF NOT EXISTS idx_knowledge_evidence_links_card
    ON knowledge_evidence_links(card_id);

CREATE INDEX IF NOT EXISTS idx_knowledge_evidence_links_evidence
    ON knowledge_evidence_links(evidence_drawer_id);

CREATE INDEX IF NOT EXISTS idx_knowledge_events_card_created_at
    ON knowledge_events(card_id, created_at);

CREATE TRIGGER IF NOT EXISTS knowledge_events_no_update
BEFORE UPDATE ON knowledge_events
BEGIN
    SELECT RAISE(ABORT, 'knowledge_events are append-only');
END;

CREATE TRIGGER IF NOT EXISTS knowledge_events_no_delete
BEFORE DELETE ON knowledge_events
BEGIN
    SELECT RAISE(ABORT, 'knowledge_events are append-only');
END;
"#;

const V9_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS runtime_adoption_events (
    id TEXT PRIMARY KEY,
    track TEXT NOT NULL CHECK(track IN ('runtime_adoption', 'card_context', 'card_embedding', 'evaluator', 'research_adapter')),
    signal TEXT NOT NULL CHECK(signal IN ('used', 'accepted', 'rejected', 'miss', 'rollback', 'contradiction', 'neutral')),
    feature TEXT NOT NULL,
    query TEXT,
    context_hash TEXT,
    card_id TEXT,
    evaluator_id TEXT,
    research_report_id TEXT,
    note TEXT,
    metadata TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY(card_id) REFERENCES knowledge_cards(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_runtime_adoption_events_track_created_at
    ON runtime_adoption_events(track, created_at);

CREATE INDEX IF NOT EXISTS idx_runtime_adoption_events_feature
    ON runtime_adoption_events(feature);

CREATE INDEX IF NOT EXISTS idx_runtime_adoption_events_signal
    ON runtime_adoption_events(signal);
"#;

const V10_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN valid_from TEXT;
ALTER TABLE drawers ADD COLUMN valid_until TEXT;

UPDATE drawers
SET valid_from = added_at
WHERE valid_from IS NULL;

CREATE INDEX IF NOT EXISTS idx_drawers_validity
    ON drawers(valid_from, valid_until)
    WHERE deleted_at IS NULL;
"#;

const V10_DRAWER_COLUMN_MIGRATIONS: &[DrawerColumnMigration] = &[
    DrawerColumnMigration {
        name: "valid_from",
        sql: "ALTER TABLE drawers ADD COLUMN valid_from TEXT;",
    },
    DrawerColumnMigration {
        name: "valid_until",
        sql: "ALTER TABLE drawers ADD COLUMN valid_until TEXT;",
    },
];

const V10_VALIDITY_BACKFILL_SQL: &str = r#"
UPDATE drawers
SET valid_from = added_at
WHERE valid_from IS NULL;

CREATE INDEX IF NOT EXISTS idx_drawers_validity
    ON drawers(valid_from, valid_until)
    WHERE deleted_at IS NULL;
"#;

const V11_SOURCE_CONFIDENCE_BACKFILL_SQL: &str = r#"
UPDATE drawers
SET source_type = CASE
        WHEN lower(COALESCE(source_file, '')) LIKE '%hook%' OR wing = 'hooks-raw'
            THEN 'system_generated'
        WHEN wing = 'agent-diary'
            THEN 'agent_observation'
        WHEN source_type = 'user_explicit'
            THEN 'user_explicit'
        WHEN source_type = 'agent_observation'
            THEN 'agent_observation'
        WHEN source_type = 'system_generated'
            THEN 'system_generated'
        ELSE 'agent_inference'
    END,
    confidence = CASE
        WHEN lower(COALESCE(source_file, '')) LIKE '%hook%' OR wing = 'hooks-raw'
            THEN 0.3
        WHEN wing = 'agent-diary'
            THEN 0.7
        WHEN source_type = 'user_explicit'
            THEN 0.9
        WHEN source_type = 'agent_observation'
            THEN 0.7
        WHEN source_type = 'system_generated'
            THEN 0.3
        ELSE 0.5
    END;
"#;

const V11_MIGRATION_SQL: &str = "";
const V12_MIGRATION_SQL: &str = "";
const V13_MIGRATION_SQL: &str = "";
const V14_MIGRATION_SQL: &str = "";
const V15_MIGRATION_SQL: &str = "";

const V16_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS leases (
    resource_path TEXT NOT NULL PRIMARY KEY,
    holder_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at TEXT NOT NULL,
    metadata TEXT
);
CREATE INDEX IF NOT EXISTS idx_leases_expires ON leases(expires_at);
"#;

const V17_MIGRATION_SQL: &str = r#"
DROP TRIGGER IF EXISTS drawers_ai;
DROP TRIGGER IF EXISTS drawers_au_softdelete;
DROP TABLE IF EXISTS drawers_fts;
CREATE VIRTUAL TABLE IF NOT EXISTS drawers_fts USING fts5(
    content,
    content='drawers',
    content_rowid='rowid'
);
"#;

const V18_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS design_insights (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL CHECK(source IN ('user_idea', 'review_finding', 'tool_friction', 'incident', 'research')),
    scope TEXT NOT NULL CHECK(scope IN ('project', 'cross_project', 'repo', 'issue')),
    target_artifact TEXT NOT NULL CHECK(target_artifact IN ('memory', 'skill', 'agents_rule', 'agents_rules_ref', 'codex_skill', 'github_issue', 'mempal_knowledge')),
    evidence_ref TEXT NOT NULL,
    summary TEXT NOT NULL,
    rule_text TEXT,
    priority INTEGER NOT NULL CHECK(priority BETWEEN 1 AND 5),
    status TEXT NOT NULL CHECK(status IN ('open', 'resolved')) DEFAULT 'open',
    created_at TEXT NOT NULL,
    resolved_at TEXT,
    resolved_by TEXT,
    resolution_note TEXT,
    redaction_count INTEGER NOT NULL DEFAULT 0,
    project_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_design_insights_status_priority
    ON design_insights(status, priority DESC, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_design_insights_target_status
    ON design_insights(target_artifact, status, priority DESC);

CREATE INDEX IF NOT EXISTS idx_design_insights_project_status
    ON design_insights(project_id, status, priority DESC);
"#;

const V19_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS foresights (
    drawer_id TEXT PRIMARY KEY REFERENCES drawers(id) ON DELETE CASCADE,
    statement TEXT NOT NULL,
    reason TEXT,
    trigger_condition TEXT NOT NULL,
    due_at TEXT NOT NULL,
    valid_from TEXT,
    valid_until TEXT,
    resolved_at TEXT,
    resolution_note TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_foresights_due
    ON foresights(due_at, resolved_at);

CREATE INDEX IF NOT EXISTS idx_foresights_validity
    ON foresights(valid_from, valid_until);
"#;

const V20_MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS runtime_writer_leases (
    name TEXT NOT NULL PRIMARY KEY,
    owner TEXT NOT NULL,
    pid INTEGER NOT NULL,
    boot_id TEXT,
    session_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    mode TEXT NOT NULL,
    metadata_json TEXT
);

CREATE INDEX IF NOT EXISTS idx_runtime_writer_leases_expires
    ON runtime_writer_leases(expires_at);

CREATE INDEX IF NOT EXISTS idx_runtime_writer_leases_mode
    ON runtime_writer_leases(mode);
"#;

const V12_COMPACTION_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_compacted_into
    ON drawers(compacted_into)
    WHERE compacted_into IS NOT NULL;

CREATE TABLE IF NOT EXISTS consolidation_log (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    wing TEXT NOT NULL,
    room TEXT,
    project_id TEXT,
    cluster_size INTEGER NOT NULL,
    strategy TEXT NOT NULL CHECK(strategy IN ('richest_content', 'llm_summary')),
    target_drawer_id TEXT NOT NULL REFERENCES drawers(id),
    source_drawer_ids TEXT NOT NULL,
    dry_run INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_consolidation_log_created_at
    ON consolidation_log(created_at);

CREATE INDEX IF NOT EXISTS idx_consolidation_log_scope
    ON consolidation_log(wing, room, project_id);
"#;

const V13_TYPED_PINNED_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_pinned
    ON drawers(is_pinned, pin_order)
    WHERE is_pinned = 1 AND deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_drawers_supersedes
    ON drawers(supersedes)
    WHERE supersedes IS NOT NULL;
"#;

const V14_SLEEP_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_drawers_consolidation_priority
    ON drawers(consolidation_priority DESC)
    WHERE deleted_at IS NULL AND compacted_into IS NULL;

CREATE INDEX IF NOT EXISTS idx_drawers_last_sleep_at
    ON drawers(last_sleep_at)
    WHERE last_sleep_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS sleep_log (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('full', 'nrem', 'rem', 'salience', 'selected')),
    processed_count INTEGER NOT NULL DEFAULT 0,
    pruned_count INTEGER NOT NULL DEFAULT 0,
    compacted_count INTEGER NOT NULL DEFAULT 0,
    conflicts_resolved_count INTEGER NOT NULL DEFAULT 0,
    salience_scored_count INTEGER NOT NULL DEFAULT 0,
    dry_run INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sleep_log_created_at
    ON sleep_log(created_at);

CREATE TABLE IF NOT EXISTS sleep_resolution_log (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    wing TEXT NOT NULL,
    drawer_id TEXT NOT NULL REFERENCES drawers(id),
    contradicted_triple_id TEXT NOT NULL REFERENCES triples(id),
    contradicted_source_drawer TEXT REFERENCES drawers(id),
    new_confidence REAL NOT NULL,
    existing_confidence REAL NOT NULL,
    action TEXT NOT NULL CHECK(action IN ('invalidated')),
    dry_run INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sleep_resolution_log_created_at
    ON sleep_resolution_log(created_at);
"#;

const V15_CRYSTALLIZE_SCHEMA_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_knowledge_cards_auto_pending
    ON knowledge_cards(auto_generated, status, created_at);
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
        Migration {
            version: 7,
            sql: V7_MIGRATION_SQL,
        },
        Migration {
            version: 8,
            sql: V8_MIGRATION_SQL,
        },
        Migration {
            version: 9,
            sql: V9_MIGRATION_SQL,
        },
        Migration {
            version: 10,
            sql: V10_MIGRATION_SQL,
        },
        Migration {
            version: 11,
            sql: V11_MIGRATION_SQL,
        },
        Migration {
            version: 12,
            sql: V12_MIGRATION_SQL,
        },
        Migration {
            version: 13,
            sql: V13_MIGRATION_SQL,
        },
        Migration {
            version: 14,
            sql: V14_MIGRATION_SQL,
        },
        Migration {
            version: 15,
            sql: V15_MIGRATION_SQL,
        },
        Migration {
            version: 16,
            sql: V16_MIGRATION_SQL,
        },
        Migration {
            version: 17,
            sql: V17_MIGRATION_SQL,
        },
        Migration {
            version: 18,
            sql: V18_MIGRATION_SQL,
        },
        Migration {
            version: 19,
            sql: V19_MIGRATION_SQL,
        },
        Migration {
            version: 20,
            sql: V20_MIGRATION_SQL,
        },
    ];
    MIGRATIONS
}

struct Migration {
    version: u32,
    sql: &'static str,
}

const V7_ALREADY_APPLIED_MIGRATION: Migration = Migration {
    version: 7,
    sql: V7_ALREADY_APPLIED_MIGRATION_SQL,
};

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
    source_type.as_str()
}

pub fn vector_metadata_key(drawer_id: &str, field: &str) -> String {
    format!("reindex:{drawer_id}:{field}")
}

fn parse_vector_fingerprint(value: &str) -> (Option<String>, Option<String>) {
    let mut parts = value.splitn(3, ':');
    let embedder = parts
        .next()
        .filter(|part| !part.is_empty())
        .map(str::to_string);
    let model = parts
        .next()
        .filter(|part| !part.is_empty())
        .map(str::to_string);
    (embedder, model)
}

fn source_type_from_str(source_type: &str) -> Result<SourceType, DbError> {
    source_type
        .parse()
        .map_err(|_| DbError::InvalidSourceType(source_type.to_string()))
}

fn memory_kind_as_str(memory_kind: &MemoryKind) -> &'static str {
    memory_kind.as_str()
}

fn memory_kind_from_str(memory_kind: &str) -> Result<MemoryKind, DbError> {
    memory_kind.parse().map_err(|_| DbError::InvalidEnumValue {
        kind: "memory_kind",
        value: memory_kind.to_string(),
    })
}

fn memory_domain_as_str(domain: &MemoryDomain) -> &'static str {
    match domain {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn memory_domain_from_str(domain: &str) -> Result<MemoryDomain, DbError> {
    match domain {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
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
        KnowledgeStatus::Active => "active",
        KnowledgeStatus::Superseded => "superseded",
        KnowledgeStatus::PendingReview => "pending_review",
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

fn knowledge_status_from_str(status: &str) -> Result<KnowledgeStatus, DbError> {
    match status {
        "active" => Ok(KnowledgeStatus::Active),
        "superseded" => Ok(KnowledgeStatus::Superseded),
        "pending_review" => Ok(KnowledgeStatus::PendingReview),
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

fn knowledge_evidence_role_as_str(role: &KnowledgeEvidenceRole) -> &'static str {
    match role {
        KnowledgeEvidenceRole::Supporting => "supporting",
        KnowledgeEvidenceRole::Verification => "verification",
        KnowledgeEvidenceRole::Counterexample => "counterexample",
        KnowledgeEvidenceRole::Teaching => "teaching",
    }
}

fn knowledge_evidence_role_from_str(role: &str) -> Result<KnowledgeEvidenceRole, DbError> {
    match role {
        "supporting" => Ok(KnowledgeEvidenceRole::Supporting),
        "verification" => Ok(KnowledgeEvidenceRole::Verification),
        "counterexample" => Ok(KnowledgeEvidenceRole::Counterexample),
        "teaching" => Ok(KnowledgeEvidenceRole::Teaching),
        other => Err(DbError::InvalidEnumValue {
            kind: "knowledge_evidence_role",
            value: other.to_string(),
        }),
    }
}

fn knowledge_event_type_as_str(event_type: &KnowledgeEventType) -> &'static str {
    match event_type {
        KnowledgeEventType::Created => "created",
        KnowledgeEventType::Promoted => "promoted",
        KnowledgeEventType::Demoted => "demoted",
        KnowledgeEventType::Retired => "retired",
        KnowledgeEventType::Linked => "linked",
        KnowledgeEventType::Unlinked => "unlinked",
        KnowledgeEventType::Updated => "updated",
        KnowledgeEventType::PublishedAnchor => "published_anchor",
    }
}

fn knowledge_event_type_from_str(event_type: &str) -> Result<KnowledgeEventType, DbError> {
    match event_type {
        "created" => Ok(KnowledgeEventType::Created),
        "promoted" => Ok(KnowledgeEventType::Promoted),
        "demoted" => Ok(KnowledgeEventType::Demoted),
        "retired" => Ok(KnowledgeEventType::Retired),
        "linked" => Ok(KnowledgeEventType::Linked),
        "unlinked" => Ok(KnowledgeEventType::Unlinked),
        "updated" => Ok(KnowledgeEventType::Updated),
        "published_anchor" => Ok(KnowledgeEventType::PublishedAnchor),
        other => Err(DbError::InvalidEnumValue {
            kind: "knowledge_event_type",
            value: other.to_string(),
        }),
    }
}

fn runtime_adoption_track_as_str(track: &RuntimeAdoptionTrack) -> &'static str {
    match track {
        RuntimeAdoptionTrack::RuntimeAdoption => "runtime_adoption",
        RuntimeAdoptionTrack::CardContext => "card_context",
        RuntimeAdoptionTrack::CardEmbedding => "card_embedding",
        RuntimeAdoptionTrack::Evaluator => "evaluator",
        RuntimeAdoptionTrack::ResearchAdapter => "research_adapter",
    }
}

fn runtime_adoption_track_from_str(track: &str) -> Result<RuntimeAdoptionTrack, DbError> {
    match track {
        "runtime_adoption" => Ok(RuntimeAdoptionTrack::RuntimeAdoption),
        "card_context" => Ok(RuntimeAdoptionTrack::CardContext),
        "card_embedding" => Ok(RuntimeAdoptionTrack::CardEmbedding),
        "evaluator" => Ok(RuntimeAdoptionTrack::Evaluator),
        "research_adapter" => Ok(RuntimeAdoptionTrack::ResearchAdapter),
        other => Err(DbError::InvalidEnumValue {
            kind: "runtime_adoption_track",
            value: other.to_string(),
        }),
    }
}

fn runtime_adoption_signal_as_str(signal: &RuntimeAdoptionSignal) -> &'static str {
    match signal {
        RuntimeAdoptionSignal::Used => "used",
        RuntimeAdoptionSignal::Accepted => "accepted",
        RuntimeAdoptionSignal::Rejected => "rejected",
        RuntimeAdoptionSignal::Miss => "miss",
        RuntimeAdoptionSignal::Rollback => "rollback",
        RuntimeAdoptionSignal::Contradiction => "contradiction",
        RuntimeAdoptionSignal::Neutral => "neutral",
    }
}

fn runtime_adoption_signal_from_str(signal: &str) -> Result<RuntimeAdoptionSignal, DbError> {
    match signal {
        "used" => Ok(RuntimeAdoptionSignal::Used),
        "accepted" => Ok(RuntimeAdoptionSignal::Accepted),
        "rejected" => Ok(RuntimeAdoptionSignal::Rejected),
        "miss" => Ok(RuntimeAdoptionSignal::Miss),
        "rollback" => Ok(RuntimeAdoptionSignal::Rollback),
        "contradiction" => Ok(RuntimeAdoptionSignal::Contradiction),
        "neutral" => Ok(RuntimeAdoptionSignal::Neutral),
        other => Err(DbError::InvalidEnumValue {
            kind: "runtime_adoption_signal",
            value: other.to_string(),
        }),
    }
}

fn encode_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, DbError> {
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

fn knowledge_card_from_row(row: &Row<'_>) -> Result<KnowledgeCard, DbError> {
    let tier = knowledge_tier_from_str(&row.get::<_, String>(3)?)?;
    let status = knowledge_status_from_str(&row.get::<_, String>(4)?)?;
    let domain = memory_domain_from_str(&row.get::<_, String>(5)?)?;
    let anchor_kind = anchor_kind_from_str(&row.get::<_, String>(7)?)?;
    let trigger_hints = parse_optional_json(row.get::<_, Option<String>>(11)?.as_deref())?;
    let source_drawer_ids =
        serde_json::from_str::<Vec<String>>(row.get::<_, String>(14)?.as_str())?;

    anchor::validate_anchor_domain(&domain, &anchor_kind)
        .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

    Ok(KnowledgeCard {
        id: row.get(0)?,
        statement: row.get(1)?,
        content: row.get(2)?,
        tier,
        status,
        domain,
        field: row.get(6)?,
        anchor_kind,
        anchor_id: row.get(8)?,
        parent_anchor_id: row.get(9)?,
        scope_constraints: row.get(10)?,
        trigger_hints,
        auto_generated: row.get::<_, i64>(12)? != 0,
        crystallization_score: row.get(13)?,
        source_drawer_ids,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn knowledge_evidence_link_from_row(row: &Row<'_>) -> Result<KnowledgeEvidenceLink, DbError> {
    Ok(KnowledgeEvidenceLink {
        id: row.get(0)?,
        card_id: row.get(1)?,
        evidence_drawer_id: row.get(2)?,
        role: knowledge_evidence_role_from_str(&row.get::<_, String>(3)?)?,
        note: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn knowledge_event_from_row(row: &Row<'_>) -> Result<KnowledgeCardEvent, DbError> {
    let from_status = row
        .get::<_, Option<String>>(3)?
        .as_deref()
        .map(knowledge_status_from_str)
        .transpose()?;
    let to_status = row
        .get::<_, Option<String>>(4)?
        .as_deref()
        .map(knowledge_status_from_str)
        .transpose()?;
    let metadata = parse_optional_json(row.get::<_, Option<String>>(7)?.as_deref())?;

    Ok(KnowledgeCardEvent {
        id: row.get(0)?,
        card_id: row.get(1)?,
        event_type: knowledge_event_type_from_str(&row.get::<_, String>(2)?)?,
        from_status,
        to_status,
        reason: row.get(5)?,
        actor: row.get(6)?,
        metadata,
        created_at: row.get(8)?,
    })
}

fn runtime_adoption_event_from_row(row: &Row<'_>) -> Result<RuntimeAdoptionEvent, DbError> {
    let metadata = parse_optional_json(row.get::<_, Option<String>>(10)?.as_deref())?;
    Ok(RuntimeAdoptionEvent {
        id: row.get(0)?,
        track: runtime_adoption_track_from_str(&row.get::<_, String>(1)?)?,
        signal: runtime_adoption_signal_from_str(&row.get::<_, String>(2)?)?,
        feature: row.get(3)?,
        query: row.get(4)?,
        context_hash: row.get(5)?,
        card_id: row.get(6)?,
        evaluator_id: row.get(7)?,
        research_report_id: row.get(8)?,
        note: row.get(9)?,
        metadata,
        created_at: row.get(11)?,
    })
}

fn drawer_from_row(row: &Row<'_>) -> Result<Drawer, DbError> {
    let source_type = source_type_from_str(&row.get::<_, String>(5)?)?;
    let confidence = row.get::<_, f64>(6)?;
    let memory_kind = memory_kind_from_str(&row.get::<_, String>(11)?)?;
    let domain = memory_domain_from_str(&row.get::<_, String>(12)?)?;
    let field = row.get::<_, String>(13)?;
    let anchor_kind = anchor_kind_from_str(&row.get::<_, String>(14)?)?;
    let anchor_id = row.get::<_, String>(15)?;
    let parent_anchor_id = row.get::<_, Option<String>>(16)?;
    let provenance = row
        .get::<_, Option<String>>(17)?
        .as_deref()
        .map(provenance_from_str)
        .transpose()?;
    let statement = row.get::<_, Option<String>>(18)?;
    let tier = row
        .get::<_, Option<String>>(19)?
        .as_deref()
        .map(knowledge_tier_from_str)
        .transpose()?;
    let status = row
        .get::<_, Option<String>>(20)?
        .as_deref()
        .map(knowledge_status_from_str)
        .transpose()?;
    let supporting_refs = parse_string_list(row.get::<_, Option<String>>(21)?.as_deref())?;
    let counterexample_refs = parse_string_list(row.get::<_, Option<String>>(22)?.as_deref())?;
    let teaching_refs = parse_string_list(row.get::<_, Option<String>>(23)?.as_deref())?;
    let verification_refs = parse_string_list(row.get::<_, Option<String>>(24)?.as_deref())?;
    let scope_constraints = row.get::<_, Option<String>>(25)?;
    let trigger_hints = parse_optional_json(row.get::<_, Option<String>>(26)?.as_deref())?;
    let is_pinned = row.get::<_, bool>(27)?;
    let pin_order = row.get::<_, Option<i64>>(28)?;
    let supersedes = row.get::<_, Option<String>>(29)?;
    let effective_importance = row.get::<_, f64>(30)?;
    let compacted_into = row.get::<_, Option<String>>(31)?;

    anchor::validate_anchor_domain(&domain, &anchor_kind)
        .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;

    Ok(Drawer {
        id: row.get(0)?,
        content: row.get(1)?,
        wing: row.get(2)?,
        room: row.get(3)?,
        source_file: row.get(4)?,
        source_type,
        confidence,
        added_at: row.get(7)?,
        chunk_index: row.get(8)?,
        normalize_version: row.get(9)?,
        importance: row.get(10)?,
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
        is_pinned,
        pin_order,
        supersedes,
        effective_importance,
        compacted_into,
    })
}

fn drawer_summary_from_details(details: DrawerDetails) -> DrawerSummary {
    DrawerSummary {
        id: details.drawer.id,
        wing: details.drawer.wing,
        room: details.drawer.room,
        source_file: details.drawer.source_file,
        project_id: details.project_id,
        added_at: details.drawer.added_at,
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
    let segments = segment_cjk_query(query);
    let terms: Vec<String> = segments
        .iter()
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

/// Tokenize drawer content exactly as the FTS index stores it.
pub fn fts_tokenize_content(content: &str) -> String {
    if !contains_cjk(content) {
        return content.to_string();
    }
    crate::aaak::codec::jieba_cut_for_search(content)
        .into_iter()
        .filter(|w| !w.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_cjk(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}' | '\u{F900}'..='\u{FAFF}'))
}

fn segment_cjk_query(query: &str) -> Vec<String> {
    let raw_terms: Vec<&str> = query.split_whitespace().collect();
    let mut result = Vec::new();
    for term in raw_terms {
        if contains_cjk(term) {
            let words: Vec<String> = crate::aaak::codec::jieba_cut_for_search(term)
                .into_iter()
                .filter(|w| !w.trim().is_empty())
                .map(|w| w.to_string())
                .collect();
            // Filter out compound words whose characters are fully covered by
            // shorter segments (cut_for_search emits both sub-words and the
            // original compound; only sub-words match the tokenized index).
            for w in &words {
                let is_compound = w.chars().count() > 1
                    && words
                        .iter()
                        .any(|other| other != w && w.contains(other.as_str()));
                if !is_compound {
                    result.push(w.clone());
                }
            }
        } else {
            result.push(term.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_drawer(id: &str, content: &str) -> Drawer {
        Drawer::new_bootstrap_evidence(super::super::types::BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: "test-wing".to_string(),
            room: Some("test-room".to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: "2026-05-13T00:00:00Z".to_string(),
            chunk_index: Some(0),
            importance: 0,
        })
    }

    fn insert_test_drawer(db: &Database, id: &str, content: &str, project_id: Option<&str>) {
        db.insert_drawer_with_project(&test_drawer(id, content), project_id)
            .expect("insert drawer");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_writer_status_preserves_and_renews_expired_live_current_process_lease() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("test.db");
        let db = Database::open(&db_path).expect("open db");
        let lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "live-daemon", "daemon", 1, None)
            .expect("acquire runtime writer lease")
            .expect("runtime writer lease");

        db.conn()
            .execute(
                "UPDATE runtime_writer_leases \
                 SET expires_at = '1970-01-01T00:00:00Z', heartbeat_at = '1970-01-01T00:00:00Z' \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                params![&lease.name, &lease.owner, &lease.session_id],
            )
            .expect("force lease expiry");

        assert!(
            db.runtime_writer_lease_is_active(&lease.name, &lease.owner, &lease.session_id)
                .expect("check runtime writer lease active"),
            "current live process must retain its runtime writer lease after delayed heartbeat"
        );
        let expired_status = db
            .runtime_writer_lease_status(Some(&lease.name))
            .expect("load runtime writer lease status");
        assert_eq!(expired_status.len(), 1);
        assert_eq!(expired_status[0].remaining_secs, 0);
        assert!(
            db.runtime_writer_lease_renew(&lease.name, &lease.owner, &lease.session_id, 300)
                .expect("renew delayed live runtime writer lease"),
            "delayed live runtime writer lease must be renewable after expiry"
        );
        let renewed_status = db
            .runtime_writer_lease_status(Some(&lease.name))
            .expect("load renewed runtime writer lease status");
        assert_eq!(renewed_status.len(), 1);
        assert!(renewed_status[0].remaining_secs > 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_writer_cooperative_acquire_preserves_expired_live_current_process_lease() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("test.db");
        let db = Database::open(&db_path).expect("open db");
        let lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "live-daemon", "daemon", 1, None)
            .expect("acquire runtime writer lease")
            .expect("runtime writer lease");

        db.conn()
            .execute(
                "UPDATE runtime_writer_leases \
                 SET expires_at = '1970-01-01T00:00:00Z', heartbeat_at = '1970-01-01T00:00:00Z' \
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3",
                params![&lease.name, &lease.owner, &lease.session_id],
            )
            .expect("force lease expiry");

        assert!(
            db.runtime_writer_lease_acquire_preserving_live_holders(
                "sqlite-writer",
                "cooperative-mcp-worker",
                "mcp-ingest-worker",
                300,
                None,
            )
            .expect("cooperative acquire")
            .is_none(),
            "cooperative writer must not steal an expired lease from a live holder"
        );
        let preserved = db
            .runtime_writer_lease_status(Some("sqlite-writer"))
            .expect("load preserved runtime writer lease status");
        assert_eq!(preserved.len(), 1);
        assert_eq!(preserved[0].owner, "live-daemon");

        let maintenance = db
            .runtime_writer_lease_acquire("sqlite-writer", "maintenance", "maintenance", 300, None)
            .expect("maintenance acquire")
            .expect("maintenance can reclaim expired lease");
        assert_eq!(maintenance.owner, "maintenance");
    }

    #[test]
    fn concurrent_default_pin_order_allocation_is_atomic_across_connections() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("test.db");
        let db = Database::open(&db_path).expect("open db");
        insert_test_drawer(&db, "already-pinned", "already pinned", None);
        insert_test_drawer(&db, "pin-a", "pin a", None);
        insert_test_drawer(&db, "pin-b", "pin b", None);
        assert!(
            db.pin_drawer("already-pinned", None)
                .expect("pin initial drawer")
        );
        let _daemon_lease = db
            .runtime_writer_lease_acquire("sqlite-writer", "daemon-owner", "daemon", 300, None)
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease");

        let worker_a = Database::open(&db_path).expect("open worker a");
        let worker_b = Database::open(&db_path).expect("open worker b");
        let lock_holder = Connection::open(&db_path).expect("open lock holder");
        lock_holder
            .busy_timeout(Duration::from_secs(5))
            .expect("set lock holder busy timeout");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold write lock");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let barrier_a = std::sync::Arc::clone(&barrier);
        let handle_a = std::thread::spawn(move || {
            barrier_a.wait();
            worker_a.pin_drawer("pin-a", None).expect("pin a")
        });
        let barrier_b = std::sync::Arc::clone(&barrier);
        let handle_b = std::thread::spawn(move || {
            barrier_b.wait();
            worker_b.pin_drawer("pin-b", None).expect("pin b")
        });

        barrier.wait();
        std::thread::sleep(Duration::from_millis(150));
        lock_holder
            .execute_batch("COMMIT;")
            .expect("release write lock");

        assert!(handle_a.join().expect("join pin a"));
        assert!(handle_b.join().expect("join pin b"));

        let order_a = db
            .get_drawer("pin-a")
            .expect("load pin a")
            .expect("pin a exists")
            .pin_order
            .expect("pin a order");
        let order_b = db
            .get_drawer("pin-b")
            .expect("load pin b")
            .expect("pin b exists")
            .pin_order
            .expect("pin b order");
        let mut orders = vec![order_a, order_b];
        orders.sort_unstable();
        assert_eq!(orders, vec![1, 2]);
    }

    #[test]
    fn unsupported_schema_version_error_guides_binary_update_and_mcp_config() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(&format!(
            "PRAGMA user_version = {};",
            CURRENT_SCHEMA_VERSION + 1
        ))
        .expect("set future schema version");

        let err = apply_migrations(&conn).expect_err("future schema should be unsupported");
        let message = err.to_string();

        assert!(message.contains(&format!(
            "database schema version {} is newer than supported version {}",
            CURRENT_SCHEMA_VERSION + 1,
            CURRENT_SCHEMA_VERSION
        )));
        assert!(message.contains("update the mempal binary"));
        assert!(message.contains("cargo install mempal"));
        assert!(message.contains("MCP server"));
        assert!(message.contains("MCP client configuration"));
        assert!(message.contains("command/path"));
    }

    #[test]
    fn database_connection_uses_low_rss_cache_without_mmap() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");

        let cache_size = db
            .conn()
            .query_row("PRAGMA cache_size", [], |row| row.get::<_, i64>(0))
            .expect("query cache_size");
        let mmap_size = db
            .conn()
            .query_row("PRAGMA mmap_size", [], |row| row.get::<_, i64>(0))
            .expect("query mmap_size");

        assert_eq!(cache_size, SQLITE_CACHE_SIZE_KIB_DEFAULT);
        assert_eq!(mmap_size, 0, "issue #311 must not add multi-GiB mmap");
    }

    #[test]
    fn query_only_connection_opens_while_writer_lock_is_held() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("initialize db");

        let lock_holder = Connection::open(&db_path).expect("open lock holder");
        lock_holder
            .busy_timeout(Duration::from_millis(25))
            .expect("set lock holder busy timeout");
        lock_holder
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("hold write lock");

        let reader = Database::open_with_mode_and_busy_timeout(
            &db_path,
            OpenMode::QueryOnly,
            Duration::from_millis(25),
        )
        .expect("query-only reader opens without startup writes");
        let query_only: i64 = reader
            .conn()
            .query_row("PRAGMA query_only", [], |row| row.get(0))
            .expect("read query_only pragma");

        assert_eq!(query_only, 1);
        reader
            .drawer_count()
            .expect("query-only reader can read while writer lock is held");
        reader
            .conn()
            .execute("CREATE TABLE query_only_must_not_write(id INTEGER)", [])
            .expect_err("query-only connection must reject mutations");
    }

    fn insert_test_source_drawer_with_vector(
        db: &Database,
        id: &str,
        content: &str,
        source_file: &str,
        project_id: Option<&str>,
    ) {
        insert_test_source_drawer_with_vector_and_root(
            db,
            id,
            content,
            source_file,
            project_id,
            None,
        );
    }

    fn insert_test_source_drawer_with_vector_and_root(
        db: &Database,
        id: &str,
        content: &str,
        source_file: &str,
        project_id: Option<&str>,
        source_root: Option<&str>,
    ) {
        let mut drawer = test_drawer(id, content);
        drawer.source_file = Some(source_file.to_string());
        db.insert_drawer_with_project_validity(&drawer, project_id, source_root, None, None)
            .expect("insert drawer");
        db.insert_vector_with_project(id, &[1.0_f32, 0.0, 0.0], project_id)
            .expect("insert vector");
    }

    fn recreate_vectors_with_metric(db: &Database, metric_fragment: &str) {
        db.conn()
            .execute_batch(&format!(
                r#"
                DROP TABLE IF EXISTS drawer_vectors;
                CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                    id TEXT PRIMARY KEY,
                    embedding FLOAT[3] {metric_fragment}
                );
                "#
            ))
            .expect("recreate vector table");
    }

    #[test]
    fn vector_index_is_stale_detects_l2_metric_tables() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");

        recreate_vectors_with_metric(&db, "distance_metric=l2");

        assert!(
            db.vector_index_is_stale()
                .expect("detect stale vector index")
        );
    }

    #[test]
    fn vector_index_is_stale_ignores_cosine_metric_tables() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");

        recreate_vectors_with_metric(&db, "distance_metric=cosine");

        assert!(
            !db.vector_index_is_stale()
                .expect("detect fresh vector index")
        );
    }

    #[test]
    fn vector_index_is_stale_ignores_missing_vector_table() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");

        db.conn()
            .execute_batch("DROP TABLE IF EXISTS drawer_vectors;")
            .expect("drop vector table");

        assert!(
            !db.vector_index_is_stale()
                .expect("detect missing vector index")
        );
    }

    fn active_drawer_project_id(db: &Database, id: &str) -> Option<Option<String>> {
        db.conn()
            .query_row(
                "SELECT project_id FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("read active drawer project_id")
    }

    fn vector_project_id(db: &Database, id: &str) -> Option<Option<String>> {
        db.conn()
            .query_row(
                "SELECT project_id FROM drawer_vectors WHERE id = ?1",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("read vector project_id")
    }

    fn drawer_rowid(db: &Database, id: &str) -> i64 {
        db.conn()
            .query_row("SELECT rowid FROM drawers WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .expect("read drawer rowid")
    }

    fn drawer_fts_doc_indexed(db: &Database, rowid: i64) -> bool {
        db.conn()
            .execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS temp.test_drawer_fts_vocab \
                 USING fts5vocab('main', 'drawers_fts', 'instance')",
            )
            .expect("create FTS vocab view");
        db.conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM temp.test_drawer_fts_vocab WHERE doc = ?1)",
                [rowid],
                |row| row.get(0),
            )
            .expect("query FTS doc presence")
    }

    fn remove_drawer_fts_doc(db: &Database, rowid: i64) {
        db.conn()
            .execute("DELETE FROM drawers_fts WHERE rowid = ?1", [rowid])
            .expect("remove FTS doc");
    }

    fn active_drawer_source_root(db: &Database, id: &str) -> Option<Option<String>> {
        db.conn()
            .query_row(
                "SELECT source_root FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                [id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("read active drawer source_root")
    }

    #[test]
    fn test_replace_active_source_drawers_global_scope_preserves_project_drawers_and_vectors() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let source_file = "shared-note.md";

        insert_test_source_drawer_with_vector(
            &db,
            "global-source",
            "global content",
            source_file,
            None,
        );
        insert_test_source_drawer_with_vector(
            &db,
            "project-source",
            "project content",
            source_file,
            Some("proj-a"),
        );

        let replaced = db
            .replace_active_source_drawers(source_file, "test-wing", Some("test-room"), None, None)
            .expect("replace global source drawers");

        assert_eq!(replaced, 1);
        assert_eq!(active_drawer_project_id(&db, "global-source"), None);
        assert_eq!(vector_project_id(&db, "global-source"), None);
        assert_eq!(
            active_drawer_project_id(&db, "project-source"),
            Some(Some("proj-a".to_string()))
        );
        assert_eq!(
            vector_project_id(&db, "project-source"),
            Some(Some("proj-a".to_string()))
        );
    }

    #[test]
    fn test_replace_active_source_drawers_project_scope_preserves_other_scopes() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let source_file = "shared-note.md";

        insert_test_source_drawer_with_vector(
            &db,
            "global-source",
            "global content",
            source_file,
            None,
        );
        insert_test_source_drawer_with_vector(
            &db,
            "project-a-source",
            "project a content",
            source_file,
            Some("proj-a"),
        );
        insert_test_source_drawer_with_vector(
            &db,
            "project-b-source",
            "project b content",
            source_file,
            Some("proj-b"),
        );

        let replaced = db
            .replace_active_source_drawers(
                source_file,
                "test-wing",
                Some("test-room"),
                Some("proj-a"),
                None,
            )
            .expect("replace project source drawers");

        assert_eq!(replaced, 1);
        assert_eq!(active_drawer_project_id(&db, "project-a-source"), None);
        assert_eq!(vector_project_id(&db, "project-a-source"), None);
        assert_eq!(
            active_drawer_project_id(&db, "project-b-source"),
            Some(Some("proj-b".to_string()))
        );
        assert_eq!(
            vector_project_id(&db, "project-b-source"),
            Some(Some("proj-b".to_string()))
        );
        assert_eq!(active_drawer_project_id(&db, "global-source"), Some(None));
        assert_eq!(vector_project_id(&db, "global-source"), Some(None));
    }

    #[test]
    fn test_replace_active_source_drawers_scopes_source_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let source_file = "note.md";
        let root_a = tempdir.path().join("root-a");
        let root_b = tempdir.path().join("root-b");
        std::fs::create_dir_all(&root_a).expect("create root-a");
        std::fs::create_dir_all(&root_b).expect("create root-b");
        let root_a = root_a.canonicalize().expect("canonical root-a");
        let root_b = root_b.canonicalize().expect("canonical root-b");
        let root_a = root_a.to_string_lossy().to_string();
        let root_b = root_b.to_string_lossy().to_string();

        insert_test_source_drawer_with_vector_and_root(
            &db,
            "project-a-root-a",
            "root a content",
            source_file,
            Some("proj-a"),
            Some(&root_a),
        );
        insert_test_source_drawer_with_vector_and_root(
            &db,
            "project-a-root-b",
            "root b content",
            source_file,
            Some("proj-a"),
            Some(&root_b),
        );

        let replaced = db
            .replace_active_source_drawers(
                source_file,
                "test-wing",
                Some("test-room"),
                Some("proj-a"),
                Some(&root_a),
            )
            .expect("replace source-root scoped drawers");

        assert_eq!(replaced, 1);
        assert_eq!(active_drawer_project_id(&db, "project-a-root-a"), None);
        assert_eq!(vector_project_id(&db, "project-a-root-a"), None);
        assert_eq!(
            active_drawer_project_id(&db, "project-a-root-b"),
            Some(Some("proj-a".to_string()))
        );
        assert_eq!(
            active_drawer_source_root(&db, "project-a-root-b"),
            Some(Some(root_b))
        );
        assert_eq!(
            vector_project_id(&db, "project-a-root-b"),
            Some(Some("proj-a".to_string()))
        );
    }

    #[test]
    fn test_find_active_drawers_by_content_respects_scope_and_soft_delete() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        insert_test_drawer(&db, "same-project", "same fact", Some("project-a"));
        insert_test_drawer(&db, "other-project", "same fact", Some("project-b"));
        insert_test_drawer(&db, "deleted", "same fact", Some("project-a"));
        db.soft_delete_drawer("deleted").expect("soft delete");

        let matches = db
            .find_active_drawers_by_content(
                "same fact",
                "test-wing",
                Some("test-room"),
                Some("project-a"),
            )
            .expect("find matches");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, "same-project");
        assert!(
            db.drawer_exists_exact(
                "same fact",
                "test-wing",
                Some("test-room"),
                Some("project-a")
            )
            .expect("exists exact")
        );
    }

    #[test]
    fn test_insert_drawer_defaults_valid_from_to_added_at() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");

        insert_test_drawer(&db, "valid-default", "valid fact", Some("project-a"));

        let validity = db
            .conn()
            .query_row(
                "SELECT added_at, valid_from, valid_until FROM drawers WHERE id = 'valid-default'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .expect("read validity");
        assert_eq!(validity.1.as_deref(), Some(validity.0.as_str()));
        assert_eq!(validity.2, None);
    }

    #[test]
    fn test_supersede_drawer_soft_deletes_sets_valid_until_and_writes_audit() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        insert_test_drawer(&db, "old", "old fact", Some("project-a"));

        let superseded = db
            .supersede_drawer("old", "replaced by new")
            .expect("supersede");

        assert!(superseded);
        assert!(!db.drawer_exists("old").expect("drawer exists"));
        let valid_until = db
            .conn()
            .query_row(
                "SELECT valid_until FROM drawers WHERE id = 'old'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .expect("read valid_until");
        assert!(valid_until.is_some());
        let audit = fs::read_to_string(tempdir.path().join("audit.jsonl")).expect("read audit");
        assert!(audit.contains("\"command\":\"supersede\""));
        assert!(audit.contains("\"drawer_id\":\"old\""));
        assert!(audit.contains("\"reason\":\"replaced by new\""));
    }

    #[test]
    fn test_resolve_replacement_target_rejects_project_mismatch() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        insert_test_drawer(&db, "old", "old fact", Some("project-a"));

        let error = db
            .resolve_replacement_target(
                Some("old"),
                None,
                "test-wing",
                Some("test-room"),
                Some("project-b"),
            )
            .expect_err("project mismatch");

        assert!(matches!(
            error,
            DbError::SupersededDrawerProjectMismatch { .. }
        ));
    }

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

    #[test]
    fn test_v10_migration_adds_validity_windows_and_backfills_valid_from() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            r#"
            CREATE TABLE drawers (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                wing TEXT NOT NULL,
                room TEXT,
                source_file TEXT,
                source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
                added_at TEXT NOT NULL,
                chunk_index INTEGER,
                deleted_at TEXT
            );
            INSERT INTO drawers (
                id, content, wing, room, source_file, source_type, added_at, chunk_index, deleted_at
            ) VALUES (
                'legacy', 'legacy body', 'code', NULL, 'legacy.md', 'manual', '2026-04-29T00:00:00Z', 0, NULL
            );
            PRAGMA user_version = 9;
            "#,
        )
        .expect("create legacy schema");

        apply_migrations(&conn).expect("apply migration");

        assert_eq!(
            read_user_version(&conn).expect("user_version"),
            CURRENT_SCHEMA_VERSION
        );
        let columns = drawers_column_names(&conn).expect("drawer columns");
        assert!(columns.contains("valid_from"));
        assert!(columns.contains("valid_until"));
        let validity = conn
            .query_row(
                "SELECT valid_from, valid_until FROM drawers WHERE id = 'legacy'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .expect("read validity");
        assert_eq!(validity.0.as_deref(), Some("2026-04-29T00:00:00Z"));
        assert_eq!(validity.1, None);
    }

    #[test]
    fn test_v11_migration_backfills_source_type_and_confidence() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            r#"
            CREATE TABLE drawers (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                wing TEXT NOT NULL,
                room TEXT,
                source_file TEXT,
                source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
                added_at TEXT NOT NULL,
                chunk_index INTEGER,
                deleted_at TEXT,
                importance INTEGER DEFAULT 0,
                normalize_version INTEGER NOT NULL DEFAULT 1,
                valid_from TEXT,
                valid_until TEXT
            );
            INSERT INTO drawers (id, content, wing, room, source_file, source_type, added_at, chunk_index, deleted_at, importance, normalize_version, valid_from, valid_until)
            VALUES
                ('hook-source', 'hook source', 'mempal', NULL, '/tmp/hook/capture.json', 'manual', '2026-05-13T00:00:00Z', 0, NULL, 0, 1, '2026-05-13T00:00:00Z', NULL),
                ('hook-wing', 'hook wing', 'hooks-raw', NULL, 'raw.json', 'manual', '2026-05-13T00:00:00Z', 0, NULL, 0, 1, '2026-05-13T00:00:00Z', NULL),
                ('diary', 'diary body', 'agent-diary', 'codex', 'agent-diary://rollup/2026-05-13', 'manual', '2026-05-13T00:00:00Z', 0, NULL, 0, 1, '2026-05-13T00:00:00Z', NULL),
                ('normal', 'normal body', 'mempal', 'decision', 'normal.md', 'manual', '2026-05-13T00:00:00Z', 0, NULL, 0, 1, '2026-05-13T00:00:00Z', NULL);
            PRAGMA user_version = 10;
            "#,
        )
        .expect("create v10 schema");

        apply_migrations(&conn).expect("apply v11 migration");

        assert_eq!(
            read_user_version(&conn).expect("user_version"),
            CURRENT_SCHEMA_VERSION
        );
        let rows = conn
            .prepare("SELECT id, source_type, confidence FROM drawers ORDER BY id")
            .expect("prepare")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                ))
            })
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect");
        assert_eq!(
            rows,
            vec![
                ("diary".to_string(), "agent_observation".to_string(), 0.7),
                (
                    "hook-source".to_string(),
                    "system_generated".to_string(),
                    0.3
                ),
                ("hook-wing".to_string(), "system_generated".to_string(), 0.3),
                ("normal".to_string(), "agent_inference".to_string(), 0.5),
            ]
        );
        conn.execute(
            "INSERT INTO drawers (id, content, wing, source_type, confidence, added_at) VALUES ('new-user', 'new body', 'mempal', 'user_explicit', 0.9, '2026-05-13T00:00:00Z')",
            [],
        )
        .expect("new source_type check accepts user_explicit");
    }

    #[test]
    fn test_v12_migration_adds_compaction_schema_idempotently() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            r#"
            CREATE TABLE drawers (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                wing TEXT NOT NULL,
                added_at TEXT NOT NULL
            );
            PRAGMA user_version = 11;
            "#,
        )
        .expect("create v11 schema");

        ensure_v12_compaction_schema(&conn, 11).expect("apply v12 migration");
        ensure_v12_compaction_schema(&conn, 12).expect("reapply v12 migration");

        assert_eq!(read_user_version(&conn).expect("user_version"), 12);
        let columns = drawers_column_names(&conn).expect("drawer columns");
        assert!(columns.contains("compacted_into"));
        let log_exists = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='consolidation_log')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("table exists");
        assert_eq!(log_exists, 1);
    }

    #[test]
    fn test_find_similar_clusters_groups_connected_components() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        for (id, vector) in [
            ("similar-a", vec![1.0_f32, 0.0, 0.0]),
            ("similar-b", vec![0.99_f32, 0.01, 0.0]),
            ("similar-c", vec![0.98_f32, 0.02, 0.0]),
            ("distant", vec![0.0_f32, 1.0, 0.0]),
        ] {
            insert_test_drawer(&db, id, id, Some("project-a"));
            db.insert_vector_with_project(id, &vector, Some("project-a"))
                .expect("insert vector");
        }

        let clusters = find_similar_clusters(
            db.conn(),
            Some("test-wing"),
            Some("test-room"),
            Some("project-a"),
            0.95,
            3,
        )
        .expect("find clusters");

        assert_eq!(clusters.len(), 1);
        let ids = clusters[0]
            .iter()
            .map(|(drawer_id, similarity)| {
                assert!(*similarity > 0.95);
                drawer_id.as_str()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["similar-a", "similar-b", "similar-c"]));
    }

    #[test]
    fn test_find_similar_clusters_respects_project_filter() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        for project_id in ["project-a", "project-b"] {
            for suffix in ["a", "b", "c"] {
                let id = format!("{project_id}-{suffix}");
                insert_test_drawer(&db, &id, &id, Some(project_id));
                db.insert_vector_with_project(&id, &[1.0_f32, 0.0, 0.0], Some(project_id))
                    .expect("insert vector");
            }
        }

        let clusters = find_similar_clusters(
            db.conn(),
            Some("test-wing"),
            Some("test-room"),
            Some("project-a"),
            0.95,
            3,
        )
        .expect("find project clusters");

        assert_eq!(clusters.len(), 1);
        let ids = clusters[0]
            .iter()
            .map(|(drawer_id, _)| drawer_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            BTreeSet::from(["project-a-a", "project-a-b", "project-a-c"])
        );
    }

    #[test]
    fn test_find_similar_clusters_null_project_filter_excludes_named_projects() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        for project_id in [None, Some("project-a")] {
            let prefix = project_id.unwrap_or("global");
            for suffix in ["a", "b", "c"] {
                let id = format!("{prefix}-{suffix}");
                insert_test_drawer(&db, &id, &id, project_id);
                db.insert_vector_with_project(&id, &[1.0_f32, 0.0, 0.0], project_id)
                    .expect("insert vector");
            }
        }

        let clusters = find_similar_clusters(
            db.conn(),
            Some("test-wing"),
            Some("test-room"),
            None,
            0.95,
            3,
        )
        .expect("find null-project clusters");

        assert_eq!(clusters.len(), 1);
        let ids = clusters[0]
            .iter()
            .map(|(drawer_id, _)| drawer_id.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from(["global-a", "global-b", "global-c"]));
    }

    #[test]
    fn test_find_similar_clusters_excludes_compacted_drawers() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        for id in ["active-a", "active-b", "compacted"] {
            insert_test_drawer(&db, id, id, Some("project-a"));
            db.insert_vector_with_project(id, &[1.0_f32, 0.0, 0.0], Some("project-a"))
                .expect("insert vector");
        }
        db.conn()
            .execute(
                "UPDATE drawers SET compacted_into = 'active-a' WHERE id = 'compacted'",
                [],
            )
            .expect("mark compacted");

        let clusters = find_similar_clusters(
            db.conn(),
            Some("test-wing"),
            Some("test-room"),
            Some("project-a"),
            0.95,
            3,
        )
        .expect("find clusters");

        assert!(clusters.is_empty());
    }

    #[test]
    fn test_v5_repair_runs_when_user_version_is_already_past_v5() {
        let conn = Connection::open_in_memory().expect("open in-memory");
        conn.execute_batch(
            r#"
            CREATE TABLE drawers (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                wing TEXT NOT NULL,
                room TEXT,
                source_file TEXT,
                source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
                added_at TEXT NOT NULL,
                chunk_index INTEGER,
                deleted_at TEXT,
                importance INTEGER DEFAULT 0,
                content_hash TEXT
            );
            CREATE TABLE tunnels (
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
            INSERT INTO drawers (
                id, content, wing, room, source_file, source_type, added_at, chunk_index, deleted_at, importance, content_hash
            ) VALUES (
                'legacy', 'legacy body', 'code', NULL, 'legacy.md', 'manual', '2026-04-29T00:00:00Z', 0, NULL, 0, NULL
            );
            PRAGMA user_version = 6;
            "#,
        )
        .expect("create legacy schema");

        apply_migrations(&conn).expect("repair schema");

        assert_eq!(
            read_user_version(&conn).expect("user_version"),
            CURRENT_SCHEMA_VERSION
        );

        let columns = drawers_column_names(&conn).expect("drawer columns");
        for required in [
            "memory_kind",
            "domain",
            "field",
            "anchor_kind",
            "anchor_id",
            "parent_anchor_id",
            "provenance",
            "statement",
            "tier",
            "status",
            "supporting_refs",
            "counterexample_refs",
            "teaching_refs",
            "verification_refs",
            "scope_constraints",
            "trigger_hints",
            "content_hash",
        ] {
            assert!(
                columns.contains(required),
                "missing repaired V5 column: {required}"
            );
        }

        let repaired = conn
            .query_row(
                "SELECT memory_kind, domain, field, anchor_kind, anchor_id, provenance, content_hash FROM drawers WHERE id = 'legacy'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .expect("read repaired row");

        assert_eq!(repaired.0, "evidence");
        assert_eq!(repaired.1, "project");
        assert_eq!(repaired.2, "general");
        assert_eq!(repaired.3, "repo");
        assert_eq!(repaired.4, "repo://legacy");
        assert_eq!(repaired.5.as_deref(), Some("research"));
        assert_eq!(
            repaired.6.as_deref(),
            Some(blake3::hash(b"legacy body").to_hex().to_string().as_str())
        );
    }

    #[test]
    fn cjk_segmentation_splits_chinese_query() {
        let segments = segment_cjk_query("记忆系统设计");
        assert!(
            segments.len() > 1,
            "should split CJK into multiple words: {:?}",
            segments
        );
    }

    #[test]
    fn cjk_segmentation_preserves_english() {
        let segments = segment_cjk_query("hello world");
        assert_eq!(segments, vec!["hello", "world"]);
    }

    #[test]
    fn cjk_segmentation_handles_mixed() {
        let segments = segment_cjk_query("mempal 记忆系统 design");
        assert!(segments.contains(&"mempal".to_string()));
        assert!(segments.contains(&"design".to_string()));
        assert!(
            segments.len() > 3,
            "CJK part should be split: {:?}",
            segments
        );
    }

    #[test]
    fn build_fts_match_query_segments_cjk() {
        let query = build_fts_match_query("记忆系统").unwrap();
        assert!(
            query.contains("AND"),
            "CJK query should have multiple terms: {}",
            query
        );
    }

    #[test]
    fn fts_chinese_content_matches_segmented_query() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("test.db")).unwrap();
        insert_test_drawer(&db, "zh1", "这是一个记忆系统的设计方案", None);
        let results = db
            .search_fts("记忆系统", None, None, "all", None, 10)
            .unwrap();
        assert!(
            !results.is_empty(),
            "CJK search should find Chinese content"
        );
    }

    #[test]
    fn hard_delete_drawers_by_ids_removes_fts_row_before_rowid_reuse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("test.db")).expect("open db");
        insert_test_drawer(&db, "old", "legacyneedle hard deleted payload", None);
        let old_rowid = drawer_rowid(&db, "old");

        let deleted = db
            .hard_delete_drawers_by_ids(&[String::from("old")])
            .expect("hard delete drawer");
        assert_eq!(deleted, 1);

        insert_test_drawer(&db, "replacement", "fresh replacement payload", None);
        assert_eq!(
            drawer_rowid(&db, "replacement"),
            old_rowid,
            "test requires SQLite to reuse the deleted drawer rowid"
        );

        let stale_matches = db
            .search_fts("legacyneedle", None, None, "all", None, 10)
            .expect("search stale hard-deleted payload through FTS");
        assert!(
            stale_matches.is_empty(),
            "hard-delete must remove stale BM25 rows before rowid reuse: {stale_matches:?}"
        );
    }

    #[test]
    fn purge_deleted_tolerates_legacy_soft_deleted_row_missing_fts_doc() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("test.db")).expect("open db");
        insert_test_drawer(
            &db,
            "legacy-missing",
            "purgelegacymissing soft deleted payload",
            None,
        );
        let old_rowid = drawer_rowid(&db, "legacy-missing");

        assert!(
            db.soft_delete_drawer("legacy-missing")
                .expect("soft delete drawer")
        );
        remove_drawer_fts_doc(&db, old_rowid);
        assert!(
            !drawer_fts_doc_indexed(&db, old_rowid),
            "test requires a legacy soft-deleted row absent from FTS"
        );

        let purged = db.purge_deleted(None).expect("purge deleted drawer");
        assert_eq!(purged, 1);
        assert_eq!(db.deleted_drawer_count().expect("count deleted drawers"), 0);
    }

    #[test]
    fn purge_deleted_removes_existing_stale_fts_row_before_rowid_reuse() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("test.db")).expect("open db");
        insert_test_drawer(&db, "old", "purgelegacyneedle soft deleted payload", None);
        let old_rowid = drawer_rowid(&db, "old");

        assert!(db.soft_delete_drawer("old").expect("soft delete drawer"));
        assert!(
            drawer_fts_doc_indexed(&db, old_rowid),
            "test requires an existing stale FTS doc before purge"
        );
        let purged = db.purge_deleted(None).expect("purge deleted drawer");
        assert_eq!(purged, 1);

        insert_test_drawer(&db, "replacement", "fresh replacement payload", None);
        assert_eq!(
            drawer_rowid(&db, "replacement"),
            old_rowid,
            "test requires SQLite to reuse the purged drawer rowid"
        );

        let stale_matches = db
            .search_fts("purgelegacyneedle", None, None, "all", None, 10)
            .expect("search stale purged payload through FTS");
        assert!(
            stale_matches.is_empty(),
            "purge must remove stale BM25 rows before rowid reuse: {stale_matches:?}"
        );
    }
}

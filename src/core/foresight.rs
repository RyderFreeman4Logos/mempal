//! First-class future-bound foresight memory over typed drawer rows.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    anchor,
    db::{Database, DbError},
    decay::parse_temporal_timestamp_secs,
    project::ProjectSearchScope,
    types::{
        AnchorKind, Drawer, KnowledgeEvidenceRole, KnowledgeStatus, MemoryDomain, MemoryKind,
        Provenance, SourceType,
    },
    utils::iso_timestamp,
};
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;

const FORESIGHT_SCHEMA_VERSION: u32 = 1;
const FORESIGHT_RECORD_TYPE: &str = "foresight";
const DEFAULT_FORESIGHT_ROOM: &str = "foresight";
const DEFAULT_FORESIGHT_FIELD: &str = "foresight";

#[derive(Debug, Error)]
pub enum ForesightError {
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    #[error("importance must be between 0 and 5")]
    InvalidImportance,
    #[error("invalid temporal field {field}: {value}")]
    InvalidTemporal { field: &'static str, value: String },
    #[error("foresight not found: {0}")]
    NotFound(String),
    #[error("drawer is not a foresight: {0}")]
    NotForesight(String),
    #[error("{field} must contain drawer ids")]
    MalformedRef { field: &'static str },
    #[error("ref drawer not found: {0}")]
    RefDrawerNotFound(String),
    #[error("{field} must point to evidence drawers: {drawer_id}")]
    RefNotEvidence {
        field: &'static str,
        drawer_id: String,
    },
    #[error("invalid foresight anchor/domain: {0}")]
    InvalidAnchorDomain(&'static str),
    #[error("database error: {0}")]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForesightStatus {
    Pending,
    Due,
    Resolved,
    Expired,
}

impl ForesightStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Due => "due",
            Self::Resolved => "resolved",
            Self::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForesightContent {
    pub schema_version: u32,
    pub record_type: String,
    pub statement: String,
    pub reason: Option<String>,
    pub trigger_condition: String,
    pub due_at: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct ForesightCreateRequest {
    pub statement: String,
    pub reason: Option<String>,
    pub trigger_condition: String,
    pub due_at: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub supporting_refs: Vec<String>,
    pub counterexample_refs: Vec<String>,
    pub verification_refs: Vec<String>,
    pub wing: String,
    pub room: Option<String>,
    pub project_id: Option<String>,
    pub domain: MemoryDomain,
    pub field: String,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub source_file: Option<String>,
    pub importance: i32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForesightCreateOutcome {
    pub drawer_id: String,
    pub created: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct ForesightListRequest {
    pub scope: ProjectSearchScope,
    pub domain: Option<MemoryDomain>,
    pub field: Option<String>,
    pub anchor_kind: Option<AnchorKind>,
    pub anchor_id: Option<String>,
    pub include_pending: bool,
    pub include_resolved: bool,
    pub include_expired: bool,
    pub now_unix: i64,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Foresight {
    pub drawer_id: String,
    pub statement: String,
    pub reason: Option<String>,
    pub trigger_condition: String,
    pub due_at: String,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub status: ForesightStatus,
    pub resolved_at: Option<String>,
    pub resolution_note: Option<String>,
    pub source_file: String,
    pub project_id: Option<String>,
    pub domain: MemoryDomain,
    pub field: String,
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
    pub supporting_refs: Vec<String>,
    pub counterexample_refs: Vec<String>,
    pub verification_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ForesightResolveRequest {
    pub drawer_id: String,
    pub resolution_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForesightResolveOutcome {
    pub drawer_id: String,
    pub resolved: bool,
    pub resolved_at: String,
}

struct ForesightRow {
    drawer_id: String,
    statement: String,
    reason: Option<String>,
    trigger_condition: String,
    due_at: String,
    valid_from: Option<String>,
    valid_until: Option<String>,
    resolved_at: Option<String>,
    resolution_note: Option<String>,
    source_file: Option<String>,
    drawer_status: Option<String>,
    supporting_refs: Vec<String>,
    counterexample_refs: Vec<String>,
    verification_refs: Vec<String>,
    domain: MemoryDomain,
    field: String,
    anchor_kind: AnchorKind,
    anchor_id: String,
    parent_anchor_id: Option<String>,
    project_id: Option<String>,
}

pub fn create_foresight(
    db: &Database,
    request: ForesightCreateRequest,
) -> Result<ForesightCreateOutcome, ForesightError> {
    if !(0..=5).contains(&request.importance) {
        return Err(ForesightError::InvalidImportance);
    }

    let statement = trim_required(&request.statement, "statement")?;
    let trigger_condition = trim_required(&request.trigger_condition, "trigger_condition")?;
    let due_at = validate_temporal("due_at", &request.due_at)?;
    let valid_from = request
        .valid_from
        .as_deref()
        .map(|value| validate_temporal("valid_from", value))
        .transpose()?
        .or_else(|| Some(due_at.clone()));
    let valid_until = request
        .valid_until
        .as_deref()
        .map(|value| validate_temporal("valid_until", value))
        .transpose()?;
    let wing = trim_required(&request.wing, "wing")?;
    let room = request
        .room
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or_else(|| Some(DEFAULT_FORESIGHT_ROOM.to_string()));
    let field = if request.field.trim().is_empty() {
        DEFAULT_FORESIGHT_FIELD.to_string()
    } else {
        request.field.trim().to_string()
    };
    if let Err(message) = anchor::validate_anchor_domain(&request.domain, &request.anchor_kind) {
        return Err(ForesightError::InvalidAnchorDomain(message));
    }
    let reason = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    let supporting_refs = normalize_refs("supporting_refs", &request.supporting_refs)?;
    let counterexample_refs = normalize_refs("counterexample_refs", &request.counterexample_refs)?;
    let verification_refs = normalize_refs("verification_refs", &request.verification_refs)?;
    validate_evidence_refs(db, "supporting_refs", &supporting_refs)?;
    validate_evidence_refs(db, "counterexample_refs", &counterexample_refs)?;
    validate_evidence_refs(db, "verification_refs", &verification_refs)?;

    let preferred_id = new_foresight_id(&statement, &due_at, &trigger_condition);
    let drawer_id = db.resolve_available_drawer_id(&preferred_id)?;
    if request.dry_run {
        return Ok(ForesightCreateOutcome {
            drawer_id,
            created: false,
            dry_run: true,
        });
    }

    let created_at = iso_timestamp();
    let content = ForesightContent {
        schema_version: FORESIGHT_SCHEMA_VERSION,
        record_type: FORESIGHT_RECORD_TYPE.to_string(),
        statement: statement.clone(),
        reason: reason.clone(),
        trigger_condition: trigger_condition.clone(),
        due_at: due_at.clone(),
        valid_from: valid_from.clone(),
        valid_until: valid_until.clone(),
        created_at: created_at.clone(),
    };
    let content_json = serde_json::to_string_pretty(&content)?;
    let source_type = SourceType::AgentObservation;
    let drawer = Drawer {
        id: drawer_id.clone(),
        content: content_json,
        wing,
        room,
        source_file: request
            .source_file
            .or_else(|| Some(format!("foresight://{}", drawer_id))),
        source_type,
        confidence: crate::core::types::default_confidence(source_type),
        added_at: created_at.clone(),
        chunk_index: Some(0),
        normalize_version: CURRENT_NORMALIZE_VERSION,
        importance: request.importance,
        memory_kind: MemoryKind::Foresight,
        domain: request.domain,
        field,
        anchor_kind: request.anchor_kind,
        anchor_id: request.anchor_id,
        parent_anchor_id: request.parent_anchor_id,
        provenance: Some(Provenance::Runtime),
        statement: Some(statement.clone()),
        tier: None,
        status: Some(KnowledgeStatus::Active),
        supporting_refs,
        counterexample_refs,
        teaching_refs: Vec::new(),
        verification_refs,
        scope_constraints: Some(format!("due_at={due_at}; trigger={trigger_condition}")),
        trigger_hints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: request.importance as f64,
        compacted_into: None,
    };

    db.conn().execute_batch("BEGIN IMMEDIATE")?;
    match insert_foresight_row(db, &drawer, &request.project_id, &content, &created_at) {
        Ok(()) => {
            db.conn().execute_batch("COMMIT")?;
            Ok(ForesightCreateOutcome {
                drawer_id,
                created: true,
                dry_run: false,
            })
        }
        Err(error) => {
            let _ = db.conn().execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

pub fn list_foresights(
    db: &Database,
    request: ForesightListRequest,
) -> Result<Vec<Foresight>, ForesightError> {
    let rows = load_candidate_rows(db.conn(), &request)?;
    let mut foresights = rows
        .into_iter()
        .filter_map(|row| row.into_foresight(request.now_unix))
        .filter(|foresight| match foresight.status {
            ForesightStatus::Due => true,
            ForesightStatus::Pending => request.include_pending,
            ForesightStatus::Resolved => request.include_resolved,
            ForesightStatus::Expired => request.include_expired,
        })
        .collect::<Vec<_>>();

    foresights.sort_by(|a, b| {
        parse_temporal_timestamp_secs(&a.due_at)
            .cmp(&parse_temporal_timestamp_secs(&b.due_at))
            .then_with(|| a.drawer_id.cmp(&b.drawer_id))
    });
    if let Some(limit) = request.limit {
        foresights.truncate(limit);
    }
    Ok(foresights)
}

pub fn resolve_foresight(
    db: &Database,
    request: ForesightResolveRequest,
) -> Result<ForesightResolveOutcome, ForesightError> {
    let drawer = db
        .get_drawer(&request.drawer_id)?
        .ok_or_else(|| ForesightError::NotFound(request.drawer_id.clone()))?;
    if drawer.memory_kind != MemoryKind::Foresight {
        return Err(ForesightError::NotForesight(request.drawer_id));
    }

    let resolved_at = iso_timestamp();
    let valid_until = current_unix_secs().saturating_sub(1).to_string();
    let note = request
        .resolution_note
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    db.conn().execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<bool, ForesightError> {
        let updated = db.conn().execute(
            r#"
            UPDATE foresights
            SET resolved_at = COALESCE(resolved_at, ?2),
                resolution_note = COALESCE(?3, resolution_note),
                updated_at = ?2
            WHERE drawer_id = ?1
              AND resolved_at IS NULL
            "#,
            params![request.drawer_id, resolved_at, note],
        )?;
        db.conn().execute(
            r#"
            UPDATE drawers
            SET status = 'retired',
                valid_until = ?2,
                updated_at = ?3
            WHERE id = ?1
              AND deleted_at IS NULL
              AND memory_kind = 'foresight'
            "#,
            params![request.drawer_id, valid_until, resolved_at],
        )?;
        Ok(updated > 0)
    })();

    match result {
        Ok(resolved) => {
            db.conn().execute_batch("COMMIT")?;
            Ok(ForesightResolveOutcome {
                drawer_id: request.drawer_id,
                resolved,
                resolved_at,
            })
        }
        Err(error) => {
            let _ = db.conn().execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

impl ForesightRow {
    fn into_foresight(self, now_unix: i64) -> Option<Foresight> {
        let due_at_secs = parse_temporal_timestamp_secs(&self.due_at)?;
        let valid_from_secs = self
            .valid_from
            .as_deref()
            .and_then(parse_temporal_timestamp_secs);
        let valid_until_secs = self
            .valid_until
            .as_deref()
            .and_then(parse_temporal_timestamp_secs);
        let resolved_by_status = self
            .drawer_status
            .as_deref()
            .is_some_and(|status| matches!(status, "retired" | "demoted" | "superseded"));
        let status = if self.resolved_at.is_some() || resolved_by_status {
            ForesightStatus::Resolved
        } else if valid_until_secs.is_some_and(|until| until < now_unix) {
            ForesightStatus::Expired
        } else if valid_from_secs.is_some_and(|from| from > now_unix) {
            ForesightStatus::Pending
        } else if due_at_secs <= now_unix {
            ForesightStatus::Due
        } else {
            ForesightStatus::Pending
        };

        Some(Foresight {
            drawer_id: self.drawer_id.clone(),
            statement: self.statement,
            reason: self.reason,
            trigger_condition: self.trigger_condition,
            due_at: self.due_at,
            valid_from: self.valid_from,
            valid_until: self.valid_until,
            status,
            resolved_at: self.resolved_at,
            resolution_note: self.resolution_note,
            source_file: self
                .source_file
                .unwrap_or_else(|| format!("foresight://{}", self.drawer_id)),
            project_id: self.project_id,
            domain: self.domain,
            field: self.field,
            anchor_kind: self.anchor_kind,
            anchor_id: self.anchor_id,
            parent_anchor_id: self.parent_anchor_id,
            supporting_refs: self.supporting_refs,
            counterexample_refs: self.counterexample_refs,
            verification_refs: self.verification_refs,
        })
    }
}

fn insert_foresight_row(
    db: &Database,
    drawer: &Drawer,
    project_id: &Option<String>,
    content: &ForesightContent,
    created_at: &str,
) -> Result<(), ForesightError> {
    db.insert_drawer_with_project_validity(
        drawer,
        project_id.as_deref(),
        None,
        content.valid_from.as_deref(),
        content.valid_until.as_deref(),
    )?;
    db.conn().execute(
        r#"
        INSERT INTO foresights (
            drawer_id,
            statement,
            reason,
            trigger_condition,
            due_at,
            valid_from,
            valid_until,
            created_at,
            updated_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
        "#,
        params![
            drawer.id.as_str(),
            content.statement.as_str(),
            content.reason.as_deref(),
            content.trigger_condition.as_str(),
            content.due_at.as_str(),
            content.valid_from.as_deref(),
            content.valid_until.as_deref(),
            created_at,
        ],
    )?;
    Ok(())
}

fn load_candidate_rows(
    conn: &Connection,
    request: &ForesightListRequest,
) -> Result<Vec<ForesightRow>, ForesightError> {
    let domain = request.domain.as_ref().map(domain_slug);
    let anchor_kind = request.anchor_kind.as_ref().map(anchor_kind_slug);
    let sql = r#"
        SELECT
            f.drawer_id,
            f.statement,
            f.reason,
            f.trigger_condition,
            f.due_at,
            COALESCE(f.valid_from, d.valid_from),
            COALESCE(f.valid_until, d.valid_until),
            f.resolved_at,
            f.resolution_note,
            d.source_file,
            d.status,
            d.supporting_refs,
            d.counterexample_refs,
            d.verification_refs,
            d.domain,
            d.field,
            d.anchor_kind,
            d.anchor_id,
            d.parent_anchor_id,
            d.project_id
        FROM foresights f
        JOIN drawers d ON d.id = f.drawer_id
        WHERE d.deleted_at IS NULL
          AND d.memory_kind = 'foresight'
          AND (?1 IS NULL OR d.domain = ?1)
          AND (?2 IS NULL OR d.field = ?2)
          AND (?3 IS NULL OR d.anchor_kind = ?3)
          AND (?4 IS NULL OR d.anchor_id = ?4)
          AND (
              ?5 = 'all'
              OR (?5 = 'project' AND d.project_id = ?6)
              OR (?5 = 'project_plus_global' AND (d.project_id = ?6 OR d.project_id IS NULL))
              OR (?5 = 'null_only' AND d.project_id IS NULL)
          )
        "#;
    let mut statement = conn.prepare(sql)?;
    statement
        .query_map(
            params![
                domain,
                request.field.as_deref(),
                anchor_kind,
                request.anchor_id.as_deref(),
                request.scope.mode_param(),
                request.scope.project_id.as_deref(),
            ],
            foresight_row_from_sql,
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(ForesightError::Sqlite)
}

fn foresight_row_from_sql(row: &rusqlite::Row<'_>) -> rusqlite::Result<ForesightRow> {
    let domain = parse_domain(&row.get::<_, String>(14)?).map_err(string_to_sql_error)?;
    let anchor_kind = parse_anchor_kind(&row.get::<_, String>(16)?).map_err(string_to_sql_error)?;
    let supporting_refs =
        parse_refs(row.get::<_, Option<String>>(11)?.as_deref()).map_err(error_to_sql_error)?;
    let counterexample_refs =
        parse_refs(row.get::<_, Option<String>>(12)?.as_deref()).map_err(error_to_sql_error)?;
    let verification_refs =
        parse_refs(row.get::<_, Option<String>>(13)?.as_deref()).map_err(error_to_sql_error)?;
    Ok(ForesightRow {
        drawer_id: row.get(0)?,
        statement: row.get(1)?,
        reason: row.get(2)?,
        trigger_condition: row.get(3)?,
        due_at: row.get(4)?,
        valid_from: row.get(5)?,
        valid_until: row.get(6)?,
        resolved_at: row.get(7)?,
        resolution_note: row.get(8)?,
        source_file: row.get(9)?,
        drawer_status: row.get(10)?,
        supporting_refs,
        counterexample_refs,
        verification_refs,
        domain,
        field: row.get(15)?,
        anchor_kind,
        anchor_id: row.get(17)?,
        parent_anchor_id: row.get(18)?,
        project_id: row.get(19)?,
    })
}

fn validate_temporal(field: &'static str, value: &str) -> Result<String, ForesightError> {
    let trimmed = trim_required(value, field)?;
    if parse_temporal_timestamp_secs(&trimmed).is_none() {
        return Err(ForesightError::InvalidTemporal {
            field,
            value: trimmed,
        });
    }
    Ok(trimmed)
}

fn validate_evidence_refs(
    db: &Database,
    field: &'static str,
    refs: &[String],
) -> Result<(), ForesightError> {
    for drawer_id in refs {
        let drawer = db
            .get_drawer(drawer_id)?
            .ok_or_else(|| ForesightError::RefDrawerNotFound(drawer_id.clone()))?;
        if drawer.memory_kind != MemoryKind::Evidence {
            return Err(ForesightError::RefNotEvidence {
                field,
                drawer_id: drawer_id.clone(),
            });
        }
    }
    Ok(())
}

fn normalize_refs(field: &'static str, values: &[String]) -> Result<Vec<String>, ForesightError> {
    let mut seen = BTreeSet::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(ForesightError::MalformedRef { field });
        }
        seen.insert(trimmed.to_string());
    }
    Ok(seen.into_iter().collect())
}

pub fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn evidence_role_refs(foresight: &Foresight) -> Vec<(KnowledgeEvidenceRole, String)> {
    foresight
        .supporting_refs
        .iter()
        .map(|id| (KnowledgeEvidenceRole::Supporting, id.clone()))
        .chain(
            foresight
                .counterexample_refs
                .iter()
                .map(|id| (KnowledgeEvidenceRole::Counterexample, id.clone())),
        )
        .chain(
            foresight
                .verification_refs
                .iter()
                .map(|id| (KnowledgeEvidenceRole::Verification, id.clone())),
        )
        .collect()
}

fn trim_required(value: &str, field: &'static str) -> Result<String, ForesightError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ForesightError::EmptyField(field));
    }
    Ok(trimmed.to_string())
}

fn new_foresight_id(statement: &str, due_at: &str, trigger_condition: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seed = format!("foresight:{statement}:{due_at}:{trigger_condition}:{nanos}");
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    format!("drawer_foresight_{}", &digest[..16])
}

fn parse_refs(raw: Option<&str>) -> Result<Vec<String>, serde_json::Error> {
    raw.map(serde_json::from_str)
        .transpose()
        .map(Option::unwrap_or_default)
}

fn parse_domain(value: &str) -> Result<MemoryDomain, String> {
    match value {
        "project" => Ok(MemoryDomain::Project),
        "user" => Ok(MemoryDomain::User),
        "agent" => Ok(MemoryDomain::Agent),
        "skill" => Ok(MemoryDomain::Skill),
        "global" => Ok(MemoryDomain::Global),
        other => Err(format!("unsupported domain: {other}")),
    }
}

fn parse_anchor_kind(value: &str) -> Result<AnchorKind, String> {
    match value {
        "global" => Ok(AnchorKind::Global),
        "repo" => Ok(AnchorKind::Repo),
        "worktree" => Ok(AnchorKind::Worktree),
        other => Err(format!("unsupported anchor kind: {other}")),
    }
}

fn domain_slug(value: &MemoryDomain) -> &'static str {
    match value {
        MemoryDomain::Project => "project",
        MemoryDomain::User => "user",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn anchor_kind_slug(value: &AnchorKind) -> &'static str {
    match value {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}

fn string_to_sql_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        error,
    )))
}

fn error_to_sql_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

//! Deterministic Case -> Skill procedural memory over typed drawer rows.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    anchor,
    db::{Database, DbError, fts_tokenize_content},
    skills::{ProposeSkillFromExemplarsArgs, propose_skill_from_exemplars},
    types::{
        AnchorKind, Drawer, KnowledgeStatus, MemoryDomain, MemoryKind, Provenance, SourceType,
    },
    utils::iso_timestamp,
};
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;

const CASE_SCHEMA_VERSION: u32 = 1;
const CASE_RECORD_TYPE: &str = "case";
const SKILL_PROPOSAL_RECORD_TYPE: &str = "skill_proposal";
const DEFAULT_CASE_FIELD: &str = "procedural_memory";

#[derive(Debug, Error)]
pub enum CaseSkillError {
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    #[error("importance must be between 0 and 5")]
    InvalidImportance,
    #[error("closing a case requires a non-open verdict")]
    OpenCloseVerdict,
    #[error("invalid case verdict: {0}")]
    InvalidVerdict(String),
    #[error("case not found: {0}")]
    CaseNotFound(String),
    #[error("drawer is not a case: {0}")]
    NotCase(String),
    #[error("case is already closed: {0}")]
    CaseAlreadyClosed(String),
    #[error("successful cases require at least one verification evidence ref")]
    MissingVerificationRef,
    #[error("verification refs must contain drawer ids")]
    MalformedVerificationRef,
    #[error("verification refs must point to evidence drawers: {0}")]
    VerificationRefNotEvidence(String),
    #[error("ref drawer not found: {0}")]
    RefDrawerNotFound(String),
    #[error("min_support must be greater than zero")]
    InvalidMinSupport,
    #[error("min_verification_refs must be greater than zero")]
    InvalidMinVerificationRefs,
    #[error("database error: {0}")]
    Db(#[from] DbError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseVerdict {
    Open,
    Success,
    Failure,
    Inconclusive,
}

impl CaseVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Inconclusive => "inconclusive",
        }
    }

    fn closing_status(self) -> Result<KnowledgeStatus, CaseSkillError> {
        match self {
            Self::Open => Err(CaseSkillError::OpenCloseVerdict),
            Self::Success | Self::Failure | Self::Inconclusive => Ok(KnowledgeStatus::Active),
        }
    }
}

impl FromStr for CaseVerdict {
    type Err = CaseSkillError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "open" => Ok(Self::Open),
            "success" | "succeeded" | "passed" => Ok(Self::Success),
            "failure" | "failed" | "rejected" => Ok(Self::Failure),
            "inconclusive" | "unknown" => Ok(Self::Inconclusive),
            other => Err(CaseSkillError::InvalidVerdict(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureMetadata {
    pub key: String,
    pub summary: String,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseContent {
    pub schema_version: u32,
    pub record_type: String,
    pub task: String,
    pub procedure: ProcedureMetadata,
    pub trajectory: Vec<String>,
    pub opened_at: String,
    pub closed_at: Option<String>,
    pub verdict: CaseVerdict,
    pub tests: Vec<String>,
    pub verification_refs: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub failed_approaches: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CaseOpenRequest {
    pub task: String,
    pub procedure_key: String,
    pub procedure_summary: String,
    pub procedure_steps: Vec<String>,
    pub trajectory: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub failed_approaches: Vec<String>,
    pub wing: String,
    pub room: String,
    pub project_id: Option<String>,
    pub importance: i32,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseOpenOutcome {
    pub case_id: String,
    pub created: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct CaseCloseRequest {
    pub case_id: String,
    pub verdict: CaseVerdict,
    pub tests: Vec<String>,
    pub verification_refs: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub failed_approaches: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseCloseOutcome {
    pub case_id: String,
    pub verdict: CaseVerdict,
    pub verification_refs: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SkillProposalOptions {
    pub from_cases: bool,
    pub min_support: usize,
    pub min_verification_refs: usize,
    pub wing: Option<String>,
    pub project_id: Option<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillProposalContent {
    pub schema_version: u32,
    pub record_type: String,
    pub procedure: ProcedureMetadata,
    pub support_count: usize,
    pub success_case_ids: Vec<String>,
    pub verification_refs: Vec<String>,
    pub counterexample_case_ids: Vec<String>,
    pub anti_patterns: Vec<String>,
    pub failed_approaches: Vec<String>,
    pub deterministic_path: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillProposal {
    pub skill_id: Option<String>,
    pub pattern_id: String,
    pub procedure_key: String,
    pub support_count: usize,
    pub verification_ref_count: usize,
    pub counterexample_count: usize,
    pub created: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillProposalBatch {
    pub proposals: Vec<SkillProposal>,
}

struct ParsedCase {
    id: String,
    content: CaseContent,
}

pub fn open_case(
    db: &Database,
    request: CaseOpenRequest,
) -> Result<CaseOpenOutcome, CaseSkillError> {
    if !(0..=5).contains(&request.importance) {
        return Err(CaseSkillError::InvalidImportance);
    }
    let task = trim_required(&request.task, "task")?;
    let procedure_key = trim_required(&request.procedure_key, "procedure_key")?;
    let procedure_summary = trim_required(&request.procedure_summary, "procedure")?;
    let wing = trim_required(&request.wing, "wing")?;
    let room = trim_required(&request.room, "room")?;

    let opened_at = iso_timestamp();
    let content = CaseContent {
        schema_version: CASE_SCHEMA_VERSION,
        record_type: CASE_RECORD_TYPE.to_string(),
        task,
        procedure: ProcedureMetadata {
            key: procedure_key,
            summary: procedure_summary,
            steps: normalized_ordered_strings(&request.procedure_steps),
        },
        trajectory: normalized_ordered_strings(&request.trajectory),
        opened_at,
        closed_at: None,
        verdict: CaseVerdict::Open,
        tests: Vec::new(),
        verification_refs: Vec::new(),
        anti_patterns: normalized_strings(&request.anti_patterns),
        failed_approaches: normalized_strings(&request.failed_approaches),
    };
    let content_json = serde_json::to_string_pretty(&content)?;
    let preferred_id = new_case_id(&content.procedure.key, &content.task);
    let case_id = db.resolve_available_drawer_id(&preferred_id)?;

    if request.dry_run {
        return Ok(CaseOpenOutcome {
            case_id,
            created: false,
            dry_run: true,
        });
    }

    let source_type = SourceType::AgentObservation;
    let drawer = Drawer {
        id: case_id.clone(),
        content: content_json,
        wing,
        room: Some(room),
        source_file: Some(format!("case://{}", content.procedure.key)),
        source_type,
        confidence: crate::core::types::default_confidence(source_type),
        added_at: content.opened_at.clone(),
        chunk_index: Some(0),
        normalize_version: CURRENT_NORMALIZE_VERSION,
        importance: request.importance,
        memory_kind: MemoryKind::Case,
        domain: MemoryDomain::Skill,
        field: DEFAULT_CASE_FIELD.to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Runtime),
        statement: Some(content.task.clone()),
        tier: None,
        status: Some(KnowledgeStatus::PendingReview),
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: Some(format!("procedure_key={}", content.procedure.key)),
        trigger_hints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: request.importance as f64,
        compacted_into: None,
    };

    db.insert_drawer_with_project(&drawer, request.project_id.as_deref())?;
    Ok(CaseOpenOutcome {
        case_id,
        created: true,
        dry_run: false,
    })
}

pub fn close_case(
    db: &Database,
    request: CaseCloseRequest,
) -> Result<CaseCloseOutcome, CaseSkillError> {
    with_immediate_transaction(db.conn(), || close_case_transaction(db, request))
}

fn close_case_transaction(
    db: &Database,
    request: CaseCloseRequest,
) -> Result<CaseCloseOutcome, CaseSkillError> {
    let status = request.verdict.closing_status()?;
    let mut drawer = db
        .get_drawer(&request.case_id)?
        .ok_or_else(|| CaseSkillError::CaseNotFound(request.case_id.clone()))?;
    if drawer.memory_kind != MemoryKind::Case {
        return Err(CaseSkillError::NotCase(request.case_id));
    }

    let mut content: CaseContent = serde_json::from_str(&drawer.content)?;
    if content.verdict != CaseVerdict::Open {
        return Err(CaseSkillError::CaseAlreadyClosed(drawer.id));
    }

    let verification_refs = normalized_strings(&request.verification_refs);
    if request.verdict == CaseVerdict::Success && verification_refs.is_empty() {
        return Err(CaseSkillError::MissingVerificationRef);
    }
    validate_verification_refs(db, &verification_refs)?;

    content.verdict = request.verdict;
    content.closed_at = Some(iso_timestamp());
    append_unique(
        &mut content.tests,
        normalized_ordered_strings(&request.tests),
    );
    append_unique(&mut content.verification_refs, verification_refs.clone());
    append_unique(
        &mut content.anti_patterns,
        normalized_strings(&request.anti_patterns),
    );
    append_unique(
        &mut content.failed_approaches,
        normalized_strings(&request.failed_approaches),
    );

    let old_content = drawer.content.clone();
    drawer.content = serde_json::to_string_pretty(&content)?;
    drawer.status = Some(status);
    drawer.verification_refs = content.verification_refs.clone();

    update_case_drawer(db, &drawer, &old_content)?;

    Ok(CaseCloseOutcome {
        case_id: drawer.id,
        verdict: request.verdict,
        verification_refs,
    })
}

fn with_immediate_transaction<T>(
    conn: &Connection,
    f: impl FnOnce() -> Result<T, CaseSkillError>,
) -> Result<T, CaseSkillError> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(value) => {
            if let Err(error) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

pub fn propose_skills_from_cases(
    db: &Database,
    options: SkillProposalOptions,
) -> Result<SkillProposalBatch, CaseSkillError> {
    if !options.from_cases {
        return Ok(SkillProposalBatch {
            proposals: Vec::new(),
        });
    }
    if options.min_support == 0 {
        return Err(CaseSkillError::InvalidMinSupport);
    }
    if options.min_verification_refs == 0 {
        return Err(CaseSkillError::InvalidMinVerificationRefs);
    }

    let cases = load_cases(db, options.wing.as_deref(), options.project_id.as_deref())?;
    let mut groups: BTreeMap<String, Vec<ParsedCase>> = BTreeMap::new();
    for case in cases {
        groups
            .entry(case.content.procedure.key.clone())
            .or_default()
            .push(case);
    }

    let mut proposals = Vec::new();
    for (procedure_key, group) in groups {
        let success_cases = verified_success_cases(&group, options.min_verification_refs);
        if success_cases.len() < options.min_support {
            continue;
        }

        let content = build_skill_proposal_content(&group, &success_cases);
        let supporting_refs = content.success_case_ids.clone();
        let verification_refs = content.verification_refs.clone();
        validate_verification_refs(db, &verification_refs)?;

        let pattern_id = build_case_skill_pattern_id(&content, options.project_id.as_deref());
        let name = skill_name(&content);
        let trigger_description = skill_trigger_description(&content);
        let (skill_id, created) = if options.dry_run {
            (None, false)
        } else {
            let proposed = propose_skill_from_exemplars(
                db.conn(),
                &ProposeSkillFromExemplarsArgs {
                    pattern_id: &pattern_id,
                    name: &name,
                    trigger_description: &trigger_description,
                    exemplar_ids: &supporting_refs,
                    project_id: options.project_id.as_deref(),
                },
            )?;
            (Some(proposed.skill.skill_id), proposed.created)
        };

        proposals.push(SkillProposal {
            skill_id,
            pattern_id,
            procedure_key,
            support_count: supporting_refs.len(),
            verification_ref_count: verification_refs.len(),
            counterexample_count: content.counterexample_case_ids.len(),
            created,
            dry_run: options.dry_run,
        });
    }

    Ok(SkillProposalBatch { proposals })
}

fn build_skill_proposal_content(
    group: &[ParsedCase],
    success_cases: &[&ParsedCase],
) -> SkillProposalContent {
    let first_success = success_cases[0];
    let mut verification_refs = BTreeSet::new();
    let mut counterexample_case_ids = BTreeSet::new();
    let mut anti_patterns = BTreeSet::new();
    let mut failed_approaches = BTreeSet::new();

    for case in success_cases {
        for item in &case.content.verification_refs {
            verification_refs.insert(item.clone());
        }
    }

    for case in group {
        for item in &case.content.anti_patterns {
            anti_patterns.insert(item.clone());
        }
        for item in &case.content.failed_approaches {
            failed_approaches.insert(item.clone());
        }
        if case.content.verdict != CaseVerdict::Success {
            counterexample_case_ids.insert(case.id.clone());
        }
    }

    SkillProposalContent {
        schema_version: CASE_SCHEMA_VERSION,
        record_type: SKILL_PROPOSAL_RECORD_TYPE.to_string(),
        procedure: first_success.content.procedure.clone(),
        support_count: success_cases.len(),
        success_case_ids: success_cases.iter().map(|case| case.id.clone()).collect(),
        verification_refs: verification_refs.into_iter().collect(),
        counterexample_case_ids: counterexample_case_ids.into_iter().collect(),
        anti_patterns: anti_patterns.into_iter().collect(),
        failed_approaches: failed_approaches.into_iter().collect(),
        deterministic_path: true,
    }
}

fn verified_success_cases(group: &[ParsedCase], min_verification_refs: usize) -> Vec<&ParsedCase> {
    group
        .iter()
        .filter(|case| {
            case.content.verdict == CaseVerdict::Success
                && case.content.verification_refs.len() >= min_verification_refs
        })
        .collect()
}

fn build_case_skill_pattern_id(
    proposal: &SkillProposalContent,
    project_id: Option<&str>,
) -> String {
    let scope = project_id.unwrap_or("global");
    let seed = format!("case-skill:{}:{}", scope, proposal.procedure.key);
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    format!("case-skill-{}", &digest[..16])
}

fn skill_name(proposal: &SkillProposalContent) -> String {
    proposal.procedure.summary.clone()
}

fn skill_trigger_description(proposal: &SkillProposalContent) -> String {
    let mut lines = vec![
        format!(
            "Use this deterministic procedure when the task matches procedure_key={}.",
            proposal.procedure.key
        ),
        format!("Procedure: {}", proposal.procedure.summary),
    ];
    if !proposal.procedure.steps.is_empty() {
        lines.push("Steps:".to_string());
        lines.extend(
            proposal
                .procedure
                .steps
                .iter()
                .map(|step| format!("- {step}")),
        );
    }
    lines.push(format!(
        "Support: {} verified case(s): {}.",
        proposal.support_count,
        proposal.success_case_ids.join(", ")
    ));
    lines.push(format!(
        "Verification refs: {}.",
        proposal.verification_refs.join(", ")
    ));
    if !proposal.counterexample_case_ids.is_empty() {
        lines.push(format!(
            "Counterexample cases: {}.",
            proposal.counterexample_case_ids.join(", ")
        ));
    }
    if !proposal.anti_patterns.is_empty() {
        lines.push("Anti-patterns:".to_string());
        lines.extend(
            proposal
                .anti_patterns
                .iter()
                .map(|item| format!("- {item}")),
        );
    }
    if !proposal.failed_approaches.is_empty() {
        lines.push("Failed approaches:".to_string());
        lines.extend(
            proposal
                .failed_approaches
                .iter()
                .map(|item| format!("- {item}")),
        );
    }
    lines.join("\n")
}

fn load_cases(
    db: &Database,
    wing: Option<&str>,
    project_id: Option<&str>,
) -> Result<Vec<ParsedCase>, CaseSkillError> {
    let mut statement = db.conn().prepare(
        r#"
        SELECT id, content
        FROM drawers
        WHERE deleted_at IS NULL
          AND memory_kind = 'case'
          AND (?1 IS NULL OR wing = ?1)
          AND (?2 IS NULL OR project_id = ?2 OR project_id IS NULL)
        ORDER BY id
        "#,
    )?;
    let rows = statement.query_map(params![wing, project_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut cases = Vec::new();
    for row in rows {
        let (id, raw_content) = row?;
        if let Ok(content) = serde_json::from_str::<CaseContent>(&raw_content) {
            if content.schema_version == CASE_SCHEMA_VERSION
                && content.record_type == CASE_RECORD_TYPE
            {
                cases.push(ParsedCase { id, content });
            }
        }
    }
    Ok(cases)
}

fn update_case_drawer(
    db: &Database,
    drawer: &Drawer,
    old_content: &str,
) -> Result<(), CaseSkillError> {
    let content_hash = blake3::hash(drawer.content.as_bytes()).to_hex().to_string();
    let old_content_hash = blake3::hash(old_content.as_bytes()).to_hex().to_string();
    let affected = db.conn().execute(
        r#"
        UPDATE drawers
        SET content = ?2,
            content_hash = ?3,
            status = ?4,
            verification_refs = ?5,
            counterexample_refs = ?6,
            updated_at = ?7
        WHERE id = ?1
          AND deleted_at IS NULL
          AND memory_kind = 'case'
          AND status = 'pending_review'
          AND content_hash = ?8
        "#,
        params![
            drawer.id,
            drawer.content,
            content_hash,
            drawer.status.as_ref().map(knowledge_status_slug),
            serde_json::to_string(&drawer.verification_refs)?,
            serde_json::to_string(&drawer.counterexample_refs)?,
            iso_timestamp(),
            old_content_hash,
        ],
    )?;
    if affected == 0 {
        return Err(CaseSkillError::CaseAlreadyClosed(drawer.id.clone()));
    }
    sync_fts_content(db, &drawer.id, old_content, &drawer.content)?;
    Ok(())
}

fn sync_fts_content(
    db: &Database,
    drawer_id: &str,
    old_content: &str,
    new_content: &str,
) -> Result<(), CaseSkillError> {
    let exists = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'drawers_fts')",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if exists != 1 {
        return Ok(());
    }

    let rowid = db
        .conn()
        .query_row(
            "SELECT rowid FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(rowid) = rowid else {
        return Ok(());
    };
    db.conn().execute(
        "INSERT INTO drawers_fts(drawers_fts, rowid, content) VALUES ('delete', ?1, ?2)",
        params![rowid, fts_tokenize_content(old_content)],
    )?;
    db.conn().execute(
        "INSERT INTO drawers_fts(rowid, content) VALUES (?1, ?2)",
        params![rowid, fts_tokenize_content(new_content)],
    )?;
    Ok(())
}

fn validate_verification_refs(db: &Database, refs: &[String]) -> Result<(), CaseSkillError> {
    for drawer_id in refs {
        if !drawer_id.starts_with("drawer_") {
            return Err(CaseSkillError::MalformedVerificationRef);
        }
        let drawer = db
            .get_drawer(drawer_id)?
            .ok_or_else(|| CaseSkillError::RefDrawerNotFound(drawer_id.clone()))?;
        if drawer.memory_kind != MemoryKind::Evidence {
            return Err(CaseSkillError::VerificationRefNotEvidence(
                drawer_id.clone(),
            ));
        }
    }
    Ok(())
}

fn trim_required(value: &str, field: &'static str) -> Result<String, CaseSkillError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CaseSkillError::EmptyField(field));
    }
    Ok(trimmed.to_string())
}

fn normalized_strings(values: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    for value in values {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            seen.insert(trimmed.to_string());
        }
    }
    seen.into_iter().collect()
}

fn normalized_ordered_strings(values: &[String]) -> Vec<String> {
    let mut normalized = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if !trimmed.is_empty() && !normalized.iter().any(|existing| existing == trimmed) {
            normalized.push(trimmed.to_string());
        }
    }
    normalized
}

fn append_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.iter().any(|existing| existing == &value) {
            target.push(value);
        }
    }
}

fn new_case_id(procedure_key: &str, task: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let seed = format!("case:{procedure_key}:{task}:{nanos}");
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    format!("drawer_mempal_cases_{}", &digest[..16])
}

fn knowledge_status_slug(status: &KnowledgeStatus) -> &'static str {
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

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::{Captures, Regex};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::utils::iso_timestamp;

const MAX_SUMMARY_CHARS: usize = 1_200;
const MAX_RULE_TEXT_CHARS: usize = 2_000;
const MAX_EVIDENCE_REF_CHARS: usize = 512;
const HIGH_VALUE_PRIORITY: u8 = 4;

#[derive(Debug, Error)]
pub enum DesignInsightError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid {kind}: {value}")]
    InvalidEnum { kind: &'static str, value: String },
    #[error("{field} is required")]
    MissingField { field: &'static str },
    #[error("{field} exceeds {max_chars} characters after redaction")]
    FieldTooLong {
        field: &'static str,
        max_chars: usize,
    },
    #[error("priority must be between 1 and 5, got {0}")]
    InvalidPriority(u8),
}

pub type Result<T> = std::result::Result<T, DesignInsightError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignInsightSource {
    UserIdea,
    ReviewFinding,
    ToolFriction,
    Incident,
    Research,
}

impl DesignInsightSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserIdea => "user_idea",
            Self::ReviewFinding => "review_finding",
            Self::ToolFriction => "tool_friction",
            Self::Incident => "incident",
            Self::Research => "research",
        }
    }
}

impl FromStr for DesignInsightSource {
    type Err = DesignInsightError;

    fn from_str(value: &str) -> Result<Self> {
        match normalize_slug(value).as_str() {
            "user_idea" => Ok(Self::UserIdea),
            "review_finding" => Ok(Self::ReviewFinding),
            "tool_friction" => Ok(Self::ToolFriction),
            "incident" => Ok(Self::Incident),
            "research" => Ok(Self::Research),
            _ => Err(DesignInsightError::InvalidEnum {
                kind: "design insight source",
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignInsightScope {
    Project,
    CrossProject,
    Repo,
    Issue,
}

impl DesignInsightScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::CrossProject => "cross_project",
            Self::Repo => "repo",
            Self::Issue => "issue",
        }
    }
}

impl FromStr for DesignInsightScope {
    type Err = DesignInsightError;

    fn from_str(value: &str) -> Result<Self> {
        match normalize_slug(value).as_str() {
            "project" => Ok(Self::Project),
            "cross_project" => Ok(Self::CrossProject),
            "repo" => Ok(Self::Repo),
            "issue" => Ok(Self::Issue),
            _ => Err(DesignInsightError::InvalidEnum {
                kind: "design insight scope",
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignInsightTargetArtifact {
    Memory,
    Skill,
    AgentsRule,
    AgentsRulesRef,
    CodexSkill,
    GithubIssue,
    MempalKnowledge,
}

impl DesignInsightTargetArtifact {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Skill => "skill",
            Self::AgentsRule => "agents_rule",
            Self::AgentsRulesRef => "agents_rules_ref",
            Self::CodexSkill => "codex_skill",
            Self::GithubIssue => "github_issue",
            Self::MempalKnowledge => "mempal_knowledge",
        }
    }
}

impl FromStr for DesignInsightTargetArtifact {
    type Err = DesignInsightError;

    fn from_str(value: &str) -> Result<Self> {
        match normalize_slug(value).as_str() {
            "memory" => Ok(Self::Memory),
            "skill" => Ok(Self::Skill),
            "agents_rule" | "agent_rule" => Ok(Self::AgentsRule),
            "agents_rules_ref" | "agents_ref" | "rules_ref" => Ok(Self::AgentsRulesRef),
            "codex_skill" | "codex_skills" => Ok(Self::CodexSkill),
            "github_issue" | "issue" => Ok(Self::GithubIssue),
            "mempal_knowledge" | "knowledge" => Ok(Self::MempalKnowledge),
            _ => Err(DesignInsightError::InvalidEnum {
                kind: "design insight target artifact",
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignInsightStatus {
    Open,
    Resolved,
}

impl DesignInsightStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
        }
    }
}

impl FromStr for DesignInsightStatus {
    type Err = DesignInsightError;

    fn from_str(value: &str) -> Result<Self> {
        match normalize_slug(value).as_str() {
            "open" => Ok(Self::Open),
            "resolved" => Ok(Self::Resolved),
            _ => Err(DesignInsightError::InvalidEnum {
                kind: "design insight status",
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewDesignInsight<'a> {
    pub source: DesignInsightSource,
    pub scope: DesignInsightScope,
    pub target_artifact: DesignInsightTargetArtifact,
    pub evidence_ref: &'a str,
    pub summary: &'a str,
    pub rule_text: Option<&'a str>,
    pub priority: u8,
    pub project_id: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct DesignInsightFilters {
    pub status: Option<DesignInsightStatus>,
    pub source: Option<DesignInsightSource>,
    pub scope: Option<DesignInsightScope>,
    pub target_artifact: Option<DesignInsightTargetArtifact>,
    pub min_priority: Option<u8>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignInsight {
    pub id: String,
    pub source: DesignInsightSource,
    pub scope: DesignInsightScope,
    pub target_artifact: DesignInsightTargetArtifact,
    pub evidence_ref: String,
    pub summary: String,
    pub rule_text: Option<String>,
    pub priority: u8,
    pub status: DesignInsightStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
    pub resolution_note: Option<String>,
    pub redaction_count: u32,
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesignInsightSummary {
    pub open_total: u64,
    pub high_value_open: u64,
    pub open_by_target: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    pub content: String,
    pub redaction_count: u32,
}

pub fn design_insights_table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='design_insights')",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        == 1
}

pub fn record_design_insight(
    conn: &Connection,
    input: &NewDesignInsight<'_>,
) -> Result<DesignInsight> {
    validate_priority(input.priority)?;
    let evidence =
        sanitize_required_field("evidence_ref", input.evidence_ref, MAX_EVIDENCE_REF_CHARS)?;
    let summary = sanitize_required_field("summary", input.summary, MAX_SUMMARY_CHARS)?;
    let rule_text = input
        .rule_text
        .map(|value| sanitize_optional_field("rule_text", value, MAX_RULE_TEXT_CHARS))
        .transpose()?
        .flatten();
    let project_id = input
        .project_id
        .map(|value| sanitize_optional_field("project_id", value, MAX_EVIDENCE_REF_CHARS))
        .transpose()?
        .flatten();
    let redaction_count = evidence.redaction_count
        + summary.redaction_count
        + rule_text
            .as_ref()
            .map(|value| value.redaction_count)
            .unwrap_or(0)
        + project_id
            .as_ref()
            .map(|value| value.redaction_count)
            .unwrap_or(0);
    let rule_content = rule_text.as_ref().map(|value| value.content.as_str());
    let project_content = project_id.as_ref().map(|value| value.content.as_str());
    let created_at = iso_timestamp();
    let id = build_design_insight_id(input, &created_at);

    conn.execute(
        r#"
        INSERT INTO design_insights (
            id, source, scope, target_artifact, evidence_ref, summary, rule_text,
            priority, status, created_at, redaction_count, project_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', ?9, ?10, ?11)
        "#,
        params![
            id,
            input.source.as_str(),
            input.scope.as_str(),
            input.target_artifact.as_str(),
            evidence.content,
            summary.content,
            rule_content,
            input.priority,
            created_at,
            redaction_count,
            project_content,
        ],
    )?;

    get_design_insight(conn, &id)?.ok_or(rusqlite::Error::QueryReturnedNoRows.into())
}

pub fn list_design_insights(
    conn: &Connection,
    filters: &DesignInsightFilters,
) -> Result<Vec<DesignInsight>> {
    if !design_insights_table_exists(conn) {
        return Ok(Vec::new());
    }
    if let Some(priority) = filters.min_priority {
        validate_priority(priority)?;
    }
    let status = filters.status.map(DesignInsightStatus::as_str);
    let source = filters.source.map(DesignInsightSource::as_str);
    let scope = filters.scope.map(DesignInsightScope::as_str);
    let target = filters
        .target_artifact
        .map(DesignInsightTargetArtifact::as_str);
    let min_priority = i64::from(filters.min_priority.unwrap_or(1));
    let limit = filters.limit.unwrap_or(100).min(1_000) as i64;

    let mut stmt = conn.prepare(
        r#"
        SELECT id, source, scope, target_artifact, evidence_ref, summary, rule_text,
               priority, status, created_at, resolved_at, resolved_by, resolution_note,
               redaction_count, project_id
        FROM design_insights
        WHERE (?1 IS NULL OR status = ?1)
          AND (?2 IS NULL OR source = ?2)
          AND (?3 IS NULL OR scope = ?3)
          AND (?4 IS NULL OR target_artifact = ?4)
          AND priority >= ?5
        ORDER BY
          CASE status WHEN 'open' THEN 0 ELSE 1 END,
          priority DESC,
          created_at DESC,
          id ASC
        LIMIT ?6
        "#,
    )?;
    let rows = stmt
        .query_map(
            params![status, source, scope, target, min_priority, limit],
            design_insight_from_row,
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter().collect()
}

pub fn get_design_insight(conn: &Connection, id: &str) -> Result<Option<DesignInsight>> {
    if !design_insights_table_exists(conn) {
        return Ok(None);
    }
    let row = conn
        .query_row(
            r#"
            SELECT id, source, scope, target_artifact, evidence_ref, summary, rule_text,
                   priority, status, created_at, resolved_at, resolved_by, resolution_note,
                   redaction_count, project_id
            FROM design_insights
            WHERE id = ?1
            "#,
            [id],
            design_insight_from_row,
        )
        .optional()?;
    row.transpose()
}

pub fn resolve_design_insight(
    conn: &Connection,
    id: &str,
    actor: Option<&str>,
    note: Option<&str>,
) -> Result<bool> {
    if !design_insights_table_exists(conn) {
        return Ok(false);
    }
    let actor = actor
        .map(|value| sanitize_optional_field("actor", value, MAX_EVIDENCE_REF_CHARS))
        .transpose()?
        .flatten();
    let note = note
        .map(|value| sanitize_optional_field("resolution_note", value, MAX_RULE_TEXT_CHARS))
        .transpose()?
        .flatten();
    let changed = conn.execute(
        r#"
        UPDATE design_insights
        SET status = 'resolved',
            resolved_at = ?2,
            resolved_by = ?3,
            resolution_note = ?4,
            redaction_count = redaction_count + ?5
        WHERE id = ?1 AND status = 'open'
        "#,
        params![
            id,
            iso_timestamp(),
            actor.as_ref().map(|value| value.content.as_str()),
            note.as_ref().map(|value| value.content.as_str()),
            actor
                .as_ref()
                .map(|value| value.redaction_count)
                .unwrap_or(0)
                + note
                    .as_ref()
                    .map(|value| value.redaction_count)
                    .unwrap_or(0),
        ],
    )?;
    Ok(changed > 0)
}

pub fn unresolved_design_insight_summary(conn: &Connection) -> Result<DesignInsightSummary> {
    if !design_insights_table_exists(conn) {
        return Ok(DesignInsightSummary::default());
    }
    let (open_total, high_value_open) = conn.query_row(
        r#"
        SELECT
            COUNT(*),
            COALESCE(SUM(CASE WHEN priority >= ?1 THEN 1 ELSE 0 END), 0)
        FROM design_insights
        WHERE status = 'open'
        "#,
        [HIGH_VALUE_PRIORITY],
        |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)),
    )?;

    let mut stmt = conn.prepare(
        r#"
        SELECT target_artifact, COUNT(*)
        FROM design_insights
        WHERE status = 'open'
        GROUP BY target_artifact
        ORDER BY target_artifact
        "#,
    )?;
    let open_by_target = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<std::result::Result<BTreeMap<_, _>, _>>()?;

    Ok(DesignInsightSummary {
        open_total,
        high_value_open,
        open_by_target,
    })
}

pub fn sanitize_design_insight_text(input: &str) -> RedactedText {
    let mut content = input.trim().to_string();
    let mut redaction_count = 0u32;

    for (regex, replacement) in [
        (private_key_re(), "<redacted-private-key>"),
        (prompt_block_re(), "<redacted-prompt>"),
        (auth_header_re(), "Authorization: <redacted>"),
        (bearer_re(), "Bearer <redacted>"),
        (url_userinfo_re(), "$1<redacted>@"),
        (url_secret_query_re(), "$1<redacted>"),
    ] {
        let count = regex.find_iter(&content).count() as u32;
        if count > 0 {
            content = regex.replace_all(&content, replacement).into_owned();
            redaction_count += count;
        }
    }

    let count = key_value_secret_re().find_iter(&content).count() as u32;
    if count > 0 {
        content = key_value_secret_re()
            .replace_all(&content, |captures: &Captures<'_>| {
                redact_captured_value(captures, "<redacted>")
            })
            .into_owned();
        redaction_count += count;
    }

    let count = quoted_prompt_field_re().find_iter(&content).count() as u32;
    if count > 0 {
        content = quoted_prompt_field_re()
            .replace_all(&content, |captures: &Captures<'_>| {
                redact_captured_value(captures, "<redacted-prompt>")
            })
            .into_owned();
        redaction_count += count;
    }

    let count = prompt_line_re().find_iter(&content).count() as u32;
    if count > 0 {
        content = prompt_line_re()
            .replace_all(&content, |captures: &Captures<'_>| {
                format!("{}<redacted-prompt>", &captures[1])
            })
            .into_owned();
        redaction_count += count;
    }

    RedactedText {
        content,
        redaction_count,
    }
}

fn sanitize_required_field(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<RedactedText> {
    let redacted = sanitize_optional_field(field, value, max_chars)?
        .ok_or(DesignInsightError::MissingField { field })?;
    Ok(redacted)
}

fn sanitize_optional_field(
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<Option<RedactedText>> {
    let redacted = sanitize_design_insight_text(value);
    if redacted.content.trim().is_empty() {
        return Ok(None);
    }
    if redacted.content.chars().count() > max_chars {
        return Err(DesignInsightError::FieldTooLong { field, max_chars });
    }
    Ok(Some(redacted))
}

fn validate_priority(priority: u8) -> Result<()> {
    if (1..=5).contains(&priority) {
        Ok(())
    } else {
        Err(DesignInsightError::InvalidPriority(priority))
    }
}

fn design_insight_from_row(row: &Row<'_>) -> rusqlite::Result<Result<DesignInsight>> {
    let priority = row.get::<_, i64>(7)? as u8;
    let redaction_count = row.get::<_, i64>(13)? as u32;
    let source = match row.get::<_, String>(1)?.parse::<DesignInsightSource>() {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let scope = match row.get::<_, String>(2)?.parse::<DesignInsightScope>() {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let target_artifact = match row
        .get::<_, String>(3)?
        .parse::<DesignInsightTargetArtifact>()
    {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let status = match row.get::<_, String>(8)?.parse::<DesignInsightStatus>() {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    Ok(Ok(DesignInsight {
        id: row.get(0)?,
        source,
        scope,
        target_artifact,
        evidence_ref: row.get(4)?,
        summary: row.get(5)?,
        rule_text: row.get(6)?,
        priority,
        status,
        created_at: row.get(9)?,
        resolved_at: row.get(10)?,
        resolved_by: row.get(11)?,
        resolution_note: row.get(12)?,
        redaction_count,
        project_id: row.get(14)?,
    }))
}

fn build_design_insight_id(input: &NewDesignInsight<'_>, created_at: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "{}:{}:{}:{}:{}:{}",
        nanos,
        created_at,
        input.source.as_str(),
        input.scope.as_str(),
        input.target_artifact.as_str(),
        input.evidence_ref
    );
    let digest = blake3::hash(seed.as_bytes()).to_hex().to_string();
    format!("insight_{}", &digest[..16])
}

fn normalize_slug(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| match ch {
            '-' | ' ' | '.' | '/' => '_',
            _ => ch,
        })
        .collect()
}

fn private_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        )
        .expect("static private key regex")
    })
}

fn prompt_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<prompt\b[^>]*>.*?</prompt>").expect("static prompt regex"))
}

fn auth_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)^\s*authorization\s*:\s*.+$").expect("static auth header regex")
    })
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("static bearer regex")
    })
}

fn url_userinfo_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(https?://)[^/\s:@]+:[^@\s/]+@").expect("static url userinfo regex")
    })
}

fn url_secret_query_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)([?&](?:api[-_]?key|access[-_]?token|refresh[-_]?token|token|secret|password|key)=)[^&#\s]+",
        )
        .expect("static url secret query regex")
    })
}

fn key_value_secret_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            (?P<prefix>
                "(?:[A-Z0-9]+[-_.])*(?:api[-_]?key|access[-_]?token|refresh[-_]?token|secret[-_]?access[-_]?key|secret[-_]?key|private[-_]?key|token|secret|password|passwd|pwd)"\s*:\s*
                |
                \b(?:[A-Z0-9]+[-_.])*(?:api[-_]?key|access[-_]?token|refresh[-_]?token|secret[-_]?access[-_]?key|secret[-_]?key|private[-_]?key|token|secret|password|passwd|pwd)\b\s*[:=]\s*
            )
            (?P<value>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,"';&]+)
            "#,
        )
        .expect("static key value secret regex")
    })
}

fn quoted_prompt_field_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            (?P<prefix>"(?:raw[-_\s]?prompt|user[-_\s]?prompt|prompt)"\s*:\s*)
            (?P<value>"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s,"';&]+)
            "#,
        )
        .expect("static quoted prompt field regex")
    })
}

fn redact_captured_value(captures: &Captures<'_>, placeholder: &str) -> String {
    let prefix = captures
        .name("prefix")
        .map(|match_| match_.as_str())
        .unwrap_or_default();
    let value = captures
        .name("value")
        .map(|match_| match_.as_str())
        .unwrap_or_default();
    match value.chars().next() {
        Some(quote @ ('\'' | '"')) => format!("{prefix}{quote}{placeholder}{quote}"),
        _ => format!("{prefix}{placeholder}"),
    }
}

fn prompt_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?im)^(\s*(?:raw\s+prompt|prompt|user\s+prompt)\s*:\s*).*$")
            .expect("static prompt line regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;

    #[test]
    fn record_list_and_resolve_design_insight() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&tempdir.path().join("palace.db")).expect("open db");

        let insight = record_design_insight(
            db.conn(),
            &NewDesignInsight {
                source: DesignInsightSource::ReviewFinding,
                scope: DesignInsightScope::Issue,
                target_artifact: DesignInsightTargetArtifact::GithubIssue,
                evidence_ref: "https://github.com/RyderFreeman4Logos/mempal/issues/415",
                summary: "Capture reusable review findings as drainable design records.",
                rule_text: Some("Every non-trivial issue gets a design-opportunity pass."),
                priority: 5,
                project_id: Some("mempal"),
            },
        )
        .expect("record insight");

        let listed = list_design_insights(
            db.conn(),
            &DesignInsightFilters {
                status: Some(DesignInsightStatus::Open),
                min_priority: Some(4),
                ..DesignInsightFilters::default()
            },
        )
        .expect("list insights");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, insight.id);
        assert_eq!(listed[0].priority, 5);

        let summary = unresolved_design_insight_summary(db.conn()).expect("summary");
        assert_eq!(summary.open_total, 1);
        assert_eq!(summary.high_value_open, 1);
        assert_eq!(summary.open_by_target["github_issue"], 1);

        assert!(
            resolve_design_insight(db.conn(), &insight.id, Some("codex"), Some("filed issue"))
                .expect("resolve")
        );
        let summary = unresolved_design_insight_summary(db.conn()).expect("summary");
        assert_eq!(summary.open_total, 0);
        assert_eq!(summary.high_value_open, 0);
    }

    #[test]
    fn redacts_sensitive_design_insight_content() {
        let input = "\
Authorization: Bearer fixturebearertoken
POST https://user:pass@example.test/path?token=abc123&safe=1
api_key=\"abc123\"
OPENAI_API_KEY=providerfixture12345
GITHUB_TOKEN=githubfixture12345
DJANGO_SECRET_KEY=secretkeyfixture12345
JWT_SECRET_KEY=jwtsecretfixture12345
<prompt>raw user prompt body</prompt>
-----BEGIN PRIVATE KEY-----
abc
-----END PRIVATE KEY-----";

        let redacted = sanitize_design_insight_text(input);

        assert!(redacted.redaction_count >= 5);
        assert!(!redacted.content.contains("fixturebearertoken"));
        assert!(!redacted.content.contains("abc123"));
        assert!(!redacted.content.contains("providerfixture12345"));
        assert!(!redacted.content.contains("githubfixture12345"));
        assert!(!redacted.content.contains("secretkeyfixture12345"));
        assert!(!redacted.content.contains("jwtsecretfixture12345"));
        assert!(!redacted.content.contains("user:pass"));
        assert!(!redacted.content.contains("raw user prompt body"));
        assert!(!redacted.content.contains("PRIVATE KEY-----\nabc"));
        assert!(redacted.content.contains("<redacted>"));
    }

    #[test]
    fn redacts_quoted_json_secret_and_prompt_fields() {
        let input = r#"provider body {"OPENAI_API_KEY":"providerfixture12345","JWT_SECRET_KEY":"secretkeyfixture12345 with spaces","GITHUB_TOKEN":"githubfixture12345","prompt":"promptfixture12345 with spaces","user_prompt":"userpromptfixture12345 with spaces"}"#;

        let redacted = sanitize_design_insight_text(input);

        assert!(redacted.redaction_count >= 5);
        assert!(!redacted.content.contains("providerfixture12345"));
        assert!(!redacted.content.contains("secretkeyfixture12345"));
        assert!(!redacted.content.contains("githubfixture12345"));
        assert!(!redacted.content.contains("promptfixture12345"));
        assert!(!redacted.content.contains("userpromptfixture12345"));
        assert!(
            redacted
                .content
                .contains(r#""OPENAI_API_KEY":"<redacted>""#)
        );
        assert!(
            redacted
                .content
                .contains(r#""JWT_SECRET_KEY":"<redacted>""#)
        );
        assert!(redacted.content.contains(r#""GITHUB_TOKEN":"<redacted>""#));
        assert!(redacted.content.contains(r#""prompt":"<redacted-prompt>""#));
        assert!(
            redacted
                .content
                .contains(r#""user_prompt":"<redacted-prompt>""#)
        );
    }
}

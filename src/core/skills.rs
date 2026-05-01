//! Skill crystallization (P15): promote validated patterns into callable skills.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const SKILLS_SCHEMA_MIN_FORK_EXT_VERSION: u32 = 13;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    Probationary,
    Active,
    Retired,
}

impl SkillStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Probationary => "probationary",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }
}

impl std::str::FromStr for SkillStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "probationary" => Ok(Self::Probationary),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            other => Err(format!("unknown skill status: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub skill_id: String,
    pub name: String,
    pub trigger_description: String,
    pub pattern_id: String,
    pub exemplar_ids: Vec<String>,
    pub adoption_count: i64,
    pub rejection_count: i64,
    pub status: SkillStatus,
    pub promoted_at: i64,
    pub updated_at: i64,
    pub project_id: Option<String>,
}

impl Skill {
    /// Laplace-smoothed adoption rate. Computed at query time, not stored.
    pub fn eta(&self) -> f64 {
        compute_eta(self.adoption_count, self.rejection_count)
    }
}

/// Lightweight skill summary for context injection into T1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillForContext {
    pub skill_id: String,
    pub name: String,
    pub trigger_description: String,
    pub eta: f64,
    pub exemplar_count: usize,
}

/// Compute Laplace-smoothed adoption rate.
/// Formula: adoption / (adoption + rejection + 1.0)
pub fn compute_eta(adoption: i64, rejection: i64) -> f64 {
    adoption as f64 / (adoption as f64 + rejection as f64 + 1.0)
}

#[derive(Debug, Error)]
pub enum PromotionError {
    #[error("pattern not found: {0}")]
    PatternNotFound(String),
    #[error("pattern is not active (current status: {0})")]
    PatternNotActive(String),
    #[error("pattern has insufficient session count ({0} < {1})")]
    InsufficientSessions(usize, usize),
    #[error("a probationary or active skill already exists for this pattern")]
    SkillAlreadyExists,
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
}

pub struct PromoteArgs<'a> {
    pub pattern_id: &'a str,
    pub name: &'a str,
    pub trigger_description: &'a str,
    pub skill_min_sessions: usize,
    pub project_id: Option<&'a str>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_skill_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (ts >> 96) as u32,
        (ts >> 80) as u16,
        (ts >> 68) as u16 & 0x0fff,
        ((ts >> 52) as u16 & 0x3fff) | 0x8000,
        ts as u64 & 0xffffffffffff,
    )
}

/// Returns true if the `skills` table exists (fork_ext_version >= 13).
pub fn skills_table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skills'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// Promote an active pattern to a probationary skill.
pub fn promote_pattern_to_skill(
    conn: &Connection,
    args: &PromoteArgs<'_>,
) -> Result<Skill, PromotionError> {
    // Fetch pattern, check status and session_count.
    let row = conn
        .query_row(
            "SELECT status, session_count, exemplar_ids FROM patterns WHERE pattern_id = ?1",
            [args.pattern_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(PromotionError::Db)?;

    let (status_str, session_count, exemplar_json) =
        row.ok_or_else(|| PromotionError::PatternNotFound(args.pattern_id.to_string()))?;

    if status_str != "active" {
        return Err(PromotionError::PatternNotActive(status_str));
    }
    if (session_count as usize) < args.skill_min_sessions {
        return Err(PromotionError::InsufficientSessions(
            session_count as usize,
            args.skill_min_sessions,
        ));
    }

    // Check for existing probationary/active skill on this pattern.
    let existing = conn
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE pattern_id = ?1 AND status IN ('probationary', 'active')",
            [args.pattern_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(PromotionError::Db)?;
    if existing > 0 {
        return Err(PromotionError::SkillAlreadyExists);
    }

    let exemplar_ids: Vec<String> = serde_json::from_str(&exemplar_json).unwrap_or_default();
    let exemplar_ids_json =
        serde_json::to_string(&exemplar_ids).unwrap_or_else(|_| "[]".to_string());

    let skill_id = new_skill_id();
    let now = now_ms();

    conn.execute(
        r#"
        INSERT INTO skills (
            skill_id, name, trigger_description, pattern_id, exemplar_ids,
            adoption_count, rejection_count, status, promoted_at, updated_at, project_id
        ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 'probationary', ?6, ?6, ?7)
        "#,
        params![
            skill_id,
            args.name,
            args.trigger_description,
            args.pattern_id,
            exemplar_ids_json,
            now,
            args.project_id,
        ],
    )
    .map_err(PromotionError::Db)?;

    Ok(Skill {
        skill_id,
        name: args.name.to_string(),
        trigger_description: args.trigger_description.to_string(),
        pattern_id: args.pattern_id.to_string(),
        exemplar_ids,
        adoption_count: 0,
        rejection_count: 0,
        status: SkillStatus::Probationary,
        promoted_at: now,
        updated_at: now,
        project_id: args.project_id.map(str::to_string),
    })
}

/// Record an adoption signal. Returns new status after the signal.
/// If adoption_count reaches active_threshold, skill transitions to active.
pub fn adopt_skill(
    conn: &Connection,
    skill_id: &str,
    active_threshold: i64,
) -> rusqlite::Result<Option<SkillStatus>> {
    let now = now_ms();
    // Fetch current state.
    let row = conn
        .query_row(
            "SELECT adoption_count, status FROM skills WHERE skill_id = ?1",
            [skill_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((adoption_count, status_str)) = row else {
        return Ok(None);
    };

    // Don't record on retired skills (spec allows free signalling, but retired is final).
    if status_str == "retired" {
        return Ok(Some(SkillStatus::Retired));
    }

    let new_adoption = adoption_count + 1;
    let new_status = if new_adoption >= active_threshold && status_str == "probationary" {
        "active"
    } else {
        &status_str
    };

    conn.execute(
        "UPDATE skills SET adoption_count = ?2, status = ?3, updated_at = ?4 WHERE skill_id = ?1",
        params![skill_id, new_adoption, new_status, now],
    )?;

    Ok(Some(
        new_status
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
    ))
}

/// Record a rejection signal. Returns new status after the signal.
/// If rejection_count >= retire_threshold AND adoption_count == 0, auto-retires.
pub fn reject_skill(
    conn: &Connection,
    skill_id: &str,
    retire_threshold: i64,
) -> rusqlite::Result<Option<SkillStatus>> {
    let now = now_ms();
    let row = conn
        .query_row(
            "SELECT adoption_count, rejection_count, status FROM skills WHERE skill_id = ?1",
            [skill_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((adoption_count, rejection_count, status_str)) = row else {
        return Ok(None);
    };

    if status_str == "retired" {
        return Ok(Some(SkillStatus::Retired));
    }

    let new_rejection = rejection_count + 1;
    let new_status = if new_rejection >= retire_threshold && adoption_count == 0 {
        "retired"
    } else {
        &status_str
    };

    conn.execute(
        "UPDATE skills SET rejection_count = ?2, status = ?3, updated_at = ?4 WHERE skill_id = ?1",
        params![skill_id, new_rejection, new_status, now],
    )?;

    Ok(Some(
        new_status
            .parse()
            .map_err(|_| rusqlite::Error::InvalidQuery)?,
    ))
}

/// Manually retire a skill. Returns true if found and updated.
pub fn retire_skill(conn: &Connection, skill_id: &str) -> rusqlite::Result<bool> {
    let now = now_ms();
    let count = conn.execute(
        "UPDATE skills SET status = 'retired', updated_at = ?2 WHERE skill_id = ?1 AND status != 'retired'",
        params![skill_id, now],
    )?;
    Ok(count > 0)
}

/// Fetch all skills matching optional status and project_id filters.
pub fn list_skills(
    conn: &Connection,
    status: Option<&str>,
    project_id: Option<&str>,
) -> rusqlite::Result<Vec<Skill>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT skill_id, name, trigger_description, pattern_id, exemplar_ids,
               adoption_count, rejection_count, status, promoted_at, updated_at, project_id
        FROM skills
        WHERE (?1 IS NULL OR status = ?1)
          AND (?2 IS NULL OR project_id = ?2 OR project_id IS NULL)
        ORDER BY updated_at DESC
        "#,
    )?;
    let rows = stmt
        .query_map(params![status, project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, Option<String>>(10)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    rows.into_iter()
        .map(
            |(
                skill_id,
                name,
                trigger_description,
                pattern_id,
                exemplar_json,
                adoption_count,
                rejection_count,
                status_str,
                promoted_at,
                updated_at,
                project_id,
            )| {
                let status = status_str
                    .parse::<SkillStatus>()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                Ok(Skill {
                    skill_id,
                    name,
                    trigger_description,
                    pattern_id,
                    exemplar_ids: serde_json::from_str(&exemplar_json).unwrap_or_default(),
                    adoption_count,
                    rejection_count,
                    status,
                    promoted_at,
                    updated_at,
                    project_id,
                })
            },
        )
        .collect()
}

/// Fetch a single skill by ID.
pub fn get_skill(conn: &Connection, skill_id: &str) -> rusqlite::Result<Option<Skill>> {
    let result = conn
        .query_row(
            r#"
            SELECT skill_id, name, trigger_description, pattern_id, exemplar_ids,
                   adoption_count, rejection_count, status, promoted_at, updated_at, project_id
            FROM skills WHERE skill_id = ?1
            "#,
            [skill_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .optional()?;

    let Some((
        skill_id,
        name,
        trigger_description,
        pattern_id,
        exemplar_json,
        adoption_count,
        rejection_count,
        status_str,
        promoted_at,
        updated_at,
        project_id,
    )) = result
    else {
        return Ok(None);
    };

    let status = status_str
        .parse::<SkillStatus>()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(Some(Skill {
        skill_id,
        name,
        trigger_description,
        pattern_id,
        exemplar_ids: serde_json::from_str(&exemplar_json).unwrap_or_default(),
        adoption_count,
        rejection_count,
        status,
        promoted_at,
        updated_at,
        project_id,
    }))
}

/// Load active skills for context injection into T1.
/// Filters by cosine similarity between query_vector and the pattern signature.
/// Returns skills ordered by eta DESC.
pub fn load_active_skills_for_context(
    conn: &Connection,
    project_id: Option<&str>,
    query_vector: &[f32],
    similarity_threshold: f32,
) -> rusqlite::Result<Vec<SkillForContext>> {
    // Fetch all active skills (with pattern signatures).
    let mut stmt = conn.prepare(
        r#"
        SELECT s.skill_id, s.name, s.trigger_description, s.exemplar_ids,
               s.adoption_count, s.rejection_count, p.signature
        FROM skills s
        JOIN patterns p ON p.pattern_id = s.pattern_id
        WHERE s.status = 'active'
          AND (?1 IS NULL OR s.project_id = ?1 OR s.project_id IS NULL)
        "#,
    )?;

    let rows = stmt
        .query_map([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Vec<u8>>(6)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut skills: Vec<SkillForContext> = rows
        .into_iter()
        .filter_map(
            |(
                skill_id,
                name,
                trigger_description,
                exemplar_json,
                adoption,
                rejection,
                sig_blob,
            )| {
                let sig = blob_to_vec(&sig_blob);
                let sim = if query_vector.is_empty() || sig.is_empty() {
                    0.0f32
                } else {
                    cosine_similarity(query_vector, &sig)
                };
                if sim < similarity_threshold {
                    return None;
                }
                let exemplar_ids: Vec<String> =
                    serde_json::from_str(&exemplar_json).unwrap_or_default();
                Some(SkillForContext {
                    skill_id,
                    name,
                    trigger_description,
                    eta: compute_eta(adoption, rejection),
                    exemplar_count: exemplar_ids.len(),
                })
            },
        )
        .collect();

    // Sort by eta descending (higher adoption rate first).
    skills.sort_by(|a, b| {
        b.eta
            .partial_cmp(&a.eta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(skills)
}

fn blob_to_vec(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .filter_map(|chunk| chunk.try_into().ok())
        .map(f32::from_le_bytes)
        .collect()
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

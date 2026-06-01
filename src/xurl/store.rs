use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::xurl::model::{Provenance, RawTurn, Role, Tool};
use crate::xurl::{XurlError, XurlResult};

pub struct StoredTurn {
    pub id: String,
    pub session_id: String,
    pub tool: Tool,
    pub turn_index: u32,
    pub role: Role,
    pub content: String,
    pub timestamp_epoch: f64,
    pub token_count: Option<i64>,
    pub project_path: Option<String>,
    pub source_path: Option<String>,
    pub git_branch: Option<String>,
    pub is_csa_delegated: bool,
    pub provenance: Provenance,
}

#[derive(Debug, Default)]
pub struct InsertStats {
    pub inserted: usize,
    pub skipped: usize,
    pub updated: usize,
}

#[derive(Debug, Default)]
pub struct TurnFilter {
    pub tool: Option<Tool>,
    pub session_id: Option<String>,
    pub since_epoch: Option<f64>,
    pub limit: usize,
    pub offset: usize,
}

pub struct ToolStat {
    pub tool: String,
    pub count: i64,
    pub min_timestamp: f64,
    pub max_timestamp: f64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct UnindexedTurnSummary {
    pub threads: i64,
    pub turns: i64,
}

#[derive(Debug, Serialize)]
pub struct TimelineTurn {
    pub id: String,
    pub session_id: String,
    pub tool: String,
    pub role: String,
    pub content: String,
    pub timestamp_epoch: f64,
    pub source_path: Option<String>,
}

impl From<&StoredTurn> for TimelineTurn {
    fn from(turn: &StoredTurn) -> Self {
        Self {
            id: turn.id.clone(),
            session_id: turn.session_id.clone(),
            tool: turn.tool.as_str().to_string(),
            role: turn.role.as_str().to_string(),
            content: turn.content.clone(),
            timestamp_epoch: turn.timestamp_epoch,
            source_path: turn.source_path.clone(),
        }
    }
}

pub fn timeline_json_turns(turns: &[StoredTurn]) -> Vec<TimelineTurn> {
    turns.iter().map(TimelineTurn::from).collect()
}

pub fn format_timeline_header(turn: &StoredTurn) -> String {
    let ts = crate::xurl::search::format_timestamp(turn.timestamp_epoch);
    let mut header = format!(
        "**[{}]** `{}` · {} · {}",
        turn.tool.as_str(),
        turn.session_id,
        ts,
        turn.role.as_str()
    );
    if let Some(ref path) = turn.source_path {
        header.push_str(" · ");
        header.push_str(path);
    }
    header
}

/// Generate a deterministic turn ID from the natural key fields.
/// Uses SHA-256 truncated to 16 hex chars for a stable, compact identifier.
fn generate_turn_id(session_id: &str, tool: &str, turn_index: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(session_id.as_bytes());
    hasher.update(b"\x00");
    hasher.update(tool.as_bytes());
    hasher.update(b"\x00");
    hasher.update(turn_index.to_le_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("turn_{}", &digest[..16])
}

/// Deterministic turn ID for a raw turn, matching the key `insert_turns` uses.
///
/// Lets callers (e.g. a single-file ingest) compute the IDs of just-stored
/// turns so the embed pass can be scoped to them without re-scanning the table.
pub fn turn_id_for(turn: &RawTurn) -> String {
    generate_turn_id(&turn.session_id, turn.tool.as_str(), turn.turn_index)
}

/// Count turns that still lack a vector (no row in `conversation_turn_vectors`).
/// Surfaced by `xurl stats` so the recall gap is visible.
pub fn count_unindexed_turns(conn: &Connection) -> XurlResult<i64> {
    count_unindexed_turns_filtered(conn, &TurnFilter::default())
}

pub fn count_unindexed_turns_filtered(conn: &Connection, filter: &TurnFilter) -> XurlResult<i64> {
    summarize_unindexed_turns_filtered(conn, filter).map(|summary| summary.turns)
}

pub fn summarize_unindexed_turns(conn: &Connection) -> XurlResult<UnindexedTurnSummary> {
    summarize_unindexed_turns_filtered(conn, &TurnFilter::default())
}

pub fn summarize_unindexed_turns_filtered(
    conn: &Connection,
    filter: &TurnFilter,
) -> XurlResult<UnindexedTurnSummary> {
    let mut conditions = vec!["ctv.turn_id IS NULL".to_string()];
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;
    push_filter_conditions(
        filter,
        Some("ct"),
        &mut conditions,
        &mut params_vec,
        &mut idx,
    );

    let sql = format!(
        "SELECT COUNT(DISTINCT ct.session_id), COUNT(*) \
         FROM conversation_turns ct \
         LEFT JOIN conversation_turn_vectors ctv ON ctv.turn_id = ct.id AND ctv.chunk_index = 0 \
         WHERE {}",
        conditions.join(" AND ")
    );
    let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    conn.query_row(&sql, refs.as_slice(), |row| {
        Ok(UnindexedTurnSummary {
            threads: row.get(0)?,
            turns: row.get(1)?,
        })
    })
    .map_err(XurlError::Database)
}

fn parse_tool(s: &str) -> Option<Tool> {
    match s {
        "cc" => Some(Tool::Cc),
        "codex" => Some(Tool::Codex),
        "hermes" => Some(Tool::Hermes),
        _ => None,
    }
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn parse_provenance(s: &str) -> Provenance {
    match s {
        "agent" => Provenance::Agent,
        "system" => Provenance::System,
        _ => Provenance::Human,
    }
}

/// Insert or update a batch of turns. For each turn:
/// - If no row with (session_id, tool, turn_index) exists: INSERT.
/// - If a row exists with identical content: skip (no write).
/// - If a row exists with different content: UPDATE content and metadata fields.
///
/// Returns stats for the batch.
pub fn insert_turns(conn: &Connection, turns: &[RawTurn]) -> XurlResult<InsertStats> {
    let mut stats = InsertStats::default();

    for turn in turns {
        let tool = turn.tool.as_str();
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT id, content FROM conversation_turns \
                 WHERE session_id=?1 AND tool=?2 AND turn_index=?3",
                params![&turn.session_id, tool, turn.turn_index],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(XurlError::Database)?;

        match existing {
            None => {
                let id = generate_turn_id(&turn.session_id, tool, turn.turn_index);
                conn.execute(
                    "INSERT INTO conversation_turns \
                     (id, session_id, tool, turn_index, role, content, timestamp_epoch, \
                      token_count, project_path, git_branch, is_csa_delegated, provenance) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![
                        id,
                        &turn.session_id,
                        tool,
                        turn.turn_index,
                        turn.role.as_str(),
                        &turn.content,
                        turn.timestamp_epoch,
                        Option::<i64>::None,
                        &turn.project_path,
                        &turn.git_branch,
                        turn.is_csa_delegated as i64,
                        turn.provenance.as_str(),
                    ],
                )
                .map_err(XurlError::Database)?;
                stats.inserted += 1;
            }
            Some((_, ref existing_content)) if existing_content == &turn.content => {
                stats.skipped += 1;
            }
            Some((existing_id, _)) => {
                conn.execute(
                    "UPDATE conversation_turns SET \
                     content=?2, timestamp_epoch=?3, token_count=?4, \
                     project_path=?5, git_branch=?6, is_csa_delegated=?7, provenance=?8 \
                     WHERE id=?1",
                    params![
                        existing_id,
                        &turn.content,
                        turn.timestamp_epoch,
                        Option::<i64>::None,
                        &turn.project_path,
                        &turn.git_branch,
                        turn.is_csa_delegated as i64,
                        turn.provenance.as_str(),
                    ],
                )
                .map_err(XurlError::Database)?;
                stats.updated += 1;
            }
        }
    }

    Ok(stats)
}

/// Query stored turns with optional filtering and pagination.
/// Results are ordered by `timestamp_epoch DESC` (newest first).
pub fn get_turns(conn: &Connection, filter: TurnFilter) -> XurlResult<Vec<StoredTurn>> {
    let limit = if filter.limit == 0 { 20 } else { filter.limit };

    let mut conditions = Vec::<String>::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;

    if let Some(ref tool) = filter.tool {
        conditions.push(format!("tool = ?{idx}"));
        params_vec.push(Box::new(tool.as_str().to_string()));
        idx += 1;
    }
    if let Some(ref sid) = filter.session_id {
        conditions.push(format!("session_id = ?{idx}"));
        params_vec.push(Box::new(sid.clone()));
        idx += 1;
    }
    if let Some(since) = filter.since_epoch {
        conditions.push(format!("timestamp_epoch >= ?{idx}"));
        params_vec.push(Box::new(since));
        idx += 1;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT id, session_id, tool, turn_index, role, content, timestamp_epoch, \
         token_count, project_path, git_branch, is_csa_delegated, provenance \
         FROM conversation_turns \
         {where_clause} \
         ORDER BY timestamp_epoch DESC \
         LIMIT ?{idx} OFFSET ?{}",
        idx + 1
    );

    params_vec.push(Box::new(limit as i64));
    params_vec.push(Box::new(filter.offset as i64));

    let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            let tool_str: String = row.get(2)?;
            let role_str: String = row.get(4)?;
            let provenance_str: String = row.get(11)?;
            let is_csa: i64 = row.get(10)?;
            let project_path: Option<String> = row.get(8)?;
            Ok(StoredTurn {
                id: row.get(0)?,
                session_id: row.get(1)?,
                tool: parse_tool(&tool_str).unwrap_or(Tool::Cc),
                turn_index: row.get::<_, i64>(3)? as u32,
                role: parse_role(&role_str).unwrap_or(Role::User),
                content: row.get(5)?,
                timestamp_epoch: row.get(6)?,
                token_count: row.get(7)?,
                source_path: project_path.clone(),
                project_path,
                git_branch: row.get(9)?,
                is_csa_delegated: is_csa != 0,
                provenance: parse_provenance(&provenance_str),
            })
        })
        .map_err(XurlError::Database)?;

    let mut turns = Vec::new();
    for row in rows {
        turns.push(row.map_err(XurlError::Database)?);
    }
    Ok(turns)
}

/// Like `get_turns` but with optional exclusion of CSA-delegated and non-human-provenance turns.
pub fn get_turns_filtered(
    conn: &Connection,
    filter: TurnFilter,
    include_csa: bool,
    include_agent_prompts: bool,
) -> XurlResult<Vec<StoredTurn>> {
    let limit = if filter.limit == 0 { 20 } else { filter.limit };

    let mut conditions = Vec::<String>::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;

    if !include_csa {
        conditions.push("is_csa_delegated = 0".into());
    }
    if !include_agent_prompts {
        conditions.push("provenance = 'human'".into());
    }
    if let Some(ref tool) = filter.tool {
        conditions.push(format!("tool = ?{idx}"));
        params_vec.push(Box::new(tool.as_str().to_string()));
        idx += 1;
    }
    if let Some(ref sid) = filter.session_id {
        conditions.push(format!("session_id = ?{idx}"));
        params_vec.push(Box::new(sid.clone()));
        idx += 1;
    }
    if let Some(since) = filter.since_epoch {
        conditions.push(format!("timestamp_epoch >= ?{idx}"));
        params_vec.push(Box::new(since));
        idx += 1;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT id, session_id, tool, turn_index, role, content, timestamp_epoch, \
         token_count, project_path, git_branch, is_csa_delegated, provenance \
         FROM conversation_turns \
         {where_clause} \
         ORDER BY timestamp_epoch DESC \
         LIMIT ?{idx} OFFSET ?{}",
        idx + 1
    );

    params_vec.push(Box::new(limit as i64));
    params_vec.push(Box::new(filter.offset as i64));

    let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            let tool_str: String = row.get(2)?;
            let role_str: String = row.get(4)?;
            let provenance_str: String = row.get(11)?;
            let is_csa: i64 = row.get(10)?;
            let project_path: Option<String> = row.get(8)?;
            Ok(StoredTurn {
                id: row.get(0)?,
                session_id: row.get(1)?,
                tool: parse_tool(&tool_str).unwrap_or(Tool::Cc),
                turn_index: row.get::<_, i64>(3)? as u32,
                role: parse_role(&role_str).unwrap_or(Role::User),
                content: row.get(5)?,
                timestamp_epoch: row.get(6)?,
                token_count: row.get(7)?,
                source_path: project_path.clone(),
                project_path,
                git_branch: row.get(9)?,
                is_csa_delegated: is_csa != 0,
                provenance: parse_provenance(&provenance_str),
            })
        })
        .map_err(XurlError::Database)?;

    let mut turns = Vec::new();
    for row in rows {
        turns.push(row.map_err(XurlError::Database)?);
    }
    Ok(turns)
}

fn filter_column(prefix: Option<&str>, column: &str) -> String {
    prefix.map_or_else(|| column.to_string(), |prefix| format!("{prefix}.{column}"))
}

fn push_filter_conditions(
    filter: &TurnFilter,
    column_prefix: Option<&str>,
    conditions: &mut Vec<String>,
    params_vec: &mut Vec<Box<dyn rusqlite::ToSql>>,
    idx: &mut usize,
) {
    if let Some(ref tool) = filter.tool {
        conditions.push(format!(
            "{} = ?{}",
            filter_column(column_prefix, "tool"),
            *idx
        ));
        params_vec.push(Box::new(tool.as_str().to_string()));
        *idx += 1;
    }
    if let Some(ref sid) = filter.session_id {
        conditions.push(format!(
            "{} = ?{}",
            filter_column(column_prefix, "session_id"),
            *idx
        ));
        params_vec.push(Box::new(sid.clone()));
        *idx += 1;
    }
    if let Some(since) = filter.since_epoch {
        conditions.push(format!(
            "{} >= ?{}",
            filter_column(column_prefix, "timestamp_epoch"),
            *idx
        ));
        params_vec.push(Box::new(since));
        *idx += 1;
    }
}

/// Per-tool aggregate statistics.
pub fn get_stats(conn: &Connection) -> XurlResult<Vec<ToolStat>> {
    get_stats_filtered(conn, &TurnFilter::default())
}

pub fn get_stats_filtered(conn: &Connection, filter: &TurnFilter) -> XurlResult<Vec<ToolStat>> {
    let mut conditions = Vec::<String>::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;
    push_filter_conditions(filter, None, &mut conditions, &mut params_vec, &mut idx);

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT tool, COUNT(*) as count, \
         MIN(timestamp_epoch), MAX(timestamp_epoch) \
         FROM conversation_turns \
         {where_clause} \
         GROUP BY tool \
         ORDER BY tool"
    );
    let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql).map_err(XurlError::Database)?;

    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            Ok(ToolStat {
                tool: row.get(0)?,
                count: row.get(1)?,
                min_timestamp: row.get(2)?,
                max_timestamp: row.get(3)?,
            })
        })
        .map_err(XurlError::Database)?;

    let mut stats = Vec::new();
    for row in rows {
        stats.push(row.map_err(XurlError::Database)?);
    }
    Ok(stats)
}

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::xurl::model::{Provenance, RawTurn, Role, Tool, TurnMetadata};
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
    pub metadata: TurnMetadata,
}

#[derive(Debug, Default)]
pub struct InsertStats {
    pub inserted: usize,
    pub skipped: usize,
    pub updated: usize,
    pub removed: usize,
    pub turn_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub struct TurnFilter {
    pub tool: Option<Tool>,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub hermes_profile: Option<String>,
    pub session_title: Option<String>,
    pub session_source: Option<String>,
    pub since_epoch: Option<f64>,
    pub until_epoch: Option<f64>,
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
    pub hermes_profile: Option<String>,
    pub session_title: Option<String>,
    pub session_source: Option<String>,
    pub message_id: Option<String>,
    pub tool_name: Option<String>,
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
            hermes_profile: turn.metadata.hermes_profile.clone(),
            session_title: turn.metadata.session_title.clone(),
            session_source: turn.metadata.session_source.clone(),
            message_id: turn.metadata.message_id.clone(),
            tool_name: turn.metadata.tool_name.clone(),
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
fn generate_turn_id_from_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\x00");
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("turn_{}", &digest[..16])
}

fn generate_turn_id(session_id: &str, tool: &str, turn_index: u32) -> String {
    let turn_index = turn_index.to_string();
    generate_turn_id_from_parts(&[session_id, tool, &turn_index])
}

/// Deterministic turn ID for newly inserted raw turns.
///
/// Existing legacy rows may keep their historical IDs when `insert_turns`
/// updates them; use `InsertStats::turn_ids` when callers need actual row IDs.
pub fn turn_id_for(turn: &RawTurn) -> String {
    if turn.tool == Tool::Hermes {
        if let (Some(profile), Some(message_id)) = (
            turn.metadata.hermes_profile.as_deref(),
            turn.metadata.message_id.as_deref(),
        ) {
            return generate_turn_id_from_parts(&[
                turn.tool.as_str(),
                profile,
                &turn.session_id,
                message_id,
            ]);
        }
    }
    generate_turn_id(&turn.session_id, turn.tool.as_str(), turn.turn_index)
}

struct ExistingTurn {
    id: String,
    content: String,
    timestamp_epoch: f64,
    project_path: Option<String>,
    git_branch: Option<String>,
    is_csa_delegated: bool,
    provenance: Provenance,
    metadata: TurnMetadata,
}

fn storage_turn_index(turn: &RawTurn) -> u32 {
    if turn.tool == Tool::Hermes {
        if let (Some(profile), Some(message_id)) = (
            turn.metadata.hermes_profile.as_deref(),
            turn.metadata.message_id.as_deref(),
        ) {
            let mut hasher = Sha256::new();
            hasher.update(profile.as_bytes());
            hasher.update(b"\x00");
            hasher.update(turn.session_id.as_bytes());
            hasher.update(b"\x00");
            hasher.update(message_id.as_bytes());
            let digest = hasher.finalize();
            return u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
        }
    }
    turn.turn_index
}

fn find_existing_turn(conn: &Connection, turn: &RawTurn) -> XurlResult<Option<ExistingTurn>> {
    if turn.tool == Tool::Hermes {
        if let (Some(profile), Some(message_id)) = (
            turn.metadata.hermes_profile.as_deref(),
            turn.metadata.message_id.as_deref(),
        ) {
            let existing = conn
                .query_row(
                    "SELECT id, content, timestamp_epoch, project_path, git_branch, \
                     is_csa_delegated, provenance, hermes_profile, session_title, session_source, \
                     message_id, tool_name, tool_call_id, previous_message_id, next_message_id \
                     FROM conversation_turns \
                     WHERE tool = ?1 AND hermes_profile = ?2 AND session_id = ?3 AND message_id = ?4",
                    params![turn.tool.as_str(), profile, &turn.session_id, message_id],
                    existing_turn_from_row,
                )
                .optional()
                .map_err(XurlError::Database)?;
            if existing.is_some() {
                return Ok(existing);
            }

            return find_existing_legacy_hermes_turn(conn, turn);
        }
    }

    let turn_index = storage_turn_index(turn);
    conn.query_row(
        "SELECT id, content, timestamp_epoch, project_path, git_branch, \
         is_csa_delegated, provenance, hermes_profile, session_title, session_source, \
         message_id, tool_name, tool_call_id, previous_message_id, next_message_id \
         FROM conversation_turns \
         WHERE session_id=?1 AND tool=?2 AND turn_index=?3",
        params![&turn.session_id, turn.tool.as_str(), turn_index],
        existing_turn_from_row,
    )
    .optional()
    .map_err(XurlError::Database)
}

fn find_existing_legacy_hermes_turn(
    conn: &Connection,
    turn: &RawTurn,
) -> XurlResult<Option<ExistingTurn>> {
    conn.query_row(
        "SELECT id, content, timestamp_epoch, project_path, git_branch, \
         is_csa_delegated, provenance, hermes_profile, session_title, session_source, \
         message_id, tool_name, tool_call_id, previous_message_id, next_message_id \
         FROM conversation_turns \
         WHERE session_id=?1 AND tool=?2 AND turn_index=?3 \
           AND hermes_profile IS NULL AND message_id IS NULL",
        params![&turn.session_id, turn.tool.as_str(), turn.turn_index],
        existing_turn_from_row,
    )
    .optional()
    .map_err(XurlError::Database)
}

fn existing_turn_from_row(row: &Row<'_>) -> rusqlite::Result<ExistingTurn> {
    let is_csa: i64 = row.get(5)?;
    let provenance_str: String = row.get(6)?;
    Ok(ExistingTurn {
        id: row.get(0)?,
        content: row.get(1)?,
        timestamp_epoch: row.get(2)?,
        project_path: row.get(3)?,
        git_branch: row.get(4)?,
        is_csa_delegated: is_csa != 0,
        provenance: parse_provenance(&provenance_str),
        metadata: TurnMetadata {
            hermes_profile: row.get(7)?,
            session_title: row.get(8)?,
            session_source: row.get(9)?,
            message_id: row.get(10)?,
            tool_name: row.get(11)?,
            tool_call_id: row.get(12)?,
            previous_message_id: row.get(13)?,
            next_message_id: row.get(14)?,
        },
    })
}

fn stored_metadata_matches(existing: &ExistingTurn, turn: &RawTurn) -> bool {
    existing.timestamp_epoch.to_bits() == turn.timestamp_epoch.to_bits()
        && existing.project_path == turn.project_path
        && existing.git_branch == turn.git_branch
        && existing.is_csa_delegated == turn.is_csa_delegated
        && existing.provenance == turn.provenance
        && existing.metadata == turn.metadata
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

fn stored_turn_from_row(row: &Row<'_>) -> rusqlite::Result<StoredTurn> {
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
        metadata: TurnMetadata {
            hermes_profile: row.get(12)?,
            session_title: row.get(13)?,
            session_source: row.get(14)?,
            message_id: row.get(15)?,
            tool_name: row.get(16)?,
            tool_call_id: row.get(17)?,
            previous_message_id: row.get(18)?,
            next_message_id: row.get(19)?,
        },
    })
}

const TURN_SELECT_COLUMNS: &str = "\
id, session_id, tool, turn_index, role, content, timestamp_epoch, \
token_count, project_path, git_branch, is_csa_delegated, provenance, \
hermes_profile, session_title, session_source, message_id, tool_name, \
tool_call_id, previous_message_id, next_message_id";

/// Insert or update a batch of turns. For each turn:
/// - If no row with the tool-specific identity exists: INSERT.
/// - If a row exists with identical content and metadata: skip (no write).
/// - If a row exists with identical content but changed metadata: UPDATE metadata only.
/// - If a row exists with different content: UPDATE content and metadata fields.
///
/// Returns stats for the batch.
pub fn insert_turns(conn: &Connection, turns: &[RawTurn]) -> XurlResult<InsertStats> {
    let mut stats = InsertStats::default();

    conn.execute_batch("SAVEPOINT xurl_insert_turns")
        .map_err(XurlError::Database)?;
    let insert_result = (|| -> XurlResult<()> {
        for turn in turns {
            let tool = turn.tool.as_str();
            let turn_index = storage_turn_index(turn);
            let existing = find_existing_turn(conn, turn)?;

            match existing {
                None => {
                    let id = turn_id_for(turn);
                    conn.execute(
                        "INSERT INTO conversation_turns \
                         (id, session_id, tool, turn_index, role, content, timestamp_epoch, \
                          token_count, project_path, git_branch, is_csa_delegated, provenance, \
                          hermes_profile, session_title, session_source, message_id, tool_name, \
                          tool_call_id, previous_message_id, next_message_id) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                        params![
                            id,
                            &turn.session_id,
                            tool,
                            turn_index,
                            turn.role.as_str(),
                            &turn.content,
                            turn.timestamp_epoch,
                            Option::<i64>::None,
                            &turn.project_path,
                            &turn.git_branch,
                            turn.is_csa_delegated as i64,
                            turn.provenance.as_str(),
                            &turn.metadata.hermes_profile,
                            &turn.metadata.session_title,
                            &turn.metadata.session_source,
                            &turn.metadata.message_id,
                            &turn.metadata.tool_name,
                            &turn.metadata.tool_call_id,
                            &turn.metadata.previous_message_id,
                            &turn.metadata.next_message_id,
                        ],
                    )
                    .map_err(XurlError::Database)?;
                    stats.turn_ids.push(id);
                    stats.inserted += 1;
                }
                Some(existing) if existing.content == turn.content => {
                    stats.turn_ids.push(existing.id.clone());
                    if stored_metadata_matches(&existing, turn) {
                        stats.skipped += 1;
                    } else {
                        update_turn_metadata(conn, &existing.id, turn)?;
                        stats.updated += 1;
                    }
                }
                Some(existing) => {
                    stats.turn_ids.push(existing.id.clone());
                    conn.execute(
                        "UPDATE conversation_turns SET \
                         content=?2, timestamp_epoch=?3, token_count=?4, \
                         project_path=?5, git_branch=?6, is_csa_delegated=?7, provenance=?8, \
                         hermes_profile=?9, session_title=?10, session_source=?11, message_id=?12, \
                         tool_name=?13, tool_call_id=?14, previous_message_id=?15, next_message_id=?16 \
                        WHERE id=?1",
                        params![
                            &existing.id,
                            &turn.content,
                            turn.timestamp_epoch,
                            Option::<i64>::None,
                            &turn.project_path,
                            &turn.git_branch,
                            turn.is_csa_delegated as i64,
                            turn.provenance.as_str(),
                            &turn.metadata.hermes_profile,
                            &turn.metadata.session_title,
                            &turn.metadata.session_source,
                            &turn.metadata.message_id,
                            &turn.metadata.tool_name,
                            &turn.metadata.tool_call_id,
                            &turn.metadata.previous_message_id,
                            &turn.metadata.next_message_id,
                        ],
                    )
                    .map_err(XurlError::Database)?;
                    conn.execute(
                        "DELETE FROM conversation_turn_vectors WHERE turn_id = ?1",
                        params![&existing.id],
                    )
                    .map_err(XurlError::Database)?;
                    stats.updated += 1;
                }
            }
        }

        conn.execute_batch("RELEASE xurl_insert_turns")
            .map_err(XurlError::Database)?;
        Ok(())
    })();

    if let Err(err) = insert_result {
        let _ = conn.execute_batch("ROLLBACK TO xurl_insert_turns; RELEASE xurl_insert_turns;");
        return Err(err);
    }

    Ok(stats)
}

fn update_turn_metadata(conn: &Connection, id: &str, turn: &RawTurn) -> XurlResult<()> {
    conn.execute(
        "UPDATE conversation_turns SET \
         timestamp_epoch=?2, project_path=?3, git_branch=?4, is_csa_delegated=?5, provenance=?6, \
         hermes_profile=?7, session_title=?8, session_source=?9, message_id=?10, \
         tool_name=?11, tool_call_id=?12, previous_message_id=?13, next_message_id=?14 \
         WHERE id=?1",
        params![
            id,
            turn.timestamp_epoch,
            &turn.project_path,
            &turn.git_branch,
            turn.is_csa_delegated as i64,
            turn.provenance.as_str(),
            &turn.metadata.hermes_profile,
            &turn.metadata.session_title,
            &turn.metadata.session_source,
            &turn.metadata.message_id,
            &turn.metadata.tool_name,
            &turn.metadata.tool_call_id,
            &turn.metadata.previous_message_id,
            &turn.metadata.next_message_id,
        ],
    )
    .map_err(XurlError::Database)?;
    Ok(())
}

/// Query stored turns with optional filtering and pagination.
/// Results are ordered by `timestamp_epoch DESC` (newest first).
pub fn get_turns(conn: &Connection, filter: TurnFilter) -> XurlResult<Vec<StoredTurn>> {
    let limit = if filter.limit == 0 { 20 } else { filter.limit };

    let mut conditions = Vec::<String>::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;
    push_filter_conditions(&filter, None, &mut conditions, &mut params_vec, &mut idx);

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT {TURN_SELECT_COLUMNS} \
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
        .query_map(refs.as_slice(), stored_turn_from_row)
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
    push_filter_conditions(&filter, None, &mut conditions, &mut params_vec, &mut idx);

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT {TURN_SELECT_COLUMNS} \
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
        .query_map(refs.as_slice(), stored_turn_from_row)
        .map_err(XurlError::Database)?;

    let mut turns = Vec::new();
    for row in rows {
        turns.push(row.map_err(XurlError::Database)?);
    }
    Ok(turns)
}

pub fn latest_session_id(
    conn: &Connection,
    filter: &TurnFilter,
    include_csa: bool,
    include_agent_prompts: bool,
) -> XurlResult<Option<String>> {
    let mut conditions = Vec::<String>::new();
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    let mut idx = 1usize;

    if !include_csa {
        conditions.push("is_csa_delegated = 0".into());
    }
    if !include_agent_prompts {
        conditions.push("provenance = 'human'".into());
    }
    push_filter_conditions(filter, None, &mut conditions, &mut params_vec, &mut idx);

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let sql = format!(
        "SELECT session_id \
         FROM conversation_turns \
         {where_clause} \
         GROUP BY session_id \
         ORDER BY MAX(timestamp_epoch) DESC \
         LIMIT 1"
    );
    let refs: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    conn.query_row(&sql, refs.as_slice(), |row| row.get::<_, String>(0))
        .optional()
        .map_err(XurlError::Database)
}

fn filter_column(prefix: Option<&str>, column: &str) -> String {
    prefix.map_or_else(|| column.to_string(), |prefix| format!("{prefix}.{column}"))
}

pub(crate) fn push_filter_conditions(
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
    if let Some(ref cwd) = filter.cwd {
        let cwd = normalize_cwd_filter(cwd);
        if !cwd.is_empty() {
            let project_col = filter_column(column_prefix, "project_path");
            conditions.push(cwd_filter_condition(&project_col, *idx, &cwd));
            params_vec.push(Box::new(cwd));
            *idx += 1;
        }
    }
    if let Some(ref profile) = filter.hermes_profile {
        conditions.push(format!(
            "{} = ?{}",
            filter_column(column_prefix, "hermes_profile"),
            *idx
        ));
        params_vec.push(Box::new(profile.clone()));
        *idx += 1;
    }
    if let Some(ref title) = filter.session_title {
        conditions.push(format!(
            "{} = ?{}",
            filter_column(column_prefix, "session_title"),
            *idx
        ));
        params_vec.push(Box::new(title.clone()));
        *idx += 1;
    }
    if let Some(ref source) = filter.session_source {
        conditions.push(format!(
            "{} = ?{}",
            filter_column(column_prefix, "session_source"),
            *idx
        ));
        params_vec.push(Box::new(source.clone()));
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
    if let Some(until) = filter.until_epoch {
        conditions.push(format!(
            "{} <= ?{}",
            filter_column(column_prefix, "timestamp_epoch"),
            *idx
        ));
        params_vec.push(Box::new(until));
        *idx += 1;
    }
}

pub(crate) fn normalize_cwd_filter(cwd: &str) -> String {
    let trimmed = cwd.trim();
    if trimmed == "/" {
        trimmed.to_string()
    } else {
        trimmed.trim_end_matches('/').to_string()
    }
}

fn cwd_filter_condition(project_col: &str, param_idx: usize, cwd: &str) -> String {
    if cwd == "/" {
        return format!("substr({project_col}, 1, 1) = ?{param_idx}");
    }

    format!(
        "({project_col} = ?{param_idx} \
         OR (substr({project_col}, 1, length(?{param_idx})) = ?{param_idx} \
             AND substr({project_col}, length(?{param_idx}) + 1, 1) = '/') \
         OR (substr(?{param_idx}, 1, length({project_col})) = {project_col} \
             AND substr(?{param_idx}, length({project_col}) + 1, 1) = '/'))"
    )
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

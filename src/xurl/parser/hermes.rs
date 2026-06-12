use std::collections::HashSet;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::Value;

use crate::xurl::model::{Provenance, RawTurn, Role, Tool, TurnMetadata};
use crate::xurl::store::normalize_cwd_filter;
use crate::xurl::{XurlError, XurlResult};

const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone)]
pub struct HermesParseOptions {
    pub fallback_session_id: String,
    pub profile: String,
    pub is_csa_delegated: bool,
    pub session_id_filter: Option<String>,
    pub cwd: Option<String>,
}

impl HermesParseOptions {
    pub fn new(fallback_session_id: &str, profile: &str, is_csa_delegated: bool) -> Self {
        Self {
            fallback_session_id: fallback_session_id.to_string(),
            profile: profile.to_string(),
            is_csa_delegated,
            session_id_filter: None,
            cwd: None,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct SessionMetadata {
    session_id: Option<String>,
    title: Option<String>,
    source: Option<String>,
    cwd: Option<String>,
}

/// Parse turns from a Hermes `state.db` SQLite database using default profile metadata.
pub fn parse_hermes_db(
    path: &Path,
    fallback_session_id: &str,
    is_csa_delegated: bool,
) -> XurlResult<Vec<RawTurn>> {
    parse_hermes_db_with_options(
        path,
        &HermesParseOptions::new(fallback_session_id, DEFAULT_PROFILE, is_csa_delegated),
    )
}

/// Parse turns from a Hermes `state.db` SQLite database.
///
/// The database is opened read-only so mempal never creates or mutates Hermes
/// sidecar files. Required message columns are `role`, `content`, and
/// `timestamp`; all Hermes metadata columns are optional and degrade to
/// session-level metadata or caller-provided fallbacks.
pub fn parse_hermes_db_with_options(
    path: &Path,
    options: &HermesParseOptions,
) -> XurlResult<Vec<RawTurn>> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| XurlError::Parse(format!("cannot open hermes db: {e}")))?;

    let columns = table_columns(&conn, "messages")?;
    for required in ["role", "content", "timestamp"] {
        if !columns.contains(required) {
            return Err(XurlError::Parse(format!(
                "Hermes messages table is missing required column `{required}`"
            )));
        }
    }

    let meta = read_session_metadata(&conn);
    let message_id_expr = optional_text_expr(&columns, &["message_id", "messageId", "id"]);
    let session_id_expr = optional_text_expr(&columns, &["session_id", "sessionId"]);
    let cwd_expr = optional_text_expr(&columns, &["cwd", "project_path", "project"]);
    let tool_name_expr = optional_text_expr(&columns, &["tool_name", "toolName"]);
    let tool_call_id_expr = optional_text_expr(&columns, &["tool_call_id", "toolCallId"]);
    let title_expr = optional_text_expr(&columns, &["session_title", "title"]);
    let source_expr = optional_text_expr(&columns, &["session_source", "source"]);
    let id_order = if columns.contains("id") { ", id" } else { "" };
    let sql = format!(
        "SELECT {message_id_expr}, role, content, timestamp, {session_id_expr}, \
         {cwd_expr}, {tool_name_expr}, {tool_call_id_expr}, {title_expr}, {source_expr} \
         FROM messages \
         WHERE role IN ('user', 'assistant') \
         ORDER BY timestamp{id_order}"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| XurlError::Parse(format!("failed to prepare hermes query: {e}")))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| XurlError::Parse(format!("hermes query error: {e}")))?;

    let mut turns = Vec::new();
    let mut turn_index: u32 = 0;
    while let Some(row) = rows
        .next()
        .map_err(|e| XurlError::Parse(format!("hermes row error: {e}")))?
    {
        let role_str = sql_text(row, 1)?.unwrap_or_default();
        let role = match role_str.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };

        let raw_content = sql_text(row, 2)?.unwrap_or_default();
        let content = sanitize_content(&raw_content);
        if content.is_empty() {
            continue;
        }

        let session_id = sql_text(row, 4)?
            .or_else(|| meta.session_id.clone())
            .unwrap_or_else(|| options.fallback_session_id.clone());
        if options
            .session_id_filter
            .as_deref()
            .is_some_and(|filter| filter != session_id)
        {
            continue;
        }

        let project_path = sql_text(row, 5)?
            .or_else(|| meta.cwd.clone())
            .or_else(|| options.cwd.clone());
        if !matches_cwd_filter(project_path.as_deref(), options.cwd.as_deref()) {
            continue;
        }

        let message_id = sql_text(row, 0)?.unwrap_or_else(|| format!("{session_id}:{turn_index}"));
        let timestamp_epoch = sql_epoch(row, 3)?;
        let metadata = TurnMetadata {
            hermes_profile: Some(options.profile.clone()),
            session_title: sql_text(row, 8)?.or_else(|| meta.title.clone()),
            session_source: sql_text(row, 9)?.or_else(|| meta.source.clone()),
            message_id: Some(message_id),
            tool_name: sql_text(row, 6)?,
            tool_call_id: sql_text(row, 7)?,
            previous_message_id: None,
            next_message_id: None,
        };

        turns.push(RawTurn {
            session_id,
            tool: Tool::Hermes,
            role,
            content,
            timestamp_epoch,
            project_path,
            git_branch: None,
            is_csa_delegated: options.is_csa_delegated,
            provenance: Provenance::Human,
            turn_index,
            metadata,
        });
        turn_index += 1;
    }

    link_neighbor_message_ids(&mut turns);
    Ok(turns)
}

pub fn parse_hermes_jsonl_export(
    content: &str,
    options: &HermesParseOptions,
) -> XurlResult<Vec<RawTurn>> {
    let mut turns = Vec::new();
    let mut turn_index: u32 = 0;
    let mut session_meta = SessionMetadata::default();

    for raw_line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let obj: Value = match serde_json::from_str(raw_line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        update_session_metadata_from_json(&mut session_meta, &obj);
        let Some(role) = extract_role(&obj) else {
            continue;
        };
        let Some(raw_content) = extract_content(&obj) else {
            continue;
        };
        let content = sanitize_content(&raw_content);
        if content.is_empty() {
            continue;
        }

        let session_id = extract_json_string(&obj, JSON_SESSION_ID_PATHS)
            .or_else(|| session_meta.session_id.clone())
            .unwrap_or_else(|| options.fallback_session_id.clone());
        if options
            .session_id_filter
            .as_deref()
            .is_some_and(|filter| filter != session_id)
        {
            continue;
        }

        let project_path = extract_json_string(&obj, JSON_CWD_PATHS)
            .or_else(|| session_meta.cwd.clone())
            .or_else(|| options.cwd.clone());
        if !matches_cwd_filter(project_path.as_deref(), options.cwd.as_deref()) {
            continue;
        }

        let message_id = extract_json_string(&obj, JSON_MESSAGE_ID_PATHS)
            .unwrap_or_else(|| format!("{session_id}:{turn_index}"));
        let metadata = TurnMetadata {
            hermes_profile: Some(options.profile.clone()),
            session_title: extract_json_string(&obj, JSON_SESSION_TITLE_PATHS)
                .or_else(|| session_meta.title.clone()),
            session_source: extract_json_string(&obj, JSON_SESSION_SOURCE_PATHS)
                .or_else(|| session_meta.source.clone()),
            message_id: Some(message_id),
            tool_name: extract_json_string(&obj, JSON_TOOL_NAME_PATHS),
            tool_call_id: extract_json_string(&obj, JSON_TOOL_CALL_ID_PATHS),
            previous_message_id: None,
            next_message_id: None,
        };

        turns.push(RawTurn {
            session_id,
            tool: Tool::Hermes,
            role,
            content,
            timestamp_epoch: extract_json_epoch(&obj),
            project_path,
            git_branch: None,
            is_csa_delegated: options.is_csa_delegated,
            provenance: Provenance::Human,
            turn_index,
            metadata,
        });
        turn_index += 1;
    }

    link_neighbor_message_ids(&mut turns);
    Ok(turns)
}

fn table_columns(conn: &Connection, table: &str) -> XurlResult<HashSet<String>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(XurlError::Database)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(XurlError::Database)?;

    let mut columns = HashSet::new();
    for row in rows {
        columns.insert(row.map_err(XurlError::Database)?);
    }
    if columns.is_empty() {
        return Err(XurlError::Parse(format!(
            "Hermes database is missing `{table}` table"
        )));
    }
    Ok(columns)
}

fn optional_text_expr(columns: &HashSet<String>, candidates: &[&str]) -> String {
    candidates
        .iter()
        .find(|candidate| columns.contains(**candidate))
        .map(|column| format!("CAST({column} AS TEXT)"))
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_text(row: &Row<'_>, idx: usize) -> XurlResult<Option<String>> {
    let value = match row.get_ref(idx).map_err(XurlError::Database)? {
        ValueRef::Null => None,
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).trim().to_string()),
        ValueRef::Integer(value) => Some(value.to_string()),
        ValueRef::Real(value) => Some(value.to_string()),
        ValueRef::Blob(_) => None,
    };
    Ok(value.filter(|text| !text.is_empty()))
}

fn sql_epoch(row: &Row<'_>, idx: usize) -> XurlResult<f64> {
    match row.get_ref(idx).map_err(XurlError::Database)? {
        ValueRef::Integer(value) => Ok(normalize_epoch_number(value as f64)),
        ValueRef::Real(value) => Ok(normalize_epoch_number(value)),
        ValueRef::Text(bytes) => Ok(parse_epoch_text(&String::from_utf8_lossy(bytes))),
        ValueRef::Null | ValueRef::Blob(_) => Ok(0.0),
    }
}

fn normalize_epoch_number(value: f64) -> f64 {
    if value > 1_000_000_000_000.0 {
        value / 1000.0
    } else {
        value
    }
}

fn parse_epoch_text(value: &str) -> f64 {
    let trimmed = value.trim();
    if let Ok(number) = trimmed.parse::<f64>() {
        return normalize_epoch_number(number);
    }
    crate::cowork::peek::parse_rfc3339(trimmed)
        .map(|secs| secs as f64)
        .unwrap_or(0.0)
}

fn read_session_metadata(conn: &Connection) -> SessionMetadata {
    SessionMetadata {
        session_id: read_session_id(conn),
        title: query_optional_text(
            conn,
            "SELECT value FROM metadata WHERE key IN ('session_title', 'title') LIMIT 1",
        )
        .or_else(|| query_optional_text(conn, "SELECT title FROM sessions LIMIT 1"))
        .or_else(|| query_optional_text(conn, "SELECT name FROM sessions LIMIT 1")),
        source: query_optional_text(
            conn,
            "SELECT value FROM metadata WHERE key IN ('session_source', 'source') LIMIT 1",
        )
        .or_else(|| query_optional_text(conn, "SELECT source FROM sessions LIMIT 1")),
        cwd: read_project_path(conn),
    }
}

/// Attempt to read a session ID from a `sessions` or `metadata` table.
fn read_session_id(conn: &Connection) -> Option<String> {
    query_optional_text(
        conn,
        "SELECT value FROM metadata WHERE key = 'session_id' LIMIT 1",
    )
    .or_else(|| query_optional_text(conn, "SELECT session_id FROM sessions LIMIT 1"))
    .or_else(|| query_optional_text(conn, "SELECT id FROM sessions LIMIT 1"))
    .or_else(|| query_optional_text(conn, "SELECT session_id FROM messages LIMIT 1"))
}

/// Attempt to read a project/cwd path from known Hermes metadata shapes.
pub(crate) fn read_project_path(conn: &Connection) -> Option<String> {
    query_optional_text(
        conn,
        "SELECT value FROM metadata \
         WHERE key IN ('project_path', 'cwd', 'project') \
         ORDER BY CASE key \
             WHEN 'project_path' THEN 0 \
             WHEN 'cwd' THEN 1 \
             ELSE 2 \
         END \
         LIMIT 1",
    )
    .or_else(|| query_optional_text(conn, "SELECT project_path FROM sessions LIMIT 1"))
    .or_else(|| query_optional_text(conn, "SELECT cwd FROM sessions LIMIT 1"))
    .or_else(|| query_optional_text(conn, "SELECT project FROM sessions LIMIT 1"))
    .or_else(|| {
        query_optional_text(
            conn,
            "SELECT project_path FROM messages \
             WHERE project_path IS NOT NULL AND project_path != '' \
             LIMIT 1",
        )
    })
    .or_else(|| {
        query_optional_text(
            conn,
            "SELECT cwd FROM messages \
             WHERE cwd IS NOT NULL AND cwd != '' \
             LIMIT 1",
        )
    })
    .or_else(|| {
        query_optional_text(
            conn,
            "SELECT project FROM messages \
             WHERE project IS NOT NULL AND project != '' \
             LIMIT 1",
        )
    })
}

fn query_optional_text(conn: &Connection, sql: &str) -> Option<String> {
    conn.query_row(sql, [], |row| row.get::<_, String>(0))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn matches_cwd_filter(project_path: Option<&str>, cwd_filter: Option<&str>) -> bool {
    let Some(filter) = cwd_filter
        .map(normalize_cwd_filter)
        .filter(|cwd| !cwd.is_empty())
    else {
        return true;
    };
    let Some(path) = project_path
        .map(normalize_cwd_filter)
        .filter(|cwd| !cwd.is_empty())
    else {
        return false;
    };
    path == filter
        || path
            .strip_prefix(&filter)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || filter
            .strip_prefix(&path)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn link_neighbor_message_ids(turns: &mut [RawTurn]) {
    for index in 0..turns.len() {
        let prev = index
            .checked_sub(1)
            .and_then(|prev| same_session_message_id(&turns[prev], &turns[index]));
        let next = turns
            .get(index + 1)
            .and_then(|next| same_session_message_id(next, &turns[index]));
        turns[index].metadata.previous_message_id = prev;
        turns[index].metadata.next_message_id = next;
    }
}

fn same_session_message_id(candidate: &RawTurn, current: &RawTurn) -> Option<String> {
    (candidate.session_id == current.session_id)
        .then(|| candidate.metadata.message_id.clone())
        .flatten()
}

fn sanitize_content(input: &str) -> String {
    let after_context = strip_tag_blocks(input, "context");
    let after_reminder = strip_tag_blocks(&after_context, "system-reminder");
    after_reminder.trim().to_string()
}

fn strip_tag_blocks(input: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut result = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find(&open) {
        result.push_str(&remaining[..start]);
        let after_open = &remaining[start + open.len()..];
        if let Some(end) = after_open.find(&close) {
            remaining = &after_open[end + close.len()..];
        } else {
            break;
        }
    }
    result.push_str(remaining);
    result
}

const JSON_SESSION_ID_PATHS: &[&[&str]] = &[
    &["session_id"],
    &["sessionId"],
    &["session", "id"],
    &["session", "session_id"],
];
const JSON_MESSAGE_ID_PATHS: &[&[&str]] =
    &[&["message_id"], &["messageId"], &["id"], &["message", "id"]];
const JSON_CWD_PATHS: &[&[&str]] = &[
    &["cwd"],
    &["project_path"],
    &["project"],
    &["session", "cwd"],
    &["session", "project_path"],
];
const JSON_SESSION_TITLE_PATHS: &[&[&str]] = &[
    &["session_title"],
    &["title"],
    &["session", "title"],
    &["session", "name"],
];
const JSON_SESSION_SOURCE_PATHS: &[&[&str]] =
    &[&["session_source"], &["source"], &["session", "source"]];
const JSON_TOOL_NAME_PATHS: &[&[&str]] = &[
    &["tool_name"],
    &["toolName"],
    &["tool", "name"],
    &["message", "tool_name"],
];
const JSON_TOOL_CALL_ID_PATHS: &[&[&str]] = &[
    &["tool_call_id"],
    &["toolCallId"],
    &["tool_use_id"],
    &["message", "tool_call_id"],
];

fn update_session_metadata_from_json(meta: &mut SessionMetadata, obj: &Value) {
    if let Some(session_id) = extract_json_string(obj, JSON_SESSION_ID_PATHS) {
        meta.session_id = Some(session_id);
    }
    if let Some(title) = extract_json_string(obj, JSON_SESSION_TITLE_PATHS) {
        meta.title = Some(title);
    }
    if let Some(source) = extract_json_string(obj, JSON_SESSION_SOURCE_PATHS) {
        meta.source = Some(source);
    }
    if let Some(cwd) = extract_json_string(obj, JSON_CWD_PATHS) {
        meta.cwd = Some(cwd);
    }
}

fn extract_role(obj: &Value) -> Option<Role> {
    let role = nested_value(obj, &["role"])
        .or_else(|| nested_value(obj, &["message", "role"]))
        .or_else(|| nested_value(obj, &["type"]))
        .and_then(Value::as_str)?;
    match role {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        _ => None,
    }
}

fn extract_content(obj: &Value) -> Option<String> {
    nested_value(obj, &["content"])
        .or_else(|| nested_value(obj, &["message", "content"]))
        .or_else(|| nested_value(obj, &["message"]))
        .and_then(content_value_to_text)
}

fn content_value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    if item.get("type").and_then(Value::as_str) == Some("text") {
                        item.get("text").and_then(Value::as_str)
                    } else {
                        item.as_str()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            non_empty(&text)
        }
        Value::Object(_) => value
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| value.get("content").and_then(Value::as_str))
            .and_then(non_empty),
        _ => None,
    }
}

fn extract_json_string(obj: &Value, paths: &[&[&str]]) -> Option<String> {
    paths
        .iter()
        .find_map(|path| nested_value(obj, path))
        .and_then(value_to_string)
}

fn nested_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty(text),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn extract_json_epoch(obj: &Value) -> f64 {
    nested_value(obj, &["timestamp"])
        .or_else(|| nested_value(obj, &["created_at"]))
        .or_else(|| nested_value(obj, &["createdAt"]))
        .map(parse_json_epoch_value)
        .unwrap_or(0.0)
}

fn parse_json_epoch_value(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.as_f64().map(normalize_epoch_number).unwrap_or(0.0),
        Value::String(text) => parse_epoch_text(text),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use std::path::PathBuf;

    fn setup_hermes_fixture(db_path: &PathBuf, rows: &[(&str, &str, f64)]) {
        let conn = Connection::open(db_path).expect("open fixture db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                role      TEXT NOT NULL,
                content   TEXT NOT NULL,
                timestamp REAL NOT NULL
            );",
        )
        .expect("create table");
        for (role, content, ts) in rows {
            conn.execute(
                "INSERT INTO messages (role, content, timestamp) VALUES (?1, ?2, ?3)",
                params![role, content, ts],
            )
            .expect("insert fixture row");
        }
    }

    #[test]
    fn hermes_parser_reads_user_and_assistant_turns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(
            &db_path,
            &[
                ("user", "Hello!", 1748356100.0),
                ("assistant", "Hi there!", 1748356200.0),
                ("tool", "bash output", 1748356250.0),
                ("assistant", "Done.", 1748356300.0),
            ],
        );

        let turns = parse_hermes_db(&db_path, "sess-hermes", false).unwrap();
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].content, "Hello!");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].content, "Hi there!");
        assert_eq!(turns[2].role, Role::Assistant);
        assert_eq!(turns[2].content, "Done.");
        assert_eq!(turns[0].metadata.message_id.as_deref(), Some("1"));
        assert_eq!(turns[0].metadata.next_message_id.as_deref(), Some("2"));
    }

    #[test]
    fn hermes_parser_opens_readonly_never_creates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);

        parse_hermes_db(&db_path, "s1", false).unwrap();

        assert!(!dir.path().join("state.db-wal").exists());
        assert!(!dir.path().join("state.db-shm").exists());
    }

    #[test]
    fn hermes_parser_sanitizes_context_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let raw = "<context>some injected context</context>real question here";
        setup_hermes_fixture(&db_path, &[("user", raw, 1.0)]);

        let turns = parse_hermes_db(&db_path, "s", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "real question here");
    }

    #[test]
    fn hermes_parser_sanitizes_system_reminder_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let raw = "hi <system-reminder>reminder text</system-reminder> there";
        setup_hermes_fixture(&db_path, &[("user", raw, 1.0)]);

        let turns = parse_hermes_db(&db_path, "s", false).unwrap();
        assert_eq!(turns[0].content, "hi  there");
    }

    #[test]
    fn hermes_parser_csa_delegated_flag_propagated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);

        let turns = parse_hermes_db(&db_path, "s", true).unwrap();
        assert!(turns[0].is_csa_delegated);
    }

    #[test]
    fn hermes_parser_reads_project_path_from_metadata_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);
        let conn = Connection::open(&db_path).expect("open fixture");
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO metadata (key, value) VALUES ('cwd', '/repo/mempal');",
        )
        .expect("insert metadata");
        drop(conn);

        let turns = parse_hermes_db(&db_path, "s", false).unwrap();
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/mempal"));
    }

    #[test]
    fn hermes_parser_preserves_profile_session_and_message_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let conn = Connection::open(&db_path).expect("open fixture db");
        conn.execute_batch(
            "CREATE TABLE messages (
                id TEXT PRIMARY KEY,
                session_id TEXT,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                cwd TEXT,
                tool_name TEXT,
                session_title TEXT,
                session_source TEXT
            );
             INSERT INTO messages
             (id, session_id, role, content, timestamp, cwd, tool_name, session_title, session_source)
             VALUES
             ('msg-1', 'sess-1', 'assistant', 'review verdict PASS', '2026-06-01T00:00:00Z',
              '/repo/mempal', 'shell', 'Issue 399', 'cli');",
        )
        .expect("create rich fixture");
        drop(conn);

        let mut options = HermesParseOptions::new("fallback", "work", false);
        options.cwd = Some("/repo".to_string());
        let turns = parse_hermes_db_with_options(&db_path, &options).unwrap();

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_id, "sess-1");
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/mempal"));
        assert_eq!(turns[0].metadata.hermes_profile.as_deref(), Some("work"));
        assert_eq!(turns[0].metadata.message_id.as_deref(), Some("msg-1"));
        assert_eq!(
            turns[0].metadata.session_title.as_deref(),
            Some("Issue 399")
        );
        assert_eq!(turns[0].metadata.session_source.as_deref(), Some("cli"));
        assert_eq!(turns[0].metadata.tool_name.as_deref(), Some("shell"));
    }

    #[test]
    fn hermes_jsonl_export_parser_reads_metadata_and_filters_cwd() {
        let jsonl = serde_json::json!({
            "id": "msg-1",
            "session_id": "sess-1",
            "role": "assistant",
            "content": "mktd Step 7 failure recovery",
            "timestamp": "2026-06-01T00:00:00Z",
            "cwd": "/repo/mempal",
            "session_title": "Queue drain",
            "session_source": "cli",
            "tool_name": "mktd"
        })
        .to_string();
        let mut options = HermesParseOptions::new("fallback", "default", false);
        options.cwd = Some("/repo".to_string());

        let turns = parse_hermes_jsonl_export(&jsonl, &options).unwrap();

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].metadata.message_id.as_deref(), Some("msg-1"));
        assert_eq!(turns[0].metadata.tool_name.as_deref(), Some("mktd"));
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/mempal"));
    }
}

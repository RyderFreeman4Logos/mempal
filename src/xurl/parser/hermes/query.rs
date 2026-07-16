use std::collections::HashSet;

use rusqlite::Connection;

use crate::xurl::{XurlError, XurlResult};

pub(super) struct SessionJoin {
    pub(super) clause: String,
    pub(super) cwd_expr: String,
    pub(super) title_expr: String,
    pub(super) source_expr: String,
}

impl SessionJoin {
    pub(super) fn new(
        message_columns: &HashSet<String>,
        session_columns: &HashSet<String>,
    ) -> Self {
        let message_key = ["session_id", "sessionId"]
            .into_iter()
            .find(|column| message_columns.contains(*column));
        let session_key = ["id", "session_id", "sessionId"]
            .into_iter()
            .find(|column| session_columns.contains(*column));
        let clause = match (message_key, session_key) {
            (Some(message_key), Some(session_key)) => format!(
                "LEFT JOIN sessions AS s ON CAST(m.{message_key} AS TEXT) = CAST(s.{session_key} AS TEXT)"
            ),
            _ => String::new(),
        };
        let has_join = !clause.is_empty();
        Self {
            cwd_expr: joined_metadata_expr(
                message_columns,
                session_columns,
                &["cwd", "project_path", "project"],
                has_join,
            ),
            title_expr: joined_metadata_expr(
                message_columns,
                session_columns,
                &["session_title", "title", "name"],
                has_join,
            ),
            source_expr: joined_metadata_expr(
                message_columns,
                session_columns,
                &["session_source", "source"],
                has_join,
            ),
            clause,
        }
    }
}

pub(super) fn table_columns(
    conn: &Connection,
    table: &str,
    required: bool,
) -> XurlResult<HashSet<String>> {
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
    if required && columns.is_empty() {
        return Err(XurlError::Parse(format!(
            "Hermes database is missing `{table}` table"
        )));
    }
    Ok(columns)
}

pub(super) fn optional_text_expr(
    columns: &HashSet<String>,
    candidates: &[&str],
    table_alias: &str,
) -> String {
    candidates
        .iter()
        .find(|candidate| columns.contains(**candidate))
        .map(|column| format!("CAST({table_alias}.{column} AS TEXT)"))
        .unwrap_or_else(|| "NULL".to_string())
}

fn joined_metadata_expr(
    message_columns: &HashSet<String>,
    session_columns: &HashSet<String>,
    candidates: &[&str],
    has_session_join: bool,
) -> String {
    let message_expr = optional_text_expr(message_columns, candidates, "m");
    let session_expr = if has_session_join {
        optional_text_expr(session_columns, candidates, "s")
    } else {
        "NULL".to_string()
    };
    format!("COALESCE(NULLIF({message_expr}, ''), NULLIF({session_expr}, ''))")
}

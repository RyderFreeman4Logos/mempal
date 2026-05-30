use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::xurl::model::{Provenance, RawTurn, Role, Tool};
use crate::xurl::{XurlError, XurlResult};

/// Parse turns from a Hermes `state.db` SQLite database.
///
/// Opens the database in read-only mode (`SQLITE_OPEN_READONLY`) so it never
/// creates -wal or -shm side-car files — the Hermes process owns the DB.
///
/// Extracts rows with `role IN ('user', 'assistant')`, ordered by timestamp and id.
/// Skips `role = 'tool'` in the WHERE clause. Applies `sanitize_content()` to
/// strip `<context>…</context>` and `<system-reminder>…</system-reminder>` blocks.
///
/// `is_csa_delegated`: pass-through flag set by the caller based on the file's
/// location (CSA state dir vs. user-facing session).
pub fn parse_hermes_db(
    path: &Path,
    fallback_session_id: &str,
    is_csa_delegated: bool,
) -> XurlResult<Vec<RawTurn>> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| XurlError::Parse(format!("cannot open hermes db: {e}")))?;

    // Attempt to read a session_id from the DB metadata (some versions store it).
    // Fall back to `fallback_session_id` if unavailable.
    let session_id = read_session_id(&conn).unwrap_or_else(|| fallback_session_id.to_string());
    let project_path = read_project_path(&conn);

    let mut stmt = conn
        .prepare(
            "SELECT id, role, content, timestamp \
             FROM messages \
             WHERE role IN ('user', 'assistant') \
             ORDER BY timestamp, id",
        )
        .map_err(|e| XurlError::Parse(format!("failed to prepare hermes query: {e}")))?;

    let mut turns = Vec::new();
    let mut turn_index: u32 = 0;

    let rows = stmt.query_map([], |row| {
        let role: String = row.get(1)?;
        let content: String = row.get(2)?;
        let timestamp: f64 = row.get(3)?;
        Ok((role, content, timestamp))
    });

    for row in rows.map_err(|e| XurlError::Parse(format!("hermes query error: {e}")))? {
        let (role_str, raw_content, timestamp_epoch) =
            row.map_err(|e| XurlError::Parse(format!("hermes row error: {e}")))?;

        let role = match role_str.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue, // defensive: WHERE clause should exclude these
        };

        let content = sanitize_content(&raw_content);
        if content.is_empty() {
            continue;
        }

        turns.push(RawTurn {
            session_id: session_id.clone(),
            tool: Tool::Hermes,
            role,
            content,
            timestamp_epoch,
            project_path: project_path.clone(),
            git_branch: None,
            is_csa_delegated,
            provenance: Provenance::Human,
            turn_index,
        });
        turn_index += 1;
    }

    Ok(turns)
}

/// Attempt to read a session ID from a `sessions` or `meta` table in the Hermes DB.
/// Returns `None` when no such table or row exists (graceful fallback).
fn read_session_id(conn: &Connection) -> Option<String> {
    // Try a `session_id` column in a metadata table if it exists.
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'session_id' LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Attempt to read a project/cwd path from known Hermes metadata shapes.
/// Returns `None` if the table/column is absent or the stored value is empty.
fn read_project_path(conn: &Connection) -> Option<String> {
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

/// Strip `<context>…</context>` and `<system-reminder>…</system-reminder>` tags.
///
/// These are injected by Hermes into message content but are not visible to the
/// user on screen. Stripping mirrors Hermes' own scrubber logic.
fn sanitize_content(input: &str) -> String {
    // Pattern: strip <context>…</context> (possibly multiline).
    let after_context = strip_tag_blocks(input, "context");
    // Pattern: strip <system-reminder>…</system-reminder>.
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
            // No closing tag — drop to end.
            break;
        }
    }
    result.push_str(remaining);
    result
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
                ("tool", "bash output", 1748356250.0), // should be skipped
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
    }

    #[test]
    fn hermes_parser_opens_readonly_never_creates_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);

        parse_hermes_db(&db_path, "s1", false).unwrap();

        // No -wal or -shm files should exist.
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
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "hi  there");
    }

    #[test]
    fn hermes_parser_skips_tool_role() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(
            &db_path,
            &[("tool", "ls output", 1.0), ("user", "good", 2.0)],
        );
        let turns = parse_hermes_db(&db_path, "s", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "good");
    }

    #[test]
    fn hermes_parser_csa_delegated_flag_propagated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);

        let turns_csa = parse_hermes_db(&db_path, "s", true).unwrap();
        assert!(turns_csa[0].is_csa_delegated);

        let turns_user = parse_hermes_db(&db_path, "s", false).unwrap();
        assert!(!turns_user[0].is_csa_delegated);
    }

    #[test]
    fn hermes_parser_reads_project_path_from_metadata_when_available() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);
        let conn = Connection::open(&db_path).expect("open fixture db");
        conn.execute_batch(
            "CREATE TABLE metadata (key TEXT NOT NULL, value TEXT NOT NULL);
             INSERT INTO metadata (key, value) VALUES ('cwd', '/repo/hermes');",
        )
        .expect("insert metadata");
        drop(conn);

        let turns = parse_hermes_db(&db_path, "s", false).unwrap();

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/hermes"));
    }

    #[test]
    fn hermes_sanitize_preserves_untagged_content() {
        assert_eq!(sanitize_content("hello world"), "hello world");
        assert_eq!(sanitize_content("  trimmed  "), "trimmed");
    }

    #[test]
    fn hermes_sanitize_multiple_context_blocks() {
        let input = "<context>a</context>keep1<context>b</context>keep2";
        assert_eq!(sanitize_content(input), "keep1keep2");
    }
}

use std::collections::HashSet;

use rusqlite::{Connection, params};

use crate::xurl::model::{RawTurn, Tool};
use crate::xurl::store::{self, InsertStats, normalize_cwd_filter};
use crate::xurl::{XurlError, XurlResult};

/// Limits which stored Hermes rows an authoritative source snapshot owns.
pub(crate) struct HermesReconcileScope<'a> {
    pub profile: &'a str,
    pub session_id: Option<&'a str>,
    pub cwd: Option<&'a str>,
}

/// Limits which Codex rows one authoritative rollout snapshot owns.
#[derive(Clone, Copy)]
pub(crate) struct CodexReconcileScope<'a> {
    pub session_id: &'a str,
    pub cwd: Option<&'a str>,
}

struct StoredIdentity {
    id: String,
    session_id: String,
    message_id: Option<String>,
    project_path: Option<String>,
}

/// Upsert one authoritative Hermes snapshot and remove rows absent from its
/// canonical `active OR compacted` seen-set in the same SQLite savepoint.
pub(crate) fn reconcile_hermes_snapshot(
    conn: &Connection,
    turns: &[RawTurn],
    scope: HermesReconcileScope<'_>,
) -> XurlResult<InsertStats> {
    validate_snapshot(turns, &scope)?;
    let seen = turns
        .iter()
        .map(|turn| {
            (
                turn.session_id.clone(),
                turn.metadata
                    .message_id
                    .clone()
                    .expect("validated message ID"),
            )
        })
        .collect::<HashSet<_>>();

    conn.execute_batch("SAVEPOINT xurl_reconcile_hermes")
        .map_err(XurlError::Database)?;
    let result = (|| {
        let mut stats = store::insert_turns(conn, turns)?;
        let stale_ids = stale_turn_ids(conn, &scope, &seen)?;
        stats.removed = stale_ids.len();
        for id in stale_ids {
            conn.execute(
                "DELETE FROM conversation_turn_vectors WHERE turn_id = ?1",
                params![&id],
            )
            .map_err(XurlError::Database)?;
            conn.execute("DELETE FROM conversation_turns WHERE id = ?1", params![&id])
                .map_err(XurlError::Database)?;
        }
        conn.execute_batch("RELEASE xurl_reconcile_hermes")
            .map_err(XurlError::Database)?;
        Ok(stats)
    })();

    if let Err(error) = result {
        let _ =
            conn.execute_batch("ROLLBACK TO xurl_reconcile_hermes; RELEASE xurl_reconcile_hermes;");
        return Err(error);
    }
    result
}

/// Upsert one authoritative Codex rollout snapshot and remove obsolete rows
/// and vectors in the same SQLite savepoint.
pub(crate) fn reconcile_codex_snapshot(
    conn: &Connection,
    turns: &[RawTurn],
    scope: CodexReconcileScope<'_>,
) -> XurlResult<InsertStats> {
    validate_codex_snapshot(turns, scope)?;
    let seen = turns
        .iter()
        .map(|turn| {
            turn.metadata
                .message_id
                .clone()
                .expect("validated Codex source identity")
        })
        .collect::<HashSet<_>>();

    conn.execute_batch("SAVEPOINT xurl_reconcile_codex")
        .map_err(XurlError::Database)?;
    let result = (|| {
        let mut stats = store::insert_turns(conn, turns)?;
        let stale_ids = stale_codex_turn_ids(conn, scope, &seen)?;
        stats.removed = stale_ids.len();
        for id in stale_ids {
            conn.execute(
                "DELETE FROM conversation_turn_vectors WHERE turn_id = ?1",
                params![&id],
            )
            .map_err(XurlError::Database)?;
            conn.execute("DELETE FROM conversation_turns WHERE id = ?1", params![&id])
                .map_err(XurlError::Database)?;
        }
        conn.execute_batch("RELEASE xurl_reconcile_codex")
            .map_err(XurlError::Database)?;
        Ok(stats)
    })();

    if let Err(error) = result {
        let _ =
            conn.execute_batch("ROLLBACK TO xurl_reconcile_codex; RELEASE xurl_reconcile_codex;");
        return Err(error);
    }
    result
}

fn validate_snapshot(turns: &[RawTurn], scope: &HermesReconcileScope<'_>) -> XurlResult<()> {
    if scope.profile.trim().is_empty() {
        return Err(XurlError::Parse(
            "Hermes reconcile requires a non-empty profile".to_string(),
        ));
    }
    for turn in turns {
        let valid_identity = turn.tool == Tool::Hermes
            && turn.metadata.hermes_profile.as_deref() == Some(scope.profile)
            && scope
                .session_id
                .is_none_or(|session_id| turn.session_id == session_id)
            && path_is_in_scope(turn.project_path.as_deref(), scope.cwd)
            && turn
                .metadata
                .message_id
                .as_deref()
                .is_some_and(|message_id| !message_id.is_empty());
        if !valid_identity {
            return Err(XurlError::Parse(
                "Hermes reconcile snapshot contains a turn outside its stable profile/session/message identity scope"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_codex_snapshot(turns: &[RawTurn], scope: CodexReconcileScope<'_>) -> XurlResult<()> {
    if scope.session_id.trim().is_empty() || turns.is_empty() {
        return Err(XurlError::Parse(
            "Codex reconcile requires a non-empty session and snapshot".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for turn in turns {
        let Some(source_id) = turn
            .metadata
            .message_id
            .as_deref()
            .filter(|id| !id.is_empty())
        else {
            return Err(XurlError::Parse(
                "Codex reconcile snapshot lacks a stable source identity".to_string(),
            ));
        };
        if turn.tool != Tool::Codex
            || turn.session_id != scope.session_id
            || !path_is_in_scope(turn.project_path.as_deref(), scope.cwd)
            || !seen.insert(source_id)
        {
            return Err(XurlError::Parse(
                "Codex reconcile snapshot contains an ambiguous turn outside its source scope"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn stale_turn_ids(
    conn: &Connection,
    scope: &HermesReconcileScope<'_>,
    seen: &HashSet<(String, String)>,
) -> XurlResult<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, message_id, project_path
             FROM conversation_turns
             WHERE tool = 'hermes'
               AND (hermes_profile = ?1
                    OR (?1 = 'default' AND hermes_profile IS NULL AND message_id IS NULL))
               AND (?2 IS NULL OR session_id = ?2)",
        )
        .map_err(XurlError::Database)?;
    let rows = stmt
        .query_map(params![scope.profile, scope.session_id], |row| {
            Ok(StoredIdentity {
                id: row.get(0)?,
                session_id: row.get(1)?,
                message_id: row.get(2)?,
                project_path: row.get(3)?,
            })
        })
        .map_err(XurlError::Database)?;

    let mut stale = Vec::new();
    for row in rows {
        let stored = row.map_err(XurlError::Database)?;
        if !path_is_in_scope(stored.project_path.as_deref(), scope.cwd) {
            continue;
        }
        let is_seen = stored.message_id.as_ref().is_some_and(|message_id| {
            seen.contains(&(stored.session_id.clone(), message_id.clone()))
        });
        if !is_seen {
            stale.push(stored.id);
        }
    }
    Ok(stale)
}

fn stale_codex_turn_ids(
    conn: &Connection,
    scope: CodexReconcileScope<'_>,
    seen: &HashSet<String>,
) -> XurlResult<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, message_id, project_path
             FROM conversation_turns
             WHERE tool = 'codex' AND session_id = ?1",
        )
        .map_err(XurlError::Database)?;
    let rows = stmt
        .query_map(params![scope.session_id], |row| {
            Ok(StoredIdentity {
                id: row.get(0)?,
                session_id: row.get(1)?,
                message_id: row.get(2)?,
                project_path: row.get(3)?,
            })
        })
        .map_err(XurlError::Database)?;

    let mut stale = Vec::new();
    for row in rows {
        let stored = row.map_err(XurlError::Database)?;
        if path_is_in_scope(stored.project_path.as_deref(), scope.cwd)
            && !stored
                .message_id
                .as_ref()
                .is_some_and(|message_id| seen.contains(message_id))
        {
            stale.push(stored.id);
        }
    }
    Ok(stale)
}

fn path_is_in_scope(project_path: Option<&str>, cwd_filter: Option<&str>) -> bool {
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

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::xurl::model::{Provenance, Role, TurnMetadata};

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open database");
        conn.execute_batch(
            "CREATE TABLE conversation_turns (
                id TEXT PRIMARY KEY, session_id TEXT, tool TEXT, turn_index INTEGER, role TEXT,
                content TEXT, timestamp_epoch REAL, token_count INTEGER, project_path TEXT,
                git_branch TEXT, is_csa_delegated INTEGER, provenance TEXT, hermes_profile TEXT,
                session_title TEXT, session_source TEXT, message_id TEXT, tool_name TEXT,
                tool_call_id TEXT, previous_message_id TEXT, next_message_id TEXT
             );
             CREATE TABLE conversation_turn_vectors (turn_id TEXT, chunk_index INTEGER);",
        )
        .expect("create xurl tables");
        conn
    }

    fn codex_turn(id: &str, content: &str, index: u32, project_path: &str) -> RawTurn {
        RawTurn {
            session_id: "codex-session".to_string(),
            tool: Tool::Codex,
            role: Role::Assistant,
            content: content.to_string(),
            timestamp_epoch: 1.0,
            project_path: Some(project_path.to_string()),
            git_branch: None,
            is_csa_delegated: false,
            provenance: Provenance::Human,
            turn_index: index,
            metadata: TurnMetadata {
                message_id: Some(id.to_string()),
                ..TurnMetadata::default()
            },
        }
    }

    #[test]
    fn codex_reconcile_replaces_snapshot_rows_vectors_and_preserves_other_projects() {
        let conn = setup_db();
        let scope = CodexReconcileScope {
            session_id: "codex-session",
            cwd: Some("/project/one"),
        };
        let original = codex_turn("original", "obsolete answer", 0, "/project/one");
        reconcile_codex_snapshot(&conn, &[original], scope).expect("store original snapshot");
        conn.execute(
            "INSERT INTO conversation_turn_vectors (turn_id, chunk_index) \
             SELECT id, 0 FROM conversation_turns WHERE content = 'obsolete answer'",
            [],
        )
        .expect("store obsolete vector");
        store::insert_turns(
            &conn,
            &[codex_turn("other", "other project", 0, "/project/two")],
        )
        .expect("store other project");

        let canonical = [
            codex_turn("compact", "continuation summary", 8, "/project/one"),
            codex_turn("rewritten", "canonical answer", 4, "/project/one"),
        ];
        let stats = reconcile_codex_snapshot(&conn, &canonical, scope)
            .expect("replace with canonical snapshot");

        assert_eq!(stats.removed, 1);
        let contents = conn
            .prepare("SELECT content FROM conversation_turns ORDER BY content")
            .expect("prepare contents")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query contents")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect contents");
        assert_eq!(
            contents,
            ["canonical answer", "continuation summary", "other project"]
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM conversation_turn_vectors",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("count vectors"),
            0
        );

        let reordered = [
            codex_turn("rewritten", "canonical answer", 0, "/project/one"),
            codex_turn("compact", "continuation summary", 1, "/project/one"),
        ];
        let repeat =
            reconcile_codex_snapshot(&conn, &reordered, scope).expect("repeat canonical snapshot");
        assert_eq!(repeat.removed, 0);
        assert_eq!(repeat.updated, 0);
        assert_eq!(repeat.skipped, 2);
    }

    #[test]
    fn codex_reconcile_fails_closed_without_removing_existing_turns() {
        let conn = setup_db();
        let scope = CodexReconcileScope {
            session_id: "codex-session",
            cwd: Some("/project/one"),
        };
        reconcile_codex_snapshot(
            &conn,
            &[codex_turn("original", "existing answer", 0, "/project/one")],
            scope,
        )
        .expect("store original snapshot");

        let duplicate = [
            codex_turn("duplicate", "first", 0, "/project/one"),
            codex_turn("duplicate", "second", 1, "/project/one"),
        ];
        assert!(reconcile_codex_snapshot(&conn, &duplicate, scope).is_err());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM conversation_turns", [], |row| row
                .get::<_, i64>(0))
                .expect("count preserved rows"),
            1
        );
    }
}

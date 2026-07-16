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

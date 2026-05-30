use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::Value;

use crate::xurl::ingest::{AutoScanConfig, decode_claude_project_path};
use crate::xurl::model::Tool;
use crate::xurl::parser::{cc, codex, hermes};
use crate::xurl::{XurlError, XurlResult};

const DEFAULT_BATCH_SIZE: usize = 1_000;

#[derive(Debug, Clone)]
pub struct BackfillSourceConfig {
    pub cc_root: PathBuf,
    pub codex_root: PathBuf,
    pub hermes_db: Option<PathBuf>,
}

impl Default for BackfillSourceConfig {
    fn default() -> Self {
        let cfg = AutoScanConfig::default();
        Self {
            cc_root: cfg.cc_root,
            codex_root: cfg.codex_root,
            hermes_db: cfg.hermes_db,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackfillOptions {
    pub dry_run: bool,
    pub batch_size: usize,
}

impl BackfillOptions {
    pub fn dry_run() -> Self {
        Self {
            dry_run: true,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    pub fn execute() -> Self {
        Self {
            dry_run: false,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct BackfillProjectGroup {
    pub sessions: usize,
    pub turns: usize,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct BackfillProjectPathStats {
    pub sessions_scanned: usize,
    pub turns_filled: usize,
    pub turns_skipped_no_source: usize,
    pub turns_already_set: usize,
    pub batches: usize,
    pub by_project_path: BTreeMap<String, BackfillProjectGroup>,
}

#[derive(Debug, Clone)]
struct NullSession {
    tool: Tool,
    session_id: String,
    turns: usize,
}

#[derive(Debug, Clone)]
struct ResolvedSession {
    tool: Tool,
    session_id: String,
    project_path: String,
}

pub fn backfill_project_paths(
    conn: &Connection,
    sources: &BackfillSourceConfig,
    options: BackfillOptions,
) -> XurlResult<BackfillProjectPathStats> {
    let batch_size = if options.batch_size == 0 {
        DEFAULT_BATCH_SIZE
    } else {
        options.batch_size
    };
    let batch_limit = i64::try_from(batch_size)
        .map_err(|_| XurlError::Parse("batch size exceeds i64".to_string()))?;

    let mut stats = BackfillProjectPathStats {
        turns_already_set: count_non_null_project_paths(conn)?,
        ..BackfillProjectPathStats::default()
    };
    let null_sessions = query_null_sessions(conn)?;
    stats.sessions_scanned = null_sessions.len();

    let mut resolved = Vec::new();
    for session in null_sessions {
        match resolve_project_path(&session, sources)? {
            Some(project_path) => {
                record_project_group(&mut stats, &project_path, session.turns);
                if options.dry_run {
                    stats.turns_filled += session.turns;
                } else {
                    resolved.push(ResolvedSession {
                        tool: session.tool,
                        session_id: session.session_id,
                        project_path,
                    });
                }
            }
            None => {
                stats.turns_skipped_no_source += session.turns;
            }
        }
    }

    if options.dry_run {
        return Ok(stats);
    }

    for session in resolved {
        let (filled, batches) = update_session_project_path(conn, &session, batch_limit)?;
        stats.turns_filled += filled;
        stats.batches += batches;
    }

    Ok(stats)
}

fn query_null_sessions(conn: &Connection) -> XurlResult<Vec<NullSession>> {
    let mut stmt = conn
        .prepare(
            "SELECT tool, session_id, COUNT(*) \
             FROM conversation_turns \
             WHERE project_path IS NULL \
             GROUP BY tool, session_id \
             ORDER BY tool, session_id",
        )
        .map_err(XurlError::Database)?;
    let rows = stmt
        .query_map([], |row| {
            let tool_name: String = row.get(0)?;
            Ok((tool_name, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        })
        .map_err(XurlError::Database)?;

    let mut sessions = Vec::new();
    for row in rows {
        let (tool_name, session_id, turns) = row.map_err(XurlError::Database)?;
        if let Some(tool) = parse_tool(&tool_name) {
            sessions.push(NullSession {
                tool,
                session_id,
                turns: usize::try_from(turns)
                    .map_err(|_| XurlError::Parse("negative turn count".to_string()))?,
            });
        }
    }
    Ok(sessions)
}

fn count_non_null_project_paths(conn: &Connection) -> XurlResult<usize> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_turns WHERE project_path IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .map_err(XurlError::Database)?;
    usize::try_from(count).map_err(|_| XurlError::Parse("negative row count".to_string()))
}

fn parse_tool(tool: &str) -> Option<Tool> {
    match tool {
        "cc" => Some(Tool::Cc),
        "codex" => Some(Tool::Codex),
        "hermes" => Some(Tool::Hermes),
        _ => None,
    }
}

fn record_project_group(stats: &mut BackfillProjectPathStats, project_path: &str, turns: usize) {
    let group = stats
        .by_project_path
        .entry(project_path.to_string())
        .or_default();
    group.sessions += 1;
    group.turns += turns;
}

fn resolve_project_path(
    session: &NullSession,
    sources: &BackfillSourceConfig,
) -> XurlResult<Option<String>> {
    match session.tool {
        Tool::Cc => resolve_cc_project_path(&sources.cc_root, &session.session_id),
        Tool::Codex => resolve_codex_project_path(&sources.codex_root, &session.session_id),
        Tool::Hermes => resolve_hermes_project_path(sources.hermes_db.as_deref()),
    }
}

fn resolve_cc_project_path(root: &Path, session_id: &str) -> XurlResult<Option<String>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut found: Option<String> = None;
    for path in collect_jsonl_files(root) {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let file_stem_matches = path.file_stem().and_then(|name| name.to_str()) == Some(session_id);
        let content_matches = cc::extract_session_id(&content).as_deref() == Some(session_id);
        if !file_stem_matches && !content_matches {
            continue;
        }
        let candidate = derive_cc_project_path(&path, &content, session_id)?;
        if !merge_unique_candidate(&mut found, candidate) {
            return Ok(None);
        }
    }
    Ok(found)
}

fn derive_cc_project_path(
    path: &Path,
    content: &str,
    session_id: &str,
) -> XurlResult<Option<String>> {
    let fallback = decode_claude_project_path(path);
    let turns = cc::parse_cc_jsonl(content, session_id, fallback.as_deref(), false)?;
    Ok(turns
        .iter()
        .find(|turn| turn.session_id == session_id)
        .and_then(|turn| turn.project_path.clone())
        .or(fallback))
}

fn resolve_codex_project_path(root: &Path, session_id: &str) -> XurlResult<Option<String>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut found: Option<String> = None;
    for path in collect_jsonl_files(root) {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(_) => continue,
        };
        let candidate = derive_codex_project_path(&content, session_id);
        if !merge_unique_candidate(&mut found, candidate) {
            return Ok(None);
        }
    }
    Ok(found)
}

fn derive_codex_project_path(content: &str, session_id: &str) -> Option<String> {
    for raw_line in content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(obj) = serde_json::from_str::<Value>(raw_line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let matches_session = obj
            .get("payload")
            .and_then(|payload| payload.get("id"))
            .and_then(Value::as_str)
            == Some(session_id);
        if matches_session {
            return codex::extract_session_cwd(&obj);
        }
    }
    None
}

fn resolve_hermes_project_path(path: Option<&Path>) -> XurlResult<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| XurlError::Parse(format!("cannot open hermes db: {e}")))?;
    Ok(hermes::read_project_path(&conn))
}

fn merge_unique_candidate(current: &mut Option<String>, candidate: Option<String>) -> bool {
    let Some(candidate) = candidate else {
        return true;
    };
    match current {
        Some(existing) if existing != &candidate => false,
        Some(_) => true,
        None => {
            *current = Some(candidate);
            true
        }
    }
}

fn collect_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_jsonl_recursive(root, &mut paths);
    paths
}

fn collect_jsonl_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_recursive(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn update_session_project_path(
    conn: &Connection,
    session: &ResolvedSession,
    batch_limit: i64,
) -> XurlResult<(usize, usize)> {
    let mut total = 0usize;
    let mut batches = 0usize;
    loop {
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(XurlError::Database)?;
        let update_result = conn.execute(
            "UPDATE conversation_turns \
             SET project_path = ?1 \
             WHERE id IN ( \
                 SELECT id FROM conversation_turns \
                 WHERE tool = ?2 AND session_id = ?3 AND project_path IS NULL \
                 ORDER BY turn_index \
                 LIMIT ?4 \
             )",
            params![
                &session.project_path,
                session.tool.as_str(),
                &session.session_id,
                batch_limit,
            ],
        );
        match update_result {
            Ok(updated) => {
                conn.execute_batch("COMMIT").map_err(XurlError::Database)?;
                batches += 1;
                total += updated;
                if updated == 0 || updated < batch_limit as usize {
                    break;
                }
            }
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(XurlError::Database(err));
            }
        }
    }
    Ok((total, batches))
}

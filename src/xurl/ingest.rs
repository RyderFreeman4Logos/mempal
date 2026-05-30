use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::core::db::Database;
use crate::embed::Embedder;
use crate::xurl::embed;
use crate::xurl::model::Tool;
use crate::xurl::parser::{cc::parse_cc_jsonl, codex::parse_codex_jsonl, hermes::parse_hermes_db};
use crate::xurl::store;
use crate::xurl::{XurlError, XurlResult};

/// Callback invoked after each file is parsed: `(filename, turns_parsed)`.
type ParsedCb<'a> = Option<&'a dyn Fn(&str, usize)>;
/// Callback invoked after each embed batch: `(done, total)`.
type EmbedCb<'a> = Option<&'a dyn Fn(usize, usize)>;

#[derive(Debug, Default, Serialize)]
pub struct IngestStats {
    pub turns_parsed: usize,
    pub turns_inserted: usize,
    pub turns_skipped: usize,
    pub turns_updated: usize,
    pub vectors_created: usize,
}

pub struct AutoScanConfig {
    pub cc_root: PathBuf,
    pub codex_root: PathBuf,
    pub hermes_db: Option<PathBuf>,
}

impl Default for AutoScanConfig {
    fn default() -> Self {
        let home = home_dir();
        let codex_root = std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home.join(".codex"))
            .join("sessions");
        let hermes_candidate = home.join(".hermes/state.db");
        Self {
            cc_root: home.join(".claude/projects"),
            codex_root,
            hermes_db: if hermes_candidate.exists() {
                Some(hermes_candidate)
            } else {
                None
            },
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Parse a file and insert turns into the DB. Does not embed.
///
/// Returns `(filename, turns_parsed, insert_stats, turn_ids)` where `turn_ids`
/// are the deterministic IDs of every parsed turn — used to scope a single-file
/// embed pass to just this file's turns.
fn parse_and_store_file(
    db: &Database,
    path: &Path,
    tool: Tool,
    session_id_override: Option<&str>,
) -> XurlResult<(String, usize, store::InsertStats, Vec<String>)> {
    let fallback = session_id_override.map(str::to_string).unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let is_csa = path
        .to_str()
        .map(|s| s.contains(".local/state/cli-sub-agent"))
        .unwrap_or(false);

    let turns = match tool {
        Tool::Cc => {
            let content = fs::read_to_string(path).map_err(XurlError::Io)?;
            let fallback_project_path = decode_claude_project_path(path);
            parse_cc_jsonl(
                &content,
                &fallback,
                fallback_project_path.as_deref(),
                is_csa,
            )?
        }
        Tool::Codex => {
            let content = fs::read_to_string(path).map_err(XurlError::Io)?;
            parse_codex_jsonl(&content, &fallback, is_csa)?
        }
        Tool::Hermes => parse_hermes_db(path, &fallback, is_csa)?,
    };

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let turns_parsed = turns.len();
    let turn_ids: Vec<String> = turns.iter().map(store::turn_id_for).collect();
    let insert_stats = store::insert_turns(db.conn(), &turns)?;
    Ok((filename, turns_parsed, insert_stats, turn_ids))
}

/// Ingest a single file (or SQLite DB for Hermes) and embed any newly inserted turns.
///
/// `on_file_parsed` is called after parsing with `(filename, turns_parsed)`.
/// `on_embed_progress` is forwarded to the scoped embed pass after parsing.
pub async fn ingest_file<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    path: &Path,
    tool: Tool,
    session_id_override: Option<&str>,
    on_file_parsed: ParsedCb<'_>,
    on_embed_progress: EmbedCb<'_>,
) -> XurlResult<IngestStats> {
    let (filename, turns_parsed, insert_stats, turn_ids) =
        parse_and_store_file(db, path, tool, session_id_override)?;
    if let Some(f) = on_file_parsed {
        f(&filename, turns_parsed);
    }
    // Scope the embed pass to just this file's turns so a single-file ingest
    // returns promptly instead of draining the entire historical backlog.
    let embed_stats =
        embed::embed_unindexed_turns_scoped(db, embedder, &turn_ids, on_embed_progress).await?;

    Ok(IngestStats {
        turns_parsed,
        turns_inserted: insert_stats.inserted,
        turns_skipped: insert_stats.skipped,
        turns_updated: insert_stats.updated,
        vectors_created: embed_stats.embedded,
    })
}

/// Scan all default tool directories and ingest every discovered file.
///
/// Uses a two-phase approach: first all files are parsed and stored (Phase 1),
/// then all unindexed turns are embedded in batched transactions (Phase 2).
/// This avoids the per-file embed overhead that stalls on large DBs.
///
/// `on_file_parsed` is called after each file's turns are stored with `(filename, turns_parsed)`.
/// `on_embed_progress` is forwarded to `embed_unindexed_turns` during Phase 2.
pub async fn ingest_all<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    cfg: &AutoScanConfig,
    on_file_parsed: ParsedCb<'_>,
    on_embed_progress: EmbedCb<'_>,
) -> XurlResult<IngestStats> {
    let mut total = IngestStats::default();

    // Phase 1: parse and store all files (no embedding yet).
    if cfg.cc_root.exists() {
        for path in collect_files_with_ext(&cfg.cc_root, "jsonl") {
            let (filename, turns_parsed, insert_stats, _turn_ids) =
                parse_and_store_file(db, &path, Tool::Cc, None)?;
            total.turns_parsed += turns_parsed;
            total.turns_inserted += insert_stats.inserted;
            total.turns_skipped += insert_stats.skipped;
            total.turns_updated += insert_stats.updated;
            if let Some(f) = on_file_parsed {
                f(&filename, turns_parsed);
            }
        }
    }

    if cfg.codex_root.exists() {
        for path in collect_files_with_ext(&cfg.codex_root, "jsonl") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with("rollout-") {
                let (filename, turns_parsed, insert_stats, _turn_ids) =
                    parse_and_store_file(db, &path, Tool::Codex, None)?;
                total.turns_parsed += turns_parsed;
                total.turns_inserted += insert_stats.inserted;
                total.turns_skipped += insert_stats.skipped;
                total.turns_updated += insert_stats.updated;
                if let Some(f) = on_file_parsed {
                    f(&filename, turns_parsed);
                }
            }
        }
    }

    if let Some(hermes_path) = &cfg.hermes_db {
        if hermes_path.exists() {
            let (filename, turns_parsed, insert_stats, _turn_ids) =
                parse_and_store_file(db, hermes_path, Tool::Hermes, None)?;
            total.turns_parsed += turns_parsed;
            total.turns_inserted += insert_stats.inserted;
            total.turns_skipped += insert_stats.skipped;
            total.turns_updated += insert_stats.updated;
            if let Some(f) = on_file_parsed {
                f(&filename, turns_parsed);
            }
        }
    }

    // Phase 2: embed all unindexed turns in one batched pass.
    let embed_stats = embed::embed_unindexed_turns(db, embedder, on_embed_progress).await?;
    total.vectors_created += embed_stats.embedded;

    Ok(total)
}

fn collect_files_with_ext(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_recursive(root, ext, &mut out);
    out
}

pub(crate) fn decode_claude_project_path(path: &Path) -> Option<String> {
    let session_dir = path.parent()?;
    let projects_dir = session_dir.parent()?;
    if projects_dir.file_name().and_then(|name| name.to_str()) != Some("projects") {
        return None;
    }
    if projects_dir
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        != Some(".claude")
    {
        return None;
    }
    session_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|encoded| !encoded.is_empty())
        .map(|encoded| encoded.replace('-', "/"))
}

fn collect_recursive(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_recursive(&path, ext, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

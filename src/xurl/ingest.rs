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

#[derive(Debug, Default, Serialize)]
pub struct IngestStats {
    pub turns_parsed: usize,
    pub turns_inserted: usize,
    pub turns_skipped: usize,
    pub turns_updated: usize,
    pub vectors_created: usize,
}

impl IngestStats {
    fn merge(&mut self, other: &Self) {
        self.turns_parsed += other.turns_parsed;
        self.turns_inserted += other.turns_inserted;
        self.turns_skipped += other.turns_skipped;
        self.turns_updated += other.turns_updated;
        self.vectors_created += other.vectors_created;
    }
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

/// Ingest a single file (or SQLite DB for Hermes) and embed any newly inserted turns.
pub async fn ingest_file<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    path: &Path,
    tool: Tool,
    session_id_override: Option<&str>,
) -> XurlResult<IngestStats> {
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
            parse_cc_jsonl(&content, &fallback, is_csa)?
        }
        Tool::Codex => {
            let content = fs::read_to_string(path).map_err(XurlError::Io)?;
            parse_codex_jsonl(&content, &fallback, is_csa)?
        }
        Tool::Hermes => parse_hermes_db(path, &fallback, is_csa)?,
    };

    let turns_parsed = turns.len();
    let insert_stats = store::insert_turns(db.conn(), &turns)?;
    let embed_stats = embed::embed_unindexed_turns(db, embedder).await?;

    Ok(IngestStats {
        turns_parsed,
        turns_inserted: insert_stats.inserted,
        turns_skipped: insert_stats.skipped,
        turns_updated: insert_stats.updated,
        vectors_created: embed_stats.embedded,
    })
}

/// Scan all default tool directories and ingest every discovered file.
pub async fn ingest_all<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    cfg: &AutoScanConfig,
) -> XurlResult<IngestStats> {
    let mut total = IngestStats::default();

    if cfg.cc_root.exists() {
        for path in collect_files_with_ext(&cfg.cc_root, "jsonl") {
            let stats = ingest_file(db, embedder, &path, Tool::Cc, None).await?;
            total.merge(&stats);
        }
    }

    if cfg.codex_root.exists() {
        for path in collect_files_with_ext(&cfg.codex_root, "jsonl") {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.starts_with("rollout-") {
                let stats = ingest_file(db, embedder, &path, Tool::Codex, None).await?;
                total.merge(&stats);
            }
        }
    }

    if let Some(hermes_path) = &cfg.hermes_db {
        if hermes_path.exists() {
            let stats = ingest_file(db, embedder, hermes_path, Tool::Hermes, None).await?;
            total.merge(&stats);
        }
    }

    Ok(total)
}

fn collect_files_with_ext(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_recursive(root, ext, &mut out);
    out
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

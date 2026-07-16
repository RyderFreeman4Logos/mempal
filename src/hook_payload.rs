//! Hook-payload retention: prune unreferenced, old spool files.
//!
//! Scans the hook-payloads spool directory and deletes files that are:
//! - NOT referenced by any pending or claimed queue row
//! - Older than the configured retention period

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::hook::HOOK_SPOOL_DIR;

/// Outcome of a prune pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneOutcome {
    pub scanned_files: usize,
    pub deleted_files: usize,
    pub referenced_files: usize,
    pub young_files: usize,
}

/// Prune old, unreferenced hook-payload files from the spool directory.
///
/// Files referenced by pending or claimed queue rows are always retained.
/// Files newer than `retention_days` are always retained.
pub fn prune_hook_payloads(
    mempal_home: &Path,
    db_path: &Path,
    retention_days: u64,
) -> Result<PruneOutcome> {
    prune_hook_payloads_with_mode(mempal_home, db_path, retention_days, true)
}

/// Like [`prune_hook_payloads`], but when `execute` is false only reports what
/// would be deleted (`deleted_files` counts candidates) without removing files.
pub fn prune_hook_payloads_with_mode(
    mempal_home: &Path,
    db_path: &Path,
    retention_days: u64,
    execute: bool,
) -> Result<PruneOutcome> {
    let spool_dir = mempal_home.join(HOOK_SPOOL_DIR);
    if !spool_dir.exists() {
        return Ok(PruneOutcome {
            scanned_files: 0,
            deleted_files: 0,
            referenced_files: 0,
            young_files: 0,
        });
    }

    let referenced = collect_referenced_payload_paths(db_path)?;
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days * 86400);

    let mut scanned_files = 0usize;
    let mut deleted_files = 0usize;
    let mut referenced_files = 0usize;
    let mut young_files = 0usize;

    for entry in std::fs::read_dir(&spool_dir)
        .with_context(|| format!("failed to read spool dir {}", spool_dir.display()))?
    {
        let entry = entry.context("failed to read spool dir entry")?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        scanned_files += 1;

        // Retain if referenced by active queue rows.
        if is_referenced(&path, &referenced) {
            referenced_files += 1;
            continue;
        }

        // Retain if younger than the retention cutoff.
        let mtime = entry
            .metadata()
            .with_context(|| format!("failed to read metadata for {}", path.display()))?
            .modified()
            .with_context(|| format!("failed to read mtime for {}", path.display()))?;

        if mtime > cutoff {
            young_files += 1;
            continue;
        }

        // Old and unreferenced — delete (or count in dry-run).
        if execute {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to prune {}", path.display()))?;
        }
        deleted_files += 1;
    }

    Ok(PruneOutcome {
        scanned_files,
        deleted_files,
        referenced_files,
        young_files,
    })
}

/// Collect the set of payload_path values from pending and claimed queue rows.
fn collect_referenced_payload_paths(db_path: &Path) -> Result<HashSet<PathBuf>> {
    let conn = Connection::open(db_path).context("failed to open database for retention scan")?;
    let mut referenced = HashSet::new();

    // pending_messages stores payload_path in the envelope JSON.
    // Extract via SQL JSON extraction if available, or scan rows.
    let mut stmt = conn
        .prepare("SELECT payload FROM pending_messages")
        .context("failed to prepare pending_messages query")?;

    let rows = stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .context("failed to query pending_messages")?;

    for row in rows {
        if let Ok(Some(payload_json)) = row {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload_json) {
                if let Some(path_str) = value.get("payload_path").and_then(|v| v.as_str()) {
                    referenced.insert(PathBuf::from(path_str));
                }
            }
        }
    }

    Ok(referenced)
}

/// Check whether a file path is referenced by any active queue row.
fn is_referenced(path: &Path, referenced: &HashSet<PathBuf>) -> bool {
    // Direct match.
    if referenced.contains(path) {
        return true;
    }
    // Match by file name (queue rows may store relative or absolute paths).
    if let Some(name) = path.file_name() {
        referenced.iter().any(|r| r.file_name() == Some(name))
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_outcome_debug() {
        let outcome = PruneOutcome {
            scanned_files: 10,
            deleted_files: 3,
            referenced_files: 5,
            young_files: 2,
        };
        assert_eq!(outcome.scanned_files, 10);
    }
}

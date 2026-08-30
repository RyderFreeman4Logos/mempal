//! Hook-payload retention: prune unreferenced, old spool files.
//!
//! Scans the hook-payloads spool directory and deletes files that are:
//! - NOT referenced by any pending or claimed queue row
//! - Older than the configured retention period

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::hook::HOOK_SPOOL_DIR;

const PAYLOAD_RETENTION_LOCK: &str = ".hook-payload-retention.lock";

/// Cross-process owner for payload publication and pruning.
pub(crate) struct PayloadRetentionLock(File);

pub(crate) fn lock_for_home(mempal_home: &Path) -> Result<PayloadRetentionLock> {
    let lock_path = mempal_home.join(PAYLOAD_RETENTION_LOCK);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| {
            format!(
                "failed to open payload retention lock {}",
                lock_path.display()
            )
        })?;
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", lock_path.display()));
    }
    Ok(PayloadRetentionLock(file))
}

pub(crate) fn lock_for_payload_path(path: &Path) -> Result<PayloadRetentionLock> {
    let payload_dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("payload path has no parent: {}", path.display()))?;
    let mempal_home = payload_dir.parent().unwrap_or(payload_dir);
    lock_for_home(mempal_home)
}

impl Drop for PayloadRetentionLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until this `Drop` completes.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

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
    let payload_dir = mempal_home.join("hook-payloads");
    let spool_dir = mempal_home.join(HOOK_SPOOL_DIR);
    if !payload_dir.exists() && !spool_dir.exists() {
        return Ok(PruneOutcome {
            scanned_files: 0,
            deleted_files: 0,
            referenced_files: 0,
            young_files: 0,
        });
    }

    let _retention_lock = lock_for_home(mempal_home)?;
    let referenced = collect_referenced_payload_paths(db_path)?;
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days * 86400);

    let mut scanned_files = 0usize;
    let mut deleted_files = 0usize;
    let mut referenced_files = 0usize;
    let mut young_files = 0usize;

    for directory in [&payload_dir, &spool_dir] {
        if !directory.exists() {
            continue;
        }
        for entry in std::fs::read_dir(directory)
            .with_context(|| format!("failed to read payload dir {}", directory.display()))?
        {
            let entry = entry.context("failed to read payload dir entry")?;
            let path = entry.path();

            if !entry
                .file_type()
                .context("failed to read payload entry type")?
                .is_file()
            {
                continue;
            }

            scanned_files += 1;
            if !is_valid_payload_path(&path, directory) {
                continue;
            }

            // Retain if referenced by an active drawer or queue row.
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

    let mut drawer_stmt = conn
        .prepare("SELECT source_file FROM drawers WHERE deleted_at IS NULL")
        .context("failed to prepare drawer payload query")?;
    let drawer_rows = drawer_stmt
        .query_map([], |row| row.get::<_, Option<String>>(0))
        .context("failed to query drawer payload paths")?;
    for row in drawer_rows {
        if let Ok(Some(path)) = row {
            referenced.insert(PathBuf::from(path));
        }
    }

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

fn is_valid_payload_path(path: &Path, directory: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    let mut parts = stem.split('.');
    let Some(hash) = parts.next() else {
        return false;
    };
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }

    match directory.file_name().and_then(|name| name.to_str()) {
        Some("hook-payloads") => parts.next().is_none(),
        Some(HOOK_SPOOL_DIR) => {
            parts.next().is_some_and(|part| {
                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
            }) && parts.next().is_some_and(|part| {
                !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
            }) && parts.next().is_none()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_old_unreferenced_rejected_payloads_but_keeps_active_and_shared_payloads() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("palace.db");
        let db = crate::core::db::Database::open(&db_path).expect("open db");
        let payload_dir = temp.path().join("hook-payloads");
        std::fs::create_dir_all(&payload_dir).expect("create payload dir");

        let path_for = |payload: &str| {
            payload_dir.join(format!(
                "{}.json",
                blake3::hash(payload.as_bytes()).to_hex()
            ))
        };
        let active_payload = "active payload";
        let shared_payload = "shared payload";
        let rejected_payload = "rejected payload";
        let active_path = path_for(active_payload);
        let shared_path = path_for(shared_payload);
        let rejected_path = path_for(rejected_payload);
        std::fs::write(&active_path, active_payload).expect("write active payload");
        std::fs::write(&shared_path, shared_payload).expect("write shared payload");
        std::fs::write(&rejected_path, rejected_payload).expect("write rejected payload");
        std::fs::write(payload_dir.join("not-a-hash.json"), "invalid path")
            .expect("write invalid payload path");
        for path in [&active_path, &shared_path, &rejected_path] {
            filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(0, 0))
                .expect("age payload");
        }
        filetime::set_file_mtime(
            payload_dir.join("not-a-hash.json"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .expect("age invalid payload path");

        let drawer = |id: &str, source_file: &std::path::Path| {
            crate::core::types::Drawer::new_bootstrap_evidence(
                crate::core::types::BootstrapEvidenceArgs {
                    id: id.to_string(),
                    content: format!("{id} content"),
                    wing: "hooks-raw".to_string(),
                    room: Some("PostToolUse".to_string()),
                    source_file: Some(source_file.to_string_lossy().into_owned()),
                    source_type: crate::core::types::SourceType::SystemGenerated,
                    added_at: "2026-08-28T00:00:00Z".to_string(),
                    chunk_index: Some(0),
                    importance: 0,
                },
            )
        };
        db.insert_drawer(&drawer("active", &active_path))
            .expect("insert active drawer");
        db.insert_drawer(&drawer("shared-one", &shared_path))
            .expect("insert first shared drawer");
        db.insert_drawer(&drawer("shared-two", &shared_path))
            .expect("insert second shared drawer");

        let outcome = prune_hook_payloads(temp.path(), &db_path, 1).expect("prune payloads");

        assert_eq!(outcome.scanned_files, 4);
        assert_eq!(outcome.deleted_files, 1);
        assert_eq!(outcome.referenced_files, 2);
        assert_eq!(outcome.young_files, 0);
        assert!(active_path.exists(), "active payload must survive");
        assert!(shared_path.exists(), "shared payload must survive");
        assert!(!rejected_path.exists(), "rejected payload must age out");
        assert!(
            payload_dir.join("not-a-hash.json").exists(),
            "unvalidated payload paths must not be deleted"
        );
    }

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

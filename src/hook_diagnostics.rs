use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 64 * 1024;
const TTL_SECS: u64 = 3600;
const LOG_FILENAME: &str = "hook-diagnostics.log";

pub enum HookOutcome {
    DaemonAccepted,
    FallbackPersisted { reason: String },
    Dropped { error: String, stage: String },
}

pub fn log_hook_failure(mempal_home: &Path, event: &str, outcome: &HookOutcome) {
    if matches!(outcome, HookOutcome::DaemonAccepted) {
        return;
    }
    let path = mempal_home.join(LOG_FILENAME);
    if let Err(error) = try_log(&path, event, outcome) {
        eprintln!(
            "hook diagnostics: failed to write {}: {error}",
            path.display()
        );
    }
}

fn try_log(path: &Path, event: &str, outcome: &HookOutcome) -> std::io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = match outcome {
        HookOutcome::DaemonAccepted => return Ok(()),
        HookOutcome::FallbackPersisted { reason } => {
            format!("{now} FALLBACK event={event} reason={reason}\n")
        }
        HookOutcome::Dropped { error, stage } => {
            format!("{now} DROPPED event={event} stage={stage} error={error}\n")
        }
    };

    let should_reset = should_reset_log(path);
    let mut file = if should_reset {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?
    } else {
        OpenOptions::new().create(true).append(true).open(path)?
    };
    file.write_all(line.as_bytes())?;
    Ok(())
}

fn should_reset_log(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() > MAX_LOG_BYTES {
        return true;
    }
    if let Ok(modified) = metadata.modified() {
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        if age.as_secs() > TTL_SECS {
            return true;
        }
    }
    false
}

pub fn diagnostic_log_path(mempal_home: &Path) -> PathBuf {
    mempal_home.join(LOG_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_success_does_not_write() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(tmp.path(), "PostToolUse", &HookOutcome::DaemonAccepted);
        assert!(!tmp.path().join(LOG_FILENAME).exists());
    }

    #[test]
    fn test_fallback_writes_entry() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(
            tmp.path(),
            "PostToolUse",
            &HookOutcome::FallbackPersisted {
                reason: "daemon not running".to_string(),
            },
        );
        let content = fs::read_to_string(tmp.path().join(LOG_FILENAME)).unwrap();
        assert!(content.contains("FALLBACK"));
        assert!(content.contains("PostToolUse"));
        assert!(content.contains("daemon not running"));
    }

    #[test]
    fn test_dropped_writes_entry() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(
            tmp.path(),
            "SessionEnd",
            &HookOutcome::Dropped {
                error: "disk full".to_string(),
                stage: "db_open".to_string(),
            },
        );
        let content = fs::read_to_string(tmp.path().join(LOG_FILENAME)).unwrap();
        assert!(content.contains("DROPPED"));
        assert!(content.contains("SessionEnd"));
        assert!(content.contains("disk full"));
        assert!(content.contains("db_open"));
    }

    #[test]
    fn test_stale_log_pruned() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOG_FILENAME);
        fs::write(&path, "old data\n").unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(0, 0)).unwrap();
        log_hook_failure(
            tmp.path(),
            "PostToolUse",
            &HookOutcome::FallbackPersisted {
                reason: "test".to_string(),
            },
        );
        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old data"));
        assert!(content.contains("FALLBACK"));
    }

    #[test]
    fn test_oversized_log_truncated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(LOG_FILENAME);
        let big = "x".repeat(MAX_LOG_BYTES as usize + 1);
        fs::write(&path, &big).unwrap();
        log_hook_failure(
            tmp.path(),
            "PostToolUse",
            &HookOutcome::FallbackPersisted {
                reason: "test".to_string(),
            },
        );
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.len() < MAX_LOG_BYTES as usize);
        assert!(content.contains("FALLBACK"));
    }
}

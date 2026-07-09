use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

const MAX_LOG_BYTES: u64 = 64 * 1024;
const TTL_SECS: u64 = 3600;
const LOG_FILENAME: &str = "hook-diagnostics.log";

pub enum HookOutcome {
    DaemonAccepted,
    FallbackPersisted {
        reason: String,
    },
    Dropped {
        error: String,
        stage: String,
    },
    Truncated {
        lower_bound_bytes: u64,
        inline_limit_bytes: u64,
    },
    Spooled {
        size_bytes: u64,
        inline_threshold_bytes: u64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookAdmissionStats {
    pub oversize_truncated_count: u64,
    pub oversize_dropped_count: u64,
    pub spooled_count: u64,
    pub inline_limit_bytes: u64,
    pub last_lower_bound_bytes: Option<u64>,
    pub last_error_class: Option<String>,
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
        HookOutcome::Truncated {
            lower_bound_bytes,
            inline_limit_bytes,
        } => {
            format!(
                "{now} TRUNCATED event={event} lower_bound_bytes={lower_bound_bytes} inline_limit_bytes={inline_limit_bytes}\n"
            )
        }
        HookOutcome::Spooled {
            size_bytes,
            inline_threshold_bytes,
        } => {
            format!(
                "{now} SPOOLED event={event} size_bytes={size_bytes} inline_threshold_bytes={inline_threshold_bytes}\n"
            )
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

pub fn hook_admission_stats(mempal_home: &Path, inline_limit_bytes: u64) -> HookAdmissionStats {
    let mut stats = HookAdmissionStats {
        inline_limit_bytes,
        ..HookAdmissionStats::default()
    };
    let Ok(content) = fs::read_to_string(diagnostic_log_path(mempal_home)) else {
        return stats;
    };

    for line in content.lines() {
        if line.contains(" TRUNCATED ") {
            stats.oversize_truncated_count = stats.oversize_truncated_count.saturating_add(1);
            stats.last_lower_bound_bytes = field_u64(line, "lower_bound_bytes=");
            stats.last_error_class = Some("oversize_truncated".to_string());
        } else if line.contains(" SPOOLED ") {
            stats.spooled_count = stats.spooled_count.saturating_add(1);
        } else if line.contains(" DROPPED ") {
            stats.last_error_class = field_str(line, "stage=")
                .map(|stage| format!("dropped:{stage}"))
                .or_else(|| Some("dropped".to_string()));
        }
    }

    stats
}

fn field_u64(line: &str, key: &str) -> Option<u64> {
    field_str(line, key)?.parse().ok()
}

fn field_str<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|part| part.strip_prefix(key))
        .filter(|value| !value.is_empty())
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
    fn test_truncated_writes_aggregate_only_entry() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(
            tmp.path(),
            "PostToolUse",
            &HookOutcome::Truncated {
                lower_bound_bytes: 10_485_761,
                inline_limit_bytes: 10_485_760,
            },
        );
        let content = fs::read_to_string(tmp.path().join(LOG_FILENAME)).unwrap();
        assert!(content.contains("TRUNCATED"));
        assert!(content.contains("lower_bound_bytes=10485761"));
        assert!(content.contains("inline_limit_bytes=10485760"));
        assert!(!content.contains("payload"));
        assert!(!content.contains("Authorization"));
    }

    #[test]
    fn test_hook_admission_stats_counts_truncated_without_content() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(
            tmp.path(),
            "UserPromptSubmit",
            &HookOutcome::Truncated {
                lower_bound_bytes: 10_485_761,
                inline_limit_bytes: 10_485_760,
            },
        );

        let stats = hook_admission_stats(tmp.path(), 10_485_760);

        assert_eq!(stats.oversize_truncated_count, 1);
        assert_eq!(stats.spooled_count, 0);
        assert_eq!(stats.inline_limit_bytes, 10_485_760);
        assert_eq!(stats.last_lower_bound_bytes, Some(10_485_761));
        assert_eq!(
            stats.last_error_class.as_deref(),
            Some("oversize_truncated")
        );
    }

    #[test]
    fn test_hook_admission_stats_counts_spooled_without_content() {
        let tmp = TempDir::new().unwrap();
        log_hook_failure(
            tmp.path(),
            "PostToolUse",
            &HookOutcome::Spooled {
                size_bytes: 131_072,
                inline_threshold_bytes: 65_536,
            },
        );
        let content = fs::read_to_string(tmp.path().join(LOG_FILENAME)).unwrap();
        assert!(content.contains("SPOOLED"));
        assert!(content.contains("size_bytes=131072"));
        assert!(content.contains("inline_threshold_bytes=65536"));
        assert!(!content.contains("payload"));
        assert!(!content.contains("Authorization"));

        let stats = hook_admission_stats(tmp.path(), 10_485_760);

        assert_eq!(stats.spooled_count, 1);
        assert_eq!(stats.oversize_truncated_count, 0);
        assert_eq!(stats.last_error_class, None);
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

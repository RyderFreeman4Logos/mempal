//! Codex session reader.

use crate::cowork::peek::{PeekError, PeekMessage, parse_rfc3339};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Read the first line of a Codex rollout jsonl and extract `payload.cwd`
/// from the `session_meta` entry. Returns `None` if the file can't be
/// opened or the first line isn't a valid `session_meta` with a `cwd` field.
pub fn read_session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let val: Value = serde_json::from_str(line.trim()).ok()?;
    if val.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return None;
    }
    val.get("payload")?
        .get("cwd")?
        .as_str()
        .map(|s| s.to_string())
}

/// Walk a Codex sessions base directory (e.g. `~/.codex/sessions`), find
/// all `rollout-*.jsonl` files whose `session_meta.payload.cwd` matches
/// `target_cwd`, and return the latest one by mtime.
pub fn find_latest_session_for_cwd(
    base: &Path,
    target_cwd: &str,
) -> Result<Option<(PathBuf, SystemTime)>, PeekError> {
    let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();

    for entry in WalkDir::new(base)
        .max_depth(6)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("rollout-") {
            continue;
        }

        if let Some(cwd) = read_session_cwd(path) {
            if cwd == target_cwd {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(mtime) = metadata.modified() {
                        candidates.push((path.to_path_buf(), mtime));
                    }
                }
            }
        }
    }

    Ok(candidates.into_iter().max_by_key(|(_, m)| *m))
}

/// Parse a Codex rollout jsonl. Returns `(messages, truncated)`.
///
/// Only `type: "response_item"` entries with `payload.type: "message"` and
/// `payload.role` in {user, assistant} are processed. Text is concatenated
/// from all blocks in `payload.content[]` that have a `text` field
/// (block types include `input_text`, `output_text`, etc.). Filters out
/// `reasoning` payloads and `event_msg` entries.
pub fn parse_codex_jsonl(
    path: &Path,
    since: Option<&str>,
    limit: usize,
) -> Result<(Vec<PeekMessage>, bool), PeekError> {
    // Pre-parse the `since` cutoff once; see claude.rs for rationale.
    let since_cutoff: Option<i64> = match since {
        Some(raw) => Some(parse_rfc3339(raw).ok_or_else(|| {
            PeekError::Parse(format!("invalid `since` RFC3339 timestamp: {raw}"))
        })?),
        None => None,
    };

    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut all: Vec<PeekMessage> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = val.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }

        let text = match payload.get("content") {
            Some(Value::Array(blocks)) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(String::from))
                    .collect();
                parts.join("\n")
            }
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }

        let at = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if let Some(cutoff) = since_cutoff {
            // Same safety rule as Claude adapter: unparseable msg timestamps
            // are kept rather than silently dropped.
            if let Some(msg_ts) = parse_rfc3339(&at) {
                if msg_ts <= cutoff {
                    continue;
                }
            }
        }

        all.push(PeekMessage {
            role: role.to_string(),
            at,
            text,
        });
    }

    let total = all.len();
    let truncated = total > limit;
    let start = total.saturating_sub(limit);
    let tail = all.split_off(start);
    Ok((tail, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reads_session_meta_cwd_from_first_line() {
        let fixture = Path::new(
            "tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl",
        );
        let cwd = read_session_cwd(fixture).expect("read cwd");
        assert_eq!(cwd, "/tmp/fake-project");
    }

    #[test]
    fn parses_codex_messages_filtering_event_and_reasoning() {
        let fixture = Path::new(
            "tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl",
        );
        let (messages, truncated) = parse_codex_jsonl(fixture, None, 30).expect("parse");
        // 8 lines total; 4 are valid response_item message entries
        // (2 user + 2 assistant), rest are session_meta / event_msg / reasoning.
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text, "codex: hello");
        assert_eq!(messages[1].text, "codex: hi back");
        assert_eq!(messages[2].text, "codex: continue");
        assert_eq!(messages[3].text, "codex: continuing");
        assert!(messages[0].at < messages[3].at);
        assert!(!truncated);
    }

    #[test]
    fn honors_limit_by_tail_and_sets_truncated_codex() {
        let fixture = Path::new(
            "tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl",
        );
        let (messages, truncated) = parse_codex_jsonl(fixture, None, 2).expect("parse");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "codex: continue");
        assert_eq!(messages[1].text, "codex: continuing");
        assert!(truncated);
    }

    #[test]
    fn walks_codex_dir_filtering_by_cwd() {
        let base = Path::new("tests/fixtures/cowork/codex");
        let result =
            find_latest_session_for_cwd(base, "/tmp/fake-project").expect("find session");
        assert!(result.is_some());
        let (path, _mtime) = result.unwrap();
        assert!(
            path.to_string_lossy()
                .contains("rollout-2026-04-13T12-00-00-fake.jsonl")
        );
    }

    #[test]
    fn walks_codex_dir_excludes_other_projects() {
        let base = Path::new("tests/fixtures/cowork/codex");
        let result =
            find_latest_session_for_cwd(base, "/tmp/fake-project").expect("find session");
        let path = result.unwrap().0;
        assert!(!path.to_string_lossy().contains("otherproject"));
    }
}

use serde_json::Value;

use crate::xurl::XurlResult;
use crate::xurl::model::{Provenance, RawTurn, Role, Tool};

const SESSION_ID_SCAN_LIMIT: usize = 64;

/// Parse a CC JSONL file into screen-visible turns.
///
/// `is_csa_delegated`: true when the file comes from a CSA sub-agent session
/// directory (`~/.local/state/cli-sub-agent/`), false for user-facing sessions
/// under `~/.claude/projects/`.
pub fn parse_cc_jsonl(
    content: &str,
    fallback_session_id: &str,
    fallback_project_path: Option<&str>,
    is_csa_delegated: bool,
) -> XurlResult<Vec<RawTurn>> {
    let session_id = extract_session_id(content).unwrap_or_else(|| fallback_session_id.to_string());
    let fallback_project_path = fallback_project_path.map(str::to_string);
    let mut last_cwd: Option<String> = None;
    let mut turns = Vec::new();
    let mut turn_index: u32 = 0;

    for raw_line in content.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let obj: Value = match serde_json::from_str(raw_line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let entry_type = match obj.get("type").and_then(Value::as_str) {
            Some(t) => t,
            None => continue,
        };

        if let Some(cwd) = extract_cwd(&obj) {
            last_cwd = Some(cwd);
        }
        let project_path = last_cwd.clone().or_else(|| fallback_project_path.clone());

        match entry_type {
            "user" => {
                let message = match obj.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let content_arr = match message.get("content").and_then(Value::as_array) {
                    Some(a) => a,
                    None => continue,
                };

                // Skip turns that consist entirely of tool_result blocks — these are
                // agent-internal feedback loops, not human-visible content.
                if content_arr
                    .iter()
                    .all(|c| c.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    continue;
                }

                // Collect text blocks only.
                let text = join_text_blocks(content_arr);
                if text.is_empty() {
                    continue;
                }

                // userType: "external" = human typed; absent or "internal" = agent-generated.
                let user_type = obj.get("userType").and_then(Value::as_str).unwrap_or("");
                let provenance = if user_type == "external" {
                    Provenance::Human
                } else {
                    Provenance::Agent
                };

                let timestamp_epoch = parse_timestamp(&obj);

                turns.push(RawTurn {
                    session_id: session_id.clone(),
                    tool: Tool::Cc,
                    role: Role::User,
                    content: text,
                    timestamp_epoch,
                    project_path,
                    git_branch: None,
                    is_csa_delegated,
                    provenance,
                    turn_index,
                });
                turn_index += 1;
            }
            "assistant" => {
                let message = match obj.get("message") {
                    Some(m) => m,
                    None => continue,
                };
                let content_arr = match message.get("content").and_then(Value::as_array) {
                    Some(a) => a,
                    None => continue,
                };

                // For assistant turns: only extract type:"text" blocks (skip tool_use, thinking).
                let text = join_text_blocks(content_arr);
                if text.is_empty() {
                    continue;
                }

                let timestamp_epoch = parse_timestamp(&obj);

                turns.push(RawTurn {
                    session_id: session_id.clone(),
                    tool: Tool::Cc,
                    role: Role::Assistant,
                    content: text,
                    timestamp_epoch,
                    project_path,
                    git_branch: None,
                    is_csa_delegated,
                    provenance: Provenance::Human,
                    turn_index,
                });
                turn_index += 1;
            }
            _ => continue,
        }
    }

    Ok(turns)
}

fn extract_cwd(obj: &Value) -> Option<String> {
    obj.get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn extract_session_id(content: &str) -> Option<String> {
    for raw_line in content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(SESSION_ID_SCAN_LIMIT)
    {
        let Ok(value) = serde_json::from_str::<Value>(raw_line) else {
            continue;
        };
        if let Some(sid) = value.get("sessionId").and_then(Value::as_str) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
}

/// Collect and join all `type:"text"` blocks from a content array.
fn join_text_blocks(content_arr: &[Value]) -> String {
    let parts: Vec<&str> = content_arr
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect();
    parts.join("\n").trim().to_string()
}

/// Parse `timestamp` field (ISO 8601 / RFC 3339) → Unix epoch f64.
/// Returns 0.0 on parse failure rather than propagating an error.
fn parse_timestamp(obj: &Value) -> f64 {
    obj.get("timestamp")
        .and_then(Value::as_str)
        .and_then(crate::cowork::peek::parse_rfc3339)
        .map(|secs| secs as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_user_line(session_id: &str, ts: &str, user_type: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": session_id,
            "userType": user_type,
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": text}]
            }
        })
        .to_string()
    }

    fn make_assistant_line(session_id: &str, ts: &str, blocks: &[(&str, &str)]) -> String {
        let content: Vec<_> = blocks
            .iter()
            .map(|(t, v)| serde_json::json!({"type": t, "text": v}))
            .collect();
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": session_id,
            "message": {
                "role": "assistant",
                "content": content
            }
        })
        .to_string()
    }

    fn make_tool_result_user(session_id: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "sessionId": session_id,
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
            }
        })
        .to_string()
    }

    /// Build a fixture with `user_text_count` user-text turns,
    /// `asst_text_count` assistant-text turns, and `tool_count` tool-result user turns.
    fn build_cc_fixture(
        user_text_count: usize,
        asst_text_count: usize,
        tool_count: usize,
    ) -> String {
        let mut lines = Vec::new();
        let ts = "2026-05-27T12:00:00Z";
        for i in 0..user_text_count {
            lines.push(make_user_line(
                "sess",
                ts,
                "external",
                &format!("user msg {i}"),
            ));
        }
        for i in 0..asst_text_count {
            lines.push(make_assistant_line(
                "sess",
                ts,
                &[("text", &format!("assistant msg {i}"))],
            ));
        }
        for _ in 0..tool_count {
            lines.push(make_tool_result_user("sess", ts));
        }
        lines.join("\n")
    }

    #[test]
    fn cc_parser_extracts_35_turns_from_50_entry_file() {
        // 20 user text + 15 assistant text + 15 tool_use/tool_result
        let jsonl = build_cc_fixture(20, 15, 15);
        let turns = parse_cc_jsonl(&jsonl, "sess123", None, false).unwrap();
        assert_eq!(turns.len(), 35);
        let user_count = turns.iter().filter(|t| t.role == Role::User).count();
        let asst_count = turns.iter().filter(|t| t.role == Role::Assistant).count();
        assert_eq!(user_count, 20);
        assert_eq!(asst_count, 15);
    }

    #[test]
    fn cc_parser_skips_non_text_content_blocks() {
        // assistant turn with [tool_use, text] blocks; only the text block should appear
        let line = serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-05-27T12:00:00Z",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "t1", "name": "Bash"},
                    {"type": "text", "text": "Here is my answer."},
                    {"type": "thinking", "text": "internal thought"}
                ]
            }
        })
        .to_string();
        let turns = parse_cc_jsonl(&line, "s1", None, false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "Here is my answer.");
    }

    #[test]
    fn cc_parser_normalizes_timestamp_to_epoch() {
        let line = make_user_line("s2", "2026-05-27T14:30:00Z", "external", "hi");
        let turns = parse_cc_jsonl(&line, "s2", None, false).unwrap();
        // 2026-05-27T14:30:00Z = 1779892200.0
        assert!((turns[0].timestamp_epoch - 1779892200.0).abs() < 1.0);
    }

    #[test]
    fn cc_parser_tool_result_only_user_turn_skipped() {
        let tool_result = make_tool_result_user("s3", "2026-05-27T12:00:00Z");
        let turns = parse_cc_jsonl(&tool_result, "s3", None, false).unwrap();
        assert_eq!(turns.len(), 0);
    }

    #[test]
    fn cc_parser_external_user_type_is_human_provenance() {
        let line = make_user_line("s4", "2026-05-27T12:00:00Z", "external", "hello");
        let turns = parse_cc_jsonl(&line, "s4", None, false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].provenance, Provenance::Human);
    }

    #[test]
    fn cc_parser_internal_user_type_is_agent_provenance() {
        let line = make_user_line("s5", "2026-05-27T12:00:00Z", "internal", "agent prompt");
        let turns = parse_cc_jsonl(&line, "s5", None, false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].provenance, Provenance::Agent);
    }

    #[test]
    fn cc_parser_absent_user_type_is_agent_provenance() {
        let line = serde_json::json!({
            "type": "user",
            "timestamp": "2026-05-27T12:00:00Z",
            "sessionId": "s6",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "no userType field"}]
            }
        })
        .to_string();
        let turns = parse_cc_jsonl(&line, "s6", None, false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].provenance, Provenance::Agent);
    }

    #[test]
    fn cc_parser_csa_delegated_flag_propagated() {
        let line = make_user_line("s7", "2026-05-27T12:00:00Z", "external", "hi");
        let turns = parse_cc_jsonl(&line, "s7", None, true).unwrap();
        assert!(turns[0].is_csa_delegated);

        let turns2 = parse_cc_jsonl(&line, "s7", None, false).unwrap();
        assert!(!turns2[0].is_csa_delegated);
    }

    #[test]
    fn cc_parser_s8_scenario_tool_result_skipped_text_kept() {
        // S8: Turn 0 external user text, Turn 1 assistant text, Turn 2 assistant tool_use only,
        // Turn 3 user tool_result only (skip), Turn 4 assistant text
        let ts = "2026-05-27T12:00:00Z";
        let t0 = make_user_line("s8", ts, "external", "sync upstream");
        let t1 = make_assistant_line("s8", ts, &[("text", "Starting upstream sync...")]);
        // assistant turn with only tool_use → no text → skipped
        let t2 = serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "sessionId": "s8",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "t1", "name": "csa"}]
            }
        })
        .to_string();
        let t3 = make_tool_result_user("s8", ts); // skip
        let t4 = make_assistant_line("s8", ts, &[("text", "Sync complete, PR #238 created")]);

        let jsonl = [t0, t1, t2, t3, t4].join("\n");
        let turns = parse_cc_jsonl(&jsonl, "s8", None, false).unwrap();
        // t0, t1, t4 → 3 turns (t2 has no text, t3 is tool_result-only user)
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].content, "sync upstream");
        assert_eq!(turns[0].provenance, Provenance::Human);
        assert_eq!(turns[1].content, "Starting upstream sync...");
        assert_eq!(turns[2].content, "Sync complete, PR #238 created");
    }

    #[test]
    fn cc_parser_propagates_cwd_with_last_seen_fallback() {
        let first = serde_json::json!({
            "type": "user",
            "timestamp": "2026-05-27T12:00:00Z",
            "sessionId": "sess",
            "userType": "external",
            "cwd": "/repo/one",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "first"}]
            }
        })
        .to_string();
        let second = make_assistant_line("sess", "2026-05-27T12:00:01Z", &[("text", "second")]);
        let fixture = format!("{first}\n{second}");

        let turns = parse_cc_jsonl(&fixture, "fallback", Some("/fallback"), false).unwrap();

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/one"));
        assert_eq!(turns[1].project_path.as_deref(), Some("/repo/one"));
    }

    #[test]
    fn cc_parser_uses_project_path_fallback_before_any_cwd() {
        let fixture = make_user_line("sess", "2026-05-27T12:00:00Z", "external", "first");

        let turns = parse_cc_jsonl(&fixture, "fallback", Some("/fallback"), false).unwrap();

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].project_path.as_deref(), Some("/fallback"));
    }
}

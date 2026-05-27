use serde_json::Value;

use crate::xurl::XurlResult;
use crate::xurl::model::{Provenance, RawTurn, Role, Tool};

/// Parse a Codex rollout JSONL file into screen-visible turns.
///
/// Format notes (from observed rollout-*.jsonl files):
/// - `session_meta` entry: `payload.id` = session ID
/// - `event_msg` + `payload.type == "user_message"` + `payload.message` = user turn
/// - `event_msg` + `payload.type == "agent_message"` + `payload.message` = assistant turn
/// - `response_item` + `payload.role == "assistant"` + `payload.content[].type == "output_text"` = final response
/// - All other types (tool_call, tool_output, token_count, turn_context, …) are skipped.
///
/// `is_csa_delegated`: true when the file is from a CSA-managed session.
pub fn parse_codex_jsonl(
    content: &str,
    fallback_session_id: &str,
    is_csa_delegated: bool,
) -> XurlResult<Vec<RawTurn>> {
    let mut session_id = fallback_session_id.to_string();
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

        match entry_type {
            "session_meta" => {
                // Extract session ID from payload.id.
                if let Some(id) = obj
                    .get("payload")
                    .and_then(|p| p.get("id"))
                    .and_then(Value::as_str)
                {
                    if !id.is_empty() {
                        session_id = id.to_string();
                    }
                }
            }

            "event_msg" => {
                let payload = match obj.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                let payload_type = match payload.get("type").and_then(Value::as_str) {
                    Some(t) => t,
                    None => continue,
                };
                let timestamp_epoch = parse_timestamp(&obj);

                match payload_type {
                    "user_message" => {
                        let text = payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if text.is_empty() {
                            continue;
                        }
                        turns.push(RawTurn {
                            session_id: session_id.clone(),
                            tool: Tool::Codex,
                            role: Role::User,
                            content: text,
                            timestamp_epoch,
                            project_path: None,
                            git_branch: None,
                            is_csa_delegated,
                            provenance: Provenance::Human,
                            turn_index,
                        });
                        turn_index += 1;
                    }
                    "agent_message" => {
                        let text = payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        if text.is_empty() {
                            continue;
                        }
                        turns.push(RawTurn {
                            session_id: session_id.clone(),
                            tool: Tool::Codex,
                            role: Role::Assistant,
                            content: text,
                            timestamp_epoch,
                            project_path: None,
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

            "response_item" => {
                let payload = match obj.get("payload") {
                    Some(p) => p,
                    None => continue,
                };
                let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");

                if role != "assistant" || payload_type != "message" {
                    continue;
                }

                // Extract content[].type == "output_text" blocks.
                let content_arr = match payload.get("content").and_then(Value::as_array) {
                    Some(a) => a,
                    None => continue,
                };
                let text: String = content_arr
                    .iter()
                    .filter(|c| c.get("type").and_then(Value::as_str) == Some("output_text"))
                    .filter_map(|c| c.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();

                if text.is_empty() {
                    continue;
                }

                let timestamp_epoch = parse_timestamp(&obj);
                turns.push(RawTurn {
                    session_id: session_id.clone(),
                    tool: Tool::Codex,
                    role: Role::Assistant,
                    content: text,
                    timestamp_epoch,
                    project_path: None,
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

    #[test]
    fn codex_parser_extracts_assistant_turns_from_response_items() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"sess42\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Hello world\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"How are you?\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "sess42", false).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::Assistant);
        assert_eq!(turns[0].content, "Hello world");
        assert_eq!(turns[1].role, Role::User);
        assert_eq!(turns[1].content, "How are you?");
    }

    #[test]
    fn codex_parser_skips_non_message_events() {
        // tool_call, tool_output events should not produce turns
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"tool_call\",\"tool_name\":\"bash\",\"input\":\"ls\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Done\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "s1", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "Done");
    }

    #[test]
    fn codex_parser_response_item_output_text() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s2\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"response_item\",\"payload\":{\"role\":\"assistant\",\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Final answer\"}]}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "s2", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, Role::Assistant);
        assert_eq!(turns[0].content, "Final answer");
    }

    #[test]
    fn codex_parser_session_id_from_session_meta() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"real-id-xyz\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "fallback", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_id, "real-id-xyz");
    }

    #[test]
    fn codex_parser_fallback_session_id_when_no_meta() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "my-fallback", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].session_id, "my-fallback");
    }

    #[test]
    fn codex_parser_csa_delegated_flag_propagated() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}",
            "\n",
        );
        let turns_csa = parse_codex_jsonl(jsonl, "s", true).unwrap();
        assert!(turns_csa[0].is_csa_delegated);

        let turns_user = parse_codex_jsonl(jsonl, "s", false).unwrap();
        assert!(!turns_user[0].is_csa_delegated);
    }

    #[test]
    fn codex_parser_skips_developer_and_system_response_items() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"s3\"}}",
            "\n",
            // developer role — should be skipped
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"response_item\",\"payload\":{\"role\":\"developer\",\"type\":\"message\",\"content\":[{\"type\":\"input_text\",\"text\":\"system instruction\"}]}}",
            "\n",
            // user role — should be skipped (not assistant)
            "{\"timestamp\":\"2026-05-27T12:00:02Z\",\"type\":\"response_item\",\"payload\":{\"role\":\"user\",\"type\":\"message\",\"content\":[{\"type\":\"input_text\",\"text\":\"user context\"}]}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:03Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"actual user message\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "s3", false).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "actual user message");
    }
}

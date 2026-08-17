use std::collections::HashSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::xurl::model::{Provenance, RawTurn, Role, Tool, TurnMetadata};
use crate::xurl::{XurlError, XurlResult};

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
    let mut project_path: Option<String> = None;
    let mut turns = Vec::new();
    let mut turn_index: u32 = 0;
    let mut source_ids = HashSet::new();
    let mut replaced_source_ids = HashSet::new();

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
                if let Some(cwd) = extract_session_cwd(&obj) {
                    project_path = Some(cwd);
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
                        push_turn(
                            &mut turns,
                            &mut source_ids,
                            &mut replaced_source_ids,
                            RawTurn {
                                session_id: session_id.clone(),
                                tool: Tool::Codex,
                                role: Role::User,
                                content: text,
                                timestamp_epoch,
                                project_path: project_path.clone(),
                                git_branch: None,
                                is_csa_delegated,
                                provenance: Provenance::Human,
                                turn_index,
                                metadata: TurnMetadata {
                                    message_id: Some(codex_source_id(&obj, payload, raw_line)),
                                    ..TurnMetadata::default()
                                },
                            },
                            replacement_target(payload)?,
                        )?;
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
                        push_turn(
                            &mut turns,
                            &mut source_ids,
                            &mut replaced_source_ids,
                            RawTurn {
                                session_id: session_id.clone(),
                                tool: Tool::Codex,
                                role: Role::Assistant,
                                content: text,
                                timestamp_epoch,
                                project_path: project_path.clone(),
                                git_branch: None,
                                is_csa_delegated,
                                provenance: Provenance::Human,
                                turn_index,
                                metadata: TurnMetadata {
                                    message_id: Some(codex_source_id(&obj, payload, raw_line)),
                                    ..TurnMetadata::default()
                                },
                            },
                            replacement_target(payload)?,
                        )?;
                        turn_index += 1;
                    }
                    "context_compacted" | "continuation_summary" => {
                        let text = continuation_text(payload).ok_or_else(|| {
                            XurlError::Parse(
                                "Codex continuation snapshot has no visible summary text"
                                    .to_string(),
                            )
                        })?;
                        push_turn(
                            &mut turns,
                            &mut source_ids,
                            &mut replaced_source_ids,
                            RawTurn {
                                session_id: session_id.clone(),
                                tool: Tool::Codex,
                                role: Role::Assistant,
                                content: text,
                                timestamp_epoch,
                                project_path: project_path.clone(),
                                git_branch: None,
                                is_csa_delegated,
                                provenance: Provenance::Human,
                                turn_index,
                                metadata: TurnMetadata {
                                    message_id: Some(codex_source_id(&obj, payload, raw_line)),
                                    ..TurnMetadata::default()
                                },
                            },
                            replacement_target(payload)?,
                        )?;
                        turn_index += 1;
                    }
                    _ => continue,
                }
            }

            "context_compacted" | "continuation_summary" => {
                let payload = obj.get("payload").ok_or_else(|| {
                    XurlError::Parse("Codex continuation snapshot has no payload".to_string())
                })?;
                let text = continuation_text(payload).ok_or_else(|| {
                    XurlError::Parse(
                        "Codex continuation snapshot has no visible summary text".to_string(),
                    )
                })?;
                push_turn(
                    &mut turns,
                    &mut source_ids,
                    &mut replaced_source_ids,
                    RawTurn {
                        session_id: session_id.clone(),
                        tool: Tool::Codex,
                        role: Role::Assistant,
                        content: text,
                        timestamp_epoch: parse_timestamp(&obj),
                        project_path: project_path.clone(),
                        git_branch: None,
                        is_csa_delegated,
                        provenance: Provenance::Human,
                        turn_index,
                        metadata: TurnMetadata {
                            message_id: Some(codex_source_id(&obj, payload, raw_line)),
                            ..TurnMetadata::default()
                        },
                    },
                    replacement_target(payload)?,
                )?;
                turn_index += 1;
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
                push_turn(
                    &mut turns,
                    &mut source_ids,
                    &mut replaced_source_ids,
                    RawTurn {
                        session_id: session_id.clone(),
                        tool: Tool::Codex,
                        role: Role::Assistant,
                        content: text,
                        timestamp_epoch,
                        project_path: project_path.clone(),
                        git_branch: None,
                        is_csa_delegated,
                        provenance: Provenance::Human,
                        turn_index,
                        metadata: TurnMetadata {
                            message_id: Some(codex_source_id(&obj, payload, raw_line)),
                            ..TurnMetadata::default()
                        },
                    },
                    replacement_target(payload)?,
                )?;
                turn_index += 1;
            }

            _ => continue,
        }
    }

    turns.retain(|turn| {
        !replaced_source_ids.contains(
            turn.metadata
                .message_id
                .as_deref()
                .expect("Codex source ID was assigned"),
        )
    });
    for (index, turn) in turns.iter_mut().enumerate() {
        turn.turn_index = index as u32;
    }

    for turn in &mut turns {
        turn.session_id = session_id.clone();
        if let Some(path) = &project_path {
            turn.project_path = Some(path.clone());
        }
    }

    Ok(turns)
}

fn continuation_text(payload: &Value) -> Option<String> {
    payload
        .get("summary")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("content"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn codex_source_id(obj: &Value, payload: &Value, raw_line: &str) -> String {
    payload
        .get("message_id")
        .or_else(|| payload.get("id"))
        .or_else(|| obj.get("message_id"))
        .or_else(|| obj.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("codex-line-{:x}", Sha256::digest(raw_line.as_bytes())))
}

fn replacement_target(payload: &Value) -> XurlResult<Option<String>> {
    let Some(value) = payload
        .get("replaces")
        .or_else(|| payload.get("supersedes"))
    else {
        return Ok(None);
    };
    value
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .map(Some)
        .ok_or_else(|| {
            XurlError::Parse(
                "Codex continuation snapshot has an ambiguous replacement identity".to_string(),
            )
        })
}

fn push_turn(
    turns: &mut Vec<RawTurn>,
    source_ids: &mut HashSet<String>,
    replaced_source_ids: &mut HashSet<String>,
    turn: RawTurn,
    replacement: Option<String>,
) -> XurlResult<()> {
    let source_id = turn
        .metadata
        .message_id
        .as_deref()
        .expect("Codex source ID was assigned");
    if !source_ids.insert(source_id.to_string()) {
        return Err(XurlError::Parse(
            "Codex continuation snapshot contains duplicate source identities".to_string(),
        ));
    }
    if let Some(replacement) = replacement {
        if replacement == source_id || !replaced_source_ids.insert(replacement) {
            return Err(XurlError::Parse(
                "Codex continuation snapshot has an ambiguous replacement identity".to_string(),
            ));
        }
    }
    turns.push(turn);
    Ok(())
}

pub(crate) fn extract_session_cwd(obj: &Value) -> Option<String> {
    obj.get("payload")
        .and_then(|p| p.get("cwd"))
        .or_else(|| obj.get("cwd"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
    fn codex_parser_applies_session_meta_cwd_to_all_turns() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-05-27T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hi\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"real-id-xyz\",\"cwd\":\"/repo/codex\"}}",
            "\n",
            "{\"timestamp\":\"2026-05-27T12:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"message\":\"hello\"}}",
            "\n",
        );
        let turns = parse_codex_jsonl(jsonl, "fallback", false).unwrap();

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].project_path.as_deref(), Some("/repo/codex"));
        assert_eq!(turns[1].project_path.as_deref(), Some("/repo/codex"));
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
    fn codex_parser_keeps_only_canonical_compacted_continuation_turns() {
        let jsonl = concat!(
            "{\"timestamp\":\"2026-08-17T12:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-session\",\"cwd\":\"/project/one\"}}\n",
            "{\"timestamp\":\"2026-08-17T12:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"id\":\"original\",\"message\":\"obsolete answer\"}}\n",
            "{\"timestamp\":\"2026-08-17T12:00:02Z\",\"type\":\"context_compacted\",\"payload\":{\"id\":\"compact\",\"summary\":\"continuation summary\"}}\n",
            "{\"timestamp\":\"2026-08-17T12:00:03Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"agent_message\",\"id\":\"rewritten\",\"replaces\":\"original\",\"message\":\"canonical answer\"}}\n",
        );

        let turns = parse_codex_jsonl(jsonl, "fallback", false).unwrap();

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].content, "continuation summary");
        assert_eq!(turns[0].metadata.message_id.as_deref(), Some("compact"));
        assert_eq!(turns[1].content, "canonical answer");
        assert_eq!(turns[1].metadata.message_id.as_deref(), Some("rewritten"));
    }

    #[test]
    fn codex_parser_rejects_ambiguous_continuation_snapshot() {
        let jsonl = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-session\"}}\n",
            "{\"type\":\"context_compacted\",\"payload\":{\"id\":\"compact\"}}\n",
        );

        assert!(parse_codex_jsonl(jsonl, "fallback", false).is_err());
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

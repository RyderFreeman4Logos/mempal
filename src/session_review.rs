use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use crate::core::config::HooksSessionEndConfig;

const SESSION_METADATA_SENTINEL: &str = "\n\n--- session_metadata ---\n";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetadata {
    pub session_id: Option<String>,
    pub linked_drawer_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionReviewRecord {
    pub wing: String,
    pub room: String,
    pub source_file: String,
    pub raw_content: String,
    pub content: String,
    pub importance: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReviewSkipReason {
    Disabled,
    MissingPayload,
    NoAssistantMessage,
    TooShort { chars: usize, min_length: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionReviewOutcome {
    Review(SessionReviewRecord),
    Skipped(SessionReviewSkipReason),
}

#[derive(Debug, Deserialize)]
struct SessionEndPayload {
    session_id: String,
    #[serde(default)]
    agent: Option<String>,
    messages: Vec<SessionMessage>,
    #[serde(default)]
    tool_calls: Vec<SessionToolCall>,
}

#[derive(Debug, Deserialize)]
struct SessionMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct SessionToolCall {
    drawer_id: String,
}

pub fn extract_session_review(
    payload: Option<&str>,
    envelope_agent: &str,
    config: &HooksSessionEndConfig,
) -> Result<SessionReviewOutcome> {
    if !config.extract_self_review {
        return Ok(SessionReviewOutcome::Skipped(
            SessionReviewSkipReason::Disabled,
        ));
    }

    let Some(payload) = payload else {
        return Ok(SessionReviewOutcome::Skipped(
            SessionReviewSkipReason::MissingPayload,
        ));
    };

    // TODO(specs/fork-ext/p9-session-self-review.spec.md:27,49-53):
    // The spec mixes two concerns: extraction comes from payload.messages,
    // while sentinel+rfind validation applies only to the stored metadata
    // suffix. This implementation keeps extraction on payload.messages and
    // uses the sentinel exclusively for the appended metadata block.
    let payload: SessionEndPayload =
        serde_json::from_str(payload).context("failed to parse session_end payload")?;
    let session_id = payload.session_id.trim();
    if session_id.is_empty() {
        return Err(anyhow!("session_end payload missing non-empty session_id"));
    }

    let assistant_messages = payload
        .messages
        .iter()
        .rev()
        .filter(|message| message.role == "assistant")
        .take(config.trailing_messages)
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>();
    if assistant_messages.is_empty() {
        return Ok(SessionReviewOutcome::Skipped(
            SessionReviewSkipReason::NoAssistantMessage,
        ));
    }

    let raw_content = assistant_messages
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n---\n");
    let char_count = raw_content.chars().count();
    if char_count < config.min_length {
        return Ok(SessionReviewOutcome::Skipped(
            SessionReviewSkipReason::TooShort {
                chars: char_count,
                min_length: config.min_length,
            },
        ));
    }

    let room = payload
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
        .or_else(|| {
            let agent = envelope_agent.trim();
            (!agent.is_empty()).then_some(agent)
        })
        .unwrap_or("unknown-agent")
        .to_string();
    let linked_drawer_ids = payload
        .tool_calls
        .into_iter()
        .map(|call| call.drawer_id)
        .collect::<Vec<_>>();
    let content = append_session_metadata(
        &raw_content,
        &SessionMetadata {
            session_id: Some(session_id.to_string()),
            linked_drawer_ids,
        },
    );

    Ok(SessionReviewOutcome::Review(SessionReviewRecord {
        wing: config.wing.clone(),
        room,
        source_file: session_id.to_string(),
        raw_content,
        content,
        importance: 3,
    }))
}

pub fn split_session_metadata(content: &str) -> (&str, SessionMetadata) {
    let Some(index) = content.rfind(SESSION_METADATA_SENTINEL) else {
        return (content, SessionMetadata::default());
    };

    let metadata_start = index + SESSION_METADATA_SENTINEL.len();
    let Some(metadata) = parse_metadata_block(&content[metadata_start..]) else {
        return (content, SessionMetadata::default());
    };

    (&content[..index], metadata)
}

fn append_session_metadata(content: &str, metadata: &SessionMetadata) -> String {
    let mut lines = Vec::new();
    if let Some(session_id) = metadata
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("session_id: {session_id}"));
    }
    if !metadata.linked_drawer_ids.is_empty() {
        lines.push(format!(
            "linked_drawer_ids: {}",
            metadata.linked_drawer_ids.join(", ")
        ));
    }
    if lines.is_empty() {
        return content.to_string();
    }

    format!("{content}{SESSION_METADATA_SENTINEL}{}", lines.join("\n"))
}

fn parse_metadata_block(block: &str) -> Option<SessionMetadata> {
    let mut metadata = SessionMetadata::default();
    let mut saw_key_value = false;

    for line in block.lines() {
        let (key, value) = line.split_once(": ")?;
        if key.is_empty()
            || value.is_empty()
            || !key
                .chars()
                .all(|char| char.is_ascii_lowercase() || char == '_')
        {
            return None;
        }
        saw_key_value = true;
        match key {
            "session_id" => metadata.session_id = Some(value.to_string()),
            "linked_drawer_ids" => {
                metadata.linked_drawer_ids = value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect();
            }
            _ => {}
        }
    }

    saw_key_value.then_some(metadata)
}

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

use crate::core::config::HooksSessionEndConfig;

const SESSION_METADATA_SENTINEL: &str = "\n\n<!-- mempal:session-review -->\n";
const LEGACY_SESSION_METADATA_SENTINEL: &str = "\n\n--- session_metadata ---\n";
const HOOKS_RAW_METADATA_PREFIX: &str = "<!-- mempal:hooks-raw -->\n";
const HOOKS_RAW_METADATA_SUFFIX: &str = "<!-- /mempal:hooks-raw -->\n";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionMetadata {
    pub session_id: Option<String>,
    pub linked_drawer_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksRawMetadata {
    pub session_id: Option<String>,
    pub captured_at: Option<String>,
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
            linked_drawer_ids,
            session_id: Some(session_id.to_string()),
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
    let Some((index, sentinel)) = latest_metadata_sentinel(content) else {
        return (content, SessionMetadata::default());
    };

    let metadata_start = index + sentinel.len();
    let Some(metadata) = parse_metadata_block(&content[metadata_start..]) else {
        return (content, SessionMetadata::default());
    };

    (&content[..index], metadata)
}

pub fn analysis_content(content: &str) -> &str {
    split_session_metadata(content).0
}

pub fn append_hooks_raw_metadata(
    content: &str,
    session_id: Option<&str>,
    captured_at: Option<&str>,
) -> String {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return content.to_string();
    };

    let mut lines = vec![format!("session_id: {:?}", session_id)];
    if let Some(captured_at) = captured_at.map(str::trim).filter(|value| !value.is_empty()) {
        lines.push(format!("captured_at: {:?}", captured_at));
    }

    format!(
        "{HOOKS_RAW_METADATA_PREFIX}{}\n{HOOKS_RAW_METADATA_SUFFIX}{content}",
        lines.join("\n")
    )
}

pub fn split_hooks_raw_metadata(content: &str) -> (&str, HooksRawMetadata) {
    if !content.starts_with(HOOKS_RAW_METADATA_PREFIX) {
        return (content, HooksRawMetadata::default());
    }

    let metadata_and_body = &content[HOOKS_RAW_METADATA_PREFIX.len()..];
    let Some(end_index) = metadata_and_body.find(HOOKS_RAW_METADATA_SUFFIX) else {
        return (content, HooksRawMetadata::default());
    };

    let metadata_block = &metadata_and_body[..end_index];
    let Some(metadata) = parse_hooks_raw_metadata_block(metadata_block) else {
        return (content, HooksRawMetadata::default());
    };
    let body_start = HOOKS_RAW_METADATA_PREFIX.len() + end_index + HOOKS_RAW_METADATA_SUFFIX.len();
    (&content[body_start..], metadata)
}

pub fn hooks_raw_content(content: &str) -> &str {
    split_hooks_raw_metadata(content).0
}

pub fn validate_linked_drawer_ids(
    conn: &Connection,
    session_id: &str,
    project_id: Option<&str>,
    linked_drawer_ids: &[String],
) -> Result<()> {
    if linked_drawer_ids.is_empty() {
        return Ok(());
    }

    let mut violations = 0usize;
    for drawer_id in linked_drawer_ids {
        let row = conn
            .query_row(
                r#"
                SELECT wing, room, content, source_file, project_id, source_type
                FROM drawers
                WHERE id = ?1 AND deleted_at IS NULL
                "#,
                [drawer_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()
            .with_context(|| format!("failed to validate linked drawer {drawer_id}"))?;

        let Some((wing, _room, content, _source_file, linked_project_id, source_type)) = row else {
            violations += 1;
            continue;
        };

        let linked_session_id = linked_session_id(&wing, &source_type, &content);
        let same_session = linked_session_id.as_deref() == Some(session_id);
        let same_project = linked_project_id.as_deref() == project_id;
        if !same_session || !same_project {
            violations += 1;
        }
    }

    if violations > 0 {
        bail!(
            "linked_drawer_ids validation failed: {violations} id(s) crossed session/project boundary for session {session_id}"
        );
    }

    Ok(())
}

fn append_session_metadata(content: &str, metadata: &SessionMetadata) -> String {
    let mut lines = Vec::new();
    if !metadata.linked_drawer_ids.is_empty() {
        lines.push(format!(
            "linked_drawer_ids: {}",
            serde_json::to_string(&metadata.linked_drawer_ids)
                .expect("session-review linked_drawer_ids should serialize")
        ));
    }
    if let Some(session_id) = metadata
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("session_id: {:?}", session_id));
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
            "session_id" => metadata.session_id = Some(unquote(value)),
            "linked_drawer_ids" => {
                metadata.linked_drawer_ids = parse_linked_drawer_ids(value)?;
            }
            _ => {}
        }
    }

    saw_key_value.then_some(metadata)
}

fn parse_hooks_raw_metadata_block(block: &str) -> Option<HooksRawMetadata> {
    let mut metadata = HooksRawMetadata::default();
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
            "session_id" => metadata.session_id = Some(unquote(value)),
            "captured_at" => metadata.captured_at = Some(unquote(value)),
            _ => {}
        }
    }

    saw_key_value.then_some(metadata)
}

fn latest_metadata_sentinel(content: &str) -> Option<(usize, &'static str)> {
    let current = content.rfind(SESSION_METADATA_SENTINEL);
    let legacy = content.rfind(LEGACY_SESSION_METADATA_SENTINEL);
    match (current, legacy) {
        (Some(index), Some(legacy_index)) if legacy_index > index => {
            Some((legacy_index, LEGACY_SESSION_METADATA_SENTINEL))
        }
        (Some(index), _) => Some((index, SESSION_METADATA_SENTINEL)),
        (None, Some(index)) => Some((index, LEGACY_SESSION_METADATA_SENTINEL)),
        (None, None) => None,
    }
}

fn parse_linked_drawer_ids(value: &str) -> Option<Vec<String>> {
    if let Ok(ids) = serde_json::from_str::<Vec<String>>(value) {
        return Some(ids);
    }
    Some(
        value
            .split(',')
            .map(str::trim)
            .map(unquote)
            .filter(|value| !value.is_empty())
            .collect(),
    )
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn linked_session_id(wing: &str, source_type: &str, content: &str) -> Option<String> {
    let metadata = split_session_metadata(content).1;
    if let Some(session_id) = metadata
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(session_id.to_string());
    }

    if wing != "hooks-raw" {
        return None;
    }

    if source_type != "conversation" {
        return None;
    }

    // Round-4 security fix: hooks-raw linkage must trust only daemon-persisted
    // metadata already stored in the drawer, never attacker-chosen disk paths.
    split_hooks_raw_metadata(content)
        .1
        .session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

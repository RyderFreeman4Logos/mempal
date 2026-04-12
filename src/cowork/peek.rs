//! Peek request/response types + orchestration.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Which agent tool's session to peek.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Auto,
}

impl Tool {
    /// Case-insensitive parse from a string; used for ClientInfo.name matching.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" => Some(Tool::Claude),
            "codex" | "codex-cli" | "codex_cli" | "codex-tui" => Some(Tool::Codex),
            "auto" => Some(Tool::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Auto => "auto",
        }
    }
}

/// Peek request — parameters to `peek_partner`.
#[derive(Debug, Clone)]
pub struct PeekRequest {
    pub tool: Tool,
    /// Max messages to return (default 30).
    pub limit: usize,
    /// Optional RFC3339 cutoff; only messages newer than this are returned.
    pub since: Option<String>,
    /// Absolute cwd of the caller (injected by orchestrator; not user-facing).
    pub cwd: PathBuf,
    /// The tool that the CALLER is; used to reject self-peek.
    /// `None` means unknown (ClientInfo missing); auto mode will then error.
    pub caller_tool: Option<Tool>,
    /// HOME override for tests. None = use $HOME env var.
    pub home_override: Option<PathBuf>,
}

/// A single message from a session log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekMessage {
    /// "user" or "assistant".
    pub role: String,
    /// RFC3339 timestamp of this message.
    pub at: String,
    /// Plain text content; tool-use internals are filtered out.
    pub text: String,
}

/// Peek response — what `peek_partner` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekResponse {
    pub partner_tool: Tool,
    pub session_path: Option<String>,
    pub session_mtime: Option<String>,
    pub partner_active: bool,
    pub messages: Vec<PeekMessage>,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum PeekError {
    #[error(
        "cannot infer partner; pass `tool` explicitly (client_info.name was missing or unrecognized)"
    )]
    CannotInferPartner,

    #[error("cannot peek your own session")]
    SelfPeek,

    #[error("I/O error reading session: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse session file: {0}")]
    Parse(String),
}

/// Orchestrator — implemented in Task 5.
pub fn peek_partner(request: PeekRequest) -> Result<PeekResponse, PeekError> {
    let _ = request;
    unimplemented!("peek_partner dispatch — Task 5")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_parses_from_str() {
        assert_eq!(Tool::from_str_ci("claude"), Some(Tool::Claude));
        assert_eq!(Tool::from_str_ci("Codex"), Some(Tool::Codex));
        assert_eq!(Tool::from_str_ci("AUTO"), Some(Tool::Auto));
        assert_eq!(Tool::from_str_ci("other"), None);
    }

    #[test]
    fn tool_parses_compound_names() {
        assert_eq!(Tool::from_str_ci("claude-code"), Some(Tool::Claude));
        assert_eq!(Tool::from_str_ci("codex-cli"), Some(Tool::Codex));
        assert_eq!(Tool::from_str_ci("codex-tui"), Some(Tool::Codex));
    }

    #[test]
    fn peek_response_serializes_with_snake_case_fields() {
        let resp = PeekResponse {
            partner_tool: Tool::Codex,
            session_path: Some("/tmp/x.jsonl".into()),
            session_mtime: Some("2026-04-13T12:00:00Z".into()),
            partner_active: true,
            messages: vec![],
            truncated: false,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("partner_tool"));
        assert!(json.contains("session_path"));
        assert!(json.contains("partner_active"));
        assert!(json.contains(r#""partner_tool":"codex""#));
    }
}

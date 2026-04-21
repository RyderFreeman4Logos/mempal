use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::core::{
    config::{Config, default_config_path},
    db::Database,
    queue::PendingMessageStore,
    utils::current_timestamp,
};
use anyhow::{Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::hook_install::{self, HookInstallTarget};

const MAX_INLINE_PAYLOAD_BYTES: usize = 10 * 1024 * 1024;
const PREVIEW_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Subcommand)]
pub enum HookCommands {
    #[command(name = "PostToolUse", alias = "hook_post_tool")]
    PostToolUse,
    #[command(name = "UserPromptSubmit", alias = "hook_user_prompt")]
    UserPromptSubmit,
    #[command(name = "SessionStart", alias = "hook_session_start")]
    SessionStart,
    #[command(name = "SessionEnd", alias = "hook_session_end")]
    SessionEnd,
    Install {
        #[arg(long, value_enum)]
        target: HookInstallTarget,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        uninstall: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookEvent {
    PostToolUse,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedHookEnvelope {
    pub event: String,
    pub kind: String,
    pub agent: String,
    pub captured_at: String,
    pub claude_cwd: String,
    pub payload: Option<String>,
    pub payload_path: Option<String>,
    pub payload_preview: Option<String>,
    pub original_size_bytes: usize,
    #[serde(default)]
    pub truncated: bool,
}

pub fn run_command(command: HookCommands) -> Result<()> {
    match command {
        HookCommands::PostToolUse => enqueue_from_stdin(HookEvent::PostToolUse),
        HookCommands::UserPromptSubmit => enqueue_from_stdin(HookEvent::UserPromptSubmit),
        HookCommands::SessionStart => enqueue_from_stdin(HookEvent::SessionStart),
        HookCommands::SessionEnd => enqueue_from_stdin(HookEvent::SessionEnd),
        HookCommands::Install {
            target,
            dry_run,
            uninstall,
        } => hook_install::install(target, dry_run, uninstall),
    }
}

pub fn enqueue_from_stdin(event: HookEvent) -> Result<()> {
    let config = Config::load_from(&default_config_path()).context("failed to load config")?;
    let db_path = expand_home_path(&config.db_path);
    let db = Database::open(&db_path).context("failed to open database for hook enqueue")?;
    let store = PendingMessageStore::new(db.path()).context("failed to open pending queue")?;
    let mempal_home = mempal_home_from_db(db.path());

    let captured = capture_stdin_payload(stdin_bytes()?, &mempal_home)?;
    let envelope = CapturedHookEnvelope {
        event: event.display_name().to_string(),
        kind: event.queue_kind().to_string(),
        agent: infer_agent_name(
            captured
                .inline_payload
                .as_deref()
                .or(captured.preview.as_deref()),
        ),
        captured_at: current_timestamp(),
        claude_cwd: current_working_directory(),
        payload: captured.inline_payload,
        payload_path: captured
            .payload_path
            .map(|path| path.to_string_lossy().to_string()),
        payload_preview: captured.preview,
        original_size_bytes: captured.original_size_bytes,
        truncated: captured.truncated,
    };

    if envelope.truncated {
        eprintln!(
            "payload envelope-wrapped for {} ({} bytes)",
            envelope.event, envelope.original_size_bytes
        );
    }

    let payload =
        serde_json::to_string(&envelope).context("failed to serialize hook capture envelope")?;
    store
        .enqueue(event.queue_kind(), &payload)
        .context("failed to enqueue hook payload")?;
    Ok(())
}

fn stdin_bytes() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    io::stdin()
        .read_to_end(&mut buf)
        .context("failed to read hook stdin payload")?;
    Ok(buf)
}

#[derive(Debug)]
struct CapturedPayload {
    inline_payload: Option<String>,
    payload_path: Option<PathBuf>,
    preview: Option<String>,
    original_size_bytes: usize,
    truncated: bool,
}

fn capture_stdin_payload(bytes: Vec<u8>, mempal_home: &Path) -> Result<CapturedPayload> {
    let original_size_bytes = bytes.len();
    if original_size_bytes <= MAX_INLINE_PAYLOAD_BYTES {
        return Ok(CapturedPayload {
            inline_payload: Some(decode_stdin_bytes(&bytes)),
            payload_path: None,
            preview: None,
            original_size_bytes,
            truncated: false,
        });
    }

    let oversize_dir = mempal_home.join("hook-oversize");
    fs::create_dir_all(&oversize_dir)
        .with_context(|| format!("failed to create {}", oversize_dir.display()))?;

    let digest = blake3::hash(&bytes).to_hex().to_string();
    let final_path = oversize_dir.join(format!("{digest}.json"));
    let tmp_path = oversize_dir.join(format!("{digest}.tmp"));
    let mut file = File::create(&tmp_path)
        .with_context(|| format!("failed to create {}", tmp_path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("failed to finalize {}", final_path.display()))?;

    Ok(CapturedPayload {
        inline_payload: None,
        payload_path: Some(final_path),
        preview: Some(safe_preview(&bytes)),
        original_size_bytes,
        truncated: true,
    })
}

fn safe_preview(bytes: &[u8]) -> String {
    let preview_bytes = &bytes[..bytes.len().min(PREVIEW_MAX_BYTES)];
    let lossy = String::from_utf8_lossy(preview_bytes);
    truncate_to_byte_boundary(lossy.as_ref(), PREVIEW_MAX_BYTES).to_string()
}

fn truncate_to_byte_boundary(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }

    let mut index = max_bytes;
    while !input.is_char_boundary(index) {
        index -= 1;
    }
    &input[..index]
}

fn decode_stdin_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(value) => value.to_owned(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn infer_agent_name(payload: Option<&str>) -> String {
    let Some(payload) = payload else {
        return "claude".to_string();
    };

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
        for field in ["agent", "originator", "model"] {
            if let Some(name) = value.get(field).and_then(|value| value.as_str())
                && let Some(inferred) = classify_agent_name(name)
            {
                return inferred.to_string();
            }
        }
    }

    if let Some(inferred) = classify_agent_name(payload) {
        return inferred.to_string();
    }

    "claude".to_string()
}

fn classify_agent_name(value: &str) -> Option<&'static str> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("codex") {
        return Some("codex");
    }
    if lower.contains("gemini") {
        return Some("gemini");
    }
    if lower.contains("claude") {
        return Some("claude");
    }
    None
}

fn current_working_directory() -> String {
    env::var("CLAUDE_PROJECT_CWD")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| ".".to_string())
}

fn mempal_home_from_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

impl HookEvent {
    pub fn queue_kind(self) -> &'static str {
        match self {
            HookEvent::PostToolUse => "hook_post_tool",
            HookEvent::UserPromptSubmit => "hook_user_prompt",
            HookEvent::SessionStart => "hook_session_start",
            HookEvent::SessionEnd => "hook_session_end",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
        }
    }
}

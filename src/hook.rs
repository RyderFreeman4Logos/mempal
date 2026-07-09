use std::env;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::core::{
    config::{Config, ConfigHandle, TurnStorageMode, default_config_path},
    db::Database,
    queue::PendingMessageStore,
    strata::is_raw_turn,
    utils::current_timestamp,
};
use anyhow::{Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::hook_install::{self, HookInstallTarget};

/// Maximum hook stdin payload size admitted inline into the hook envelope.
///
/// The hook process reads at most one byte past this limit so oversized streams
/// can be detected without draining or buffering unbounded stdin.
pub const MAX_INLINE_PAYLOAD_BYTES: usize = 10 * 1024 * 1024;
const MAX_STDIN_READ_BYTES: usize = MAX_INLINE_PAYLOAD_BYTES + 1;

#[derive(Debug, Clone, Subcommand)]
pub enum HookCommands {
    /// Capture a PostToolUse hook payload.
    #[command(name = "PostToolUse", alias = "hook_post_tool")]
    PostToolUse,
    /// Capture a UserPromptSubmit hook payload.
    #[command(name = "UserPromptSubmit", alias = "hook_user_prompt")]
    UserPromptSubmit,
    /// Capture a SessionStart hook payload.
    #[command(name = "SessionStart", alias = "hook_session_start")]
    SessionStart,
    /// Capture a SessionEnd hook payload.
    #[command(name = "SessionEnd", alias = "hook_session_end")]
    SessionEnd,
    /// Install or uninstall passive capture hooks.
    Install {
        #[arg(long, value_enum)]
        target: HookInstallTarget,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        uninstall: bool,
        /// Skip ensuring `~/.mcp.json` registers the mempal MCP server.
        /// By default the install also wires user-level MCP so projects
        /// without their own `.mcp.json` still expose mempal_* tools.
        #[arg(long, default_value_t = false)]
        skip_mcp: bool,
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
        HookCommands::PostToolUse => run_capture_command(HookEvent::PostToolUse),
        HookCommands::UserPromptSubmit => run_capture_command(HookEvent::UserPromptSubmit),
        HookCommands::SessionStart => run_capture_command(HookEvent::SessionStart),
        HookCommands::SessionEnd => run_capture_command(HookEvent::SessionEnd),
        HookCommands::Install {
            target,
            dry_run,
            uninstall,
            skip_mcp,
        } => hook_install::install(target, dry_run, uninstall, skip_mcp),
    }
}

fn run_capture_command(event: HookEvent) -> Result<()> {
    ConfigHandle::bootstrap_quiet(default_config_path()).context("failed to bootstrap config")?;
    enqueue_from_stdin(event)
}

pub fn enqueue_from_stdin(event: HookEvent) -> Result<()> {
    let config = ConfigHandle::current();
    let stdin = stdin_bytes()?;
    if should_drop_hook_capture(event, &stdin, config.as_ref()) {
        return Ok(());
    }

    let db_path = expand_home_path(&config.db_path);
    let mempal_home = mempal_home_from_db(&db_path);
    let event_name = event.display_name();

    let captured = capture_stdin_payload(stdin, &mempal_home)?;
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
        let size_label = if captured.original_size_is_lower_bound {
            format!(">= {} bytes", MAX_INLINE_PAYLOAD_BYTES)
        } else {
            format!("{} bytes", envelope.original_size_bytes)
        };
        eprintln!(
            "hook payload exceeded inline limit for {}; raw body omitted ({})",
            envelope.event, size_label
        );
        crate::hook_diagnostics::log_hook_failure(
            &mempal_home,
            event_name,
            &crate::hook_diagnostics::HookOutcome::Truncated {
                lower_bound_bytes: envelope.original_size_bytes as u64,
                inline_limit_bytes: MAX_INLINE_PAYLOAD_BYTES as u64,
            },
        );
    }

    let payload =
        serde_json::to_string(&envelope).context("failed to serialize hook capture envelope")?;
    let fallback = match try_enqueue_via_daemon(&mempal_home, event.queue_kind(), &payload) {
        DaemonEnqueueOutcome::Accepted => return Ok(()),
        DaemonEnqueueOutcome::Fallback(fallback) => fallback,
    };

    crate::hook_diagnostics::log_hook_failure(
        &mempal_home,
        event_name,
        &crate::hook_diagnostics::HookOutcome::FallbackPersisted {
            reason: fallback.reason().to_string(),
        },
    );

    let database = match Database::open(&db_path) {
        Ok(database) => database,
        Err(error) => {
            crate::hook_diagnostics::log_hook_failure(
                &mempal_home,
                event_name,
                &crate::hook_diagnostics::HookOutcome::Dropped {
                    error: format!("{error:#}"),
                    stage: "db_init".to_string(),
                },
            );
            return Err(error).context("failed to initialize pending queue database");
        }
    };
    let store = match PendingMessageStore::new(&db_path) {
        Ok(store) => store,
        Err(error) => {
            crate::hook_diagnostics::log_hook_failure(
                &mempal_home,
                event_name,
                &crate::hook_diagnostics::HookOutcome::Dropped {
                    error: format!("{error:#}"),
                    stage: "queue_init".to_string(),
                },
            );
            return Err(error).context("failed to open pending queue");
        }
    };
    drop(database);
    let enqueue_result = match fallback.identity() {
        FallbackEnqueueIdentity::Fresh => store.enqueue(event.queue_kind(), &payload),
        FallbackEnqueueIdentity::Idempotent { key } => {
            store.enqueue_idempotent_with_key(event.queue_kind(), &payload, key)
        }
    };
    if let Err(error) = &enqueue_result {
        crate::hook_diagnostics::log_hook_failure(
            &mempal_home,
            event_name,
            &crate::hook_diagnostics::HookOutcome::Dropped {
                error: format!("{error:#}"),
                stage: "enqueue".to_string(),
            },
        );
    }
    enqueue_result
        .with_context(|| format!("failed to enqueue hook payload after {}", fallback.reason()))?;
    Ok(())
}

enum DaemonEnqueueOutcome {
    Accepted,
    Fallback(DaemonFallback),
}

enum DaemonFallback {
    Fresh(String),
    Idempotent { reason: String, key: String },
}

enum FallbackEnqueueIdentity<'a> {
    Fresh,
    Idempotent { key: &'a str },
}

impl DaemonFallback {
    fn reason(&self) -> &str {
        match self {
            Self::Fresh(reason) | Self::Idempotent { reason, .. } => reason,
        }
    }

    fn identity(&self) -> FallbackEnqueueIdentity<'_> {
        match self {
            Self::Fresh(_) => FallbackEnqueueIdentity::Fresh,
            Self::Idempotent { key, .. } => FallbackEnqueueIdentity::Idempotent { key },
        }
    }
}

#[cfg(unix)]
fn try_enqueue_via_daemon(mempal_home: &Path, kind: &str, payload: &str) -> DaemonEnqueueOutcome {
    let request = crate::hook_ipc::HookIpcEnqueueRequest::new(kind, payload);
    let idempotency_key = request.idempotency_key.clone();
    match crate::hook_ipc::enqueue_with_default_timeout(mempal_home, request) {
        crate::hook_ipc::HookIpcClientOutcome::Accepted => DaemonEnqueueOutcome::Accepted,
        crate::hook_ipc::HookIpcClientOutcome::Fallback(reason) => {
            let reason_text = reason.to_string();
            if reason.may_have_reached_daemon() {
                DaemonEnqueueOutcome::Fallback(DaemonFallback::Idempotent {
                    reason: reason_text,
                    key: idempotency_key,
                })
            } else {
                DaemonEnqueueOutcome::Fallback(DaemonFallback::Fresh(reason_text))
            }
        }
    }
}

#[cfg(not(unix))]
fn try_enqueue_via_daemon(
    _mempal_home: &Path,
    _kind: &str,
    _payload: &str,
) -> DaemonEnqueueOutcome {
    DaemonEnqueueOutcome::Fallback(DaemonFallback::Fresh(
        "daemon IPC unsupported on this platform".to_string(),
    ))
}

fn should_drop_hook_capture(event: HookEvent, bytes: &[u8], config: &Config) -> bool {
    matches!(config.turns.storage_mode, TurnStorageMode::Off)
        && is_raw_turn_for_hook_event(event, bytes, config)
}

fn is_raw_turn_for_hook_event(event: HookEvent, bytes: &[u8], config: &Config) -> bool {
    let (wing, room) = raw_turn_target_for_hook_event(event, bytes);
    is_raw_turn(wing, Some(room.as_str()), None, &config.turns)
}

fn raw_turn_target_for_hook_event(event: HookEvent, bytes: &[u8]) -> (&'static str, String) {
    let room = match event {
        HookEvent::PostToolUse => serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|value| {
                value
                    .get("tool_name")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| "unknown-tool".to_string()),
        HookEvent::UserPromptSubmit => "user-prompt".to_string(),
        HookEvent::SessionStart | HookEvent::SessionEnd => "session-lifecycle".to_string(),
    };
    ("hooks-raw", room)
}

fn stdin_bytes() -> Result<Vec<u8>> {
    stdin_bytes_from(io::stdin().lock())
}

fn stdin_bytes_from(reader: impl Read) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .take(MAX_STDIN_READ_BYTES as u64)
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
    original_size_is_lower_bound: bool,
    truncated: bool,
}

fn capture_stdin_payload(bytes: Vec<u8>, _mempal_home: &Path) -> Result<CapturedPayload> {
    let original_size_bytes = bytes.len();
    if original_size_bytes <= MAX_INLINE_PAYLOAD_BYTES {
        return Ok(CapturedPayload {
            inline_payload: Some(decode_stdin_bytes(&bytes)),
            payload_path: None,
            preview: None,
            original_size_bytes,
            original_size_is_lower_bound: false,
            truncated: false,
        });
    }

    Ok(CapturedPayload {
        inline_payload: None,
        payload_path: None,
        preview: None,
        original_size_bytes: MAX_STDIN_READ_BYTES,
        original_size_is_lower_bound: true,
        truncated: true,
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    struct CountingReader {
        remaining: usize,
        read_bytes: Rc<Cell<usize>>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Ok(0);
            }
            let len = buf.len().min(self.remaining);
            buf[..len].fill(b'x');
            self.remaining -= len;
            self.read_bytes.set(self.read_bytes.get() + len);
            Ok(len)
        }
    }

    #[test]
    fn test_stdin_reader_stops_at_inline_limit_sentinel() {
        let read_bytes = Rc::new(Cell::new(0));
        let reader = CountingReader {
            remaining: MAX_STDIN_READ_BYTES + (1024 * 1024),
            read_bytes: Rc::clone(&read_bytes),
        };

        let bytes = stdin_bytes_from(reader).expect("read bounded stdin");

        assert_eq!(bytes.len(), MAX_STDIN_READ_BYTES);
        assert_eq!(read_bytes.get(), MAX_STDIN_READ_BYTES);
    }

    #[test]
    fn test_oversize_capture_omits_raw_payload_and_preview() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let bytes = vec![b'x'; MAX_INLINE_PAYLOAD_BYTES + 1];

        let captured = capture_stdin_payload(bytes, tmp.path()).expect("capture oversize payload");

        assert!(captured.inline_payload.is_none());
        assert!(captured.payload_path.is_none());
        assert!(captured.preview.is_none());
        assert_eq!(captured.original_size_bytes, MAX_INLINE_PAYLOAD_BYTES + 1);
        assert!(captured.original_size_is_lower_bound);
        assert!(captured.truncated);
        assert!(
            !tmp.path().join("hook-oversize").exists(),
            "automatic hook capture must not persist oversized raw payload before LLM gate"
        );
    }

    #[test]
    fn test_small_capture_preserves_raw_payload() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let bytes = br#"{"prompt":"raw verbatim"}"#.to_vec();

        let captured = capture_stdin_payload(bytes, tmp.path()).expect("capture small payload");

        assert_eq!(
            captured.inline_payload.as_deref(),
            Some(r#"{"prompt":"raw verbatim"}"#)
        );
        assert!(captured.payload_path.is_none());
        assert!(captured.preview.is_none());
        assert!(!captured.original_size_is_lower_bound);
        assert!(!captured.truncated);
    }
}

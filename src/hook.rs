use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
/// Maximum payload admitted directly into the JSON hook envelope.
///
/// Larger accepted payloads, still capped by [`MAX_INLINE_PAYLOAD_BYTES`], are
/// written once to `hook-spool/` and queued by handle so IPC/SQLite do not carry
/// near-10 MiB raw bodies through every stage.
pub const MAX_ENVELOPE_INLINE_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_STDIN_READ_BYTES: usize = MAX_INLINE_PAYLOAD_BYTES + 1;
pub const HOOK_SPOOL_DIR: &str = "hook-spool";

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
    /// Inject a bounded citation-first project brief as Codex hook JSON.
    Brief {
        /// Read `cwd` from Codex hook stdin JSON (`cwd` field).
        #[arg(long, value_name = "SOURCE")]
        cwd_source: Option<String>,
    },
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
    /// Prune unreferenced hook-spool payload files older than retention.
    ///
    /// Dry-run by default (report only). Pass `--execute` to delete.
    #[command(name = "retain-payloads")]
    RetainPayloads {
        /// Actually delete eligible files (default is dry-run).
        #[arg(long, default_value_t = false)]
        execute: bool,
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
        HookCommands::Brief { cwd_source } => run_brief_inject(cwd_source.as_deref()),
        HookCommands::Install {
            target,
            dry_run,
            uninstall,
            skip_mcp,
        } => hook_install::install(target, dry_run, uninstall, skip_mcp),
        HookCommands::RetainPayloads { execute } => run_retain_payloads(execute),
    }
}

fn run_brief_inject(cwd_source: Option<&str>) -> Result<()> {
    if let Err(error) = try_run_brief_inject(cwd_source) {
        tracing::debug!(error = %error, "codex hook brief failed open");
    }
    Ok(())
}

fn try_run_brief_inject(cwd_source: Option<&str>) -> Result<()> {
    ConfigHandle::bootstrap_quiet(default_config_path()).context("failed to bootstrap config")?;
    let payload = parse_hook_stdin_json()?;
    let cwd = resolve_brief_cwd(cwd_source, &payload)?;
    let event = payload
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(HookEvent::UserPromptSubmit.display_name());
    let query = payload
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or("project brief")
        .to_string();
    let config = ConfigHandle::current();
    let project_id = crate::core::project::resolve_project_id(None, config.as_ref(), Some(&cwd))
        .context("failed to resolve project scope")?;
    let db_path = expand_home_path(&config.db_path);
    let db = Database::open(&db_path).context("failed to open palace db")?;
    let budget_chars = config.context.budget.total_tokens.saturating_mul(4).max(1);
    let pinned = db
        .get_pinned_facts(project_id.as_deref(), budget_chars)
        .context("failed to load pinned facts")?;
    let request = crate::brief::BriefRequest {
        query,
        domain: crate::core::types::MemoryDomain::Project,
        field: "general".to_string(),
        cwd,
        max_items: 12,
        dao_tian_limit: 4,
    };
    let warning = crate::search::bm25_fallback_warning_embed_error("codex hook brief uses BM25");
    let brief =
        crate::brief::assemble_brief_from_bm25_for_project(&db, request, warning, project_id)
            .context("failed to assemble hook brief")?;
    print!("{}", format_codex_brief_hook_json(event, &pinned, &brief)?);
    Ok(())
}

fn parse_hook_stdin_json() -> Result<serde_json::Value> {
    let bytes = stdin_bytes()?;
    serde_json::from_slice(&bytes).context("invalid hook stdin JSON")
}

fn resolve_brief_cwd(cwd_source: Option<&str>, payload: &serde_json::Value) -> Result<PathBuf> {
    match cwd_source {
        Some("stdin-json") => payload
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("stdin JSON payload missing `cwd` string field")),
        Some(other) => Err(anyhow::anyhow!("unsupported --cwd-source: {other}")),
        None => env::current_dir().context("failed to resolve current working directory"),
    }
}

fn format_codex_brief_hook_json(
    event: &str,
    pinned: &[crate::core::types::Drawer],
    brief: &crate::brief::CognitiveBrief,
) -> Result<String> {
    Ok(serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": render_hook_brief_context(pinned, brief),
        }
    })
    .to_string())
}

pub fn render_hook_brief_context(
    pinned: &[crate::core::types::Drawer],
    brief: &crate::brief::CognitiveBrief,
) -> String {
    let mut out = String::from("## Project brief\n");
    if !pinned.is_empty() {
        out.push_str("## Pinned facts\n");
        for drawer in pinned {
            let source = drawer.source_file.as_deref().unwrap_or(&drawer.id);
            out.push_str(&format!(
                "- {}\n  drawer: {}\n  source: {}\n",
                drawer.content, drawer.id, source
            ));
        }
        out.push('\n');
    }
    if !brief.warnings.is_empty() {
        out.push_str("## Warnings\n");
        for warning in &brief.warnings {
            out.push_str(&format!("- {warning}\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!("## Summary\n{}\n", brief.summary.narrative));
    if !brief.key_facts.is_empty() {
        out.push_str("\n## Key Facts\n");
        for fact in &brief.key_facts {
            out.push_str(&format!(
                "- {}\n  drawer: {}\n  source: {}\n",
                fact.text, fact.citation.drawer_id, fact.citation.source_file
            ));
        }
    }
    if !brief.evidence.is_empty() {
        out.push_str("\n## Evidence\n");
        for ev in &brief.evidence {
            out.push_str(&format!(
                "- {}\n  drawer: {}\n  source: {}\n",
                ev.text, ev.citation.drawer_id, ev.citation.source_file
            ));
        }
    }
    if !brief.uncertainty.is_empty() {
        out.push_str("\n## Uncertainty\n");
        for item in &brief.uncertainty {
            out.push_str(&format!("- [{}] {}\n", item.kind, item.message));
        }
    }
    if !brief.next_actions.is_empty() {
        out.push_str("\n## Next Actions\n");
        for action in &brief.next_actions {
            out.push_str(&format!("- {action}\n"));
        }
    }
    out
}

fn run_retain_payloads(execute: bool) -> Result<()> {
    ConfigHandle::bootstrap_quiet(default_config_path()).context("failed to bootstrap config")?;
    let config = ConfigHandle::current();
    let db_path = expand_home_path(&config.db_path);
    let mempal_home = mempal_home_from_db(&db_path);
    let retention_days = config.hooks.payload_retention_days;
    let outcome = crate::hook_payload::prune_hook_payloads_with_mode(
        &mempal_home,
        &db_path,
        retention_days,
        execute,
    )
    .context("hook payload retention prune failed")?;
    let mode = if execute { "execute" } else { "dry-run" };
    println!(
        "hook-payload retain ({mode}): scanned={} deleted_or_eligible={} referenced={} young={} retention_days={retention_days}",
        outcome.scanned_files, outcome.deleted_files, outcome.referenced_files, outcome.young_files,
    );
    Ok(())
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
    let was_spooled = captured.payload_path.is_some() && captured.inline_payload.is_none();
    let payload_path = captured
        .payload_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
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
        payload_path,
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
    } else if was_spooled {
        crate::hook_diagnostics::log_hook_failure(
            &mempal_home,
            event_name,
            &crate::hook_diagnostics::HookOutcome::Spooled {
                size_bytes: envelope.original_size_bytes as u64,
                inline_threshold_bytes: MAX_ENVELOPE_INLINE_PAYLOAD_BYTES as u64,
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

fn capture_stdin_payload(bytes: Vec<u8>, mempal_home: &Path) -> Result<CapturedPayload> {
    let original_size_bytes = bytes.len();
    if original_size_bytes > MAX_INLINE_PAYLOAD_BYTES {
        return Ok(CapturedPayload {
            inline_payload: None,
            payload_path: None,
            preview: None,
            original_size_bytes: MAX_STDIN_READ_BYTES,
            original_size_is_lower_bound: true,
            truncated: true,
        });
    }

    let payload = decode_stdin_bytes(&bytes);
    if original_size_bytes <= MAX_ENVELOPE_INLINE_PAYLOAD_BYTES {
        return Ok(CapturedPayload {
            inline_payload: Some(payload),
            payload_path: None,
            preview: None,
            original_size_bytes,
            original_size_is_lower_bound: false,
            truncated: false,
        });
    }

    let payload_path = spool_hook_payload(&payload, mempal_home)?;
    Ok(CapturedPayload {
        inline_payload: None,
        payload_path: Some(payload_path),
        preview: None,
        original_size_bytes,
        original_size_is_lower_bound: false,
        truncated: false,
    })
}

fn spool_hook_payload(raw_payload: &str, mempal_home: &Path) -> Result<PathBuf> {
    let digest = blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
    fs::create_dir_all(mempal_home)
        .with_context(|| format!("failed to create mempal home {}", mempal_home.display()))?;
    let _retention_lock = crate::hook_payload::lock_for_home(mempal_home)?;
    let spool_dir = mempal_home.join(HOOK_SPOOL_DIR);
    fs::create_dir_all(&spool_dir)
        .with_context(|| format!("failed to create {}", spool_dir.display()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = spool_dir.join(format!("{}.{}.{}.json", digest, std::process::id(), nonce));
    let tmp_path = spool_dir.join(format!("{digest}.{}.{}.tmp", std::process::id(), nonce));
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("failed to create {}", tmp_path.display()))?;
        file.write_all(raw_payload.as_bytes())
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", tmp_path.display()))?;
        sync_hook_spool_payload(&file, &tmp_path)?;
    }

    match fs::rename(&tmp_path, &path) {
        Ok(()) => {
            sync_hook_spool_directory(&spool_dir)?;
            Ok(path)
        }
        Err(error) => {
            Err(error).with_context(|| format!("failed to publish hook spool {}", path.display()))
        }
    }
}

fn sync_hook_spool_payload(file: &fs::File, path: &Path) -> Result<()> {
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    #[cfg(test)]
    crate::hook_payload::HOOK_SPOOL_SYNC_EVENTS.with(|events| events.borrow_mut().push("payload"));
    Ok(())
}
fn sync_hook_spool_directory(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("failed to open hook spool directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync hook spool directory {}", path.display()))?;
    #[cfg(test)]
    crate::hook_payload::HOOK_SPOOL_SYNC_EVENTS
        .with(|events| events.borrow_mut().push("directory"));
    Ok(())
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
    include!("hook_durable_tests.rs");
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

    #[test]
    fn test_medium_capture_spools_payload_by_handle() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let payload = format!(
            r#"{{"prompt":"{}"}}"#,
            "medium-payload-marker".repeat(4 * 1024)
        );

        let captured = capture_stdin_payload(payload.as_bytes().to_vec(), tmp.path())
            .expect("capture medium payload");

        assert!(captured.inline_payload.is_none());
        assert!(!captured.truncated);
        assert_eq!(captured.original_size_bytes, payload.len());
        let payload_path = captured.payload_path.expect("spooled path");
        assert!(payload_path.starts_with(tmp.path().join(HOOK_SPOOL_DIR)));
        assert_eq!(
            std::fs::read_to_string(payload_path).expect("read spooled payload"),
            payload
        );
    }
}

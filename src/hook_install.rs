use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Value, json};

const CLAUDE_SETTINGS_RELATIVE: &str = ".claude/settings.json";
const CLAUDE_SETTINGS_DIR: &str = ".claude";
const CLAUDE_SETTINGS_FILE: &str = "settings.json";
const CODEX_SETTINGS_DIR: &str = ".codex";
const CODEX_SETTINGS_FILE: &str = "hooks.json";
const CODEX_HOOKS_RELATIVE: &str = ".codex/hooks.json";
const USER_MCP_FILE: &str = ".mcp.json";
const MEMPAL_MCP_SERVER_NAME: &str = "mempal";
const FORBIDDEN_TARGET_NAMES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md"];
const HOOK_COMMAND_EVENTS: [(&str, &str); 4] = [
    ("PostToolUse", "PostToolUse"),
    ("UserPromptSubmit", "UserPromptSubmit"),
    ("SessionStart", "SessionStart"),
    ("SessionEnd", "SessionEnd"),
];
const CODEX_BRIEF_EVENTS: [&str; 2] = ["UserPromptSubmit", "SessionStart"];
const LEGACY_ALIASES: [(&str, &str); 4] = [
    ("hook_post_tool", "PostToolUse"),
    ("hook_user_prompt", "UserPromptSubmit"),
    ("hook_session_start", "SessionStart"),
    ("hook_session_end", "SessionEnd"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookInstallTarget {
    #[value(name = "claude-code")]
    ClaudeCode,
    #[value(name = "gemini-cli")]
    GeminiCli,
    #[value(name = "codex")]
    Codex,
    #[value(name = "all")]
    All,
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub display_path: PathBuf,
    pub write_path: PathBuf,
    pub rendered: String,
    pub changed: bool,
    pub removed_commands: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpInstallStatus {
    /// New `~/.mcp.json` created with mempal entry.
    Created,
    /// Existing `~/.mcp.json` updated to add the mempal entry.
    Added,
    /// `~/.mcp.json` already had a mempal entry — left unchanged.
    AlreadyPresent,
    /// Skipped: dry-run, uninstall, or user opted out.
    Skipped,
    /// Removed mempal entry from `~/.mcp.json` (uninstall path).
    Removed,
    /// File exists with a different (non-mempal) `mcpServers.mempal` entry that
    /// we declined to overwrite. Caller should warn the user.
    Conflict,
}

#[derive(Debug, Clone)]
pub struct McpInstallOutcome {
    pub display_path: PathBuf,
    pub write_path: PathBuf,
    pub status: McpInstallStatus,
    pub rendered: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedSettingsPath {
    display_path: PathBuf,
    write_path: PathBuf,
}

pub fn install(
    target: HookInstallTarget,
    dry_run: bool,
    uninstall: bool,
    skip_mcp: bool,
) -> Result<()> {
    let home = home_dir()?;
    let cwd = env::current_dir().context("failed to resolve current working directory")?;

    let targets = match target {
        HookInstallTarget::All => vec![HookInstallTarget::ClaudeCode, HookInstallTarget::Codex],
        other => vec![other],
    };

    for t in targets {
        match t {
            HookInstallTarget::ClaudeCode => {
                let outcome = install_claude_code(&cwd, &home, dry_run, uninstall)?;
                report_install_outcome(&outcome, dry_run, uninstall);
            }
            HookInstallTarget::Codex => {
                let outcome = install_codex(&cwd, &home, dry_run, uninstall)?;
                report_install_outcome(&outcome, dry_run, uninstall);
                if !dry_run && !uninstall {
                    if let Err(e) = check_codex_feature_flag(&home) {
                        eprintln!("warning: failed to check Codex feature flag: {}", e);
                    }
                }
            }
            HookInstallTarget::GeminiCli => {
                bail!("hook install currently supports only --target claude-code, codex, and all");
            }
            HookInstallTarget::All => unreachable!(),
        }
    }

    if !skip_mcp {
        let mcp_outcome = install_user_mcp(&home, dry_run, uninstall)?;
        report_mcp_outcome(&mcp_outcome, dry_run);
    }

    Ok(())
}

fn report_install_outcome(outcome: &InstallOutcome, dry_run: bool, uninstall: bool) {
    if dry_run {
        println!(
            "--- dry run: {} ({}) ---\n{}",
            outcome.display_path.display(),
            outcome.write_path.display(),
            outcome.rendered
        );
    } else if uninstall {
        println!(
            "removed {} hook entr{} from {} ({})",
            outcome.removed_commands,
            if outcome.removed_commands == 1 {
                "y"
            } else {
                "ies"
            },
            outcome.display_path.display(),
            outcome.write_path.display()
        );
    } else if outcome.changed {
        println!(
            "updated {} ({})",
            outcome.display_path.display(),
            outcome.write_path.display()
        );
    } else {
        println!(
            "no-op {} ({})",
            outcome.display_path.display(),
            outcome.write_path.display()
        );
    }
}

fn report_mcp_outcome(outcome: &McpInstallOutcome, dry_run: bool) {
    match outcome.status {
        McpInstallStatus::Created => println!(
            "created {} with mempal MCP server entry",
            outcome.display_path.display()
        ),
        McpInstallStatus::Added => println!(
            "added mempal MCP server entry to {}",
            outcome.display_path.display()
        ),
        McpInstallStatus::AlreadyPresent => println!(
            "= mempal MCP server already registered in {}",
            outcome.display_path.display()
        ),
        McpInstallStatus::Removed => println!(
            "removed mempal MCP server entry from {}",
            outcome.display_path.display()
        ),
        McpInstallStatus::Skipped => {
            if dry_run && let Some(rendered) = outcome.rendered.as_deref() {
                println!(
                    "--- dry run: {} ---\n{}",
                    outcome.display_path.display(),
                    rendered
                );
            }
        }
        McpInstallStatus::Conflict => eprintln!(
            "warning: {} already has a different `mcpServers.{}` entry; left unchanged. \
             Inspect the file and add `command: \"mempal\", args: [\"serve\", \"--mcp\"]` manually.",
            outcome.display_path.display(),
            MEMPAL_MCP_SERVER_NAME
        ),
    }
}

pub fn install_claude_code(
    cwd: &Path,
    home: &Path,
    dry_run: bool,
    uninstall: bool,
) -> Result<InstallOutcome> {
    let resolved = resolve_claude_settings_path(cwd, home)?;
    let hook_commands = hook_commands()?;
    let mut root = read_settings_json(&resolved.write_path)?;
    let mut removed_commands = 0usize;
    let mut changed = false;

    for (event_name, command) in &hook_commands {
        let event_array = ensure_hook_event_array(&mut root, event_name)?;
        let before_len = event_array.len();
        event_array.retain(|entry| !entry_contains_command(entry, command));
        let removed = before_len.saturating_sub(event_array.len());
        removed_commands += removed;

        let inserted = !uninstall
            && !event_array
                .iter()
                .any(|entry| entry_contains_command(entry, command));

        if inserted {
            event_array.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": command
                }]
            }));
        }

        changed |= removed > 0 || inserted;
    }

    let rendered =
        serde_json::to_string_pretty(&root).context("failed to serialize hook settings JSON")?;
    if !dry_run {
        if let Some(parent) = resolved.write_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create hook settings parent {}", parent.display())
            })?;
        }
        let existing = fs::read_to_string(&resolved.write_path).ok();
        changed = existing.as_deref() != Some(rendered.as_str());
        if changed {
            fs::write(&resolved.write_path, &rendered).with_context(|| {
                format!(
                    "failed to write hook settings {}",
                    resolved.write_path.display()
                )
            })?;
        }
    }

    Ok(InstallOutcome {
        display_path: resolved.display_path,
        write_path: resolved.write_path,
        rendered,
        changed,
        removed_commands,
    })
}

pub fn install_codex(
    cwd: &Path,
    home: &Path,
    dry_run: bool,
    uninstall: bool,
) -> Result<InstallOutcome> {
    let path = home.join(CODEX_HOOKS_RELATIVE);
    let write_path = canonicalize_if_symlink(&path)?;
    validate_write_target(cwd, home, &write_path, HookInstallTarget::Codex)?;

    let hook_commands = hook_commands()?;
    let mut root = read_settings_json(&write_path)?;
    let mut removed_commands = 0usize;
    let mut changed = false;

    for (event_name, command) in &hook_commands {
        let event_array = ensure_hook_event_array(&mut root, event_name)?;
        let before_len = event_array.len();
        event_array.retain(|entry| !entry_contains_command(entry, command));
        let removed = before_len.saturating_sub(event_array.len());
        removed_commands += removed;

        let inserted = !uninstall
            && !event_array
                .iter()
                .any(|entry| entry_contains_command(entry, command));

        if inserted {
            event_array.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": command
                }]
            }));
        }

        changed |= removed > 0 || inserted;
    }

    let brief_command = format!(
        "{} hook brief --cwd-source stdin-json",
        shell_escape_path(&resolve_mempal_binary()?)
    );
    for event_name in CODEX_BRIEF_EVENTS {
        let event_array = ensure_hook_event_array(&mut root, event_name)?;
        let before_len = event_array.len();
        event_array.retain(|entry| !entry_contains_command(entry, &brief_command));
        let removed = before_len.saturating_sub(event_array.len());
        removed_commands += removed;
        let inserted = !uninstall
            && !event_array
                .iter()
                .any(|entry| entry_contains_command(entry, &brief_command));
        if inserted {
            event_array.push(json!({
                "hooks": [{
                    "type": "command",
                    "command": brief_command
                }]
            }));
        }
        changed |= removed > 0 || inserted;
    }

    let rendered =
        serde_json::to_string_pretty(&root).context("failed to serialize hook settings JSON")?;
    if !dry_run {
        if let Some(parent) = write_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create hook settings parent {}", parent.display())
            })?;
        }
        let existing = fs::read_to_string(&write_path).ok();
        changed = existing.as_deref() != Some(rendered.as_str());
        if changed {
            fs::write(&write_path, &rendered).with_context(|| {
                format!("failed to write hook settings {}", write_path.display())
            })?;
        }
    }

    Ok(InstallOutcome {
        display_path: path,
        write_path,
        rendered,
        changed,
        removed_commands,
    })
}

fn check_codex_feature_flag(home: &Path) -> Result<()> {
    let path = home.join(".codex/config.toml");
    let mut enabled = false;

    if path.exists() {
        let content = fs::read_to_string(&path).context("failed to read Codex config")?;

        #[derive(Deserialize)]
        struct CodexConfig {
            codex_hooks: Option<bool>,
            features: Option<CodexFeatures>,
        }
        #[derive(Deserialize)]
        struct CodexFeatures {
            codex_hooks: Option<bool>,
        }

        if let Ok(config) = toml::from_str::<CodexConfig>(&content) {
            enabled = config.codex_hooks.unwrap_or(false)
                || config.features.and_then(|f| f.codex_hooks).unwrap_or(false);
        }
    }

    if !enabled {
        println!(
            "warning: Codex hook runtime (codex_hooks) is currently disabled. hooks in {} will be ignored.",
            home.join(CODEX_HOOKS_RELATIVE).display()
        );
        println!("Run `codex features enable codex_hooks` to activate hook support.");
    }

    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve $HOME"))
}

/// Ensure `~/.mcp.json` registers the mempal MCP server so any project (not
/// just the mempal repo itself) exposes mempal_ingest / mempal_search to
/// Claude Code. Issue #129.
///
/// Behavior:
/// - File missing       → write a minimal file with one mempal entry.
/// - File exists, no entry → merge in the mempal entry (preserving siblings).
/// - File exists, mempal entry already correct → no-op.
/// - File exists, mempal entry differs from canonical → leave alone, surface
///   a Conflict status so the user can resolve manually (avoids overwriting
///   an intentional override).
/// - `dry_run = true`   → never writes; renders the would-be JSON for display.
/// - `uninstall = true` → removes only our `mempal` entry; leaves the file
///   (and other servers) intact. Removes the file if it becomes empty.
pub fn install_user_mcp(home: &Path, dry_run: bool, uninstall: bool) -> Result<McpInstallOutcome> {
    let path = home.join(USER_MCP_FILE);
    let existed = path.exists();
    let mut root = if existed {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed: Value = serde_json::from_str(&content)
            .with_context(|| format!("invalid JSON in {}", path.display()))?;
        if !parsed.is_object() {
            bail!(
                "refusing to overwrite {}: top-level JSON must be an object",
                path.display()
            );
        }
        parsed
    } else {
        json!({ "mcpServers": {} })
    };

    let canonical = canonical_mempal_entry();
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("`{}` JSON root is not an object", path.display()))?;
    let servers_value = root_obj.entry("mcpServers").or_insert_with(|| json!({}));
    if !servers_value.is_object() {
        bail!(
            "refusing to overwrite {}: `mcpServers` field is not an object",
            path.display()
        );
    }
    let servers = servers_value
        .as_object_mut()
        .expect("just verified mcpServers is an object");

    let existing_entry = servers.get(MEMPAL_MCP_SERVER_NAME).cloned();

    let (status, mutated) = if uninstall {
        if existing_entry.is_some() {
            servers.remove(MEMPAL_MCP_SERVER_NAME);
            (McpInstallStatus::Removed, true)
        } else {
            (McpInstallStatus::Skipped, false)
        }
    } else {
        match existing_entry {
            Some(existing) if mempal_entry_matches_canonical(&existing) => {
                (McpInstallStatus::AlreadyPresent, false)
            }
            Some(_) => (McpInstallStatus::Conflict, false),
            None => {
                servers.insert(MEMPAL_MCP_SERVER_NAME.to_string(), canonical);
                if existed {
                    (McpInstallStatus::Added, true)
                } else {
                    (McpInstallStatus::Created, true)
                }
            }
        }
    };

    let rendered = if mutated || dry_run {
        Some(serde_json::to_string_pretty(&root).context("failed to serialize MCP servers JSON")?)
    } else {
        None
    };

    if mutated && !dry_run {
        if uninstall && servers_is_empty(&root) {
            // Remove the now-empty file so we don't leave behind a stub.
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error)
                        .with_context(|| format!("failed to remove {}", path.display()));
                }
            }
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create MCP file parent {}", parent.display())
                })?;
            }
            let serialized = rendered
                .as_deref()
                .expect("rendered JSON populated when mutating");
            fs::write(&path, serialized)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }
    }

    Ok(McpInstallOutcome {
        display_path: path.clone(),
        write_path: path,
        status,
        rendered,
    })
}

fn canonical_mempal_entry() -> Value {
    json!({
        "command": "mempal",
        "args": ["serve", "--mcp"]
    })
}

fn mempal_entry_matches_canonical(entry: &Value) -> bool {
    let Some(obj) = entry.as_object() else {
        return false;
    };
    let command_ok = obj
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "mempal");
    let args_ok = obj
        .get("args")
        .and_then(Value::as_array)
        .and_then(|args| {
            args.iter()
                .all(|item| item.is_string())
                .then(|| args.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        })
        .is_some_and(|args| args == ["serve", "--mcp"]);
    command_ok && args_ok
}

fn servers_is_empty(root: &Value) -> bool {
    root.get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|map| map.is_empty())
}

fn resolve_claude_settings_path(cwd: &Path, home: &Path) -> Result<ResolvedSettingsPath> {
    let local_path = cwd.join(CLAUDE_SETTINGS_RELATIVE);
    if local_path.exists() || is_symlink(&local_path)? {
        let write_path = canonicalize_if_symlink(&local_path)?;
        validate_write_target(cwd, home, &write_path, HookInstallTarget::ClaudeCode)?;
        return Ok(ResolvedSettingsPath {
            display_path: local_path,
            write_path,
        });
    }

    let global_path = home.join(CLAUDE_SETTINGS_RELATIVE);
    let write_path = canonicalize_if_symlink(&global_path)?;
    validate_write_target(cwd, home, &write_path, HookInstallTarget::ClaudeCode)?;
    Ok(ResolvedSettingsPath {
        display_path: global_path.clone(),
        write_path,
    })
}

fn read_settings_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({ "hooks": {} }));
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let root: Value = serde_json::from_str(&content)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    if !root.is_object() {
        bail!(
            "refusing to overwrite {}: top-level JSON must be an object",
            path.display()
        );
    }
    Ok(root)
}

fn ensure_hook_event_array<'a>(
    root: &'a mut Value,
    event_name: &str,
) -> Result<&'a mut Vec<Value>> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings JSON root is not an object"))?;
    let hooks = root_obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("`hooks` field must be an object"))?;
    let event = hooks_obj.entry(event_name).or_insert_with(|| json!([]));
    event
        .as_array_mut()
        .ok_or_else(|| anyhow!("`hooks.{event_name}` must be an array"))
}

fn entry_contains_command(entry: &Value, expected: &str) -> bool {
    let hooks = match entry.get("hooks").and_then(Value::as_array) {
        Some(h) => h,
        None => return false,
    };

    hooks.iter().any(|hook| {
        let cmd = match hook.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return false,
        };

        if cmd == expected {
            return true;
        }

        // Check for aliases. Both cmd and expected should share the same
        // '.../mempal hook ' prefix for us to consider them related.
        if let (Some((_base_cmd, sub_cmd)), Some((_base_expected, sub_expected))) =
            (cmd.rsplit_once(" hook "), expected.rsplit_once(" hook "))
        {
            if sub_cmd == sub_expected {
                return true;
            }
            for (old, new) in LEGACY_ALIASES {
                if sub_expected == new && sub_cmd == old {
                    return true;
                }
            }
        }

        false
    })
}

fn is_symlink(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn canonicalize_if_symlink(path: &Path) -> Result<PathBuf> {
    if path.exists() && is_symlink(path)? {
        return path
            .canonicalize()
            .with_context(|| format!("failed to resolve symlink {}", path.display()));
    }
    Ok(path.to_path_buf())
}

fn validate_write_target(
    cwd: &Path,
    home: &Path,
    path: &Path,
    target: HookInstallTarget,
) -> Result<()> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| FORBIDDEN_TARGET_NAMES.contains(&name))
    {
        bail!(
            "refusing to edit agent-instruction target {}",
            path.display()
        );
    }

    for component in path.components() {
        if matches!(component, Component::Normal(part) if part == ".agents") {
            bail!(
                "refusing to edit agent-instruction target {}",
                path.display()
            );
        }
    }

    let parent_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    let file_name = path.file_name().and_then(|name| name.to_str());

    match target {
        HookInstallTarget::ClaudeCode
            if parent_name != Some(CLAUDE_SETTINGS_DIR)
                || file_name != Some(CLAUDE_SETTINGS_FILE) =>
        {
            bail!(
                "refusing to edit non-canonical Claude settings target {}",
                path.display()
            );
        }
        HookInstallTarget::Codex
            if parent_name != Some(CODEX_SETTINGS_DIR)
                || file_name != Some(CODEX_SETTINGS_FILE) =>
        {
            bail!(
                "refusing to edit non-canonical Codex settings target {}",
                path.display()
            );
        }
        _ => {}
    }

    let allowed_roots = [
        cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
        home.canonicalize().unwrap_or_else(|_| home.to_path_buf()),
    ];
    if !allowed_roots
        .iter()
        .any(|root| path.starts_with(root) || path == root)
    {
        bail!(
            "refusing to edit settings target outside allowed roots {}",
            path.display()
        );
    }

    Ok(())
}

fn hook_commands() -> Result<Vec<(&'static str, String)>> {
    let binary = shell_escape_path(&resolve_mempal_binary()?);
    Ok(HOOK_COMMAND_EVENTS
        .iter()
        .map(|(event_name, subcommand)| (*event_name, format!("{binary} hook {subcommand}")))
        .collect())
}

fn resolve_mempal_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CARGO_BIN_EXE_mempal") {
        let path = PathBuf::from(path);
        return Ok(path.canonicalize().unwrap_or(path));
    }

    let current = env::current_exe().context("failed to resolve current executable path")?;
    if current.file_name().and_then(|name| name.to_str()) == Some("mempal") {
        return current
            .canonicalize()
            .context("failed to canonicalize current executable path");
    }

    if let Some(candidate) = current
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("mempal"))
        .filter(|candidate| candidate.exists())
    {
        return Ok(candidate.canonicalize().unwrap_or(candidate));
    }

    current
        .canonicalize()
        .context("failed to canonicalize current executable path")
}

fn shell_escape_path(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    if !rendered.contains([' ', '\t', '\n', '\'', '"']) {
        return rendered.into_owned();
    }
    format!("'{}'", rendered.replace('\'', r"'\''"))
}

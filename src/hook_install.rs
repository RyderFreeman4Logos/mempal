use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
use serde_json::{Value, json};

const CLAUDE_SETTINGS_RELATIVE: &str = ".claude/settings.json";
const FORBIDDEN_TARGET_NAMES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md"];
const HOOK_COMMANDS: [(&str, &str); 4] = [
    ("PostToolUse", "mempal hook hook_post_tool"),
    ("UserPromptSubmit", "mempal hook hook_user_prompt"),
    ("SessionStart", "mempal hook hook_session_start"),
    ("SessionEnd", "mempal hook hook_session_end"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookInstallTarget {
    #[value(name = "claude-code")]
    ClaudeCode,
    #[value(name = "gemini-cli")]
    GeminiCli,
    #[value(name = "codex")]
    Codex,
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    pub display_path: PathBuf,
    pub write_path: PathBuf,
    pub rendered: String,
    pub changed: bool,
    pub removed_commands: usize,
}

#[derive(Debug, Clone)]
struct ResolvedSettingsPath {
    display_path: PathBuf,
    write_path: PathBuf,
}

pub fn install(target: HookInstallTarget, dry_run: bool, uninstall: bool) -> Result<()> {
    match target {
        HookInstallTarget::ClaudeCode => {
            let cwd = env::current_dir().context("failed to resolve current working directory")?;
            let home = home_dir()?;
            let outcome = install_claude_code(&cwd, &home, dry_run, uninstall)?;
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
            Ok(())
        }
        HookInstallTarget::GeminiCli | HookInstallTarget::Codex => {
            bail!("hook install currently supports only --target claude-code");
        }
    }
}

pub fn install_claude_code(
    cwd: &Path,
    home: &Path,
    dry_run: bool,
    uninstall: bool,
) -> Result<InstallOutcome> {
    let resolved = resolve_claude_settings_path(cwd, home)?;
    let mut root = read_settings_json(&resolved.write_path)?;
    let mut removed_commands = 0usize;
    let mut changed = false;

    for (event_name, command) in HOOK_COMMANDS {
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

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve $HOME"))
}

fn resolve_claude_settings_path(cwd: &Path, home: &Path) -> Result<ResolvedSettingsPath> {
    let local_path = cwd.join(CLAUDE_SETTINGS_RELATIVE);
    if local_path.exists() || is_symlink(&local_path)? {
        let write_path = canonicalize_if_symlink(&local_path)?;
        validate_write_target(&write_path)?;
        return Ok(ResolvedSettingsPath {
            display_path: local_path,
            write_path,
        });
    }

    let global_path = home.join(CLAUDE_SETTINGS_RELATIVE);
    let write_path = canonicalize_if_symlink(&global_path)?;
    validate_write_target(&write_path)?;
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
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|hook| hook.get("command").and_then(Value::as_str) == Some(expected))
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

fn validate_write_target(path: &Path) -> Result<()> {
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

    Ok(())
}

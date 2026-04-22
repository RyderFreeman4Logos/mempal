use std::fs;
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    IntegrationContext, IntegrationProfile, SETTINGS_SNIPPET_PLACEHOLDER, ToolActionReport,
    ToolIntegration, ToolStatusReport, read_json_object_or_default, shell_escape_path,
    write_text_if_changed,
};

const DEFAULT_SETTINGS_JSON: &str = "{\n  \"hooks\": {}\n}\n";
const SETTINGS_RELATIVE_PATH: &str = ".claude/settings.json";
const SNIPPET_RELATIVE_PATH: &str = "claude-code/settings-snippet.json";

pub(crate) struct ClaudeCodeIntegration;

impl ToolIntegration for ClaudeCodeIntegration {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn config_paths(&self, context: &IntegrationContext) -> Vec<PathBuf> {
        vec![context.home.join(SETTINGS_RELATIVE_PATH)]
    }

    fn install(
        &self,
        context: &IntegrationContext,
        profile: IntegrationProfile,
    ) -> Result<ToolActionReport> {
        if profile == IntegrationProfile::Project {
            bail!("project profile disabled by P11 spec; use --profile user");
        }

        let settings_path = self
            .config_paths(context)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Claude Code settings path resolved"))?;
        let existing = read_json_object_or_default(&settings_path, DEFAULT_SETTINGS_JSON)?;
        let snippet = render_snippet(context)?;
        let merged = self.merge_snippet(&existing, &snippet)?;
        let changed = write_text_if_changed(&settings_path, &merged)?;

        Ok(ToolActionReport {
            changed,
            message: if changed {
                format!(
                    "installed claude-code integration into {}",
                    settings_path.display()
                )
            } else {
                format!(
                    "claude-code integration already current at {}",
                    settings_path.display()
                )
            },
        })
    }

    fn uninstall(&self, context: &IntegrationContext) -> Result<ToolActionReport> {
        let settings_path = self
            .config_paths(context)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Claude Code settings path resolved"))?;
        if !settings_path.exists() {
            return Ok(ToolActionReport {
                changed: false,
                message: format!(
                    "claude-code integration already absent at {}",
                    settings_path.display()
                ),
            });
        }

        let existing = read_json_object_or_default(&settings_path, DEFAULT_SETTINGS_JSON)?;
        let mut root: Value = serde_json::from_str(&existing)?;
        let mut changed = false;

        if let Some(events) = root.get_mut("hooks").and_then(Value::as_object_mut) {
            for value in events.values_mut() {
                let Some(entries) = value.as_array_mut() else {
                    continue;
                };
                let before = entries.len();
                entries.retain(|entry| !is_mempal_entry(entry));
                if entries.len() != before {
                    changed = true;
                }
            }
        }

        let rendered = serde_json::to_string_pretty(&root)?;
        let written = if changed {
            write_text_if_changed(&settings_path, &rendered)?
        } else {
            false
        };

        Ok(ToolActionReport {
            changed: written,
            message: if written {
                format!(
                    "removed claude-code integration from {}",
                    settings_path.display()
                )
            } else {
                format!(
                    "claude-code integration already absent at {}",
                    settings_path.display()
                )
            },
        })
    }

    fn status(&self, context: &IntegrationContext) -> Result<ToolStatusReport> {
        let settings_path = self
            .config_paths(context)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no Claude Code settings path resolved"))?;
        if !settings_path.exists() {
            return Ok(ToolStatusReport {
                name: self.name(),
                installed: false,
                detail: "global settings missing".to_string(),
            });
        }

        let existing = read_json_object_or_default(&settings_path, DEFAULT_SETTINGS_JSON)?;
        let markers = self.detect_our_entries(&existing)?;
        Ok(ToolStatusReport {
            name: self.name(),
            installed: !markers.is_empty(),
            detail: if markers.is_empty() {
                "no mempal marker".to_string()
            } else {
                format!("{} marker(s) in {}", markers.len(), settings_path.display())
            },
        })
    }
}

impl ClaudeCodeIntegration {
    fn merge_snippet(&self, existing: &str, snippet: &str) -> Result<String> {
        let mut root: Value = serde_json::from_str(existing)?;
        let snippet_root: Value = serde_json::from_str(snippet)?;

        let root_hooks = ensure_hooks_object(&mut root)?;
        let snippet_hooks = snippet_root
            .get("hooks")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("Claude Code snippet must contain object hooks"))?;

        for (event_name, snippet_value) in snippet_hooks {
            let snippet_entries = snippet_value
                .as_array()
                .ok_or_else(|| anyhow!("snippet hooks.{event_name} must be an array"))?;
            let event_entries = root_hooks
                .entry(event_name.clone())
                .or_insert_with(|| json!([]))
                .as_array_mut()
                .ok_or_else(|| anyhow!("existing hooks.{event_name} must be an array"))?;

            let unrelated: Vec<Value> = event_entries
                .iter()
                .filter(|entry| !is_mempal_entry(entry))
                .cloned()
                .collect();
            let mut desired = unrelated;
            desired.extend(snippet_entries.iter().cloned());
            if *event_entries != desired {
                *event_entries = desired;
            }
        }

        serde_json::to_string_pretty(&root).map_err(Into::into)
    }

    fn detect_our_entries(&self, existing: &str) -> Result<Vec<String>> {
        let root: Value = serde_json::from_str(existing)?;
        let mut markers = Vec::new();
        let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
            return Ok(markers);
        };
        for (event_name, entries) in hooks {
            let Some(entries) = entries.as_array() else {
                continue;
            };
            let count = entries
                .iter()
                .filter(|entry| is_mempal_entry(entry))
                .count();
            if count > 0 {
                markers.push(format!("{event_name}:{count}"));
            }
        }
        Ok(markers)
    }
}

fn ensure_hooks_object(root: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings JSON root must be an object"))?;
    let hooks = root_obj.entry("hooks").or_insert_with(|| json!({}));
    hooks
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings JSON hooks field must be an object"))
}

fn render_snippet(context: &IntegrationContext) -> Result<String> {
    let snippet_path = context.root.join(SNIPPET_RELATIVE_PATH);
    let snippet = fs::read_to_string(&snippet_path)?;
    let mut value: Value = serde_json::from_str(&snippet)?;
    let command = canonical_command(context);
    let hook = value
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .and_then(|hooks| hooks.get_mut("SessionStart"))
        .and_then(Value::as_array_mut)
        .and_then(|entries| entries.first_mut())
        .and_then(|entry| entry.get_mut("hooks"))
        .and_then(Value::as_array_mut)
        .and_then(|handlers| handlers.first_mut())
        .ok_or_else(|| anyhow!("invalid Claude Code settings snippet"))?;
    let command_field = hook
        .get_mut("command")
        .ok_or_else(|| anyhow!("missing command field in Claude Code settings snippet"))?;
    *command_field = Value::String(command);
    if command_field.as_str() == Some(SETTINGS_SNIPPET_PLACEHOLDER) {
        bail!("failed to render Claude Code settings snippet");
    }
    serde_json::to_string_pretty(&value).map_err(Into::into)
}

fn canonical_command(context: &IntegrationContext) -> String {
    let script_path = context
        .root
        .join("claude-code")
        .join("hooks")
        .join("session-start.sh");
    format!("bash {}", shell_escape_path(&script_path))
}

fn is_mempal_entry(entry: &Value) -> bool {
    entry.get("mempal_source").and_then(Value::as_bool) == Some(true)
}

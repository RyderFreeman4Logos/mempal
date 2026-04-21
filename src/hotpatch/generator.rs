use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::core::{
    config::Config,
    db::Database,
    project::{ProjectSearchScope, resolve_project_id},
};
use crate::hotpatch::{FileLock, short_drawer_id};
use crate::search::preview;

#[derive(Debug, Clone, Copy, Default)]
pub struct GenerationOptions {
    pub all_projects: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GenerationOutcome {
    pub appended: usize,
    pub skipped: usize,
}

pub fn suggest_for_drawer(
    db: &Database,
    config: &Config,
    mempal_home: &Path,
    drawer_id: &str,
    options: GenerationOptions,
) -> Result<GenerationOutcome> {
    if !config.hotpatch.enabled {
        return Ok(GenerationOutcome::default());
    }

    let details = match db
        .get_drawer_details(drawer_id)
        .with_context(|| format!("failed to load drawer {drawer_id}"))?
    {
        Some(details) => details,
        None => return Ok(GenerationOutcome::default()),
    };

    let resolved_project = resolve_generation_project(config, &details.drawer.content)?;
    let scope = ProjectSearchScope::from_request(
        resolved_project,
        false,
        options.all_projects,
        config.search.strict_project_isolation,
    );
    if !scope.allows_row(details.project_id.as_deref()) {
        return Ok(GenerationOutcome {
            appended: 0,
            skipped: 1,
        });
    }

    let (summary_source, flags, relative_base) = summary_inputs(&details.drawer.content)?;
    if !is_eligible(
        details.drawer.importance,
        &flags,
        summary_source
            .as_deref()
            .unwrap_or(details.drawer.content.as_str()),
        config.hotpatch.min_importance_stars,
    ) {
        return Ok(GenerationOutcome {
            appended: 0,
            skipped: 1,
        });
    }

    let source_file = details
        .drawer
        .source_file
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.exists());
    let payload_paths = extract_candidate_paths(
        source_file.as_deref(),
        summary_source.as_deref(),
        relative_base.as_deref(),
    )?;
    if payload_paths.is_empty() {
        return Ok(GenerationOutcome {
            appended: 0,
            skipped: 1,
        });
    }

    let mut appended = 0usize;
    let mut skipped = 0usize;
    let summary = summarize(
        summary_source
            .as_deref()
            .unwrap_or(details.drawer.content.as_str()),
        config.hotpatch.max_suggestion_length,
    );
    let topic = classify_topic(
        &flags,
        summary_source.as_deref().unwrap_or(&details.drawer.content),
    );
    let line = format!(
        "- {} {}: {} [drawer:{}]",
        stars(
            details
                .drawer
                .importance
                .max(config.hotpatch.min_importance_stars)
        ),
        topic,
        summary,
        short_drawer_id(&details.drawer.id)
    );

    for path in payload_paths {
        let watched_dir = match find_watched_dir(&path, &config.hotpatch.watch_files) {
            Ok(Some(dir)) => dir,
            Ok(None) => {
                skipped += 1;
                continue;
            }
            Err(error) => {
                tracing::warn!(?error, path = %path.display(), "hotpatch watched-dir resolution failed");
                skipped += 1;
                continue;
            }
        };
        let suggestion_path = suggestion_file_path(mempal_home, &watched_dir)?;
        fs::create_dir_all(
            suggestion_path
                .parent()
                .context("hotpatch suggestion file missing parent")?,
        )
        .with_context(|| format!("failed to create {}", suggestion_path.display()))?;
        let changed =
            append_suggestion_line(&suggestion_path, &watched_dir, &details.drawer.id, &line)?;
        if changed {
            appended += 1;
        } else {
            skipped += 1;
        }
    }

    Ok(GenerationOutcome { appended, skipped })
}

fn resolve_generation_project(config: &Config, drawer_content: &str) -> Result<Option<String>> {
    let cwd = serde_json::from_str::<Value>(drawer_content)
        .ok()
        .and_then(|value| {
            value
                .get("claude_cwd")
                .and_then(Value::as_str)
                .map(PathBuf::from)
        });
    resolve_project_id(None, config, cwd.as_deref()).map_err(anyhow::Error::from)
}

fn summary_inputs(drawer_content: &str) -> Result<(Option<String>, Vec<String>, Option<PathBuf>)> {
    let parsed = match serde_json::from_str::<Value>(drawer_content) {
        Ok(value) => value,
        Err(_) => return Ok((Some(drawer_content.to_string()), Vec::new(), None)),
    };
    let summary_source = parsed
        .get("preview")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("payload_preview").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .or_else(|| {
            parsed
                .get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    let flags = parsed
        .get("flags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.to_ascii_uppercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let relative_base = parsed
        .get("claude_cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Ok((summary_source, flags, relative_base))
}

fn is_eligible(importance: i32, flags: &[String], summary_source: &str, threshold: i32) -> bool {
    if importance >= threshold {
        return true;
    }
    if flags
        .iter()
        .any(|flag| flag == "DECISION" || flag == "PIVOT")
    {
        return true;
    }
    let lower = summary_source.to_ascii_lowercase();
    lower.contains("decision") || lower.contains("pivot")
}

fn extract_candidate_paths(
    source_file: Option<&Path>,
    summary_source: Option<&str>,
    relative_base: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    if let Some(source_file) = source_file {
        let raw = fs::read_to_string(source_file)
            .with_context(|| format!("failed to read hook payload {}", source_file.display()))?;
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            collect_paths_from_value(&value, &mut paths);
        }
    }
    if paths.is_empty()
        && let Some(summary_source) = summary_source
        && let Ok(value) = serde_json::from_str::<Value>(summary_source)
    {
        collect_paths_from_value(&value, &mut paths);
    }
    let mut resolved = Vec::new();
    for path in paths {
        resolved.push(resolve_candidate_path(&path, relative_base)?);
    }
    Ok(resolved)
}

fn collect_paths_from_value(value: &Value, out: &mut BTreeSet<PathBuf>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if matches!(key.as_str(), "file_path" | "path")
                    && let Some(value) = nested.as_str()
                {
                    out.insert(PathBuf::from(value));
                }
                if key == "files" {
                    match nested {
                        Value::Array(items) => {
                            for item in items {
                                match item {
                                    Value::String(path) => {
                                        out.insert(PathBuf::from(path));
                                    }
                                    Value::Object(inner) => {
                                        for inner_key in ["file_path", "path"] {
                                            if let Some(path) =
                                                inner.get(inner_key).and_then(Value::as_str)
                                            {
                                                out.insert(PathBuf::from(path));
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Value::String(path) => {
                            out.insert(PathBuf::from(path));
                        }
                        _ => {}
                    }
                }
                collect_paths_from_value(nested, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths_from_value(item, out);
            }
        }
        _ => {}
    }
}

fn resolve_candidate_path(path: &Path, relative_base: Option<&Path>) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let base = relative_base
        .context("hotpatch payload contained relative path without claude_cwd base")?;
    Ok(base.join(path))
}

fn find_watched_dir(path: &Path, watch_files: &[String]) -> Result<Option<PathBuf>> {
    let mut current = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .context("candidate path missing parent directory")?
    };
    loop {
        if watch_files.iter().any(|watch| current.join(watch).exists()) {
            return Ok(Some(current));
        }
        if !current.pop() {
            return Ok(None);
        }
    }
}

pub(crate) fn suggestion_file_path(mempal_home: &Path, dir: &Path) -> Result<PathBuf> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", dir.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    Ok(mempal_home
        .join("hotpatch")
        .join(format!("CLAUDE-{}.md", &hash[..12])))
}

pub(crate) fn append_suggestion_line(
    suggestion_path: &Path,
    canonical_dir: &Path,
    drawer_id: &str,
    line: &str,
) -> Result<bool> {
    let mut lock = FileLock::open_exclusive(suggestion_path)?;
    let file = lock.file_mut();
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek {}", suggestion_path.display()))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("failed to read {}", suggestion_path.display()))?;
    let marker = format!("[drawer:{}]", short_drawer_id(drawer_id));
    if content.contains(&marker) {
        return Ok(false);
    }

    if content.trim().is_empty() {
        content = format!(
            "# mempal hotpatch suggestions for {}\n\n<!-- managed by mempal, safe to edit — apply via `mempal hotpatch apply --dir {}` -->\n\n",
            canonical_dir.display(),
            canonical_dir.display()
        );
    } else if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(line);
    content.push('\n');

    file.set_len(0)
        .with_context(|| format!("failed to truncate {}", suggestion_path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {}", suggestion_path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write {}", suggestion_path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", suggestion_path.display()))?;
    Ok(true)
}

fn summarize(source: &str, max_chars: usize) -> String {
    let first_line = source
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(source);
    let trimmed = first_line
        .trim()
        .trim_start_matches('#')
        .trim()
        .trim_start_matches("Decision:")
        .trim_start_matches("decision:")
        .trim_start_matches("Pivot:")
        .trim_start_matches("pivot:")
        .trim();
    let preview = preview::truncate(trimmed, max_chars);
    preview.content
}

fn classify_topic(flags: &[String], source: &str) -> &'static str {
    if flags.iter().any(|flag| flag == "PIVOT") || source.to_ascii_lowercase().contains("pivot") {
        return "pivot";
    }
    if flags.iter().any(|flag| flag == "DECISION")
        || source.to_ascii_lowercase().contains("decision")
    {
        return "decision";
    }
    "note"
}

fn stars(importance: i32) -> String {
    let clamped = importance.clamp(1, 5) as usize;
    "★".repeat(clamped)
}

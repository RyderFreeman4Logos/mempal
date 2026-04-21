use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::core::{config::Config, utils::current_timestamp};
use crate::hotpatch::FileLock;

use super::generator::suggestion_file_path;

#[derive(Debug, Clone)]
pub struct ReviewOptions {
    pub dir: Option<PathBuf>,
    pub include_applied: bool,
    pub include_dismissed: bool,
}

#[derive(Debug, Clone)]
pub struct ApplyOptions {
    pub dir: PathBuf,
    pub confirm: bool,
}

#[derive(Debug, Clone)]
pub struct DismissOptions {
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub older_than: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReviewReport {
    pub pending_count: usize,
    pub entries: Vec<SuggestionEntry>,
    pub stdout: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied_count: usize,
    pub stdout: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DismissReport {
    pub dismissed_count: usize,
    pub stdout: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanReport {
    pub removed_files: usize,
    pub stdout: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestionEntry {
    pub dir: PathBuf,
    pub suggestion_file: PathBuf,
    pub line: String,
    pub marker: SuggestionMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionMarker {
    Pending,
    Applied,
    Dismissed,
}

pub fn review(config: &Config, mempal_home: &Path, options: ReviewOptions) -> Result<ReviewReport> {
    let filter_dir = options
        .dir
        .as_deref()
        .map(|dir| dir.canonicalize())
        .transpose()
        .with_context(|| {
            options
                .dir
                .as_ref()
                .map(|dir| format!("failed to canonicalize {}", dir.display()))
                .unwrap_or_else(|| "failed to canonicalize review directory".to_string())
        })?;
    let mut entries = load_entries(mempal_home)?;
    if let Some(filter_dir) = filter_dir.as_deref() {
        entries.retain(|entry| entry.dir == filter_dir);
    }
    entries.retain(|entry| match entry.marker {
        SuggestionMarker::Pending => true,
        SuggestionMarker::Applied => options.include_applied,
        SuggestionMarker::Dismissed => options.include_dismissed,
    });
    let pending_count = entries
        .iter()
        .filter(|entry| entry.marker == SuggestionMarker::Pending)
        .count();

    let mut stdout = String::new();
    if entries.is_empty() {
        stdout.push_str("no hotpatch suggestions\n");
    } else {
        for entry in &entries {
            stdout.push_str(&format!(
                "{}\n  suggestion_file: {}\n  {}\n",
                entry.dir.display(),
                entry.suggestion_file.display(),
                entry.line
            ));
        }
        stdout.push_str(&format!("pending={pending_count}\n"));
    }

    let _ = config;
    Ok(ReviewReport {
        pending_count,
        entries,
        stdout,
    })
}

pub fn apply(config: &Config, mempal_home: &Path, options: ApplyOptions) -> Result<ApplyReport> {
    let canonical_dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", options.dir.display()))?;
    let suggestion_path = suggestion_file_path(mempal_home, &canonical_dir)?;
    if !suggestion_path.exists() {
        return Ok(ApplyReport {
            applied_count: 0,
            stdout: format!("no suggestion file for {}\n", canonical_dir.display()),
        });
    }

    let mut suggestion_lock = FileLock::open_exclusive(&suggestion_path)?;
    let suggestion_file = suggestion_lock.file_mut();
    suggestion_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek {}", suggestion_path.display()))?;
    let mut suggestion_content = String::new();
    suggestion_file
        .read_to_string(&mut suggestion_content)
        .with_context(|| format!("failed to read {}", suggestion_path.display()))?;
    let parsed = parse_suggestion_file(&suggestion_path, &suggestion_content)?;
    let pending = parsed
        .entries
        .iter()
        .filter(|entry| entry.marker == SuggestionMarker::Pending)
        .cloned()
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(ApplyReport {
            applied_count: 0,
            stdout: format!("no pending suggestions for {}\n", canonical_dir.display()),
        });
    }
    if pending.len() > 10 {
        bail!(
            "refusing to append {} lines into {}: split the content per rule 034 before applying",
            pending.len(),
            canonical_dir.display()
        );
    }

    let target_path = resolve_target_file(config, &canonical_dir)?;
    let mut target_lock = FileLock::open_exclusive(&target_path)?;
    let target_file = target_lock.file_mut();
    target_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek {}", target_path.display()))?;
    let mut original = String::new();
    target_file
        .read_to_string(&mut original)
        .with_context(|| format!("failed to read {}", target_path.display()))?;

    let to_append = pending
        .iter()
        .filter(|entry| !original.contains(&entry.line))
        .map(|entry| entry.line.clone())
        .collect::<Vec<_>>();

    if !options.confirm {
        let mut stdout = format!("dry-run for {}\n", target_path.display());
        for line in &to_append {
            stdout.push_str("+ ");
            stdout.push_str(line);
            stdout.push('\n');
        }
        return Ok(ApplyReport {
            applied_count: 0,
            stdout,
        });
    }

    if !to_append.is_empty() {
        target_file
            .seek(SeekFrom::End(0))
            .with_context(|| format!("failed to seek end of {}", target_path.display()))?;
        if !original.is_empty() && !original.ends_with('\n') {
            target_file
                .write_all(b"\n")
                .with_context(|| format!("failed to write newline to {}", target_path.display()))?;
        }
        for line in &to_append {
            target_file
                .write_all(line.as_bytes())
                .with_context(|| format!("failed to append {}", target_path.display()))?;
            target_file.write_all(b"\n").with_context(|| {
                format!("failed to append newline to {}", target_path.display())
            })?;
        }
        target_file
            .flush()
            .with_context(|| format!("failed to flush {}", target_path.display()))?;
    }

    let marked = mark_entries(&suggestion_content, &pending, "applied")?;
    suggestion_file
        .set_len(0)
        .with_context(|| format!("failed to truncate {}", suggestion_path.display()))?;
    suggestion_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {}", suggestion_path.display()))?;
    suggestion_file
        .write_all(marked.as_bytes())
        .with_context(|| format!("failed to update {}", suggestion_path.display()))?;
    suggestion_file
        .flush()
        .with_context(|| format!("failed to flush {}", suggestion_path.display()))?;

    Ok(ApplyReport {
        applied_count: pending.len(),
        stdout: format!(
            "applied {} suggestion(s) to {}\n",
            pending.len(),
            target_path.display()
        ),
    })
}

pub fn dismiss(
    _config: &Config,
    mempal_home: &Path,
    options: DismissOptions,
) -> Result<DismissReport> {
    let canonical_dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", options.dir.display()))?;
    let suggestion_path = suggestion_file_path(mempal_home, &canonical_dir)?;
    let mut suggestion_lock = FileLock::open_exclusive(&suggestion_path)?;
    let suggestion_file = suggestion_lock.file_mut();
    suggestion_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to seek {}", suggestion_path.display()))?;
    let mut content = String::new();
    suggestion_file
        .read_to_string(&mut content)
        .with_context(|| format!("failed to read {}", suggestion_path.display()))?;
    let parsed = parse_suggestion_file(&suggestion_path, &content)?;
    let pending = parsed
        .entries
        .iter()
        .filter(|entry| entry.marker == SuggestionMarker::Pending)
        .cloned()
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(DismissReport {
            dismissed_count: 0,
            stdout: format!("no pending suggestions for {}\n", canonical_dir.display()),
        });
    }
    let marked = mark_entries(&content, &pending, "dismissed")?;
    suggestion_file
        .set_len(0)
        .with_context(|| format!("failed to truncate {}", suggestion_path.display()))?;
    suggestion_file
        .seek(SeekFrom::Start(0))
        .with_context(|| format!("failed to rewind {}", suggestion_path.display()))?;
    suggestion_file
        .write_all(marked.as_bytes())
        .with_context(|| format!("failed to write {}", suggestion_path.display()))?;
    suggestion_file
        .flush()
        .with_context(|| format!("failed to flush {}", suggestion_path.display()))?;
    Ok(DismissReport {
        dismissed_count: pending.len(),
        stdout: format!("dismissed {} suggestion(s)\n", pending.len()),
    })
}

pub fn clean(_config: &Config, mempal_home: &Path, options: CleanOptions) -> Result<CleanReport> {
    let threshold = parse_older_than(&options.older_than)?;
    let hotpatch_dir = mempal_home.join("hotpatch");
    if !hotpatch_dir.exists() {
        return Ok(CleanReport {
            removed_files: 0,
            stdout: "removed 0 hotpatch files\n".to_string(),
        });
    }
    let mut removed_files = 0usize;
    for entry in fs::read_dir(&hotpatch_dir)
        .with_context(|| format!("failed to read {}", hotpatch_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed = parse_suggestion_file(&path, &content)?;
        if parsed
            .entries
            .iter()
            .all(|entry| entry.marker != SuggestionMarker::Pending)
        {
            let modified = fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if modified.elapsed().unwrap_or(Duration::ZERO) >= threshold {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
                removed_files += 1;
            }
        }
    }
    Ok(CleanReport {
        removed_files,
        stdout: format!("removed {removed_files} hotpatch files\n"),
    })
}

struct SuggestionFile {
    entries: Vec<SuggestionEntry>,
}

fn load_entries(mempal_home: &Path) -> Result<Vec<SuggestionEntry>> {
    let hotpatch_dir = mempal_home.join("hotpatch");
    if !hotpatch_dir.exists() {
        return Ok(Vec::new());
    }
    let mut all = Vec::new();
    for entry in fs::read_dir(&hotpatch_dir)
        .with_context(|| format!("failed to read {}", hotpatch_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let parsed = parse_suggestion_file(&path, &content)?;
        all.extend(parsed.entries);
    }
    Ok(all)
}

fn parse_suggestion_file(path: &Path, content: &str) -> Result<SuggestionFile> {
    let header = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("suggestion file missing header")?;
    let dir = PathBuf::from(
        header
            .strip_prefix("# mempal hotpatch suggestions for ")
            .context("invalid suggestion header")?,
    );
    let canonical_dir = dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", dir.display()))?;
    let mut entries = Vec::new();
    for line in content.lines().filter(|line| line.starts_with("- ")) {
        let marker = if line.contains("<!-- applied ") {
            SuggestionMarker::Applied
        } else if line.contains("<!-- dismissed ") {
            SuggestionMarker::Dismissed
        } else {
            SuggestionMarker::Pending
        };
        entries.push(SuggestionEntry {
            dir: canonical_dir.clone(),
            suggestion_file: path.to_path_buf(),
            line: strip_marker(line),
            marker,
        });
    }
    Ok(SuggestionFile { entries })
}

fn strip_marker(line: &str) -> String {
    line.split(" <!-- ")
        .next()
        .unwrap_or(line)
        .trim_end()
        .to_string()
}

fn resolve_target_file(config: &Config, canonical_dir: &Path) -> Result<PathBuf> {
    let candidate = config
        .hotpatch
        .watch_files
        .iter()
        .map(|watch| canonical_dir.join(watch))
        .find(|path| path.exists())
        .with_context(|| {
            format!(
                "no watched CLAUDE/AGENTS/GEMINI file found in {}",
                canonical_dir.display()
            )
        })?;
    let resolved_target = fs::canonicalize(&candidate)
        .with_context(|| format!("failed to canonicalize {}", candidate.display()))?;
    ensure_allowed_target(config, &resolved_target)?;
    Ok(resolved_target)
}

fn ensure_allowed_target(config: &Config, resolved_target: &Path) -> Result<()> {
    let prefixes = allowed_prefixes(config)?;
    if prefixes
        .iter()
        .any(|prefix| resolved_target.starts_with(prefix))
    {
        return Ok(());
    }
    bail!(
        "refusing to write {} because it escapes hotpatch.allowed_target_prefixes",
        resolved_target.display()
    );
}

fn allowed_prefixes(config: &Config) -> Result<Vec<PathBuf>> {
    if !config.hotpatch.allowed_target_prefixes.is_empty() {
        return config
            .hotpatch
            .allowed_target_prefixes
            .iter()
            .map(|value| canonicalize_prefix(value))
            .collect();
    }
    let mut defaults = Vec::new();
    defaults.push(env::current_dir().context("failed to resolve current directory")?);
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        defaults.push(home.join("drafts"));
        defaults.push(home.join("s/llm"));
    }
    Ok(defaults
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect())
}

fn canonicalize_prefix(value: &str) -> Result<PathBuf> {
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        PathBuf::from(env::var_os("HOME").context("HOME missing while expanding hotpatch prefix")?)
            .join(rest)
    } else {
        PathBuf::from(value)
    };
    Ok(expanded.canonicalize().unwrap_or(expanded))
}

fn mark_entries(content: &str, entries: &[SuggestionEntry], marker: &str) -> Result<String> {
    let timestamp = current_timestamp();
    let targets = entries
        .iter()
        .map(|entry| entry.line.as_str())
        .collect::<Vec<_>>();
    let mut lines = Vec::new();
    for line in content.lines() {
        if targets.contains(&line) {
            lines.push(format!("{line} <!-- {marker} {timestamp} -->"));
        } else {
            lines.push(line.to_string());
        }
    }
    let mut output = lines.join("\n");
    if content.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

fn parse_older_than(raw: &str) -> Result<Duration> {
    let days = raw
        .strip_suffix('d')
        .context("hotpatch clean --older-than currently supports only <days>d")?
        .parse::<u64>()
        .context("failed to parse clean --older-than days")?;
    Ok(Duration::from_secs(days * 24 * 60 * 60))
}

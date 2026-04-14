//! Bidirectional cowork inbox for P8 cowork-push protocol.
//!
//! File-based ephemeral message queue between Claude Code and Codex
//! agents working in the same project (git root). Push appends a jsonl
//! entry; drain atomically renames + reads + deletes the file.
//!
//! Design: docs/specs/2026-04-14-p8-cowork-inbox-push.md
//! Spec:   specs/p8-cowork-inbox-push.spec.md

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::peek::Tool;

pub const MAX_MESSAGE_SIZE: usize = 8 * 1024;
pub const MAX_PENDING_MESSAGES: usize = 16;
pub const MAX_TOTAL_INBOX_BYTES: u64 = 32 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("message content exceeds {MAX_MESSAGE_SIZE} bytes: got {0} bytes")]
    MessageTooLarge(usize),
    #[error("invalid cwd path (contains `..` or is not absolute): {0}")]
    InvalidCwd(String),
    #[error("cannot push to self (both caller and target resolve to {0:?})")]
    SelfPush(Tool),
    #[error(
        "inbox full: {current_count} messages / {current_bytes} bytes pending \
         (limits: {MAX_PENDING_MESSAGES} messages, {MAX_TOTAL_INBOX_BYTES} bytes) — \
         partner must drain first"
    )]
    InboxFull {
        current_count: usize,
        current_bytes: u64,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub pushed_at: String,
    pub from: String,
    pub content: String,
}

/// Resolve ~/.mempal using the HOME env var. Matches the existing
/// `expand_home` pattern at src/main.rs:949-957. Used by both the CLI
/// subcommands (cowork-drain / cowork-status / cowork-install-hooks)
/// and the MCP server handler (mempal_cowork_push).
///
/// No `dirs` crate dependency — P8 explicitly promises zero new runtime deps.
pub fn mempal_home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".mempal"),
        None => PathBuf::from(".mempal"),
    }
}

/// Resolve the given cwd to a canonical "project identity" path. Walks the
/// directory tree looking for a `.git` entry (git repo root); falls back to
/// the raw cwd if no `.git` ancestor is found.
///
/// This normalizes the "Claude in repo root, Codex in src/cowork" scenario —
/// both resolve to the same project identity, so push and drain see the same
/// inbox file.
pub fn project_identity(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return cwd.to_path_buf(),
        }
    }
}

/// Encode an already-normalized project identity path into the dashed
/// filename format. Input should be the OUTPUT of `project_identity`, not
/// a raw cwd. Rejects non-absolute paths and paths containing `..`.
pub fn encode_project_identity(identity: &Path) -> Result<String, InboxError> {
    let s = identity.to_string_lossy();
    if !identity.is_absolute() || s.contains("..") {
        return Err(InboxError::InvalidCwd(s.to_string()));
    }
    Ok(s.replace('/', "-"))
}

/// Return `<mempal_home>/cowork-inbox/<target>/<encoded_project_identity>.jsonl`.
pub fn inbox_path(
    mempal_home: &Path,
    target: Tool,
    cwd: &Path,
) -> Result<PathBuf, InboxError> {
    let identity = project_identity(cwd);
    let encoded = encode_project_identity(&identity)?;
    Ok(mempal_home
        .join("cowork-inbox")
        .join(target.dir_name())
        .join(format!("{encoded}.jsonl")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn project_identity_walks_to_git_root_from_subdir() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("project-gamma");
        let subdir = repo_root.join("src").join("cowork");
        fs::create_dir_all(&subdir).unwrap();
        fs::create_dir_all(repo_root.join(".git")).unwrap();

        assert_eq!(project_identity(&subdir), repo_root);
        assert_eq!(project_identity(&repo_root), repo_root);
    }

    #[test]
    fn project_identity_falls_back_to_raw_cwd_without_git() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("no-git-dir");
        fs::create_dir_all(&plain).unwrap();

        assert_eq!(project_identity(&plain), plain);
    }

    #[test]
    fn encode_project_identity_rejects_relative_path() {
        let result = encode_project_identity(Path::new("relative/path"));
        assert!(matches!(result, Err(InboxError::InvalidCwd(_))));
    }

    #[test]
    fn encode_project_identity_rejects_parent_traversal() {
        let result = encode_project_identity(Path::new("/tmp/../etc"));
        assert!(matches!(result, Err(InboxError::InvalidCwd(_))));
    }

    #[test]
    fn encode_project_identity_replaces_slashes_with_dashes() {
        let encoded = encode_project_identity(
            Path::new("/Users/zhangalex/Work/Projects/AI/mempal"),
        )
        .unwrap();
        assert_eq!(encoded, "-Users-zhangalex-Work-Projects-AI-mempal");
    }

    #[test]
    fn mempal_home_resolves_from_home_env_var() {
        // `mempal_home()` reads `$HOME` at call time. This test verifies the
        // shape — `$HOME/.mempal` — without mutating the process env.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let resolved = mempal_home();
        assert_eq!(resolved, PathBuf::from(home).join(".mempal"));
    }

    #[test]
    fn inbox_path_composes_home_target_and_encoded_identity() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("proj");
        fs::create_dir_all(repo.join(".git")).unwrap();

        let path = inbox_path(tmp.path(), Tool::Codex, &repo).unwrap();
        assert!(path.starts_with(tmp.path().join("cowork-inbox").join("codex")));
        assert!(path.to_string_lossy().ends_with(".jsonl"));
        let encoded_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(encoded_name.contains("proj"));
    }
}

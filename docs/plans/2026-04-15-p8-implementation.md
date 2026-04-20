# P8 Cowork Inbox Push Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 mempal 新增第 9 个 MCP 工具 `mempal_cowork_push` + 配套 `mempal cowork-drain` / `cowork-status` / `cowork-install-hooks` CLI + 双向 UserPromptSubmit hook，让 agent 可以主动投递 ephemeral handoff 消息到 partner 的 inbox，partner 下一次 user turn 自动接收注入。

**Architecture:** 新模块 `src/cowork/inbox.rs` 实现 file-backed 消息队列 `~/.mempal/cowork-inbox/<target>/<git-root-encoded>.jsonl`。Push 走 MCP tool（带 prospective budget 检查），Drain 走 CLI（原子 rename winner-takes-all at-most-once）。Codex hook 通过 stdin JSON 取 cwd（`UserPromptSubmitCommandInput.cwd` 是 Codex 官方 protocol contract），Claude Code hook 通过 env var/arg 取 cwd，两端最终都经 `project_identity()` 归一化到 git root。palace.db schema 不动（v4），P6 peek_partner 零修改。

**Tech Stack:** Rust 2024 + 既有依赖（`serde` / `serde_json` / `rmcp` 1.3 / `thiserror` / `tempfile` / `tokio`）。无新 runtime dependency。git repo 检测用纯 `std::fs` 爬 ancestor，不引入 `git2` crate。

**Source Spec:** `specs/p8-cowork-inbox-push.spec.md` (lint 100%, 25 scenarios, 4 轮 Codex review 全部吸收)
**Source Design Doc:** `docs/specs/2026-04-14-p8-cowork-inbox-push.md` (v2 final)
**Baseline Commit:** `595e467` (docs-only, no code yet)

**Plan Review History**:
- v1 drafted by Claude based on the converged contract at `595e467`
- Codex plan review caught 4 findings (2 HIGH + 2 MED):
  - **HIGH #1**: plan referenced non-existent `src/cli.rs` — actual CLI is in `src/main.rs:30+` `enum Commands`. Full replace.
  - **HIGH #2**: `stdin-json` CLI test seeded inbox at `tmp/mempal-home/` but CLI resolves `$HOME/.mempal/` — path mismatch would silently return empty drain. Fixed: seed at `tmp/.mempal/` consistently with `HOME=tmp`.
  - **MED #3**: plan's MCP/CLI pseudocode referenced `dirs::home_dir()` but `dirs` is **not** in `Cargo.toml`, violating plan's "no new runtime dep" promise. Fixed: new shared helper `mempal::cowork::inbox::mempal_home()` uses `std::env::var_os("HOME")` per `src/main.rs:481, 951` precedent.
  - **MED #4**: S16.5 "exact boundary accepted" test used `overhead ≈ 60 bytes` estimate and only asserted `final_bytes <= MAX_TOTAL_INBOX_BYTES` — could not actually distinguish `>` from `>=`. Rewritten: compute probe overhead via `serde_json::to_string(&probe_msg).len() + 1` (deterministic), assert `final_bytes == MAX_TOTAL_INBOX_BYTES` (exact), and add a **complementary** "one byte over → InboxFull" assertion. Together these two assertions bracket the `>` semantic.
- All 4 findings absorbed into this plan file **before** execution begins.
- **Round-2 plan review** (Codex): caught 3 residual MED findings missed in round-1 absorb:
  - **MED #1**: round-1 `replace_all` only touched backtick-wrapped `` `src/cli.rs` `` references, missed the 3 plain `git add src/cli.rs` lines in Task 8/9/10 commit commands. Fixed: separate `replace_all` targeting `git add src/cli.rs` string.
  - **MED #2**: `Tool::dir_name()` doc comment claimed "rejects Auto" but the impl returned `"auto"`. Fixed: rewrote the doc comment to accurately describe the actual behavior — `"auto"` is dead defensive output; the push/drain paths never reach it because `partner()` returns `None` for `Auto` so `Auto` cannot flow into `inbox_path` under the current API. Kept the `"auto"` return as safe garbage for hypothetical future callers.
  - **MED #3**: S16' "crossing threshold" test used a loose `3 × 7KB push` loop that landed somewhere near 32 KB, not the spec's exact `current_bytes == 32700 ∧ current_count == 10`. Fixed: rewrote using the same serde probe technique as S16.5, landing the inbox on exactly `32700 bytes` in `10 pushes` before attempting the crossing push, then asserting the `InboxError::InboxFull` error carries exactly `{ current_count: 10, current_bytes: 32700 }` to match spec precondition verbatim.
- All 3 round-2 findings absorbed. Plan is now v2 final.

---

## Scope Sanity Check

P8 是 cowork family 的单一 push primitive + 2 个配套 CLI。虽然 scope 比 P7 略大（~600 LoC vs P7 ~320 LoC），但所有工作围绕同一个内聚的 inbox 子系统。不需要拆子项目。

## File Structure

| 文件 | 职责 |
|------|------|
| `src/cowork/inbox.rs` (new) | `InboxMessage` / `InboxError` / `MAX_*` 常量 / `mempal_home` (shared helper, reads `$HOME/.mempal` using `std::env::var_os` — NO `dirs` crate) / `project_identity` / `encode_project_identity` / `inbox_path` / `push` / `drain` / `format_plain` / `format_codex_hook_json`。含 `#[cfg(test)] mod tests` 覆盖 unit 场景 |
| `src/cowork/peek.rs` | 仅追加 `Tool::dir_name()` / `Tool::partner()` 两个纯函数辅助方法。**零逻辑变更**，不动 `peek_partner` 编排 |
| `src/cowork/mod.rs` | `pub mod inbox;` + `pub use inbox::{InboxError, InboxMessage, MAX_*};`（re-exports） |
| `src/mcp/tools.rs` | 新增 `CoworkPushRequest` / `CoworkPushResponse` DTO + rustdoc |
| `src/mcp/server.rs` | 新增 `mempal_cowork_push` tool handler（复用 P6 `client_name` 做 self-push 拒绝和 target 自动推断） |
| `src/main.rs` | 新增 `CoworkDrain` / `CoworkStatus` / `CoworkInstallHooks` 三个子命令 |
| `src/core/protocol.rs` | 追加 Rule 10 "COWORK PUSH" + 更新 TOOLS 列表 8 → 9 |
| `tests/cowork_inbox.rs` (new) | 所有 integration-level scenarios（winner-takes-all concurrent drain / P6 regression check / CLI hook graceful degrade / CLI stdin-json path 等） |

**不动**：`Cargo.toml`（无新 dep）、`src/aaak/**`、`src/core/db.rs`、`drawers` / `drawer_vectors` / `triples` 表 schema、`tests/cowork_peek.rs`、任何 P7 代码。

## Pre-Flight Facts (开工前必读)

> 开工前对照这些事实。任一条和当前源码不符就**立即停下** surface 给 author，**不要基于 stale plan 动工**。

**`src/cowork/peek.rs:14-38`**（已验证）：
- `pub enum Tool { Claude, Codex, Auto }`
- `Tool::from_str_ci(s)` 已有（case-insensitive parse from ClientInfo.name）
- `Tool::as_str(self)` 已有，返回 `"claude"` / `"codex"` / `"auto"`
- **无** `dir_name()` / **无** `partner()` —— Task 1 新增

**`src/mcp/server.rs`**（已验证）：
- line 28: `pub struct MempalMcpServer { ... }`
- line 34: `client_name: Arc<Mutex<Option<String>>>` 字段（P6 已有）
- line 467: `fn initialize` override，line 478 里把 ClientInfo.name 写入 `self.client_name` guard
- 新 handler 可直接 `self.client_name.lock().unwrap().clone()` 拿到 caller 身份

**`src/core/protocol.rs`**（已验证）：
- line 13: `pub const MEMORY_PROTOCOL: &str = r#"..."#`
- 当前最后一条 rule 是 `9. DECISION CAPTURE`（line 78）
- `TOOLS:` 列表从 line 90 开始，列出 8 个工具（最后一条 `mempal_peek_partner`）
- Task 11 在 Rule 9 后追加 Rule 10，并在 TOOLS 列表追加 `mempal_cowork_push`

**`src/main.rs`**（已验证；CLI 定义所在，**不是** `src/cli.rs`——该文件不存在）：
- line 30-33: `#[derive(Parser)] #[command(name = "mempal")] struct Cli { #[command(subcommand)] command: Commands }`
- line 37-88: `#[derive(Subcommand)] enum Commands { Init, Ingest, Search, WakeUp, Compress, Bench, Delete, Purge, Reindex, Kg, Tunnels, Taxonomy, ... }` —— Task 8/9/10 在这里追加 `CoworkDrain` / `CoworkStatus` / `CoworkInstallHooks` 三个 variant
- line 168-173: `async fn run() -> Result<()>` 通过 `Cli::parse()` + `match cli.command { ... }` dispatch 各 command —— Task 8/9/10 在这个 match 里加 3 个分支
- line 481: `env::var_os("HOME")` 解析 identity path（precedent pattern）
- line 949-957: `fn expand_home(path: &str) -> PathBuf` 辅助函数，同样用 `env::var_os("HOME")`
- **Cargo.toml 无 `dirs` dep**。Task 2 新增的 `mempal::cowork::inbox::mempal_home()` 辅助函数使用 `std::env::var_os("HOME")` 和 `src/main.rs:481/949` 的 pattern 对称

**`src/aaak/codec.rs`** / P7 structured signals：**零影响**，整个 P8 实现不 import/touch P7 代码

**`tests/cowork_peek.rs`**：**零改动**。Task 12 只是 run 它们作为回归 gate

**Codex hook schema** (从 Codex 源码 100% 验证)：
- hooks.json 形状：`{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"...","statusMessage":"..."}]}]}}`
- matcher 字段对 UserPromptSubmit **无效**（`codex-rs/hooks/src/events/user_prompt_submit.rs:65-69` + `common.rs:98`）
- stdin JSON payload schema：`UserPromptSubmitCommandInput` 在 `codex-rs/hooks/src/schema.rs:316`，字段包括 `{session_id, turn_id, transcript_path, cwd, hook_event_name, model, permission_mode, prompt}`
- **我们只读 `cwd` 字段**，其他字段缺失/错误不影响 drain 语义

**常量值**（design + spec 一致）：
- `MAX_MESSAGE_SIZE: usize = 8 * 1024` (8 KB per push)
- `MAX_PENDING_MESSAGES: usize = 16`
- `MAX_TOTAL_INBOX_BYTES: u64 = 32 * 1024` (32 KB)

**Prospective check 语义**（Task 3 必读）：
- count: `existing_count + 1 > MAX_PENDING_MESSAGES` → reject
- bytes: `existing_bytes + content.len() + 1 > MAX_TOTAL_INBOX_BYTES` → reject（+1 是 writeln! 的 `\n`）
- 严格 `>`，不是 `>=`，因为我们要允许"刚好达到上限但不越界"的 push 通过（对应 S16.5 exact-boundary accepted scenario）

---

## Task 1: Scaffold `src/cowork/inbox.rs` + `Tool` helpers + module exports

**Files:**
- Create: `src/cowork/inbox.rs` (stub)
- Modify: `src/cowork/peek.rs` (add `Tool::dir_name` / `Tool::partner`)
- Modify: `src/cowork/mod.rs` (pub mod inbox + re-exports)

**TDD approach**: 本 task 没有行为测试；纯 scaffold。通过 `cargo check` 验证编译通过 + 本任务后才能给后续 task 红灯。

- [ ] **Step 1: 创建 `src/cowork/inbox.rs` stub**

Write this exact content:

```rust
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

// Implementations added in subsequent tasks:
// - Task 2: project_identity / encode_project_identity / inbox_path
// - Task 3: push
// - Task 4: drain
// - Task 5: format_plain / format_codex_hook_json
```

- [ ] **Step 2: 给 `Tool` 加 `dir_name` / `partner` 方法**

Edit `src/cowork/peek.rs`, after the existing `impl Tool` block (after line 38), expand it to:

```rust
impl Tool {
    pub fn from_str_ci(s: &str) -> Option<Self> {
        // ... existing body unchanged ...
    }

    pub fn as_str(self) -> &'static str {
        // ... existing body unchanged ...
    }

    /// Returns the canonical directory name used under
    /// `~/.mempal/cowork-inbox/<dir_name>/`.
    ///
    /// Semantic note: `Tool::Auto` maps to `"auto"` — this is syntactically
    /// valid but should never appear in a real inbox_path call. The push/drain
    /// code paths only ever call `dir_name` on a concrete `Claude` / `Codex`
    /// value because `push` rejects self-push and `partner()` returns
    /// `None` for `Auto`, so `Auto` cannot flow into `inbox_path` in the
    /// handler or CLI. The `"auto"` return is dead defensive behavior; if
    /// a future caller somehow passes `Auto`, they'd get a
    /// `cowork-inbox/auto/` directory which is harmless garbage but
    /// unreachable under the current API.
    pub fn dir_name(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Auto => "auto",
        }
    }

    /// Returns the partner tool for push addressing.
    /// Claude → Codex, Codex → Claude, Auto → None.
    pub fn partner(self) -> Option<Self> {
        match self {
            Tool::Claude => Some(Tool::Codex),
            Tool::Codex => Some(Tool::Claude),
            Tool::Auto => None,
        }
    }
}
```

Don't delete the existing methods; extend the impl block.

- [ ] **Step 3: 更新 `src/cowork/mod.rs`**

```rust
pub mod claude;
pub mod codex;
pub mod inbox;
pub mod peek;

pub use peek::{PeekError, PeekMessage, PeekRequest, PeekResponse, Tool, peek_partner};
pub use inbox::{
    InboxError, InboxMessage, MAX_MESSAGE_SIZE, MAX_PENDING_MESSAGES, MAX_TOTAL_INBOX_BYTES,
};
```

- [ ] **Step 4: 编译检查**

Run: `cargo check --no-default-features --features model2vec`
Expected: clean build. Possibly `unused import` warnings for now — that's fine.

- [ ] **Step 5: Commit**

```bash
git add src/cowork/inbox.rs src/cowork/peek.rs src/cowork/mod.rs
git commit -m "$(cat <<'EOF'
feat(cowork): scaffold inbox module + add Tool::dir_name/partner helpers (P8 task 1)

New src/cowork/inbox.rs with empty struct + error enum + constants.
Tool enum gains dir_name() and partner() helpers for inbox path resolution
and auto target inference. Zero behavior change to peek_partner.

Spec: specs/p8-cowork-inbox-push.spec.md
Design: docs/specs/2026-04-14-p8-cowork-inbox-push.md

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `project_identity` + `encode_project_identity` + `inbox_path`

**Files:**
- Modify: `src/cowork/inbox.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/cowork/inbox.rs`

**Scenarios covered**: project identity normalization (fragment of S10 + dedicated test).

- [ ] **Step 1: 写失败的 unit test**

Append to `src/cowork/inbox.rs`:

```rust
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
        // `mempal_home()` reads `$HOME` at call time. This test just
        // verifies that it appends `.mempal` to whatever $HOME is set to.
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
        // encoded path contains repo's absolute path with slashes replaced
        let encoded_name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(encoded_name.contains("proj"));
    }
}
```

- [ ] **Step 2: 运行测试确认 FAIL**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests
```
Expected: 6 compile errors — `project_identity` / `encode_project_identity` / `inbox_path` don't exist yet.

- [ ] **Step 3: 实现 `mempal_home` + 3 个 path 函数**

Add to `src/cowork/inbox.rs` above the tests module. `mempal_home` is shared by CLI and MCP server — **do not** pull in `dirs` crate (it's not a dependency). Use `std::env::var_os("HOME")` per the existing `src/main.rs:481, 951` pattern:

```rust
/// Resolve ~/.mempal using the HOME env var. Matches the existing
/// `expand_home` pattern at src/main.rs:949-957. Used by both the CLI
/// subcommands (cowork-drain / cowork-status / cowork-install-hooks)
/// and the MCP server handler (mempal_cowork_push).
///
/// No dirs crate dependency — P8 explicitly promises zero new runtime deps.
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
```

- [ ] **Step 4: 再运行测试确认 PASS**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests
```
Expected: `test result: ok. 6 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add src/cowork/inbox.rs
git commit -m "feat(cowork): project_identity + encode + inbox_path with tests (P8 task 2)"
```

---

## Task 3: `push()` with prospective budget check + self-push rejection + size caps

**Files:**
- Modify: `src/cowork/inbox.rs`

**Scenarios covered**: S5 (MessageTooLarge), S6 (InvalidCwd), S7 (SelfPush), S15' (prospective count limit), S16' (prospective bytes crossing), S16.5 (exact boundary accepted).

- [ ] **Step 1: 写 6 个失败的 unit tests**

Append inside `#[cfg(test)] mod tests`:

```rust
fn rfc3339() -> String {
    "2026-04-15T00:00:00Z".to_string()
}

fn tmpdir_with_git() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("proj");
    fs::create_dir_all(repo.join(".git")).unwrap();
    (tmp, repo)
}

#[test]
fn push_rejects_content_over_max_size() {
    let tmp = TempDir::new().unwrap();
    let (_tmp_repo, repo) = tmpdir_with_git();
    let oversize = "x".repeat(MAX_MESSAGE_SIZE + 1);
    let err = push(
        tmp.path(), Tool::Claude, Tool::Codex, &repo, oversize, rfc3339(),
    )
    .unwrap_err();
    assert!(matches!(err, InboxError::MessageTooLarge(n) if n == MAX_MESSAGE_SIZE + 1));
}

#[test]
fn push_rejects_cwd_with_parent_traversal() {
    let tmp = TempDir::new().unwrap();
    let weird = Path::new("/tmp/../etc");
    let err = push(
        tmp.path(), Tool::Claude, Tool::Codex, weird, "x".into(), rfc3339(),
    )
    .unwrap_err();
    assert!(matches!(err, InboxError::InvalidCwd(_)));
}

#[test]
fn push_rejects_self_push() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    let err = push(
        tmp.path(), Tool::Codex, Tool::Codex, &repo, "x".into(), rfc3339(),
    )
    .unwrap_err();
    assert!(matches!(err, InboxError::SelfPush(Tool::Codex)));
}

#[test]
fn push_rejects_when_prospective_count_would_exceed_limit() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    for _ in 0..MAX_PENDING_MESSAGES {
        push(tmp.path(), Tool::Claude, Tool::Codex, &repo, "a".into(), rfc3339()).unwrap();
    }
    let err = push(
        tmp.path(), Tool::Claude, Tool::Codex, &repo, "a".into(), rfc3339(),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        InboxError::InboxFull { current_count: 16, .. }
    ));
}

#[test]
fn push_rejects_when_prospective_bytes_would_cross_limit() {
    // Spec S16' requires a precise precondition: existing_bytes == 32700 AND
    // existing_count == 10. We land on that exactly by using the same serde
    // probe technique as S16.5: compute real line overhead from
    // `serde_json::to_string(&probe)`, then synthesize a content length that
    // adds the exact bytes needed per push to reach 32700 / 10.
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();

    const TARGET_BYTES: u64 = 32_700;
    const TARGET_COUNT: usize = 10;
    // bytes per seed push = TARGET_BYTES / TARGET_COUNT = 3270 exactly
    let bytes_per_push = (TARGET_BYTES / TARGET_COUNT as u64) as usize;

    // Probe overhead for an empty-content message in bytes (serde_json line + \n).
    let probe = InboxMessage {
        pushed_at: rfc3339(),
        from: Tool::Claude.dir_name().to_string(),
        content: String::new(),
    };
    let empty_line_bytes = serde_json::to_string(&probe).unwrap().len() + 1;
    assert!(
        bytes_per_push > empty_line_bytes,
        "bytes_per_push ({bytes_per_push}) must exceed empty_line_bytes ({empty_line_bytes})"
    );
    let content_per_push = "a".repeat(bytes_per_push - empty_line_bytes);

    for _ in 0..TARGET_COUNT {
        push(
            tmp.path(),
            Tool::Claude,
            Tool::Codex,
            &repo,
            content_per_push.clone(),
            rfc3339(),
        )
        .unwrap();
    }

    // Verify we landed on the exact precondition spec S16' requires.
    let inbox = inbox_path(tmp.path(), Tool::Codex, &repo).unwrap();
    let current_bytes = fs::metadata(&inbox).unwrap().len();
    let current_count = std::fs::read_to_string(&inbox)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    assert_eq!(current_bytes, TARGET_BYTES, "precondition: current_bytes must equal 32700");
    assert_eq!(current_count, TARGET_COUNT, "precondition: current_count must equal 10");

    // Now the crossing push: content of 200 bytes, content.len() + 1 (for \n)
    // + 60+ bytes overhead > remaining 68 → crosses the 32768 threshold.
    let would_cross = "y".repeat(200);
    let err = push(
        tmp.path(),
        Tool::Claude,
        Tool::Codex,
        &repo,
        would_cross,
        rfc3339(),
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            InboxError::InboxFull {
                current_count: 10,
                current_bytes: 32_700,
            }
        ),
        "expected InboxFull with exact 10/32700 preconditions, got: {err:?}"
    );

    // File state unchanged by the rejected push.
    let after = fs::metadata(&inbox).unwrap().len();
    assert_eq!(after, TARGET_BYTES);
}

#[test]
fn push_accepts_when_prospective_bytes_exactly_at_limit_and_rejects_one_more() {
    // This test is DELIBERATELY precise: it proves `>` vs `>=` by bracketing
    // the boundary. A naïve `existing_bytes >= MAX_TOTAL_INBOX_BYTES` check
    // (the pre-round-3 bug) would fail the exact-hit case. A naïve `>=`
    // check on prospective_bytes would incorrectly reject the exact hit.
    // A correct `prospective_bytes > MAX_TOTAL_INBOX_BYTES` check passes both
    // halves of this test.

    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();

    // Step 1: compute the EXACT byte footprint of a probe message with a
    // 1-byte ASCII content, by running the same serde_json serialization
    // the production push() will do. Since InboxMessage derives Serialize
    // with default field order, this is deterministic.
    let probe = InboxMessage {
        pushed_at: rfc3339(),
        from: Tool::Claude.dir_name().to_string(),
        content: String::new(),
    };
    let probe_empty_line_bytes = serde_json::to_string(&probe).unwrap().len() as u64 + 1;
    // writeln! adds exactly 1 byte for `\n`. The serialized probe has an
    // empty content field; a message with ASCII content of length N has
    // N additional bytes in the JSON payload (no escaping needed).

    // Step 2: we want `existing_bytes + new_line_bytes == MAX_TOTAL_INBOX_BYTES`
    // where `new_line_bytes == probe_empty_line_bytes + content.len()`.
    // Strategy: seed the inbox to a known small size, then compute the exact
    // `content` length needed to land on the boundary.

    // Seed with ASCII-only payload so each seed push adds a predictable
    // number of bytes. Single seed push.
    push(
        tmp.path(),
        Tool::Claude,
        Tool::Codex,
        &repo,
        "seed-msg".into(),
        rfc3339(),
    )
    .unwrap();

    let inbox = inbox_path(tmp.path(), Tool::Codex, &repo).unwrap();
    let current_bytes = fs::metadata(&inbox).unwrap().len();

    // Bytes remaining in the budget for exactly-hit.
    let remaining = MAX_TOTAL_INBOX_BYTES - current_bytes;
    // The new line will consist of: probe_empty_line_bytes + content_len bytes.
    // We want probe_empty_line_bytes + content_len == remaining.
    // → content_len = remaining - probe_empty_line_bytes
    assert!(
        remaining > probe_empty_line_bytes,
        "not enough budget to land an exact-boundary message (remaining={remaining}, overhead={probe_empty_line_bytes})"
    );
    let content_len = (remaining - probe_empty_line_bytes) as usize;
    let exact_content = "a".repeat(content_len);

    // Step 3: push the exact-boundary message. prospective_bytes == MAX_TOTAL_INBOX_BYTES,
    // which must be ACCEPTED (the check is strict `>`, not `>=`).
    push(
        tmp.path(),
        Tool::Claude,
        Tool::Codex,
        &repo,
        exact_content,
        rfc3339(),
    )
    .unwrap();

    let final_bytes = fs::metadata(&inbox).unwrap().len();
    assert_eq!(
        final_bytes, MAX_TOTAL_INBOX_BYTES,
        "inbox MUST land exactly on the 32 KB boundary — if this assertion fails, \
         the probe overhead estimate is wrong, not a production bug"
    );

    // Step 4: pushing ONE more byte (any content) must be REJECTED.
    // This is the complementary half that actually nails the `>` vs `>=` distinction.
    let err = push(
        tmp.path(),
        Tool::Claude,
        Tool::Codex,
        &repo,
        "x".into(),
        rfc3339(),
    )
    .unwrap_err();
    assert!(
        matches!(err, InboxError::InboxFull { .. }),
        "one byte over the limit MUST be rejected, got: {err:?}"
    );

    // And the file size is unchanged by the rejected push.
    let after_rejected = fs::metadata(&inbox).unwrap().len();
    assert_eq!(after_rejected, MAX_TOTAL_INBOX_BYTES);
}
```

- [ ] **Step 2: 运行测试确认 FAIL**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::push
```
Expected: compile errors — `push` doesn't exist yet.

- [ ] **Step 3: 实现 `push`**

Add to `src/cowork/inbox.rs` above the tests module:

```rust
/// Append a message to the target agent's inbox. Enforces self-push rejection,
/// size cap, and PROSPECTIVE backpressure (checks post-append state, not
/// pre-append state — ensures MAX_TOTAL_INBOX_BYTES is a real upper bound).
pub fn push(
    mempal_home: &Path,
    caller: Tool,
    target: Tool,
    cwd: &Path,
    content: String,
    pushed_at: String,
) -> Result<(PathBuf, u64), InboxError> {
    use std::fs;
    use std::io::Write;

    if caller == target {
        return Err(InboxError::SelfPush(caller));
    }
    if content.len() > MAX_MESSAGE_SIZE {
        return Err(InboxError::MessageTooLarge(content.len()));
    }

    let path = inbox_path(mempal_home, target, cwd)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Count existing entries + bytes for prospective check.
    let (existing_count, existing_bytes) = if path.exists() {
        let content_bytes = fs::read_to_string(&path).unwrap_or_default();
        let line_count = content_bytes
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        (line_count, content_bytes.len() as u64)
    } else {
        (0, 0)
    };

    let msg = InboxMessage {
        pushed_at,
        from: caller.dir_name().to_string(),
        content,
    };
    let line = serde_json::to_string(&msg)?;
    let new_line_bytes = (line.len() as u64) + 1; // +1 for writeln's `\n`
    let prospective_count = existing_count + 1;
    let prospective_bytes = existing_bytes.saturating_add(new_line_bytes);
    if prospective_count > MAX_PENDING_MESSAGES || prospective_bytes > MAX_TOTAL_INBOX_BYTES {
        return Err(InboxError::InboxFull {
            current_count: existing_count,
            current_bytes: existing_bytes,
        });
    }

    let mut file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{line}")?;
    file.flush()?;

    let size = fs::metadata(&path)?.len();
    Ok((path, size))
}
```

- [ ] **Step 4: 运行测试确认 PASS**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::push
```
Expected: `test result: ok. 6 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add src/cowork/inbox.rs
git commit -m "feat(cowork): inbox::push with prospective budget + self-push guard (P8 task 3)"
```

---

## Task 4: `drain()` winner-takes-all at-most-once

**Files:**
- Modify: `src/cowork/inbox.rs`

**Scenarios covered**: S1 roundtrip, S2 Unicode, S3 empty drain, S4 nonexistent, S8 FIFO order, S9 one-shot, S10 per-project isolation.

- [ ] **Step 1: 写失败的 unit tests**

Append inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn drain_round_trip_preserves_content_bytes() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    let content = "hello from claude, P8 test #1".to_string();
    push(tmp.path(), Tool::Claude, Tool::Codex, &repo, content.clone(), rfc3339()).unwrap();

    let messages = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, content);
    assert_eq!(messages[0].from, "claude");
}

#[test]
fn drain_preserves_unicode_bytes_round_trip() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    let content = "决策：采用 Arc<Mutex<>> 🔒 because 'shared ownership' 需要".to_string();
    push(tmp.path(), Tool::Claude, Tool::Codex, &repo, content.clone(), rfc3339()).unwrap();

    let messages = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, content);
}

#[test]
fn drain_empty_inbox_returns_empty_vec() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    let messages = drain(tmp.path(), Tool::Claude, &repo).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn drain_nonexistent_inbox_dir_returns_empty_vec() {
    let tmp = TempDir::new().unwrap();
    // No cowork-inbox/ dir at all
    let (_t, repo) = tmpdir_with_git();
    let messages = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert!(messages.is_empty());
}

#[test]
fn drain_preserves_fifo_order() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    for i in 0..3 {
        push(
            tmp.path(), Tool::Claude, Tool::Codex, &repo,
            format!("message-{i}"), rfc3339(),
        )
        .unwrap();
    }

    let messages = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].content, "message-0");
    assert_eq!(messages[1].content, "message-1");
    assert_eq!(messages[2].content, "message-2");
}

#[test]
fn drain_is_one_shot_file_disappears() {
    let tmp = TempDir::new().unwrap();
    let (_t, repo) = tmpdir_with_git();
    push(tmp.path(), Tool::Claude, Tool::Codex, &repo, "one".into(), rfc3339()).unwrap();

    let first = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert_eq!(first.len(), 1);

    let second = drain(tmp.path(), Tool::Codex, &repo).unwrap();
    assert!(second.is_empty());

    let path = inbox_path(tmp.path(), Tool::Codex, &repo).unwrap();
    assert!(!path.exists());
}

#[test]
fn drain_is_isolated_per_distinct_project() {
    let tmp = TempDir::new().unwrap();
    let proj_a = tmp.path().join("alpha");
    let proj_b = tmp.path().join("beta");
    fs::create_dir_all(proj_a.join(".git")).unwrap();
    fs::create_dir_all(proj_b.join(".git")).unwrap();

    push(tmp.path(), Tool::Claude, Tool::Codex, &proj_a, "for alpha".into(), rfc3339()).unwrap();

    let drained = drain(tmp.path(), Tool::Codex, &proj_b).unwrap();
    assert!(drained.is_empty(), "proj-b drain must not see proj-a messages");

    let path_a = inbox_path(tmp.path(), Tool::Codex, &proj_a).unwrap();
    assert!(path_a.exists(), "proj-a inbox still present");
}
```

- [ ] **Step 2: 运行测试确认 FAIL**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::drain
```
Expected: compile errors — `drain` doesn't exist yet.

- [ ] **Step 3: 实现 `drain`**

Add to `src/cowork/inbox.rs` above tests:

```rust
/// Drain all messages from this (target, project_identity) inbox.
///
/// **At-most-once, winner-takes-all.** Two concurrent drain calls race on
/// `fs::rename(path → path.draining)`. POSIX guarantees this rename is atomic:
/// exactly one caller wins and proceeds to read+delete; the loser sees
/// `ErrorKind::NotFound` and returns an empty Vec. **Crash window**: a winner
/// crashing after rename but before delete leaves an orphaned `.draining`
/// file whose content is lost. This is an accepted tradeoff; P8 does not
/// implement crash recovery.
pub fn drain(
    mempal_home: &Path,
    target: Tool,
    cwd: &Path,
) -> Result<Vec<InboxMessage>, InboxError> {
    use std::fs;

    let path = inbox_path(mempal_home, target, cwd)?;
    let draining = path.with_extension("draining");

    match fs::rename(&path, &draining) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.into()),
    }

    let content = fs::read_to_string(&draining)?;
    let mut messages = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<InboxMessage>(trimmed) {
            messages.push(msg);
        }
    }

    // Best-effort cleanup; already read into memory.
    let _ = fs::remove_file(&draining);
    Ok(messages)
}
```

- [ ] **Step 4: 运行测试确认 PASS**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::drain
```
Expected: `test result: ok. 7 passed; 0 failed`

- [ ] **Step 5: Commit**

```bash
git add src/cowork/inbox.rs
git commit -m "feat(cowork): inbox::drain winner-takes-all at-most-once (P8 task 4)"
```

---

## Task 5: `format_plain` + `format_codex_hook_json`

**Files:**
- Modify: `src/cowork/inbox.rs`

**Scenarios covered**: S11 Codex hook JSON envelope correctness.

- [ ] **Step 1: 写失败 tests**

Append inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn format_plain_empty_messages_returns_empty_string() {
    let out = format_plain(Tool::Codex, &[]);
    assert!(out.is_empty());
}

#[test]
fn format_plain_includes_count_and_message_lines() {
    let msgs = vec![
        InboxMessage {
            pushed_at: "2026-04-15T01:00:00Z".into(),
            from: "codex".into(),
            content: "first".into(),
        },
        InboxMessage {
            pushed_at: "2026-04-15T01:01:00Z".into(),
            from: "codex".into(),
            content: "second".into(),
        },
    ];
    let out = format_plain(Tool::Codex, &msgs);
    assert!(out.contains("Partner inbox from codex"));
    assert!(out.contains("2 messages"));
    assert!(out.contains("first"));
    assert!(out.contains("second"));
    assert!(out.contains("[End partner inbox]"));
}

#[test]
fn format_codex_hook_json_wraps_plain_in_correct_envelope() {
    let msgs = vec![InboxMessage {
        pushed_at: "2026-04-15T01:00:00Z".into(),
        from: "claude".into(),
        content: "test\nwith\nnewlines and \"quotes\"".into(),
    }];
    let out = format_codex_hook_json(Tool::Claude, &msgs).unwrap();

    // Must be valid JSON
    let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        parsed["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );

    let ac = parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    // additionalContext equals the plain format output
    let expected_plain = format_plain(Tool::Claude, &msgs);
    assert_eq!(ac, expected_plain);

    // Newlines and quotes survived JSON serialization
    assert!(ac.contains("test\nwith\nnewlines"));
    assert!(ac.contains("\"quotes\""));
}

#[test]
fn format_codex_hook_json_empty_returns_empty_string() {
    let out = format_codex_hook_json(Tool::Claude, &[]).unwrap();
    assert!(out.is_empty());
}
```

- [ ] **Step 2: 确认 FAIL**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::format
```
Expected: compile errors.

- [ ] **Step 3: 实现**

Add to `src/cowork/inbox.rs`:

```rust
/// Format drained messages as plain text for prepend-to-prompt hooks.
pub fn format_plain(from: Tool, messages: &[InboxMessage]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "[Partner inbox from {} ({} message{} since last check):]\n",
        from.dir_name(),
        messages.len(),
        if messages.len() == 1 { "" } else { "s" }
    );
    for msg in messages {
        out.push_str(&format!("- {}: {}\n", msg.pushed_at, msg.content));
    }
    out.push_str("[End partner inbox]\n");
    out
}

/// Format drained messages as Codex native hook JSON envelope.
pub fn format_codex_hook_json(
    from: Tool,
    messages: &[InboxMessage],
) -> Result<String, InboxError> {
    if messages.is_empty() {
        return Ok(String::new());
    }
    let plain = format_plain(from, messages);
    let envelope = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": plain
        }
    });
    Ok(envelope.to_string())
}
```

- [ ] **Step 4: PASS**

```
cargo test --no-default-features --features model2vec --lib cowork::inbox::tests::format
```

- [ ] **Step 5: Commit**

```bash
git add src/cowork/inbox.rs
git commit -m "feat(cowork): inbox::format_plain + format_codex_hook_json (P8 task 5)"
```

---

## Task 6: Concurrent drain integration test (S13)

**Files:**
- Create: `tests/cowork_inbox.rs`

**Scenarios covered**: S13 winner-takes-all under real concurrency.

- [ ] **Step 1: 创建 integration test file + 写 S13**

Create `tests/cowork_inbox.rs`:

```rust
//! Integration tests for P8 cowork inbox push.
//!
//! Run with:
//!   cargo test --test cowork_inbox --no-default-features --features model2vec

use mempal::cowork::inbox::{drain, push, InboxMessage, MAX_PENDING_MESSAGES};
use mempal::cowork::Tool;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

fn setup_repo(tmp: &TempDir, name: &str) -> PathBuf {
    let repo = tmp.path().join(name);
    fs::create_dir_all(repo.join(".git")).unwrap();
    repo
}

#[tokio::test]
async fn concurrent_drain_is_winner_takes_all_at_most_once() {
    let tmp = TempDir::new().unwrap();
    let mempal_home = Arc::new(tmp.path().to_path_buf());
    let repo = setup_repo(&tmp, "proj");
    let repo_arc = Arc::new(repo);

    for i in 0..3 {
        push(
            &mempal_home,
            Tool::Claude,
            Tool::Codex,
            &repo_arc,
            format!("concurrent-{i}"),
            "2026-04-15T02:00:00Z".into(),
        )
        .unwrap();
    }

    let home_a = Arc::clone(&mempal_home);
    let repo_a = Arc::clone(&repo_arc);
    let home_b = Arc::clone(&mempal_home);
    let repo_b = Arc::clone(&repo_arc);

    let task_a =
        tokio::task::spawn_blocking(move || drain(&home_a, Tool::Codex, &repo_a).unwrap());
    let task_b =
        tokio::task::spawn_blocking(move || drain(&home_b, Tool::Codex, &repo_b).unwrap());

    let (a, b) = tokio::join!(task_a, task_b);
    let a_msgs: Vec<InboxMessage> = a.unwrap();
    let b_msgs: Vec<InboxMessage> = b.unwrap();

    // Exactly one task won the whole batch; the other got nothing.
    let total_received = a_msgs.len() + b_msgs.len();
    assert_eq!(total_received, 3, "both tasks combined must see all 3 messages");

    let winner_count = std::cmp::max(a_msgs.len(), b_msgs.len());
    let loser_count = std::cmp::min(a_msgs.len(), b_msgs.len());
    assert_eq!(winner_count, 3, "winner takes all 3");
    assert_eq!(loser_count, 0, "loser empty");

    // No duplicate delivery.
    let winner_contents: Vec<String> = if a_msgs.len() == 3 {
        a_msgs.iter().map(|m| m.content.clone()).collect()
    } else {
        b_msgs.iter().map(|m| m.content.clone()).collect()
    };
    assert_eq!(winner_contents, vec!["concurrent-0", "concurrent-1", "concurrent-2"]);
}
```

- [ ] **Step 2: 跑 test 确认 PASS**

```
cargo test --no-default-features --features model2vec --test cowork_inbox concurrent_drain
```
Expected: PASS。If flaky（极不可能，POSIX rename 是硬原子），重跑 3 次确认都 pass。

- [ ] **Step 3: Commit**

```bash
git add tests/cowork_inbox.rs
git commit -m "test(cowork): integration test for concurrent drain winner-takes-all (P8 task 6)"
```

---

## Task 7: MCP tool `mempal_cowork_push`

**Files:**
- Modify: `src/mcp/tools.rs` (add DTOs)
- Modify: `src/mcp/server.rs` (add handler)
- Test: inline `#[cfg(test)] mod tests` in `src/mcp/server.rs` + integration test in `tests/cowork_inbox.rs`

**Scenarios covered**: S12 (no palace.db side effects for push), S16 (MCP push without client_info rejects auto target).

- [ ] **Step 1: DTOs in tools.rs**

Append to `src/mcp/tools.rs`:

```rust
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkPushRequest {
    /// The message content to deliver. Maximum 8 KB. Short status updates,
    /// decision summaries, or drawer_id pointers. Do NOT push search results
    /// or large reasoning blocks.
    pub content: String,

    /// Target agent: "claude" or "codex". OMIT to infer partner from MCP
    /// client identity. Self-push is rejected.
    #[serde(default)]
    pub target_tool: Option<String>,

    /// Absolute filesystem path of the project cwd this push is scoped to.
    /// Internally normalized to git repo root via project_identity().
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkPushResponse {
    pub target_tool: String,
    pub inbox_path: String,
    pub pushed_at: String,
    pub inbox_size_after: u64,
}
```

- [ ] **Step 2: Handler in server.rs**

Add to `src/mcp/server.rs` inside the `#[tool_router]` `impl MempalMcpServer` block:

```rust
#[tool(
    name = "mempal_cowork_push",
    description = "Proactively deliver a short handoff message to the PARTNER agent. \
                   Partner reads it at their next UserPromptSubmit hook (NOT real-time). \
                   Use for transient handoffs too important for peek_partner and too \
                   ephemeral for mempal_ingest. Max 8 KB per message. Call mempal_ingest \
                   for decisions you want to PERSIST."
)]
async fn mempal_cowork_push(
    &self,
    Parameters(request): Parameters<CoworkPushRequest>,
) -> std::result::Result<Json<CoworkPushResponse>, ErrorData> {
    let caller_name = self.client_name.lock().unwrap().clone();
    let caller_tool = caller_name
        .as_deref()
        .and_then(Tool::from_str_ci)
        .ok_or_else(|| {
            ErrorData::invalid_params(
                "cannot infer caller tool from client info (client_name missing or unrecognized)",
                None,
            )
        })?;

    let target = match request.target_tool.as_deref() {
        Some(name) => Tool::from_str_ci(name).ok_or_else(|| {
            ErrorData::invalid_params(format!("invalid target_tool: {name}"), None)
        })?,
        None => caller_tool.partner().ok_or_else(|| {
            ErrorData::invalid_params(
                "caller tool has no partner (tool=Auto or unknown)",
                None,
            )
        })?,
    };

    let mempal_home = crate::cowork::inbox::mempal_home();
    let cwd = PathBuf::from(&request.cwd);
    let pushed_at = current_rfc3339();

    let (path, size) = crate::cowork::inbox::push(
        &mempal_home,
        caller_tool,
        target,
        &cwd,
        request.content,
        pushed_at.clone(),
    )
    .map_err(|e| ErrorData::invalid_params(e.to_string(), None))?;

    Ok(Json(CoworkPushResponse {
        target_tool: target.dir_name().to_string(),
        inbox_path: path.to_string_lossy().to_string(),
        pushed_at,
        inbox_size_after: size,
    }))
}
```

`mempal_home()` is the helper added in Task 2 at `src/cowork/inbox.rs` (uses `std::env::var_os("HOME")`, no `dirs` crate dependency). `current_rfc3339()` returns a simple timestamp string; if no such helper exists yet, add it next to the handler. Do NOT introduce `dirs` crate — the plan's "no new runtime dep" claim must hold.

- [ ] **Step 3: Also import the new types at the top**

Update the `use super::tools::{...}` block in server.rs to include `CoworkPushRequest, CoworkPushResponse`.

- [ ] **Step 4: Add integration tests**

Append to `tests/cowork_inbox.rs`:

```rust
#[tokio::test]
async fn push_and_drain_have_no_palace_db_side_effects() {
    use mempal::core::db::Database;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let drawers_before = db.drawer_count().expect("drawer count");
    let triples_before = db.triple_count().expect("triple count");
    let schema_before = db.schema_version().expect("schema version");
    assert_eq!(schema_before, 4);
    drop(db);

    let mempal_home = tmp.path().join("home");
    let repo = setup_repo(&tmp, "proj");

    for i in 0..3 {
        push(
            &mempal_home,
            Tool::Claude,
            Tool::Codex,
            &repo,
            format!("msg-{i}"),
            "2026-04-15T03:00:00Z".into(),
        )
        .unwrap();
    }
    let _ = drain(&mempal_home, Tool::Codex, &repo).unwrap();
    let _ = drain(&mempal_home, Tool::Codex, &repo).unwrap();

    let db = Database::open(&db_path).expect("reopen db");
    assert_eq!(db.drawer_count().unwrap(), drawers_before);
    assert_eq!(db.triple_count().unwrap(), triples_before);
    assert_eq!(db.schema_version().unwrap(), schema_before);
}
```

- [ ] **Step 5: Run & commit**

```
cargo test --no-default-features --features model2vec --test cowork_inbox
cargo check --no-default-features --features model2vec
```

```bash
git add src/mcp/tools.rs src/mcp/server.rs tests/cowork_inbox.rs
git commit -m "feat(mcp): mempal_cowork_push tool + no-side-effects integration test (P8 task 7)"
```

---

## Task 8: CLI `cowork-drain` subcommand

**Files:**
- Modify: `src/main.rs` (new subcommand + handler)
- Test: inline in `tests/cowork_inbox.rs`

**Scenarios covered**: S14 (graceful degrade), S17 (stdin-json happy path), S18 (stdin-json malformed 3 subcases).

- [ ] **Step 1: Add the subcommand to the Commands enum in `src/main.rs`**

Find the `Commands` enum and add:

```rust
/// Drain cowork inbox for the given target. Always exit 0 (hook graceful degrade).
CoworkDrain {
    #[arg(long)]
    target: String,

    /// Project cwd (used by Claude Code hook).
    #[arg(long, conflicts_with = "cwd_source")]
    cwd: Option<PathBuf>,

    /// Alternative cwd source (used by Codex hook). Currently only "stdin-json".
    #[arg(long, conflicts_with = "cwd")]
    cwd_source: Option<String>,

    #[arg(long, default_value = "plain")]
    format: String,
},
```

- [ ] **Step 2: Handler**

Add the handler. Graceful degrade is THE hard contract — any error exits 0 with empty stdout.

```rust
fn run_cowork_drain(
    target: String,
    cwd: Option<PathBuf>,
    cwd_source: Option<String>,
    format: String,
) -> ExitCode {
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        let target_tool = mempal::cowork::Tool::from_str_ci(&target)
            .ok_or_else(|| format!("invalid target: {target}"))?;
        let mempal_home = mempal::cowork::inbox::mempal_home();

        let resolved_cwd: PathBuf = match (cwd, cwd_source.as_deref()) {
            (Some(path), None) => path,
            (None, Some("stdin-json")) => {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                let payload: serde_json::Value = serde_json::from_str(&buf)?;
                let cwd_str = payload
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .ok_or("stdin JSON payload missing `cwd` string field")?;
                PathBuf::from(cwd_str)
            }
            (None, Some(other)) => {
                return Err(format!("unsupported --cwd-source: {other}").into());
            }
            (None, None) => return Err("must provide --cwd or --cwd-source".into()),
            (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
        };

        let messages =
            mempal::cowork::inbox::drain(&mempal_home, target_tool, &resolved_cwd)?;
        if messages.is_empty() {
            return Ok(());
        }
        let partner = target_tool
            .partner()
            .ok_or("target has no partner (auto)")?;
        let out = match format.as_str() {
            "plain" => mempal::cowork::inbox::format_plain(partner, &messages),
            "codex-hook-json" => {
                mempal::cowork::inbox::format_codex_hook_json(partner, &messages)?
            }
            _ => return Err(format!("unknown format: {format}").into()),
        };
        print!("{out}");
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("mempal cowork-drain: {e}");
    }
    ExitCode::SUCCESS // 永远 exit 0
}
```

Wire it into the main dispatch `match` of the CLI.

- [ ] **Step 3: Integration tests in `tests/cowork_inbox.rs`**

Append:

```rust
use std::process::{Command, Stdio};

fn mempal_bin() -> String {
    std::env::var("CARGO_BIN_EXE_mempal").expect("CARGO_BIN_EXE_mempal set by cargo test")
}

#[test]
fn cowork_drain_cli_graceful_degrade_when_mempal_home_missing() {
    let tmp = TempDir::new().unwrap();
    // HOME points to an empty dir with NO .mempal/ subdirectory.
    // mempal CLI will resolve mempal_home to tmp/.mempal, which doesn't exist.
    // drain must gracefully return empty stdout + exit 0, not error.

    let output = Command::new(mempal_bin())
        .args(["cowork-drain", "--target", "claude", "--cwd", "/tmp/fake-project"])
        .env("HOME", tmp.path())
        .output()
        .expect("spawn");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "stdout should be empty on graceful degrade");
}

#[tokio::test]
async fn cowork_drain_reads_cwd_from_stdin_json_codex_path() {
    let tmp = TempDir::new().unwrap();
    // CLI resolves mempal_home via HOME/.mempal (see mempal::cowork::inbox::mempal_home).
    // Set HOME=tmp AND seed inbox to tmp/.mempal — the two must agree.
    let mempal_home = tmp.path().join(".mempal");
    let repo = setup_repo(&tmp, "proj-delta");

    push(
        &mempal_home,
        Tool::Claude,
        Tool::Codex,
        &repo,
        "stdin json test".into(),
        "2026-04-15T04:00:00Z".into(),
    )
    .unwrap();

    let stdin_payload = format!(
        r#"{{"session_id":"s1","turn_id":"t1","transcript_path":null,"cwd":"{}","hook_event_name":"UserPromptSubmit","model":"gpt-5-codex","permission_mode":"workspace-write","prompt":"继续"}}"#,
        repo.display()
    );

    let mut child = Command::new(mempal_bin())
        .args([
            "cowork-drain",
            "--target",
            "codex",
            "--format",
            "codex-hook-json",
            "--cwd-source",
            "stdin-json",
        ])
        .env("HOME", tmp.path())  // mempal CLI will resolve to tmp/.mempal
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    use std::io::Write;
    child.stdin.as_mut().unwrap().write_all(stdin_payload.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    assert!(stdout_str.contains("stdin json test"));
    let parsed: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
}

#[test]
fn cowork_drain_stdin_json_malformed_payload_graceful_degrade() {
    let tmp = TempDir::new().unwrap();
    let bad_inputs = [
        "not json at all".to_string(),
        r#"{"session_id":"s","prompt":"继续"}"#.to_string(), // missing cwd
        r#"{"cwd":42}"#.to_string(),                          // wrong type
    ];
    for payload in &bad_inputs {
        let mut child = Command::new(mempal_bin())
            .args([
                "cowork-drain",
                "--target",
                "codex",
                "--format",
                "codex-hook-json",
                "--cwd-source",
                "stdin-json",
            ])
            .env("HOME", tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.as_mut().unwrap().write_all(payload.as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();

        assert_eq!(
            output.status.code(),
            Some(0),
            "malformed payload {payload:?} must exit 0"
        );
        assert!(
            output.stdout.is_empty(),
            "stdout must be empty for malformed payload {payload:?}"
        );
    }
}
```

- [ ] **Step 4: Run + commit**

```
cargo test --no-default-features --features model2vec --test cowork_inbox
```

```bash
git add src/main.rs tests/cowork_inbox.rs
git commit -m "feat(cli): mempal cowork-drain with --cwd/--cwd-source + graceful degrade (P8 task 8)"
```

---

## Task 9: CLI `cowork-status` subcommand

**Files:**
- Modify: `src/main.rs`
- Test: integration in `tests/cowork_inbox.rs`

**Scenarios covered**: S15 cowork-status lists both inboxes.

- [ ] **Step 1: Subcommand**

```rust
/// List current inbox state for both targets at the given cwd (read-only).
CoworkStatus {
    #[arg(long)]
    cwd: PathBuf,
},
```

- [ ] **Step 2: Handler**

```rust
fn run_cowork_status(cwd: PathBuf) -> ExitCode {
    let mempal_home = mempal::cowork::inbox::mempal_home();
    println!("Project: {}", cwd.display());
    println!();
    for target in [Tool::Claude, Tool::Codex] {
        let path = match mempal::cowork::inbox::inbox_path(&mempal_home, target, &cwd) {
            Ok(p) => p,
            Err(_) => {
                println!("{} inbox:  <invalid cwd>", target.dir_name());
                continue;
            }
        };
        if !path.exists() {
            println!("{} inbox:  0 messages", target.dir_name());
            continue;
        }
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let count = content.lines().filter(|l| !l.trim().is_empty()).count();
        let bytes = content.len();
        println!(
            "{} inbox:  {} message{}, {} B",
            target.dir_name(),
            count,
            if count == 1 { "" } else { "s" },
            bytes
        );
        // Preview first 3 lines
        for line in content.lines().take(3) {
            if let Ok(msg) = serde_json::from_str::<mempal::cowork::inbox::InboxMessage>(line) {
                println!("  from {} @ {}: {}", msg.from, msg.pushed_at, msg.content);
            }
        }
    }
    ExitCode::SUCCESS
}
```

- [ ] **Step 3: Integration test**

Append to `tests/cowork_inbox.rs`:

```rust
#[test]
fn cowork_status_cli_lists_both_inboxes_without_draining() {
    let tmp = TempDir::new().unwrap();
    // HOME=tmp → mempal_home resolves to tmp/.mempal, which matches our seed path.
    let mempal_home = tmp.path().join(".mempal");
    let repo = setup_repo(&tmp, "proj");

    push(&mempal_home, Tool::Codex, Tool::Claude, &repo, "for claude a".into(), "t".into()).unwrap();
    push(&mempal_home, Tool::Codex, Tool::Claude, &repo, "for claude b".into(), "t".into()).unwrap();
    push(&mempal_home, Tool::Claude, Tool::Codex, &repo, "for codex".into(), "t".into()).unwrap();

    let output = Command::new(mempal_bin())
        .args(["cowork-status", "--cwd", repo.to_str().unwrap()])
        .env("HOME", tmp.path())  // tmp/.mempal is where we seeded
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("claude inbox"));
    assert!(stdout.contains("2 messages"));
    assert!(stdout.contains("codex inbox"));
    assert!(stdout.contains("1 message"));

    // cowork-status must NOT drain
    let drained = drain(&mempal_home, Tool::Claude, &repo).unwrap();
    assert_eq!(drained.len(), 2, "cowork-status must not have drained");
}
```

- [ ] **Step 4: Commit**

```bash
git add src/main.rs tests/cowork_inbox.rs
git commit -m "feat(cli): mempal cowork-status read-only inbox inspector (P8 task 9)"
```

---

## Task 10: CLI `cowork-install-hooks` subcommand

**Files:**
- Modify: `src/main.rs`
- Test: integration in `tests/cowork_inbox.rs`

**Scenarios covered**: S17 (Claude hook script install with exec bit), S19 (Codex hooks.json nested shape).

- [ ] **Step 1: Subcommand**

```rust
/// Install cowork hooks for Claude Code (project-level) and optionally Codex (global).
CoworkInstallHooks {
    #[arg(long)]
    global_codex: bool,
},
```

- [ ] **Step 2: Handler**

```rust
fn run_cowork_install_hooks(global_codex: bool) -> ExitCode {
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        // Install Claude Code hook (project-local)
        let cwd = std::env::current_dir()?;
        let claude_dir = cwd.join(".claude/hooks");
        std::fs::create_dir_all(&claude_dir)?;
        let claude_script = claude_dir.join("user-prompt-submit.sh");
        let claude_content = r#"#!/bin/bash
# mempal cowork inbox drain — prepends partner handoff messages to user prompt
# Graceful degrade: any failure exits 0 with empty stdout
mempal cowork-drain --target claude --cwd "${CLAUDE_PROJECT_CWD:-$PWD}" 2>/dev/null || true
"#;
        std::fs::write(&claude_script, claude_content)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&claude_script)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&claude_script, perms)?;
        }
        println!("✓ installed Claude Code hook at {}", claude_script.display());

        if global_codex {
            // Use env::var_os("HOME") directly — do NOT introduce `dirs` crate.
            let home = match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h),
                None => return Err("cannot resolve $HOME env var".into()),
            };
            let codex_dir = home.join(".codex");
            std::fs::create_dir_all(&codex_dir)?;
            let hooks_path = codex_dir.join("hooks.json");

            let existing: serde_json::Value = if hooks_path.exists() {
                let s = std::fs::read_to_string(&hooks_path)?;
                serde_json::from_str(&s)?
            } else {
                serde_json::json!({ "hooks": {} })
            };
            let mut root = existing;
            if !root.is_object() {
                root = serde_json::json!({ "hooks": {} });
            }
            let hooks_obj = root
                .as_object_mut()
                .ok_or("hooks.json root is not object")?
                .entry("hooks")
                .or_insert_with(|| serde_json::json!({}));
            let hooks_obj = hooks_obj
                .as_object_mut()
                .ok_or("hooks field is not object")?;
            let event_arr = hooks_obj
                .entry("UserPromptSubmit")
                .or_insert_with(|| serde_json::json!([]));
            let event_arr = event_arr
                .as_array_mut()
                .ok_or("UserPromptSubmit is not array")?;

            // Append a new entry (don't overwrite pre-existing entries)
            event_arr.push(serde_json::json!({
                "hooks": [{
                    "type": "command",
                    "command": "mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json",
                    "statusMessage": "mempal cowork drain"
                }]
            }));

            std::fs::write(&hooks_path, serde_json::to_string_pretty(&root)?)?;
            println!("✓ merged Codex hook into {}", hooks_path.display());
        }

        println!();
        println!("Next steps:");
        println!("  1. Restart Claude Code and Codex TUI so new hooks take effect");
        println!("  2. Test: in Claude, ask it to push a test message to codex;");
        println!("     then in Codex, type anything — you should see the message prepended");

        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("mempal cowork-install-hooks: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
```

- [ ] **Step 3: Integration tests**

```rust
#[test]
fn cowork_install_hooks_writes_claude_hook_script_with_exec_bit() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(mempal_bin())
        .args(["cowork-install-hooks"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));

    let script = tmp.path().join(".claude/hooks/user-prompt-submit.sh");
    assert!(script.exists());
    let content = fs::read_to_string(&script).unwrap();
    assert!(content.contains("mempal cowork-drain"));
    assert!(content.contains("--target claude"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script).unwrap().permissions().mode();
        assert_ne!(mode & 0o100, 0, "owner execute bit must be set");
    }
}

#[test]
fn cowork_install_hooks_writes_correct_codex_hooks_json_shape() {
    let tmp = TempDir::new().unwrap();
    let fake_home = tmp.path().join("home");
    fs::create_dir_all(&fake_home).unwrap();
    let output = Command::new(mempal_bin())
        .args(["cowork-install-hooks", "--global-codex"])
        .current_dir(tmp.path())
        .env("HOME", &fake_home)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));

    let hooks_path = fake_home.join(".codex/hooks.json");
    assert!(hooks_path.exists());
    let content = fs::read_to_string(&hooks_path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();

    // Verify nested shape
    assert!(parsed["hooks"].is_object());
    assert!(parsed["hooks"]["UserPromptSubmit"].is_array());
    assert!(parsed["hooks"]["UserPromptSubmit"].as_array().unwrap().len() >= 1);

    let entry = &parsed["hooks"]["UserPromptSubmit"][0];
    assert!(entry["hooks"].is_array());
    let handler = &entry["hooks"][0];
    assert_eq!(handler["type"], "command");
    let cmd = handler["command"].as_str().unwrap();
    assert!(cmd.contains("mempal cowork-drain"));
    assert!(cmd.contains("--target codex"));
    assert!(cmd.contains("--format codex-hook-json"));
    assert!(cmd.contains("--cwd-source stdin-json"));
    assert!(!cmd.contains("$PWD"), "must not reference $PWD");

    // matcher must NOT be present (Codex ignores it anyway)
    assert!(entry.get("matcher").is_none() || entry["matcher"].is_null());
}
```

- [ ] **Step 4: Commit**

```bash
git add src/main.rs tests/cowork_inbox.rs
git commit -m "feat(cli): mempal cowork-install-hooks for Claude + Codex (P8 task 10)"
```

---

## Task 11: Protocol Rule 10 + TOOLS list update

**Files:**
- Modify: `src/core/protocol.rs`

- [ ] **Step 1: Append Rule 10 after Rule 9**

Edit `src/core/protocol.rs`, after the Rule 9 block (line 89), insert:

```
10. COWORK PUSH (proactive handoff to partner)
   Call mempal_cowork_push when YOU (the agent) want the partner agent
   to see something on their next user turn. This is a SEND primitive —
   orthogonal to mempal_peek_partner (READ live state) and mempal_ingest
   (PERSIST decisions). Typical use: partner should notice a status
   update, blocker, or in-flight decision that is too transient for a
   drawer but too important for the user to have to relay manually.

   Delivery semantics: at-next-UserPromptSubmit, NOT real-time. The
   partner's TUI does not re-render on external events; delivery
   happens when the user types their next prompt in the partner's
   session, triggering the UserPromptSubmit hook which drains the
   inbox and injects via the standard hook stdout protocol.

   Addressing: pass target_tool="claude" or target_tool="codex" to
   choose explicitly, or omit to infer partner from MCP client
   identity. Self-push (target == you) is rejected.

   When NOT to push:
   - Content you also want to persist → use mempal_ingest (drawers)
   - Trigger partner mid-turn → not supported (at-next-submit only)
   - Broadcast to multiple targets → one target per push
   - Rich content / file attachments → only plain text body (≤ 8 KB)

   On InboxFull error: STOP pushing and wait for partner to drain.
   Do NOT retry — that would just fail again.
```

- [ ] **Step 2: Update TOOLS list (line 90-98)**

Add at the end of the TOOLS list:
```
  mempal_cowork_push   — send a short handoff message to partner agent (P8)
```

Also update `mempal_peek_partner` comment if needed for consistency.

- [ ] **Step 3: Update the existing protocol.rs test**

The test at `src/core/protocol.rs:108+` currently asserts Rule 8 + Rule 9 + `mempal_peek_partner` are present. Add assertions for Rule 10 + `mempal_cowork_push`:

```rust
assert!(
    MEMORY_PROTOCOL.contains("10. COWORK PUSH"),
    "MEMORY_PROTOCOL must include Rule 10 COWORK PUSH"
);
assert!(
    MEMORY_PROTOCOL.contains("mempal_cowork_push"),
    "MEMORY_PROTOCOL must mention mempal_cowork_push in TOOLS list"
);
```

- [ ] **Step 4: Run + commit**

```
cargo test --no-default-features --features model2vec --lib core::protocol
```

```bash
git add src/core/protocol.rs
git commit -m "feat(protocol): Rule 10 COWORK PUSH + tools list 8→9 (P8 task 11)"
```

---

## Task 12: P6 regression gate + full verification sweep

**Files:** (read-only)

- [ ] **Step 1: P6 regression check**

The P6 `test_peek_partner_has_no_mempal_side_effects` and other P6 tests must still pass, untouched.

```
cargo test --no-default-features --features model2vec --test cowork_peek
```
Expected: `test result: ok. 7 passed; 0 failed`

- [ ] **Step 2: Full test suite**

```
cargo test --no-default-features --features model2vec
```
Expected: everything PASS. Count should now include:
- P6 tests (7)
- Existing lib tests + P7 signals (4 + 2 + 3 = 9)
- New P8 inbox.rs unit tests (~20+)
- New tests/cowork_inbox.rs integration (~8)

- [ ] **Step 3: Clippy**

```
cargo clippy --no-default-features --features model2vec --all-targets -- -D warnings
```
Expected: clean. `--all-targets` is **MANDATORY** (P7 taught this lesson).

- [ ] **Step 4: Rustfmt**

```
cargo fmt --check
```
Expected: clean. If any drift, `cargo fmt` and add a separate `style:` fixup commit.

- [ ] **Step 5: agent-spec lint on source spec**

```
agent-spec lint specs/p8-cowork-inbox-push.spec.md --min-score 0.7
```
Expected: 100% maintained.

- [ ] **Step 6: Tool count sanity check**

```
grep -c '#\[tool(' src/mcp/server.rs
```
Expected: `9` (was 8, +1 for `mempal_cowork_push`)

---

## Task 13: CLAUDE.md + AGENTS.md closure

**Files:**
- Modify: `CLAUDE.md`
- Modify: `AGENTS.md` (if it has a mirror spec table)

**Note**: 关于 AGENTS.md/CLAUDE.md 里既存的 +18/+18 discipline tightening dirty file，那条改动是和 P8 无关的。本 task 只追加 P8 closure 行，不改 discipline 那几行。如果 dirty file 还在 worktree，**stage 时精确到 P8 行**，不要 `git add -u` 批量。

- [ ] **Step 1: CLAUDE.md spec table**

Find "已完成的 Spec（P0-P7）" heading, change to "P0-P8", append row:

```markdown
| `specs/p8-cowork-inbox-push.spec.md` | 完成 | 双向 cowork push — `mempal_cowork_push` MCP 工具 + cowork-drain/status/install-hooks CLI + 对称 UserPromptSubmit hook 注入 |
```

Update "当前 Spec" section: 从 "P7 已完成" → "P8 已完成"

Update "实现计划" list: append
```markdown
- `docs/plans/2026-04-15-p8-implementation.md` — P8（已完成）
```

Update "MCP 工具（8 个）" table heading to "（9 个）", append row:
```markdown
| `mempal_cowork_push` | 主动投递 ephemeral handoff 到 partner inbox（at-next-turn 交付） |
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs(p8): mark P8 complete in project spec index

- Spec table: P0-P7 → P0-P8
- MCP tool count: 8 → 9 with mempal_cowork_push row
- Current Spec: P8 done
- Implementation plans: add P8 plan reference

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 3: Save a decision drawer via `mempal_ingest`**

After commit lands, call `mempal_ingest` per MEMORY_PROTOCOL Rule 4:
- wing: `mempal`
- room: `design`
- source: `docs/plans/2026-04-15-p8-implementation.md`
- importance: `4`
- content: 3-5 sentence summary of what P8 shipped: 9th MCP tool, bidirectional handoff, at-next-submit delivery, 4-round Codex review absorbed before first commit. Cite the spec + design + plan paths and the final baseline commit SHA (which will be the HEAD after this plan's execution).

**CHECK-BEFORE-WRITE**: before ingesting, `mempal_search("P8 cowork push shipped")` to make sure no duplicate drawer already exists.

---

## Post-Plan Review Gate (DO NOT SKIP)

**Stop here.** Per writing-plans skill, this plan must pass a `plan-document-reviewer` review before execution begins. Current P8 spec has 4 rounds of Codex review absorbed in commit `595e467`, so the contract is stable — but the plan itself has not been reviewed.

**When execution begins** (possibly in a later session), the executor has two options:
1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task (13 tasks total); review between tasks; fast iteration
2. **Inline Execution** — follow `superpowers:executing-plans` in a long-running session with checkpoints between every ~3 tasks

Both paths honor the bite-sized `- [ ]` steps above. Do not batch-execute multiple tasks in one commit unless a step explicitly instructs it. Task 1 must land cleanly as a scaffold commit before Task 2 starts.

## Scenario → Task mapping (traceability)

| Scenario | Filter | Task |
|---|---|---|
| S1 | test_drain_round_trip_preserves_content_bytes | Task 4 |
| S2 | test_drain_preserves_unicode_bytes_round_trip | Task 4 |
| S3 | test_drain_empty_inbox_returns_empty_vec | Task 4 |
| S4 | test_drain_nonexistent_inbox_dir_returns_empty_vec | Task 4 |
| S5 | test_push_rejects_content_over_max_size | Task 3 |
| S6 | test_push_rejects_cwd_with_parent_traversal | Task 3 |
| S7 | test_push_rejects_self_push | Task 3 |
| S8 | test_drain_preserves_fifo_order | Task 4 |
| S9 | test_drain_is_one_shot_file_disappears | Task 4 |
| S10 | test_drain_is_isolated_per_distinct_project | Task 4 |
| (project identity sub) | test_project_identity_normalizes_subdir_to_git_root | Task 2 |
| S11 | test_format_codex_hook_json_wraps_plain_in_correct_envelope | Task 5 |
| S12 | test_push_and_drain_have_no_palace_db_side_effects | Task 7 |
| S13 | test_concurrent_drain_is_winner_takes_all_at_most_once | Task 6 |
| S14 | test_cowork_drain_cli_graceful_degrade_when_mempal_home_missing | Task 8 |
| S15 | test_cowork_status_cli_lists_both_inboxes_without_draining | Task 9 |
| S16 (MCP push auto target) | test_mcp_push_without_client_info_rejects_auto_target | Task 7 |
| S17 (install Claude hook) | test_cowork_install_hooks_writes_claude_hook_script_with_exec_bit | Task 10 |
| S18 (P6 regression) | test_p6_peek_partner_suite_remains_green_after_p8_changes | Task 12 |
| (prospective count) | test_push_rejects_when_prospective_count_would_exceed_limit | Task 3 |
| (prospective crossing) | test_push_rejects_when_prospective_bytes_would_cross_limit | Task 3 |
| S16.5 (exact boundary) | test_push_accepts_when_prospective_bytes_exactly_at_limit | Task 3 |
| S17' (stdin-json happy) | test_cowork_drain_reads_cwd_from_stdin_json_codex_path | Task 8 |
| S18' (stdin-json malformed) | test_cowork_drain_stdin_json_malformed_payload_graceful_degrade | Task 8 |
| S19 (Codex hooks.json shape) | test_cowork_install_hooks_writes_correct_codex_hooks_json_shape | Task 10 |

**25 scenarios → 13 tasks → 1-to-1 mapping**. No scenario left uncovered, no test without a spec source.

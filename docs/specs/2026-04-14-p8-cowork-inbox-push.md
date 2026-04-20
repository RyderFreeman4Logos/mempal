# P8 — `mempal_cowork_push` bidirectional cowork inbox — 设计文档

**日期**：2026-04-14
**状态**：Draft — awaiting review
**前置**：
- `docs/specs/2026-04-13-cowork-peek-and-decide.md`（P6 — 为 P8 提供 `client_name` 捕获、`Tool` 枚举、自推断逻辑、cwd-scoped 过滤约定）
- `docs/specs/2026-04-13-p7-search-structured-signals.md`（P7 — 为 P8 提供 MCP 工具面的最新基线）

**关联代码路径（已读并确认 P8 相关事实）**：
- `src/mcp/server.rs:30-35`（`MempalMcpServer.client_name: Arc<Mutex<Option<String>>>`，P6 已在 `initialize` 回调中捕获，P8 直接复用）
- `src/cowork/peek.rs::infer_partner` 和 `src/cowork/peek.rs` 里的 `Tool` 枚举（P6 完整自推断逻辑）
- `/Users/zhangalex/Work/Projects/AI/codex/codex-rs/hooks/src/schema.rs:244`（`UserPromptSubmitHookSpecificOutputWire` 类型，证明 Codex 原生支持 hook 返回 `additionalContext`）
- `/Users/zhangalex/Work/Projects/AI/codex/codex-rs/hooks/src/events/user_prompt_submit.rs:290`（JSON 包络正例）

## 一句话定位

> **给 mempal 增加第 9 个 MCP 工具 `mempal_cowork_push`，允许一个 agent 主动向 partner agent 的 "inbox" 投递一条临时消息；对端下一次 UserPromptSubmit hook 触发时，mempal CLI 的 `cowork-drain` 子命令会把消息内容作为 context 注入到该 agent 的当前 turn。push 是 ephemeral 的（drain 即消），和 P6 peek（READ live）+ P7 ingest（PERSIST decisions）正交。**

## 动机

P6 交付了 peek_partner，解决了"**我想读对方当前在说什么**"。P7 交付了 structured signals，解决了"**我想让搜索结果更好消费**"。两者都是 pull 模型：接收方主动拉取。

当前**缺的是 push**：

- Claude 做完一个决策，想让 Codex **不用等人类 relay** 就能在下次开口时自然看到这个决策
- Codex 完成一个 task，想 Claude **知道该做下一步** 而不需要用户在 Claude TUI 里手动打字复述
- 这些**不值得**进 drawer（transient handoff，不是持久决策）
- 这些**不能**靠 peek（接收方不知道有什么要看，不会主动 peek）
- 这些**不能**靠"用户当 broker"（打字复述是 signature pain point）

P8 填的就是这个**主动 handoff 通道**。

## 三种访问模式的正交性

| 用途 | 工具 | 方向 | 持久性 | 接收方何时看到 |
|---|---|---|---|---|
| **读 partner 当前状态** | `mempal_peek_partner` (P6) | pull | transient | 调用时 |
| **读项目历史决策** | `mempal_search` (P0-P7) | pull | persistent（drawers 表） | 调用时 |
| **留话给 partner** | **`mempal_cowork_push` (P8)** | **push** | **ephemeral（drain 即消）** | **partner 下一次 UserPromptSubmit hook 触发时** |

Rule 10 的协议文案会把这个边界写死，避免 agent 把三者混用。

## 为什么对称架构是刚需

Codex 核心源码中我独立验证的事实：
- `codex-rs/hooks/src/schema.rs:244`：`UserPromptSubmitHookSpecificOutputWire` 是 Codex 协议里的一等类型
- `codex-rs/hooks/src/events/user_prompt_submit.rs:290`：Codex UserPromptSubmit hook 接受 stdout JSON 返回 `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"..."}}`，运行时会把 `additionalContext` 注入到当前 turn 的 model context
- `codex-rs/hooks/src/events/session_start.rs:295`：SessionStart 同形
- `codex-rs/hooks/src/events/pre_tool_use.rs:267`：错误串 `"PreToolUse hook returned unsupported additionalContext"` 反证 `additionalContext` 只在 UserPromptSubmit/SessionStart 上支持

Claude Code 侧同样支持 UserPromptSubmit 时 hook stdout prepend 到 prompt（本项目早期 skills 配置已经在用，例如当前 session 里的 Rust/dora skills routing 和 CLAUDE.md 检索纪律都走这条路径）。

**结论**：两端都有原生 hook context injection 能力。P8 可以**一次设计覆盖双向**，不用拆 P8a / P8b。

## 架构决策记录

| 决策 | 选择 | 理由 |
|---|---|---|
| 存储介质 | 文件（`~/.mempal/cowork-inbox/<target>/<encoded_project_identity>.jsonl`），**不**进 palace.db | 避免 schema v5 迁移；ephemeral 语义不适合关系型持久化；drain 即删符合 jsonl 模型 |
| 消息格式 | 每行一条 JSON `{pushed_at, from, content}` | 最小 envelope；serde 序列化/反序列化开销可忽略 |
| **Project identity 归一化** | `project_identity(cwd)` 爬 ancestor 找 `.git`，命中就用 git repo root，否则 fallback raw cwd；最终路径再 P6 风格 `/` → `-` 编码 | 修 Codex review #2：raw cwd 做 identity 会让 "Claude 在 repo root、Codex 在子目录" 永远对不到同一个 inbox；git root 归一化是同项目内路径的唯一 canonical 锚点 |
| **drain 原子性 = winner-takes-all at-most-once** | `match fs::rename(path, path.draining) { Ok → 我是 winner，read+delete；Err::NotFound → 空 Vec；Err → fail }`。POSIX 保证 rename 是原子的；race 失败方直接返回空 | 修 Codex review #3：原先的 "if !draining.exists() then rename" pseudocode 有 race 允许两个 drain 读同一份内容造成重复投递；winner-takes-all 消除 duplicate，代价是 crash window 可能丢失 .draining 孤儿文件（明确记为 accepted tradeoff） |
| push size 上限 | 每条消息 8 KB | 避免 agent 把整个 search 结果塞进 push；超限返回 `MessageTooLarge` |
| **总 inbox budget**（新） | `MAX_PENDING_MESSAGES = 16` AND `MAX_TOTAL_INBOX_BYTES = 32 KB`。push 前计算 **prospective**（append 后）总量：`existing_count + 1 > MAX_PENDING_MESSAGES` 或 `existing_bytes + new_line_bytes > MAX_TOTAL_INBOX_BYTES` 任一触发，直接返回 `InboxFull { current_count, current_bytes }`。**检查的是写后总量，不是写前状态**——这保证 `MAX_TOTAL_INBOX_BYTES` 是**真正的硬上界**而不是 high-water mark | 修 Codex review round-1 #4 + round-3 #1：只限单条 8 KB 不等于总量限制，partner 长期不 drain 可以累出几百 KB 把下次 prompt 撑爆；round-3 捕捉到 v2 的 "existing_bytes >= limit" 检查会让最后一次 push 跨过上限（32760 → 32760 + 8193 = 40953，越界 25%），必须用 prospective check 才能实际兜住 |
| 多条累积 | append 到 jsonl 末尾，保序，但受总 budget 限制 | 和 peek_partner 返回的 message 顺序策略一致 |
| self-push 拒绝 | 复用 P6 的 `client_name` capture；caller's client_info.name 和 target_tool 相同则返回 error | 和 P6 self-peek 拒绝对称；避免 Codex 给 Codex 留话这种 no-op |
| target 自动推断 | 不传 `target_tool` 时，从 `client_name` 反推 partner（Claude→Codex / Codex→Claude） | 和 P6 auto 模式对称 |
| hook stdout 格式 | CLI 支持 `--format plain` 和 `--format codex-hook-json` 两种输出 | Shell 里做 JSON escape 是脏活，让 CLI 用 serde 正确序列化 |
| palace.db | **完全不触**（drawers / triples / drawer_vectors 全部零变化） | P6 peek 的"no side effects"不变量继续 hold |
| schema 版本 | **不 bump**（仍是 v4） | P8 不引入 DB 表 |
| hook 安装 | `mempal cowork-install-hooks` 子命令 + 文档说明 | 两端安装机制不同（Claude 是 shell 脚本文件，Codex 是 `~/.codex/hooks.json` JSON 配置），封装到一个命令里 |
| 交付语义 | at-next-UserPromptSubmit，**不**是真正实时 | Codex / Claude Code 都不 watch session 文件；UserPromptSubmit 是唯一可被外部触发的 hook 边界 |
| 失败降级 | `mempal cowork-drain` 任何 error → exit 0 + 空 stdout；hook 脚本 `|| true` 兜底 | 不阻塞 user 的正常 prompt 流 |

## 数据流

**正向**：Claude push → Codex drain
```
Claude Code agent 调用 mempal MCP
    ↓
mempal_cowork_push(content, target_tool="codex", cwd="/Users/zhangalex/Work/Projects/AI/mempal")
    ↓
self-push check (client_info.name == "claude-code" ≠ "codex" → OK)
    ↓
append 一行 jsonl 到 ~/.mempal/cowork-inbox/codex/-Users-zhangalex-Work-Projects-AI-mempal.jsonl
    ↓
return CoworkPushResponse { target_tool, inbox_path, pushed_at, inbox_size_after }

[... 时间流逝 ...]

用户在 Codex TUI 里敲任意 prompt (e.g. "继续")
    ↓
Codex runtime 触发 UserPromptSubmit hook
    ↓
Codex runtime 把一条 JSON payload 写入 hook 子进程的 stdin：
  {"session_id":..,"turn_id":..,"cwd":"/Users/zhangalex/Work/Projects/AI/mempal/src/cowork",..}
    ↓
hook 命令: mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json
    ↓
mempal cowork-drain 读 stdin → parse JSON → 取 cwd 字段
    ↓
cwd 归一化到 git repo root via project_identity(cwd)
    ↓
drain: atomic rename(path, path.draining)
  - winner: read 所有行 → delete .draining → 返回 messages
  - loser: rename NotFound → 立即返回空 Vec
    ↓
stdout:
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"[Partner inbox from claude:]\n- 2026-04-14T01:00:00Z: <content>\n[End partner inbox]"}}
    ↓
Codex 解析 stdout JSON，把 additionalContext 注入到当前 turn 的 LLM context
    ↓
Codex 模型生成回复时已经"看到"Claude 留的那条消息，可以 ack / act / ignore
```

**反向**：Codex push → Claude drain（对称，只是 target 换一侧）
```
Codex → mempal_cowork_push(target_tool="claude", ...)
    ↓
~/.mempal/cowork-inbox/claude/-Users-zhangalex-Work-Projects-AI-mempal.jsonl

[...]

Claude Code 用户敲 prompt
    ↓
.claude/hooks/user-prompt-submit.sh (项目级)
    ↓
mempal cowork-drain --target claude --cwd "$CLAUDE_PROJECT_CWD" --format plain
    ↓
stdout (plain text):
[Partner inbox from codex (2 messages since last check):]
- 2026-04-14T01:02:15Z: P8 task 3 done, drawer_abc
- 2026-04-14T01:03:40Z: need review on inbox.rs:80
[End partner inbox]
    ↓
Claude Code runtime prepend 到 user 的 prompt 里（和 UserPromptSubmit skills routing 同一条路径）
    ↓
Claude 模型看到 user prompt 前面带着 Codex 的留言
```

**四个必须不变的 invariant**：
1. `content` 字段字节级 round-trip 不变（push 什么内容，drain 出来完全一样，除了被包在 envelope 里）
2. 任意 push 或 drain 调用后，`palace.db` 的 `drawer_count` / `triple_count` / `schema_version` 都不变
3. drain 之后 inbox 文件消失（或变空文件），**并发两个 drain 恰好一个拿到全部消息、另一个拿到空**（at-most-once winner-takes-all，不是 "任意切分"）
4. 同一个 git repo 下的任意子目录作为 cwd，`project_identity(cwd)` 都解析到同一个 repo root，因此 push from repo root + drain from subdir 能命中同一个 inbox 文件

## 新模块：`src/cowork/inbox.rs`

```rust
//! Bidirectional cowork inbox for P8 cowork-push protocol.
//!
//! File-based ephemeral message queue between Claude Code and Codex
//! agents working in the same project (cwd). Push appends a jsonl
//! entry; drain atomically renames + reads + deletes the file.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::peek::Tool;

pub const MAX_MESSAGE_SIZE: usize = 8 * 1024;      // 8 KB per single push
pub const MAX_PENDING_MESSAGES: usize = 16;         // total queued before push backpressure
pub const MAX_TOTAL_INBOX_BYTES: u64 = 32 * 1024;   // total bytes queued before push backpressure

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("message content exceeds {MAX_MESSAGE_SIZE} bytes: got {0} bytes")]
    MessageTooLarge(usize),
    #[error("invalid cwd path (contains `..` or is not absolute): {0}")]
    InvalidCwd(String),
    #[error("cannot push to self (caller tool and target tool both resolve to {0:?})")]
    SelfPush(Tool),
    #[error("inbox is full: {current_count} messages or {current_bytes} bytes pending (limits: {MAX_PENDING_MESSAGES} messages, {MAX_TOTAL_INBOX_BYTES} bytes) — partner must drain first")]
    InboxFull { current_count: usize, current_bytes: u64 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub pushed_at: String,      // RFC3339 UTC
    pub from: String,           // "claude" | "codex"
    pub content: String,
}

/// Resolve the given cwd to a canonical "project identity" path. Tries to
/// walk up the directory tree looking for a `.git` directory (git repo root);
/// falls back to the raw cwd if no `.git` ancestor is found.
///
/// This normalizes the "Claude in repo root, Codex in src/cowork" scenario —
/// both resolve to the same project identity, so push and drain see the same
/// inbox file. Without this step, sub-directory sessions would get isolated
/// inboxes and handoff would silently fail to deliver.
///
/// Not an expensive operation: bounded by directory depth, O(depth) stat
/// calls, no fork/exec. Called once per push and once per drain.
pub fn project_identity(cwd: &Path) -> PathBuf {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return cwd.to_path_buf(),  // no .git ancestor, fallback to raw cwd
        }
    }
}

/// Encode a project identity path into the dashed filename format.
/// Input should be the OUTPUT of `project_identity`, not a raw cwd.
pub fn encode_project_identity(identity: &Path) -> Result<String, InboxError> {
    let s = identity.to_string_lossy();
    if !identity.is_absolute() || s.contains("..") {
        return Err(InboxError::InvalidCwd(s.to_string()));
    }
    Ok(s.replace('/', "-"))
}

/// Return `~/.mempal/cowork-inbox/<target>/<encoded_project_identity>.jsonl`
/// where project identity is the git repo root if cwd is inside one, else raw cwd.
pub fn inbox_path(mempal_home: &Path, target: Tool, cwd: &Path) -> Result<PathBuf, InboxError> {
    let identity = project_identity(cwd);
    let encoded = encode_project_identity(&identity)?;
    Ok(mempal_home
        .join("cowork-inbox")
        .join(target.dir_name())
        .join(format!("{encoded}.jsonl")))
}

/// Append a message to the target agent's inbox for this project identity.
///
/// Backpressure semantics: if the inbox already has ≥ MAX_PENDING_MESSAGES
/// entries or ≥ MAX_TOTAL_INBOX_BYTES bytes of content, this push is REJECTED
/// with `InboxError::InboxFull`. The caller must wait for partner to drain.
/// This prevents one agent from flooding the other's next-turn context.
///
/// Returns the inbox_path and new total byte size after this append.
pub fn push(
    mempal_home: &Path,
    caller: Tool,
    target: Tool,
    cwd: &Path,
    content: String,
    pushed_at: String,
) -> Result<(PathBuf, u64), InboxError> {
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

    // Backpressure check: count existing entries + bytes, then verify that
    // THIS append would not cross either upper bound. We check prospective
    // state (after append) not current state — otherwise a last-minute push
    // that starts at e.g. 32760 bytes and adds an 8 KB payload would slip
    // through the current-state check but land at 40 KB, violating the
    // contract that MAX_TOTAL_INBOX_BYTES is a real upper bound.
    let (existing_count, existing_bytes) = if path.exists() {
        let content_bytes = fs::read_to_string(&path).unwrap_or_default();
        let line_count = content_bytes.lines().filter(|l| !l.trim().is_empty()).count();
        (line_count, content_bytes.len() as u64)
    } else {
        (0, 0)
    };
    let new_line_bytes = (content.len() as u64) + 1; // +1 for the newline writeln! appends
    let prospective_bytes = existing_bytes.saturating_add(new_line_bytes);
    let prospective_count = existing_count + 1;
    if prospective_count > MAX_PENDING_MESSAGES || prospective_bytes > MAX_TOTAL_INBOX_BYTES {
        return Err(InboxError::InboxFull {
            current_count: existing_count,
            current_bytes: existing_bytes,
        });
    }

    let msg = InboxMessage {
        pushed_at,
        from: caller.dir_name().to_string(),
        content,
    };
    let line = serde_json::to_string(&msg)?;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    file.flush()?;

    let size = fs::metadata(&path)?.len();
    Ok((path, size))
}

/// Drain all messages from this (target, project-identity) inbox.
///
/// **Delivery semantics: at-most-once, winner-takes-all.**
///
/// Two concurrent drain calls race on `fs::rename(path → path.draining)`. POSIX
/// guarantees this rename is atomic: exactly one caller wins, the loser sees
/// the source path gone and returns empty. The winner reads + deletes. Neither
/// caller ever reads duplicate content.
///
/// **Crash window**: if the winner crashes AFTER rename but BEFORE the delete,
/// the `.draining` file is orphaned and its content is lost forever — at-most-
/// once means "never duplicate delivery, may lose on crash". This is an
/// accepted tradeoff: handoff messages are ephemeral by design, and the sender
/// can observe `inbox_size_after` in the push response to detect when a
/// partner isn't draining. P8 does NOT implement crash recovery (orphaned
/// .draining scan on startup). If this becomes a real pain point, a future
/// spec can add it.
pub fn drain(
    mempal_home: &Path,
    target: Tool,
    cwd: &Path,
) -> Result<Vec<InboxMessage>, InboxError> {
    let path = inbox_path(mempal_home, target, cwd)?;
    let draining = path.with_extension("draining");

    // Atomic rename race: only the winner proceeds to read+delete.
    // On POSIX, rename(2) is atomic and atomically moves the inode.
    match fs::rename(&path, &draining) {
        Ok(_) => { /* winner: fall through to read + delete */ }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Loser, or first-time call with no inbox yet.
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
        // Skip malformed lines rather than failing the whole drain.
        if let Ok(msg) = serde_json::from_str::<InboxMessage>(trimmed) {
            messages.push(msg);
        }
    }

    // If delete fails (e.g. permissions, concurrent fs ops), log but don't
    // fail the drain — the messages are already safely in `messages` and the
    // worst case is a leftover orphan file (which next drain will also ignore
    // since path no longer exists; only a future push would see stale state).
    let _ = fs::remove_file(&draining);
    Ok(messages)
}

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
pub fn format_codex_hook_json(from: Tool, messages: &[InboxMessage]) -> Result<String, InboxError> {
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

对应地，`Tool` 需要加一个 `dir_name()` 方法（`claude` / `codex`）——P6 里 `Tool` 已经有 `claude` / `codex` / `auto` 三种值。

## MCP 工具签名：`mempal_cowork_push`

**`src/mcp/tools.rs`** 新增：

```rust
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkPushRequest {
    /// The message content to deliver. Maximum 8 KB. Typical content:
    /// a short status update, decision summary, or pointer to a drawer_id
    /// that the partner should be aware of. Do NOT push search results,
    /// large reasoning blocks, or file contents.
    pub content: String,

    /// Target agent: "claude" or "codex". OMIT to let mempal infer the
    /// partner from your MCP client identity (Claude Code ↔ Codex).
    /// Passing your own tool name is rejected as self-push.
    pub target_tool: Option<String>,

    /// Absolute filesystem path of the project cwd this push is scoped
    /// to. Partner's drain is filtered by this cwd, so cross-project
    /// pushes don't leak into unrelated Claude Code / Codex sessions.
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

**`src/mcp/server.rs`** 新增 handler（约 45 LoC）：

```rust
#[tool(
    name = "mempal_cowork_push",
    description = "Proactively deliver a short handoff message to the PARTNER agent. \
                   Partner reads it at their next UserPromptSubmit hook (NOT real-time). \
                   Use for transient handoffs that are too important for peek_partner and \
                   too ephemeral for mempal_ingest. Message max 8 KB. Call mempal_ingest \
                   for decisions you want to PERSIST."
)]
async fn mempal_cowork_push(
    &self,
    Parameters(request): Parameters<CoworkPushRequest>,
) -> std::result::Result<Json<CoworkPushResponse>, ErrorData> {
    let caller_name = self.client_name.lock().unwrap().clone();
    let caller_tool = Tool::infer_from_client_name(caller_name.as_deref())
        .ok_or_else(|| ErrorData::invalid_params("cannot infer caller tool from client info", None))?;

    let target = match request.target_tool.as_deref() {
        Some(name) => Tool::from_name(name)
            .ok_or_else(|| ErrorData::invalid_params(format!("invalid target_tool: {name}"), None))?,
        None => caller_tool.partner()
            .ok_or_else(|| ErrorData::invalid_params("caller has no partner (tool=auto)", None))?,
    };

    let mempal_home = self.resolve_mempal_home();
    let cwd = PathBuf::from(request.cwd);
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

self-push 拒绝在 `inbox::push` 里。`Tool::infer_from_client_name` 和 `Tool::partner()` 是 P6 里已有的辅助函数（如果还没有就 P8 里新增）。

## CLI：`mempal cowork-drain`

**`src/cli.rs`** 新增子命令：

```rust
/// Drain this agent's cowork inbox for the given cwd (ephemeral handoff messages).
/// Intended to be called from a UserPromptSubmit hook on each user turn.
/// Always exits 0 (even on error), so hook failure never blocks the user's prompt.
CoworkDrain {
    /// Which agent's inbox to drain ("claude" or "codex"). Normally "$MY_TOOL".
    #[arg(long)]
    target: String,

    /// Project cwd this drain is scoped to. Exactly ONE of --cwd or
    /// --cwd-source must be provided.
    ///
    /// Use --cwd <path> for Claude Code hook (caller passes explicit path,
    /// usually $CLAUDE_PROJECT_CWD or $PWD).
    #[arg(long, conflicts_with = "cwd_source")]
    cwd: Option<PathBuf>,

    /// Alternative cwd source for hooks whose runtime provides a structured
    /// input payload. Currently supported: "stdin-json" (reads stdin as a
    /// JSON object and extracts the `cwd` string field, per Codex's
    /// `UserPromptSubmitCommandInput` schema at
    /// `codex-rs/hooks/src/schema.rs:316`).
    #[arg(long, conflicts_with = "cwd")]
    cwd_source: Option<String>,

    /// Output format: "plain" for Claude Code hook (prepend to prompt),
    /// or "codex-hook-json" for Codex native hook (wrap in hookSpecificOutput).
    #[arg(long, default_value = "plain")]
    format: String,
}
```

Handler：

```rust
fn run_cowork_drain(
    target: String,
    cwd: Option<PathBuf>,
    cwd_source: Option<String>,
    format: String,
) -> ExitCode {
    // Catch any error and exit 0 with empty stdout — hook graceful degrade.
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        let target_tool = Tool::from_name(&target).ok_or("invalid target")?;
        let mempal_home = resolve_mempal_home();

        // Resolve cwd from exactly one of --cwd or --cwd-source stdin-json.
        let resolved_cwd: PathBuf = match (cwd, cwd_source.as_deref()) {
            (Some(path), None) => path,
            (None, Some("stdin-json")) => {
                // Codex UserPromptSubmit hook passes a JSON payload on stdin
                // containing a `cwd` field. Parse it and use the declared
                // session cwd instead of relying on $PWD (which is a runtime
                // side-effect, not a protocol contract — see v2 round-2
                // rationale in the Codex hook section above).
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
                return Err(format!("unsupported --cwd-source value: {other}").into());
            }
            (None, None) => return Err("must provide --cwd or --cwd-source".into()),
            (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this combination"),
        };

        let messages = mempal::cowork::inbox::drain(&mempal_home, target_tool, &resolved_cwd)?;
        if messages.is_empty() {
            return Ok(());
        }
        let out = match format.as_str() {
            "plain" => mempal::cowork::inbox::format_plain(target_tool.partner().unwrap(), &messages),
            "codex-hook-json" => mempal::cowork::inbox::format_codex_hook_json(
                target_tool.partner().unwrap(),
                &messages,
            )?,
            _ => return Err(format!("unknown format: {format}").into()),
        };
        print!("{out}");
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("mempal cowork-drain: {e}");
    }
    ExitCode::SUCCESS
}
```

**注意**：stdin-json 路径的错误处理**全部走 graceful degrade**——stdin 读不到 / 不是合法 JSON / 缺 `cwd` 字段 / `cwd` 值不是 string，都归一到 stderr error + exit 0 + 空 stdout。一次 hook 失效**绝不阻塞**用户 prompt 提交，这是 Codex / Claude Code 两端对称的硬约束。

## Hook 脚本

### Claude Code 侧

**`/Users/zhangalex/Work/Projects/AI/mempal/.claude/hooks/user-prompt-submit.sh`**（项目级）：

```bash
#!/bin/bash
# mempal cowork inbox drain — prepends partner handoff messages to user prompt
# Graceful degrade: any failure (mempal missing, lock contention, parse error) → exit 0 with empty stdout
mempal cowork-drain --target claude --cwd "${CLAUDE_PROJECT_CWD:-$PWD}" 2>/dev/null || true
```

文件需要 `chmod +x`。Claude Code runtime 在 UserPromptSubmit 时会执行这个脚本并把 stdout prepend 到 user prompt（和当前 session 里 Rust/dora skills routing hook 相同机制）。

### Codex 侧

**`~/.codex/hooks.json`**（用户级，P8 不支持项目级 Codex hook，因为 Codex 的 hook 发现路径是 codex home）：

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json",
            "statusMessage": "mempal cowork drain"
          }
        ]
      }
    ]
  }
}
```

**schema 事实**（从 Codex 源码 100% 独立验证，不是推断）：

- 顶层键是 `hooks`（**不**是事件名直接作为顶层）
- 事件名 `UserPromptSubmit` 使用 CamelCase（**不**是 snake_case `user_prompt_submit`）
- 每个事件下是数组，每个元素再有一个嵌套的 `hooks` 数组（双层嵌套）
- 每个 handler entry 必须有 `type: "command"` + `command: "..."` + optional `statusMessage`
- **`matcher` 字段对 `UserPromptSubmit` 事件完全无效**——Codex 有两处独立证据：
  - `codex-rs/hooks/src/events/user_prompt_submit.rs:67`：运行时明确传 `matcher_input: None` 给 dispatcher
  - `codex-rs/hooks/src/events/common.rs:98`：`matcher_pattern_for_event()` 对 UserPromptSubmit 返回 `None`
- 源码位置：`codex-rs/core/tests/suite/hooks.rs:160-170`（schema 正例）

**这意味着**：Codex 的 UserPromptSubmit hook 是**全局 hook**，user 所有 Codex session 都会触发。按项目 scope 必须在 hook 命令本身里做——通过**读取 stdin JSON 的 `cwd` 字段**。

### 为什么通过 stdin JSON 而不是 `$PWD` 取 cwd

Codex 的 UserPromptSubmit hook 给子进程**两条**可用的 cwd 信号，但两条**不是对等的 contract**：

| 信号来源 | 源码位置 | 合约层级 |
|---|---|---|
| **Stdin JSON 的 `cwd` 字段** | `codex-rs/hooks/src/schema.rs:316` 的 `UserPromptSubmitCommandInput` 结构体；运行时序列化在 `events/user_prompt_submit.rs:79-97` | **官方协议契约**——结构体是公开 schema，字段变更属于 breaking change |
| **子进程的 `$PWD` env var** | `codex-rs/hooks/src/engine/command_runner.rs:35` 的 `command.current_dir(cwd)` | **implementation detail**——`current_dir` 是 runtime 副作用，Codex 未来重构 hook 调用链路（例如改用 nohup / 自定义 env override）可以静默变掉 |

**v2 原方案用 `$PWD`** 的问题：虽然 `command_runner.rs:35` 目前会把子进程 cwd 设成 session cwd，但这不是 Codex 的 published contract；如果上游某天为了别的原因改掉 `current_dir` 调用（比如为了 sandbox / permissions），P8 会静默失效。

**v2 round-2 修正**：改走 stdin JSON 路径。`mempal cowork-drain` 支持 `--cwd-source stdin-json` 参数，读 stdin 整条 JSON，parse 出 `cwd` 字段，再走 `project_identity` 归一化。这条路径**基于 Codex 明确声明的 `UserPromptSubmitCommandInput` schema**，更稳固。

**对称性**：Claude Code hook 的 runtime 不传 stdin JSON 给 shell script，只能用 env var / arg；所以 Claude Code 侧继续用 `--cwd "${CLAUDE_PROJECT_CWD:-$PWD}"`。Codex 侧用 `--cwd-source stdin-json`。**两条路径最后都归一到同一个 `project_identity(...)` 函数**，对称性在**语义层**保留（两端最终都用 git root 作为 inbox key），在**语法层**放弃（两个 hook 协议本来就不一样）。

### Flag 设计

`mempal cowork-drain` 的 cwd 来源参数是**两种互斥 mode**：

- `--cwd <path>`：直接取 path 参数（Claude Code hook 场景）
- `--cwd-source <source>`：从声明的 source 读取。当前唯一支持的 source 是 `stdin-json`，表示 stdin 是一条 JSON 对象，parse 其 `cwd` 字段。如果未来需要更多来源（如 env var / file / stdin-raw），加新的 value 即可

必须提供其中一个；两者传就 clap `conflicts_with` 报错。

Codex 给 hook 的 stdin payload 完整字段：`{session_id, turn_id, transcript_path, cwd, hook_event_name, model, permission_mode, prompt}`（见 `codex-rs/hooks/src/schema.rs:316`），我们只读 `cwd`。任何其他字段缺失 / 不存在 / 类型错误都不影响我们。

## `mempal cowork-install-hooks` 子命令

**`src/cli.rs`** 新增：

```rust
CoworkInstallHooks {
    /// Install hooks for this project's cwd only (default).
    /// Use --global to install the Codex hook in ~/.codex/hooks.json.
    #[arg(long, default_value_t = false)]
    global_codex: bool,
}
```

行为：
1. 写 Claude Code hook 脚本到 `$PWD/.claude/hooks/user-prompt-submit.sh`，`chmod +x`。该脚本调用 `mempal cowork-drain --target claude --cwd "${CLAUDE_PROJECT_CWD:-$PWD}"`
2. 如果 `--global-codex`，**合并**（不覆盖）entry 到 `~/.codex/hooks.json` 的 `hooks.UserPromptSubmit` 数组（CamelCase + 嵌套 hooks 结构，见 Codex 侧 hook 子章节），使用标准 `{type:"command", command, statusMessage}` entry 格式。entry 的 `command` 是 `mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json`。**不写 matcher**——Codex 源码 `events/user_prompt_submit.rs:65-69` 和 `events/common.rs:98` 两处独立证据，UserPromptSubmit 的 matcher 会被完全忽略；跨项目 scope 由 `mempal cowork-drain` 从 stdin JSON 读 `cwd` 字段后 `project_identity` 归一化处理
3. 打印每一步做了什么 + 用户侧需要手动重启 Claude Code / Codex TUI 才生效

**安装命令是可选的**——用户也可以 `cat > hook && chmod +x`。安装子命令只是封装。

## Protocol Rule 10

**`src/core/protocol.rs`** 末尾追加：

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
   inbox and injects via the standard hook stdout protocol
   (Claude Code: prepend to prompt; Codex: additionalContext).

   Addressing: pass target_tool="claude" or target_tool="codex" to
   choose explicitly, or omit to let mempal infer partner from your
   MCP client identity. Self-push (target == you) is rejected.

   When NOT to push:
   - Content you also want to persist → use mempal_ingest (drawers)
   - Trigger partner mid-turn → not supported (at-next-submit only)
   - Broadcast to multiple targets → one target per push
   - Rich content / file attachments → only plain text body (≤ 8 KB)
```

Rule 0 里的工具列表从 8 个更新到 9 个。

## 测试 plan

新文件 `tests/cowork_inbox.rs`。所有测试 hermetic，使用 `tempfile::TempDir` 作为 mempal_home。

| # | Scenario | Level | 证明 |
|---|---|---|---|
| S1 | push → drain round trip（Claude push to Codex, Codex drain Codex） | integration | 最小可行交付路径 |
| S2 | push 包含 Unicode（CJK / emoji / 引号）→ drain 字节级相同 | integration | serde 序列化正确 |
| S3 | drain 空 inbox 返回空 Vec，不报错 | unit | hook no-op 正确 |
| S4 | drain 不存在的 inbox 目录返回空 Vec | unit | 首次运行 / 无 push 时不挂 |
| S5 | push 超 8 KB → MessageTooLarge error | unit | size guard |
| S6 | push 到 cwd 含 `..` → InvalidCwd error | unit | path traversal 防守 |
| S7 | self-push（caller=codex, target=codex）→ SelfPush error | unit | 对称 P6 self-peek 拒绝 |
| S8 | 3 次 push 后 drain 一次，返回顺序和 push 顺序一致 | unit | FIFO |
| S9 | drain 后文件消失，第二次 drain 同一 target 返回空 | integration | at-most-once atomicity |
| S10 | 同一 git repo 下的 repo root 和子目录 cwd，drain 命中同一 inbox 文件（project identity 归一化） | integration | 修 Codex review #2：sub-dir session 不能被隔离成不同 inbox |
| S11 | `format_codex_hook_json` 产出合法 JSON，`additionalContext` 字段内容等于 `format_plain` 输出 | unit | Codex hook envelope 正确性 |
| S12 | push 和 drain 操作不改变 palace.db 的 drawer_count / triple_count / schema_version | integration | P0 约束 + P6 invariant 延续 |
| S13 | 并发两个 drain 调用，恰好一个拿到全部 N 条，另一个拿到 0 条（winner-takes-all，不是任意切分） | integration | 修 Codex review #3：atomic rename 的 winner-loser 语义 |
| S14 | `mempal cowork-drain` CLI 在 mempal home 不存在时 exit 0 + 空 stdout | integration | hook graceful degrade |
| S15 | 已累积 16 条 pending 消息时再 push 一条 → `InboxFull` error | unit | 修 Codex review #4：push 端 backpressure（count 限制） |
| S16 | 已累积 ≥32 KB pending bytes 时再 push 一条 → `InboxFull` error | unit | 修 Codex review #4：push 端 backpressure（byte 限制） |

**Happy paths**：S1, S2, S8, S11（4 条）
**Exception / invariant / isolation paths**：S3, S4, S5, S6, S7, S9, S10, S12, S13, S14, S15, S16（12 条）

12 异常/不变量 vs 4 happy，远超 spec lint "exception ≥ happy" 规则。

## 代码量估算

| 文件 | 改动 | LoC |
|---|---|---|
| `src/cowork/inbox.rs` (new) | push / drain / format_plain / format_codex_hook_json + `InboxError` enum | ~150 |
| `src/cowork/peek.rs` | `Tool::dir_name()` / `Tool::from_name()` / `Tool::partner()` 小工具（如未有） | ~20 |
| `src/cowork/mod.rs` | `pub mod inbox` + re-exports | ~3 |
| `src/mcp/tools.rs` | `CoworkPushRequest` / `CoworkPushResponse` DTOs | ~25 |
| `src/mcp/server.rs` | `mempal_cowork_push` handler | ~45 |
| `src/cli.rs` | `cowork-drain` + `cowork-install-hooks` subcommands | ~80 |
| `src/core/protocol.rs` | Rule 10 追加 + 工具列表 8→9 | ~25 |
| `tests/cowork_inbox.rs` (new) | 14 个 scenario | ~280 |
| `docs/specs/2026-04-14-p8-cowork-inbox-push.md` (this file) | 设计文档 | ~480 |
| `specs/p8-cowork-inbox-push.spec.md` | agent-spec 合约 | ~220 |
| `docs/plans/2026-04-14-p8-implementation.md` | 实施计划 | ~300 |

**Rust 代码 delta**：~320 生产 + ~280 测试 = **~600 LoC**。比 P7 略大，但和 P6 同量级。

**0 schema migration，0 新 runtime dep，0 既有测试需要修改。**

## 风险和限制

| 风险 | 缓解 |
|---|---|
| Hook 未安装 → push 静默丢失 | 提供 `mempal cowork-install-hooks` + 文档；push 本身始终 server-side 成功（除非 InboxFull），即使对端 hook 未装；`inbox_size_after` 返回给 sender 让它自己注意 |
| **并发 drain race** | POSIX `fs::rename` 保证单一 winner；loser 立即返回空 Vec。**at-most-once 不是 exactly-once**：winner 崩溃在 rename-之后 / delete-之前，`.draining` 孤儿文件的内容永久丢失——accepted tradeoff |
| **Inbox overflow 撑爆下次 prompt** | `MAX_PENDING_MESSAGES = 16` + `MAX_TOTAL_INBOX_BYTES = 32 KB` push 端 backpressure；超限返回 `InboxFull`，sender 立即知道 partner 长期没 drain |
| 超大单条 push 滥用 | `MAX_MESSAGE_SIZE = 8 KB` hard cap，超限返回 `MessageTooLarge` |
| **跨子目录 project identity 不匹配** | `project_identity(cwd)` 归一化到 git repo root，fallback raw cwd。任意子目录的 cwd 都解析到同一个 identity。S10 scenario 专门验证 |
| mempal binary 不在 hook runtime PATH | hook 脚本 `|| true` 兜底；drain CLI 无论如何 exit 0 |
| Inbox 文件越积越多（对端长期不 drain） | `MAX_PENDING_MESSAGES` + `MAX_TOTAL_INBOX_BYTES` 双 backpressure；user 可以手动 `rm` 清理；未来可以加 TTL（out of scope） |
| Unicode / JSON escape 错误（尤其 Codex hook JSON envelope） | CLI 用 serde `to_string()` 而不是 shell string concat；测试 S2 + S11 双重覆盖 |
| cwd 路径里有特殊字符（空格、`[]`、`'`）| `encode_project_identity` 只做 `/` → `-` 替换，其他字符保留；OS 文件系统能处理的就能处理 |
| **`project_identity` 对非 git 仓库的项目失效** | 明确 fallback 到 raw cwd，和 P8 v1 行为一致——仅在 non-git 项目下有 sub-directory 隔离问题，这是可接受的。主要目标用户（coding agent 在 git 项目中协作）不受影响 |
| **Codex UserPromptSubmit hook 全局触发，无 cwd scoping** | Codex 源码 `user_prompt_submit.rs:67` + `common.rs:98` 两处独立证据确认 matcher 对 UserPromptSubmit 硬编码 None——所有 Codex session 都会 fire。`mempal cowork-drain --cwd-source stdin-json` 从 Codex hook stdin JSON 的 `cwd` 字段读取当前 session cwd（`schema.rs:316` 官方契约），再 `project_identity` 归一化；对非目标项目的 session drain 返回空（因 inbox 文件不存在），开销是一次 stdin parse + 文件 stat + rename 尝试，可忽略。**不**依赖 `$PWD` / `current_dir` 这类 Codex runtime 实现细节 |
| 跨机器 / 跨用户 cowork | **out of scope**。文件在 `~/.mempal/`，单机单用户 |
| P6 `test_peek_partner_has_no_mempal_side_effects` 回归 | 测试不变，P8 不写 palace.db |

## 明确不做（Out of scope）

- ❌ **真正实时 mid-turn 注入**——Codex / Claude Code runtime 都不 watch session 文件，at-next-UserPromptSubmit 已是 Codex 原生 hook 能做到的最接近实时
- ❌ **消息加密 / 鉴权**——单用户单机场景，filesystem permission 足够
- ❌ **跨机 cowork**——需要新的传输层
- ❌ **消息 TTL / retention**——drained = gone，不存历史
- ❌ **Priority levels / urgency 字段**——YAGNI
- ❌ **广播 / fanout**——一次一个 target
- ❌ **Reply / threading**——push 是 fire-and-forget，没有 reply ID
- ❌ **Rich content（tool calls / 文件附件 / 图片）**——只支持 plain text content body
- ❌ **自动触发**（Codex 自己判断何时 push）——由 agent 自己在 prompt 上下文里决定，不写自动规则
- ❌ **ingest drawer from push**——push 是 ephemeral，想持久化走 `mempal_ingest`
- ❌ **unread indicator / 状态查询 tool**——sender 拿到 `inbox_size_after` 就够了
- ❌ **Codex 项目级 hook**（`.codex/hooks.json` in project dir）——Codex 的 hook 发现路径从 `~/.codex/hooks.json` 开始，我们不改 Codex 上游

## Follow-up

P8 合入后：

1. **观察实际使用频率**：Codex 和 Claude 是否真的用 `mempal_cowork_push` handoff，还是仍然倾向于让 user relay？如果 0 使用，下一步是更严格的 protocol rule 或者 deprecate
2. **观察 hook 执行开销**：每次 UserPromptSubmit 都会 fork mempal CLI，drain 空 inbox 也要执行。在快速打字的用户场景下，累计开销如何？如果有问题，考虑 inbox sentinel（`~/.mempal/cowork-inbox/<target>/.any`）让 drain 可以 O(1) 提前退出
3. **跨 spec 的 `cowork` 家族抽象**：P6 peek + P8 push 都走 `src/cowork/`，未来可能有 P9/P10（e.g. partner status broadcast、partner capability discovery），这个家族的 module 设计要留扩展
4. **统一 handoff semantics**：如果 P8 用起来发现用户更想要"partner 完成 X 后自动 push"，那就是下一条 spec 的题目（需要在 agent 层面加行为触发）

## 开放问题（非阻塞）

1. ~~**Codex hooks.json 的 matcher schema 确切形式**~~ **已解决** (v2)：从 Codex 源码 `events/user_prompt_submit.rs:65-69` + `core/tests/suite/hooks.rs:160-170` 独立验证得出正确 schema；matcher 对 UserPromptSubmit 100% 被忽略
2. **如果 Codex 或 Claude Code 上游以后支持 session 文件 watch + live reload**，P8 的 "at-next-submit" 语义会不会需要升级？初步答案：不会，P8 保持"push 到 inbox + hook 时 drain"的语义，live reload 是正交的 upstream feature
3. **`cwd` 由 caller 传还是自动捕获？** caller 显式传 cwd，server 接收后调 `project_identity` 归一化。显式传更可靠且允许 caller 明确表达 "我要 push 到哪个项目的 inbox"
4. **crash recovery for orphaned .draining files**：如果实际使用中发现 at-most-once 的数据丢失率不可接受，后续 spec 可以加 "startup orphan scan" 恢复逻辑。P8 刻意不做

---

## v1 → v2 修订记录（2026-04-14 下午，两轮 Codex review）

基于 Codex partner review（session file `/Users/zhangalex/.codex/sessions/2026/04/14/rollout-2026-04-14T07-27-42-019d892c-1a9c-7b52-b74d-d8a418edbd9a.jsonl`），v2 经两轮修订。**Round-1** 捕捉了 4 条 findings，round-2 在 round-1 修复基础上又发现了一条 contract-purity 提升机会；两轮都在 first commit **之前**吸收完毕，提交时即是收敛后版本。

### Round-1 findings (4 条)

| Finding (severity) | v1 做法 | v2 修正 |
|---|---|---|
| **#1 (HIGH)** hooks.json schema 错 | `{user_prompt_submit: [{matcher: {cwd}, command}]}` (snake_case, matcher 对象, 无 type 字段) | `{hooks: {UserPromptSubmit: [{hooks: [{type: "command", command, statusMessage}]}]}}` (CamelCase + 嵌套, matcher 省略因为 Codex 硬编码 None) |
| **#2 (HIGH)** raw cwd 做 identity 不稳 | `encode_cwd(cwd)` 直接 `/` → `-` | `encode_project_identity(project_identity(cwd))`——先爬 ancestor 找 `.git`，命中就用 git root，否则 fallback raw cwd |
| **#3 (HIGH)** drain 原子性不 self-consistent | `if !draining.exists() then rename` 有 race；S13 写"任意切分" | `match rename { Ok → winner, NotFound → loser 空, Err → fail }`；S13 改成"one wins all, others empty"；文档诚实说明 at-most-once + crash window |
| **#4 (MED)** 无总 inbox budget | 只有单条 `MAX_MESSAGE_SIZE = 8 KB` | 新增 `MAX_PENDING_MESSAGES = 16` + `MAX_TOTAL_INBOX_BYTES = 32 KB`；push 前检查，超限返回 `InboxFull`；新增 S15 + S16 scenario |

### Round-2 finding（absorbed before first commit）

| Finding (severity) | Round-1 后的做法 | Round-2 修正 |
|---|---|---|
| **#5 (HIGH — contract purity)** round-1 v2 的 Codex hook 用 `--cwd "$PWD"` 依赖 `command_runner.rs:35` 的 `current_dir(cwd)` 副作用，这是 Codex implementation detail 而非 published contract | `$PWD` env var 取 session cwd | `mempal cowork-drain` 新增 `--cwd-source stdin-json` 参数，read stdin JSON 并提取 `cwd` 字段（`schema.rs:316` 里 `UserPromptSubmitCommandInput.cwd` 是正式声明的协议字段）。Codex hook 命令改为 `mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json`。Claude Code 侧继续用 `--cwd` 参数。两条路径最后都归一到同一个 `project_identity(...)` 函数。Flag 设计用中性命名 `--cwd-source <source>` 而非 Codex-specific 的 `--cwd-from-stdin-json` |

### Round-3 findings (4 条，absorbed into same d3376b6 via amend)

| Finding (severity) | Round-2 后的做法 | Round-3 修正 |
|---|---|---|
| **#6 (HIGH — real contract bug)** `MAX_TOTAL_INBOX_BYTES` 的检查是 "已到上限才拒绝"（`existing_bytes >= MAX_TOTAL_INBOX_BYTES`），不是 "这次 push 会不会推过上限"。反例：`existing=32760`，push 8 KB 消息 → 预检查通过 → append 后 40953 字节，超界 25%。真实上界是 `MAX_TOTAL_INBOX_BYTES + MAX_MESSAGE_SIZE = 40 KB`，不是声称的 32 KB。S16 场景也只测 "to the limit" 不测 "crossing the limit" | `if existing_bytes >= MAX_TOTAL_INBOX_BYTES { reject }` | `let prospective_bytes = existing_bytes + content.len() + 1; if prospective_bytes > MAX_TOTAL_INBOX_BYTES { reject }`。同时 count 检查也改成 `existing_count + 1 > MAX_PENDING_MESSAGES`。S16 改成 "32700 inbox + 200 bytes push → crossing threshold → 拒绝"。加 **S16.5** "精确到 32768 字节边界 → 接受" 验证 prospective 边界精度（`>` vs `>=` 的区别）|
| **#7 (MED — coverage gap)** stdin-json 失败路径在 decision 里写了 graceful degrade contract，但 S17 只测 happy path。round-2 新增的最脆弱面没有 failing case 覆盖 | 只有 happy path S17 | 加 S18 覆盖 3 个失败子用例：非 JSON 文本 / 合法 JSON 缺 cwd 字段 / cwd 字段类型错。断言 exit 0 + 空 stdout + inbox 文件未被触碰 |
| **#8 (MED — stale doc / self-contradiction)** 风险表里有一行还写着 `--cwd "$PWD"`，和 round-2 改的主要章节自相矛盾。round-2 吸收时我只 grep 改了主要章节，没全文扫 `$PWD` | 风险表 Codex UserPromptSubmit 那一行仍保留 `$PWD` 文字 | Grep 全文 `$PWD`，改掉 stale row 为 stdin-json 语义（`schema.rs:316` + `common.rs:98` 双重证据引用） |
| **#9 (MED — round-1 fix untested)** round-1 finding #1 改了 Codex hooks.json 格式，但 spec 里没有任何 scenario 验证 `mempal cowork-install-hooks --global-codex` 实际写出的 JSON 形状正确。round-1 出过错的表面继续 untested | 只有 Claude Code hook script 写入的 S 有 test | 加 S19：install-hooks --global-codex 写入 tempdir `$HOME/.codex/hooks.json`，parse JSON 后断言 nested shape 正确（`hooks.UserPromptSubmit[0].hooks[0]` + `type == "command"` + command 子串包含 `--cwd-source stdin-json` + 不含 `$PWD` + 不含 matcher 字段） |

**Round-3 rationale**：Finding #6 是真实 contract bug（`MAX_TOTAL_INBOX_BYTES` 不是真正上界），其他 3 条是 coverage / stale text 问题。4 条都 defensible，round-3 捕捉到了 round-1/round-2 没发现的东西——review loop 正常工作。修完后 amend d3376b6（local-only，没 push，amend 安全），保持 "first P8 commit = 真正收敛版" 原则和前两轮一致。

### 新增 scenario 汇总（round-2 + round-3）
- **S15'（改）** `test_push_rejected_when_prospective_count_would_exceed_limit`：count 用 prospective 语义
- **S16'（改）** `test_push_rejected_when_prospective_bytes_would_cross_limit`：bytes crossing threshold 场景
- **S16.5（新，round-3）** `test_push_accepted_at_exact_byte_limit_boundary`：精确到 32768 字节边界仍接受
- **S17（round-2）** `test_cowork_drain_reads_cwd_from_stdin_json_codex_path`：stdin-json happy path
- **S18（新，round-3）** `test_cowork_drain_stdin_json_malformed_payload_graceful_degrade`：3 个 malformed 子用例
- **S19（新，round-3）** `test_cowork_install_hooks_writes_correct_codex_hooks_json_shape`：验证 install-hooks 写出的 Codex hooks.json schema

**下一步**：agent-spec 合约 v2 同步以上修订（round-1 + round-2 + round-3）→ re-lint 100% ✅ → amend d3376b6 → 再 relay 给 Codex 做 round-4 check → 实施计划 → TDD。

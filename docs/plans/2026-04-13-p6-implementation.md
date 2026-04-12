# P6 Cowork Peek-and-Decide Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 mempal 新增一个 `mempal_peek_partner` MCP 工具，让 Claude Code 和 Codex 能通过直接读对方的 session .jsonl 文件获取 live 上下文，并在 MEMORY_PROTOCOL 里新增 Rule 8/9 明确"live 走 peek、决策走 ingest"的分工。

**Architecture:** 新增 `src/cowork/` 子模块承载 Claude / Codex 两个 session 文件 adapter 和 peek orchestrator。MCP 层（`src/mcp/tools.rs` + `src/mcp/server.rs`）注册新工具并从 rmcp 的 `initialize` 回调中捕获 `client_info.name` 用于 `auto` 模式的 partner 推断。`palace.db` schema 不变，无迁移。

**Tech Stack:** Rust 2024 + `rmcp` 1.3（MCP server）+ `serde_json`（session jsonl 解析）+ `walkdir` 0.2（Codex sessions 目录扫描，新增 dep）+ `tempfile`（test fixtures，已有 dev-dep）。

**Source Spec:** `specs/p6-cowork-peek-and-decide.spec.md`
**Source Design Doc:** `docs/specs/2026-04-13-cowork-peek-and-decide.md`

## File Structure

| 文件 | 职责 |
|------|------|
| `src/cowork/mod.rs` | 子模块入口，re-export 公开类型 |
| `src/cowork/peek.rs` | 请求/响应类型 + `peek_partner` 编排逻辑 + self-peek 拒绝 + 活跃检测 + home override 注入 |
| `src/cowork/claude.rs` | `ClaudeSessionReader` adapter：cwd 编码、jsonl 解析、消息过滤 |
| `src/cowork/codex.rs` | `CodexSessionReader` adapter：日期目录遍历、`session_meta.cwd` 过滤、jsonl 解析 |
| `src/lib.rs` | 新增 `pub mod cowork;` |
| `src/mcp/tools.rs` | `PeekPartnerRequest` / `PeekPartnerResponse` / `PeekMessageDto` DTO |
| `src/mcp/server.rs` | 新 tool `mempal_peek_partner` handler + `initialize` override 保存 `client_name` |
| `src/core/protocol.rs` | `MEMORY_PROTOCOL` 常量末尾追加 Rule 8 (PARTNER AWARENESS) 和 Rule 9 (DECISION CAPTURE) |
| `tests/cowork_peek.rs` | 集成测试（首次创建 `tests/` 目录） |
| `tests/fixtures/cowork/claude/*.jsonl` | Claude session fixture（真实 schema） |
| `tests/fixtures/cowork/codex/*.jsonl` | Codex session fixture（真实 schema） |
| `Cargo.toml` | 新增 `walkdir = "2"` 到 `[dependencies]` |

## Real-World Schema Notes（开工前必读）

**Claude Code session jsonl（`~/.claude/projects/<encoded>/*.jsonl`）**
- 每行要么是 `{"type":"permission-mode",...}`（跳过）
- 要么是消息条目：
  ```json
  {
    "parentUuid": "...",
    "isSidechain": false,
    "type": "user" | "assistant",
    "message": {
      "role": "user" | "assistant",
      "content": "纯字符串" OR [{"type": "text", "text": "..."}, {"type": "tool_use", ...}, {"type": "tool_result", ...}]
    },
    "isMeta": true | false(默认),
    "uuid": "...",
    "timestamp": "RFC3339",
    "cwd": "/abs/path",
    "sessionId": "...",
    "gitBranch": "main",
    ...
  }
  ```
- **关键规则**：
  - 只处理顶层 `type` ∈ {"user", "assistant"}
  - 跳过 `isMeta: true` 的条目（命令回显等内部）
  - `message.content` 可能是字符串也可能是数组
  - 如果是数组，拼接所有 `type: "text"` 的块；忽略 `tool_use` / `tool_result` 块
  - 如果字符串以 `<local-command-caveat>` 开头等 shell meta 模式，建议 skip（属于 cli 内部）

**Codex session jsonl（`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`）**
- 首行：`{"timestamp":"...","type":"session_meta","payload":{"id":"...","cwd":"/abs/path","originator":"codex-tui",...}}`
- 后续行有多种 `type`：
  - `response_item` with `payload.type: "message"` — **真实 user/assistant 轮次**
  - `response_item` with `payload.type: "reasoning"` — 跳过（assistant 内部思考）
  - `event_msg` with `payload.type: "user_message"` — 跳过（CLI 事件层的重复表示）
  - `event_msg` with `payload.type: "token_count"` 等其他 — 跳过
- **关键规则**：只取 `type: "response_item"` 且 `payload.type: "message"`，role 为 user/assistant，text 从 `payload.content[]` 的 `text` 字段拼接（content 块类型包括 `input_text`/`output_text`/其他）

---

## Task 1: Scaffold `src/cowork/` 模块 + 加 walkdir 依赖

**Files:**
- Create: `src/cowork/mod.rs`
- Create: `src/cowork/peek.rs`
- Create: `src/cowork/claude.rs`
- Create: `src/cowork/codex.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: 创建 cowork 模块空骨架**

Create `src/cowork/mod.rs`:
```rust
//! Cross-agent cowork: live session peek (no storage) + decision-only ingest.
//!
//! See `docs/specs/2026-04-13-cowork-peek-and-decide.md`.

pub mod claude;
pub mod codex;
pub mod peek;

pub use peek::{
    PeekError, PeekMessage, PeekRequest, PeekResponse, Tool, peek_partner,
};
```

Create `src/cowork/peek.rs`:
```rust
//! Peek request/response types + orchestration.
```

Create `src/cowork/claude.rs`:
```rust
//! Claude Code session reader.
```

Create `src/cowork/codex.rs`:
```rust
//! Codex session reader.
```

- [ ] **Step 2: 在 `src/lib.rs` 加 pub mod**

Modify `src/lib.rs` to add `pub mod cowork;` in alphabetical position after `pub mod core;`:
```rust
#![warn(clippy::all)]

pub mod aaak;
#[cfg(feature = "rest")]
pub mod api;
pub mod core;
pub mod cowork;
pub mod embed;
pub mod ingest;
pub mod mcp;
pub mod search;
```

- [ ] **Step 3: 在 `Cargo.toml` 加 walkdir**

Add `walkdir = "2"` to `[dependencies]`, alphabetically placed:
```toml
walkdir = "2"
```

- [ ] **Step 4: 验证构建**

```bash
cargo build --no-default-features --features model2vec 2>&1 | tail -20
```
Expected: 构建成功。

- [ ] **Step 5: Commit**

```bash
git add src/cowork/ src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat(cowork): scaffold cowork module + walkdir dep (P6 task 1)"
```

---

## Task 2: 定义 peek 模块的共享类型（含 home_override 注入点）

**Files:**
- Modify: `src/cowork/peek.rs`

- [ ] **Step 1: 写失败的单元测试**

Append to `src/cowork/peek.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_parses_from_str() {
        assert_eq!(Tool::from_str_ci("claude"), Some(Tool::Claude));
        assert_eq!(Tool::from_str_ci("Codex"), Some(Tool::Codex));
        assert_eq!(Tool::from_str_ci("AUTO"), Some(Tool::Auto));
        assert_eq!(Tool::from_str_ci("other"), None);
    }

    #[test]
    fn tool_parses_compound_names() {
        assert_eq!(Tool::from_str_ci("claude-code"), Some(Tool::Claude));
        assert_eq!(Tool::from_str_ci("codex-cli"), Some(Tool::Codex));
    }

    #[test]
    fn peek_response_serializes_with_snake_case_fields() {
        let resp = PeekResponse {
            partner_tool: Tool::Codex,
            session_path: Some("/tmp/x.jsonl".into()),
            session_mtime: Some("2026-04-13T12:00:00Z".into()),
            partner_active: true,
            messages: vec![],
            truncated: false,
        };
        let json = serde_json::to_string(&resp).expect("serialize");
        assert!(json.contains("partner_tool"));
        assert!(json.contains("session_path"));
        assert!(json.contains("partner_active"));
        assert!(json.contains(r#""partner_tool":"codex""#));
    }
}
```

- [ ] **Step 2: 运行测试，验证失败**

```bash
cargo test --no-default-features --features model2vec --lib cowork::peek::tests 2>&1 | tail -20
```
Expected: compile error (types undefined).

- [ ] **Step 3: 实现类型**

Prepend to `src/cowork/peek.rs` (before `#[cfg(test)]`):
```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;

/// Which agent tool's session to peek.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Claude,
    Codex,
    Auto,
}

impl Tool {
    /// Case-insensitive parse from a string; used for ClientInfo.name matching.
    pub fn from_str_ci(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "claude_code" => Some(Tool::Claude),
            "codex" | "codex-cli" | "codex_cli" | "codex-tui" => Some(Tool::Codex),
            "auto" => Some(Tool::Auto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Auto => "auto",
        }
    }
}

/// Peek request — parameters to `peek_partner`.
#[derive(Debug, Clone)]
pub struct PeekRequest {
    pub tool: Tool,
    /// Max messages to return (default 30).
    pub limit: usize,
    /// Optional RFC3339 cutoff; only messages newer than this are returned.
    pub since: Option<String>,
    /// Absolute cwd of the caller (injected by orchestrator; not user-facing).
    pub cwd: PathBuf,
    /// The tool that the CALLER is; used to reject self-peek.
    /// `None` means unknown (ClientInfo missing); auto mode will then error.
    pub caller_tool: Option<Tool>,
    /// HOME override for tests. None = use $HOME env var.
    pub home_override: Option<PathBuf>,
}

/// A single message from a session log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekMessage {
    /// "user" or "assistant".
    pub role: String,
    /// RFC3339 timestamp of this message.
    pub at: String,
    /// Plain text content; tool-use internals are filtered out.
    pub text: String,
}

/// Peek response — what `peek_partner` returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeekResponse {
    pub partner_tool: Tool,
    pub session_path: Option<String>,
    pub session_mtime: Option<String>,
    pub partner_active: bool,
    pub messages: Vec<PeekMessage>,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum PeekError {
    #[error("cannot infer partner; pass `tool` explicitly (client_info.name was missing or unrecognized)")]
    CannotInferPartner,

    #[error("cannot peek your own session")]
    SelfPeek,

    #[error("I/O error reading session: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to parse session file: {0}")]
    Parse(String),
}

/// Orchestrator — implemented in Task 5.
pub fn peek_partner(request: PeekRequest) -> Result<PeekResponse, PeekError> {
    let _ = request;
    unimplemented!("peek_partner dispatch — Task 5")
}
```

- [ ] **Step 4: 运行测试，验证通过**

```bash
cargo test --no-default-features --features model2vec --lib cowork::peek::tests 2>&1 | tail -20
```
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/cowork/peek.rs
git commit -m "feat(cowork): define PeekRequest/Response/Tool types (P6 task 2)"
```

---

## Task 3: `ClaudeSessionReader` adapter + real-schema fixtures

**Files:**
- Modify: `src/cowork/claude.rs`
- Create: `tests/fixtures/cowork/claude/session.jsonl`
- Create: `tests/fixtures/cowork/claude/session_with_tools.jsonl`

- [ ] **Step 1: 创建 Claude fixture（真实 schema）**

Create `tests/fixtures/cowork/claude/session.jsonl`:
```
{"type":"permission-mode","permissionMode":"default","sessionId":"fake-sess"}
{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"Hello from user turn 1"},"uuid":"u1","timestamp":"2026-04-13T10:00:00Z","cwd":"/tmp/fake-project","sessionId":"fake-sess"}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello from assistant turn 1"}]},"uuid":"a1","timestamp":"2026-04-13T10:00:05Z","cwd":"/tmp/fake-project","sessionId":"fake-sess"}
{"parentUuid":"a1","isSidechain":false,"type":"user","message":{"role":"user","content":"Second user message"},"uuid":"u2","timestamp":"2026-04-13T10:01:00Z","cwd":"/tmp/fake-project","sessionId":"fake-sess"}
{"parentUuid":"u2","isSidechain":false,"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Second assistant reply"}]},"uuid":"a2","timestamp":"2026-04-13T10:01:10Z","cwd":"/tmp/fake-project","sessionId":"fake-sess"}
```

Create `tests/fixtures/cowork/claude/session_with_tools.jsonl`:
```
{"parentUuid":null,"isSidechain":false,"type":"user","message":{"role":"user","content":"User turn with tool"},"uuid":"u1","timestamp":"2026-04-13T10:00:00Z","cwd":"/tmp/fake-project"}
{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me check"},{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"ls"}}]},"uuid":"a1","timestamp":"2026-04-13T10:00:05Z","cwd":"/tmp/fake-project"}
{"parentUuid":"a1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"file.txt"}]},"uuid":"u2","timestamp":"2026-04-13T10:00:06Z","cwd":"/tmp/fake-project"}
{"parentUuid":"u2","isSidechain":false,"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Here is the listing"}]},"uuid":"a2","timestamp":"2026-04-13T10:00:10Z","cwd":"/tmp/fake-project"}
{"parentUuid":"a2","isSidechain":false,"type":"user","message":{"role":"user","content":"<local-command-caveat>meta</local-command-caveat>"},"isMeta":true,"uuid":"u3","timestamp":"2026-04-13T10:00:20Z","cwd":"/tmp/fake-project"}
{"parentUuid":"u3","isSidechain":false,"type":"user","message":{"role":"user","content":"Follow-up question"},"uuid":"u4","timestamp":"2026-04-13T10:01:00Z","cwd":"/tmp/fake-project"}
```

The second fixture has 6 lines: 2 "pure text" assistants + 2 pure-text users + 1 meta user (isMeta:true, should be skipped) + 1 user whose content is a tool_result array (has no text block, should be skipped since there's no extractable text).

Wait — let me recount for the test. The adapter should return: `u1` (text), `a1` (text from "Let me check"), `a2` (text "Here is the listing"), `u4` (text). That's 4 messages. `u2`'s content is only a tool_result (no text block, no plain string), `u3` has `isMeta:true`. So result count = 4.

- [ ] **Step 2: 写失败的单元测试**

Append to `src/cowork/claude.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reads_plain_text_and_structured_content() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session.jsonl");
        let (messages, truncated) = parse_jsonl_messages(fixture, None, 30).expect("parse");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].text, "Hello from user turn 1");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].text, "Hello from assistant turn 1");
        assert_eq!(messages[3].text, "Second assistant reply");
        assert!(messages[0].at <= messages[3].at);
        assert!(!truncated);
    }

    #[test]
    fn filters_tool_use_blocks_and_is_meta_entries() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session_with_tools.jsonl");
        let (messages, _) = parse_jsonl_messages(fixture, None, 30).expect("parse");
        // Expected: u1 ("User turn with tool"), a1 ("Let me check"),
        //           a2 ("Here is the listing"), u4 ("Follow-up question")
        // Skipped: u2 (only tool_result), u3 (isMeta:true)
        assert_eq!(messages.len(), 4);
        for m in &messages {
            assert!(m.role == "user" || m.role == "assistant");
            assert!(!m.text.is_empty());
            assert!(!m.text.contains("tool_use"));
            assert!(!m.text.contains("tool_result"));
        }
        assert_eq!(messages[0].text, "User turn with tool");
        assert_eq!(messages[1].text, "Let me check");
        assert_eq!(messages[2].text, "Here is the listing");
        assert_eq!(messages[3].text, "Follow-up question");
    }

    #[test]
    fn honors_limit_by_taking_tail_and_sets_truncated() {
        let fixture = Path::new("tests/fixtures/cowork/claude/session.jsonl");
        let (messages, truncated) = parse_jsonl_messages(fixture, None, 2).expect("parse");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "Second user message");
        assert_eq!(messages[1].text, "Second assistant reply");
        assert!(truncated);
    }

    #[test]
    fn encoded_cwd_replaces_slashes_with_dashes() {
        assert_eq!(encode_cwd(Path::new("/Users/foo/bar")), "-Users-foo-bar");
        assert_eq!(encode_cwd(Path::new("/a")), "-a");
    }
}
```

- [ ] **Step 3: 运行测试，验证失败**

```bash
cargo test --no-default-features --features model2vec --lib cowork::claude::tests 2>&1 | tail -20
```
Expected: compile errors.

- [ ] **Step 4: 实现 Claude adapter**

Prepend to `src/cowork/claude.rs`:
```rust
use crate::cowork::peek::{PeekError, PeekMessage};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Build the Claude Code project encoding from a cwd: `/` → `-`.
pub fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy().replace('/', "-")
}

/// Build the Claude Code project directory for a given cwd + home dir.
pub fn claude_project_dir(home: &Path, cwd: &Path) -> PathBuf {
    home.join(".claude/projects").join(encode_cwd(cwd))
}

/// Find the latest (by mtime) `.jsonl` in the Claude project directory.
pub fn latest_session_file(project_dir: &Path) -> Option<(PathBuf, SystemTime)> {
    let entries = fs::read_dir(project_dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), mtime))
        })
        .max_by_key(|(_, m)| *m)
}

/// Parse a Claude Code session jsonl. Returns `(messages, truncated)` where
/// `messages` is the tail `limit` user+assistant text messages in ascending
/// order and `truncated` is true iff more than `limit` candidates existed.
///
/// Single-pass: reads the whole file once, accumulates all candidate messages,
/// then takes the tail.
pub fn parse_jsonl_messages(
    path: &Path,
    since: Option<&str>,
    limit: usize,
) -> Result<(Vec<PeekMessage>, bool), PeekError> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut all: Vec<PeekMessage> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(msg) = extract_message(&val) {
            if let Some(cutoff) = since {
                if msg.at.as_str() <= cutoff {
                    continue;
                }
            }
            all.push(msg);
        }
    }

    let total = all.len();
    let truncated = total > limit;
    let start = total.saturating_sub(limit);
    let tail = all.split_off(start);
    Ok((tail, truncated))
}

/// Extract a PeekMessage from one Claude jsonl entry if it's a user/assistant
/// text message. Returns None for permission-mode, isMeta, and messages whose
/// content has no extractable text (e.g. only tool_use or only tool_result).
fn extract_message(val: &Value) -> Option<PeekMessage> {
    // Skip permission-mode and other non-message top-level types
    let top_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if top_type != "user" && top_type != "assistant" {
        return None;
    }

    // Skip meta entries (command echoes, caveats, etc.)
    if val.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }

    let message = val.get("message")?;
    let role = message.get("role").and_then(|v| v.as_str())?;
    if role != "user" && role != "assistant" {
        return None;
    }

    // message.content can be either a plain string or an array of content blocks
    let content = message.get("content")?;
    let text = match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| {
                    let block_type = b.get("type").and_then(|v| v.as_str())?;
                    if block_type == "text" {
                        b.get("text").and_then(|v| v.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            parts.join("\n")
        }
        _ => return None,
    };

    if text.is_empty() {
        return None;
    }

    let at = val
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(PeekMessage {
        role: role.to_string(),
        at,
        text,
    })
}
```

- [ ] **Step 5: 运行测试，验证通过**

```bash
cargo test --no-default-features --features model2vec --lib cowork::claude::tests 2>&1 | tail -30
```
Expected: 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/cowork/claude.rs tests/fixtures/cowork/claude/
git commit -m "feat(cowork): Claude session reader with real jsonl schema (P6 task 3)"
```

---

## Task 4: `CodexSessionReader` adapter + real-schema fixtures

**Files:**
- Modify: `src/cowork/codex.rs`
- Create: `tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl`
- Create: `tests/fixtures/cowork/codex/2026/04/12/rollout-2026-04-12T12-00-00-otherproject.jsonl`

- [ ] **Step 1: 创建 Codex fixture（真实 schema）**

Create `tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl`:
```
{"timestamp":"2026-04-13T12:00:00Z","type":"session_meta","payload":{"id":"fake","timestamp":"2026-04-13T12:00:00Z","cwd":"/tmp/fake-project","originator":"codex-tui","cli_version":"0.118.0"}}
{"timestamp":"2026-04-13T12:00:10Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex: hello"}]}}
{"timestamp":"2026-04-13T12:00:11Z","type":"event_msg","payload":{"type":"user_message","message":"codex: hello"}}
{"timestamp":"2026-04-13T12:00:15Z","type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"..."}}
{"timestamp":"2026-04-13T12:00:20Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex: hi back"}],"phase":"commentary"}}
{"timestamp":"2026-04-13T12:00:30Z","type":"event_msg","payload":{"type":"token_count","info":null}}
{"timestamp":"2026-04-13T12:00:35Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex: continue"}]}}
{"timestamp":"2026-04-13T12:00:40Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex: continuing"}]}}
```

File has 8 lines. Adapter should return 4 messages (2 user + 2 assistant), filtering out: session_meta, event_msg×2, reasoning payload.

Create `tests/fixtures/cowork/codex/2026/04/12/rollout-2026-04-12T12-00-00-otherproject.jsonl`:
```
{"timestamp":"2026-04-12T12:00:00Z","type":"session_meta","payload":{"id":"other","timestamp":"2026-04-12T12:00:00Z","cwd":"/tmp/other-project","originator":"codex-tui","cli_version":"0.118.0"}}
{"timestamp":"2026-04-12T12:00:10Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"otherproject: should not appear"}]}}
```

- [ ] **Step 2: 写失败的单元测试**

Append to `src/cowork/codex.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reads_session_meta_cwd_from_first_line() {
        let fixture = Path::new("tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl");
        let cwd = read_session_cwd(fixture).expect("read cwd");
        assert_eq!(cwd, "/tmp/fake-project");
    }

    #[test]
    fn parses_codex_messages_filtering_event_and_reasoning() {
        let fixture = Path::new("tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl");
        let (messages, truncated) = parse_codex_jsonl(fixture, None, 30).expect("parse");
        // 8 lines in file, 4 are response_item message entries (2 user + 2 assistant)
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text, "codex: hello");
        assert_eq!(messages[1].text, "codex: hi back");
        assert_eq!(messages[2].text, "codex: continue");
        assert_eq!(messages[3].text, "codex: continuing");
        assert!(messages[0].at < messages[3].at);
        assert!(!truncated);
    }

    #[test]
    fn honors_limit_by_tail_and_sets_truncated_codex() {
        let fixture = Path::new("tests/fixtures/cowork/codex/2026/04/13/rollout-2026-04-13T12-00-00-fake.jsonl");
        let (messages, truncated) = parse_codex_jsonl(fixture, None, 2).expect("parse");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text, "codex: continue");
        assert_eq!(messages[1].text, "codex: continuing");
        assert!(truncated);
    }

    #[test]
    fn walks_codex_dir_filtering_by_cwd() {
        let base = Path::new("tests/fixtures/cowork/codex");
        let result = find_latest_session_for_cwd(base, "/tmp/fake-project")
            .expect("find session");
        assert!(result.is_some());
        let (path, _mtime) = result.unwrap();
        assert!(path.to_string_lossy().contains("rollout-2026-04-13T12-00-00-fake.jsonl"));
    }

    #[test]
    fn walks_codex_dir_excludes_other_projects() {
        let base = Path::new("tests/fixtures/cowork/codex");
        let result = find_latest_session_for_cwd(base, "/tmp/fake-project")
            .expect("find session");
        let path = result.unwrap().0;
        assert!(!path.to_string_lossy().contains("otherproject"));
    }
}
```

- [ ] **Step 3: 运行测试，验证失败**

```bash
cargo test --no-default-features --features model2vec --lib cowork::codex::tests 2>&1 | tail -20
```
Expected: compile errors.

- [ ] **Step 4: 实现 Codex adapter**

Prepend to `src/cowork/codex.rs`:
```rust
use crate::cowork::peek::{PeekError, PeekMessage};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Read the first line of a Codex rollout jsonl and extract `payload.cwd`.
pub fn read_session_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let val: Value = serde_json::from_str(line.trim()).ok()?;
    if val.get("type").and_then(|v| v.as_str()) != Some("session_meta") {
        return None;
    }
    val.get("payload")?
        .get("cwd")?
        .as_str()
        .map(|s| s.to_string())
}

/// Walk a Codex sessions base (e.g. `~/.codex/sessions`), find all rollout jsonl
/// whose `session_meta.payload.cwd` matches target, return latest by mtime.
pub fn find_latest_session_for_cwd(
    base: &Path,
    target_cwd: &str,
) -> Result<Option<(PathBuf, SystemTime)>, PeekError> {
    let mut candidates: Vec<(PathBuf, SystemTime)> = Vec::new();

    for entry in WalkDir::new(base)
        .max_depth(6)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("rollout-") {
            continue;
        }

        if let Some(cwd) = read_session_cwd(path) {
            if cwd == target_cwd {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(mtime) = metadata.modified() {
                        candidates.push((path.to_path_buf(), mtime));
                    }
                }
            }
        }
    }

    Ok(candidates.into_iter().max_by_key(|(_, m)| *m))
}

/// Parse a Codex rollout jsonl. Returns `(messages, truncated)`. Only keeps
/// `type: "response_item"` entries whose `payload.type: "message"` and
/// `payload.role` ∈ {user, assistant}. Text comes from `payload.content[]`
/// where each block's `text` field is concatenated.
pub fn parse_codex_jsonl(
    path: &Path,
    since: Option<&str>,
    limit: usize,
) -> Result<(Vec<PeekMessage>, bool), PeekError> {
    let file = fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut all: Vec<PeekMessage> = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let val: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Only process response_item entries
        if val.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = val.get("payload") else {
            continue;
        };
        // Only process message entries (skip reasoning, etc.)
        if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let role = payload.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }

        // Concatenate all text blocks in payload.content[]
        let text = match payload.get("content") {
            Some(Value::Array(blocks)) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(String::from))
                    .collect();
                parts.join("\n")
            }
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }

        let at = val
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if let Some(cutoff) = since {
            if at.as_str() <= cutoff {
                continue;
            }
        }

        all.push(PeekMessage {
            role: role.to_string(),
            at,
            text,
        });
    }

    let total = all.len();
    let truncated = total > limit;
    let start = total.saturating_sub(limit);
    let tail = all.split_off(start);
    Ok((tail, truncated))
}
```

- [ ] **Step 5: 运行测试，验证通过**

```bash
cargo test --no-default-features --features model2vec --lib cowork::codex::tests 2>&1 | tail -30
```
Expected: 5 tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/cowork/codex.rs tests/fixtures/cowork/codex/
git commit -m "feat(cowork): Codex session reader with real jsonl schema (P6 task 4)"
```

---

## Task 5: peek 编排层

**Files:**
- Modify: `src/cowork/peek.rs`

- [ ] **Step 1: 写失败的单元测试**

Append to existing `#[cfg(test)] mod tests` in `src/cowork/peek.rs`:
```rust
    #[test]
    fn rejects_self_peek_when_caller_is_same_tool() {
        let req = PeekRequest {
            tool: Tool::Codex,
            limit: 30,
            since: None,
            cwd: std::path::PathBuf::from("/tmp"),
            caller_tool: Some(Tool::Codex),
            home_override: None,
        };
        let err = peek_partner(req).unwrap_err();
        assert!(matches!(err, PeekError::SelfPeek));
    }

    #[test]
    fn auto_mode_errors_without_caller_tool() {
        let req = PeekRequest {
            tool: Tool::Auto,
            limit: 30,
            since: None,
            cwd: std::path::PathBuf::from("/tmp"),
            caller_tool: None,
            home_override: None,
        };
        let err = peek_partner(req).unwrap_err();
        assert!(matches!(err, PeekError::CannotInferPartner));
    }

    #[test]
    fn infer_partner_maps_claude_to_codex_and_vice_versa() {
        assert_eq!(infer_partner(Tool::Auto, Some(Tool::Claude)).unwrap(), Tool::Codex);
        assert_eq!(infer_partner(Tool::Auto, Some(Tool::Codex)).unwrap(), Tool::Claude);
        assert_eq!(infer_partner(Tool::Claude, Some(Tool::Codex)).unwrap(), Tool::Claude);
    }

    #[test]
    fn is_active_true_when_mtime_within_30_minutes() {
        use std::time::{Duration, SystemTime};
        let recent = SystemTime::now() - Duration::from_secs(10 * 60);
        let old = SystemTime::now() - Duration::from_secs(45 * 60);
        assert!(is_active(recent));
        assert!(!is_active(old));
    }
```

- [ ] **Step 2: 运行测试，验证失败**

```bash
cargo test --no-default-features --features model2vec --lib cowork::peek::tests 2>&1 | tail -30
```
Expected: `unimplemented!()` panics or compile errors on `infer_partner` / `is_active`.

- [ ] **Step 3: 实现编排层**

Replace the stub `peek_partner` in `src/cowork/peek.rs` with a complete implementation. Insert this block where the stub was:

```rust
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cowork::claude::{claude_project_dir, latest_session_file, parse_jsonl_messages};
use crate::cowork::codex::{find_latest_session_for_cwd, parse_codex_jsonl};

/// A partner session is "active" if its mtime is within 30 minutes.
const ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);

pub fn is_active(mtime: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(mtime)
        .map(|d| d <= ACTIVE_WINDOW)
        .unwrap_or(true) // future mtime treated as "active"
}

/// Resolve `Tool::Auto` into a concrete partner tool based on caller identity.
pub fn infer_partner(requested: Tool, caller_tool: Option<Tool>) -> Result<Tool, PeekError> {
    match requested {
        Tool::Claude | Tool::Codex => Ok(requested),
        Tool::Auto => match caller_tool {
            Some(Tool::Claude) => Ok(Tool::Codex),
            Some(Tool::Codex) => Ok(Tool::Claude),
            _ => Err(PeekError::CannotInferPartner),
        },
    }
}

/// Format a SystemTime as RFC3339 UTC (seconds precision).
pub fn format_rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let days = (secs / 86400) as i64;
    let sec_of_day = secs % 86400;
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day / 60) % 60;
    let second = sec_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's civil_from_days
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn resolve_home(request: &PeekRequest) -> Result<PathBuf, PeekError> {
    if let Some(h) = &request.home_override {
        return Ok(h.clone());
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| PeekError::Parse("HOME environment variable not set".to_string()))
}

/// Main entry point: dispatch to the correct adapter based on target tool.
pub fn peek_partner(request: PeekRequest) -> Result<PeekResponse, PeekError> {
    let target = infer_partner(request.tool, request.caller_tool)?;

    if let Some(caller) = request.caller_tool {
        if caller == target {
            return Err(PeekError::SelfPeek);
        }
    }

    match target {
        Tool::Claude => peek_claude(&request, target),
        Tool::Codex => peek_codex(&request, target),
        Tool::Auto => unreachable!("infer_partner resolved Auto"),
    }
}

fn peek_claude(request: &PeekRequest, target: Tool) -> Result<PeekResponse, PeekError> {
    let home = resolve_home(request)?;
    let project_dir = claude_project_dir(&home, &request.cwd);
    let Some((path, mtime)) = latest_session_file(&project_dir) else {
        return Ok(empty_response(target));
    };

    let (messages, truncated) =
        parse_jsonl_messages(&path, request.since.as_deref(), request.limit)?;

    Ok(PeekResponse {
        partner_tool: target,
        session_path: Some(path.to_string_lossy().into_owned()),
        session_mtime: Some(format_rfc3339(mtime)),
        partner_active: is_active(mtime),
        messages,
        truncated,
    })
}

fn peek_codex(request: &PeekRequest, target: Tool) -> Result<PeekResponse, PeekError> {
    let home = resolve_home(request)?;
    let base = home.join(".codex/sessions");
    let target_cwd = request.cwd.to_string_lossy().into_owned();
    let Some((path, mtime)) = find_latest_session_for_cwd(&base, &target_cwd)? else {
        return Ok(empty_response(target));
    };

    let (messages, truncated) =
        parse_codex_jsonl(&path, request.since.as_deref(), request.limit)?;

    Ok(PeekResponse {
        partner_tool: target,
        session_path: Some(path.to_string_lossy().into_owned()),
        session_mtime: Some(format_rfc3339(mtime)),
        partner_active: is_active(mtime),
        messages,
        truncated,
    })
}

fn empty_response(target: Tool) -> PeekResponse {
    PeekResponse {
        partner_tool: target,
        session_path: None,
        session_mtime: None,
        partner_active: false,
        messages: vec![],
        truncated: false,
    }
}
```

- [ ] **Step 4: 运行测试，验证通过**

```bash
cargo test --no-default-features --features model2vec --lib cowork 2>&1 | tail -30
```
Expected: all cowork tests pass (types, claude, codex, peek orchestration).

- [ ] **Step 5: Commit**

```bash
git add src/cowork/peek.rs
git commit -m "feat(cowork): peek_partner orchestrator + home_override resolve (P6 task 5)"
```

---

## Task 6: 注册 `mempal_peek_partner` MCP 工具

**Files:**
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`

- [ ] **Step 1: 加请求/响应 DTO**

Append to `src/mcp/tools.rs`:
```rust
// --- Cowork peek ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PeekPartnerRequest {
    /// Which agent tool's session to read. "auto" uses MCP ClientInfo.name
    /// to infer the partner; "claude" or "codex" bypasses inference.
    pub tool: String,

    /// Maximum number of user+assistant messages to return. Default 30.
    pub limit: Option<usize>,

    /// Optional RFC3339 timestamp cutoff — only messages strictly newer are returned.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekPartnerResponse {
    pub partner_tool: String,
    pub session_path: Option<String>,
    pub session_mtime: Option<String>,
    pub partner_active: bool,
    pub messages: Vec<PeekMessageDto>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekMessageDto {
    pub role: String,
    pub at: String,
    pub text: String,
}

impl From<crate::cowork::PeekMessage> for PeekMessageDto {
    fn from(m: crate::cowork::PeekMessage) -> Self {
        Self {
            role: m.role,
            at: m.at,
            text: m.text,
        }
    }
}
```

- [ ] **Step 2: 加 ClientInfo 状态 + initialize 拦截 + 新工具 handler**

Modify `src/mcp/server.rs`:

1. Add use statements at the top:
```rust
use std::sync::Mutex;
use crate::cowork::{peek_partner, PeekError, PeekRequest as CoworkPeekRequest, Tool};
```

2. Extend the `super::tools::` use list to include `PeekMessageDto, PeekPartnerRequest, PeekPartnerResponse`.

3. Add a `client_name` field to `MempalMcpServer`:
```rust
#[derive(Clone)]
pub struct MempalMcpServer {
    db_path: PathBuf,
    embedder_factory: Arc<dyn EmbedderFactory>,
    tool_router: ToolRouter<Self>,
    client_name: Arc<Mutex<Option<String>>>,
}
```

4. Update `new_with_factory` to initialize `client_name: Arc::new(Mutex::new(None))`.

5. Add `initialize` override inside `impl ServerHandler for MempalMcpServer` block. **rmcp 1.3 签名可能不同**：遇到编译错误时，`cargo doc --open -p rmcp` 或查 `~/.cargo/registry/src/.../rmcp-1.3.0/src/handler/server.rs` 的 `ServerHandler` trait 定义，参考其他示例。基本模式是：

```rust
async fn initialize(
    &self,
    params: rmcp::model::InitializeRequestParam,
    _ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
) -> std::result::Result<rmcp::model::InitializeResult, ErrorData> {
    if let Some(info) = params.client_info {
        if let Ok(mut guard) = self.client_name.lock() {
            *guard = Some(info.name);
        }
    }
    // Return standard InitializeResult built from get_info()
    Ok(self.get_info().into_initialize_result())
}
```

如果 `client_info` 字段或 `into_initialize_result()` 方法名不对，参考 rmcp 源码里 `InitializeResult` 的构造方式手工搭一个。如果 rmcp 1.3 不允许 override initialize，改用其他 capture 点（例如把 client_name 保持为 `None` 并让 `auto` 模式直接 fail，由用户 side 显式指定 `tool`）——这是回退方案，`auto` 失效但其他功能不受影响。

6. Add the new tool method inside the `#[tool_router]` impl block, after `mempal_tunnels`:
```rust
    #[tool(
        name = "mempal_peek_partner",
        description = "Read the partner coding agent's live session log (Claude Code ↔ Codex) without storing it. Returns the most recent user+assistant messages from their current session file. Use this for CURRENT partner state; use mempal_search for CRYSTALLIZED past decisions. Peek is a pure read — never writes to mempal. Specify tool='auto' to let the server infer the partner from MCP ClientInfo."
    )]
    async fn mempal_peek_partner(
        &self,
        Parameters(request): Parameters<PeekPartnerRequest>,
    ) -> std::result::Result<Json<PeekPartnerResponse>, ErrorData> {
        let tool = Tool::from_str_ci(&request.tool).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("unknown tool `{}`: expected claude|codex|auto", request.tool),
                None,
            )
        })?;

        let caller_tool = self
            .client_name
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|n| Tool::from_str_ci(&n));

        let cwd = std::env::current_dir()
            .map_err(|e| ErrorData::internal_error(format!("cwd unavailable: {e}"), None))?;

        let cowork_req = CoworkPeekRequest {
            tool,
            limit: request.limit.unwrap_or(30),
            since: request.since,
            cwd,
            caller_tool,
            home_override: None,
        };

        let resp = peek_partner(cowork_req).map_err(|e| match e {
            PeekError::CannotInferPartner | PeekError::SelfPeek => {
                ErrorData::invalid_params(e.to_string(), None)
            }
            PeekError::Io(_) | PeekError::Parse(_) => {
                ErrorData::internal_error(e.to_string(), None)
            }
        })?;

        Ok(Json(PeekPartnerResponse {
            partner_tool: resp.partner_tool.as_str().to_string(),
            session_path: resp.session_path,
            session_mtime: resp.session_mtime,
            partner_active: resp.partner_active,
            messages: resp.messages.into_iter().map(PeekMessageDto::from).collect(),
            truncated: resp.truncated,
        }))
    }
```

- [ ] **Step 3: 验证构建**

```bash
cargo build --no-default-features --features model2vec 2>&1 | tail -30
```
Expected: compiles. If `initialize` signature doesn't match, look up real signature via `cargo doc` and adjust. Accept the fallback path (no auto-infer) if rmcp won't allow capture.

- [ ] **Step 4: 跑全部 lib tests 确保无回归**

```bash
cargo test --no-default-features --features model2vec --lib 2>&1 | tail -20
```
Expected: existing + new cowork tests all pass.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/tools.rs src/mcp/server.rs
git commit -m "feat(mcp): register mempal_peek_partner tool + ClientInfo capture (P6 task 6)"
```

---

## Task 7: MEMORY_PROTOCOL Rule 8 + Rule 9

**Files:**
- Modify: `src/core/protocol.rs`

- [ ] **Step 1: 写失败的单元测试**

Append to `src/core/protocol.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::MEMORY_PROTOCOL;

    #[test]
    fn contains_rule_8_partner_awareness() {
        assert!(
            MEMORY_PROTOCOL.contains("Rule 8") || MEMORY_PROTOCOL.contains("PARTNER AWARENESS"),
            "MEMORY_PROTOCOL must include Rule 8 / PARTNER AWARENESS"
        );
    }

    #[test]
    fn contains_rule_9_decision_capture() {
        assert!(
            MEMORY_PROTOCOL.contains("Rule 9") || MEMORY_PROTOCOL.contains("DECISION CAPTURE"),
            "MEMORY_PROTOCOL must include Rule 9 / DECISION CAPTURE"
        );
    }

    #[test]
    fn contains_peek_partner_tool_name() {
        assert!(
            MEMORY_PROTOCOL.contains("mempal_peek_partner"),
            "MEMORY_PROTOCOL must mention the mempal_peek_partner tool"
        );
    }
}
```

- [ ] **Step 2: 运行测试，验证失败**

```bash
cargo test --no-default-features --features model2vec --lib core::protocol 2>&1 | tail -20
```
Expected: 3 fails.

- [ ] **Step 3: 追加 Rule 8 + Rule 9 到 MEMORY_PROTOCOL**

Find the closing `"#;` of the `MEMORY_PROTOCOL` constant in `src/core/protocol.rs`. Before it, append:

```text

8. PARTNER AWARENESS (cross-agent cowork)
   When the user references the partner coding agent ("Codex 那边...",
   "ask Claude what...", "partner is working on..."), call
   mempal_peek_partner to read the partner's LIVE session rather than
   searching mempal drawers. Live conversation is transient and stays in
   session logs, not mempal. Use peek for CURRENT state; use mempal_search
   for CRYSTALLIZED past decisions. Don't conflate the two.

9. DECISION CAPTURE (what goes into mempal)
   mempal_ingest is for decisions, not chat logs. A drawer-worthy item is
   one where the user (and you, optionally with partner agent input via
   peek) have reached a firm conclusion: an architectural choice, a
   naming/API contract, a bug root cause + patch, a spec change. Do NOT
   ingest brainstorming scratchpad, intermediate exploration, or raw
   conversation. When the decision was shaped by partner involvement
   (you called mempal_peek_partner this turn), include the partner's key
   points in the drawer body so the drawer is self-contained without
   re-peeking. Cite the partner session file path in source_file alongside
   your own citation.
```

- [ ] **Step 4: 运行测试，验证通过**

```bash
cargo test --no-default-features --features model2vec --lib core::protocol 2>&1 | tail -20
```
Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/core/protocol.rs
git commit -m "feat(protocol): add Rule 8 PARTNER AWARENESS + Rule 9 DECISION CAPTURE (P6 task 7)"
```

---

## Task 8: 集成测试 `tests/cowork_peek.rs`

**Files:**
- Create: `tests/cowork_peek.rs`

使用 `home_override` 字段（Task 2 已经定义）构造隔离的 fake HOME 目录。

- [ ] **Step 1: 写集成测试**

Create `tests/cowork_peek.rs`:
```rust
//! Integration tests for P6 cowork peek-and-decide.

use mempal::cowork::{PeekError, PeekRequest, Tool, peek_partner};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

/// Build a fake HOME dir containing Claude and Codex fixture sessions for the
/// given cwd.
fn build_fake_home(cwd: &Path) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().to_path_buf();

    // Claude: ~/.claude/projects/<encoded>/session.jsonl
    let encoded = cwd.to_string_lossy().replace('/', "-");
    let claude_dir = home.join(".claude/projects").join(&encoded);
    fs::create_dir_all(&claude_dir).unwrap();
    let cwd_str = cwd.to_string_lossy();
    let claude_jsonl = format!(
        r#"{{"type":"permission-mode","permissionMode":"default"}}
{{"parentUuid":null,"isSidechain":false,"type":"user","message":{{"role":"user","content":"Claude user msg"}},"uuid":"u1","timestamp":"2026-04-13T10:00:00Z","cwd":"{cwd_str}"}}
{{"parentUuid":"u1","isSidechain":false,"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"Claude reply"}}]}},"uuid":"a1","timestamp":"2026-04-13T10:00:05Z","cwd":"{cwd_str}"}}
"#
    );
    fs::write(claude_dir.join("session.jsonl"), claude_jsonl).unwrap();

    // Codex: ~/.codex/sessions/2026/04/13/rollout-*.jsonl
    let codex_dir = home.join(".codex/sessions/2026/04/13");
    fs::create_dir_all(&codex_dir).unwrap();
    let codex_jsonl = format!(
        r#"{{"timestamp":"2026-04-13T12:00:00Z","type":"session_meta","payload":{{"id":"x","timestamp":"2026-04-13T12:00:00Z","cwd":"{cwd_str}","originator":"codex-tui"}}}}
{{"timestamp":"2026-04-13T12:00:10Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"Codex user msg"}}]}}}}
{{"timestamp":"2026-04-13T12:00:20Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"Codex reply"}}]}}}}
"#
    );
    fs::write(codex_dir.join("rollout-2026-04-13T12-00-00-x.jsonl"), codex_jsonl).unwrap();

    (tmp, home)
}

#[test]
fn test_peek_partner_claude_reads_codex_session() {
    let cwd = PathBuf::from("/tmp/fake-project-1");
    let (_tmp, home) = build_fake_home(&cwd);

    let req = PeekRequest {
        tool: Tool::Codex,
        limit: 30,
        since: None,
        cwd,
        caller_tool: Some(Tool::Claude),
        home_override: Some(home),
    };
    let resp = peek_partner(req).expect("peek");

    assert_eq!(resp.partner_tool, Tool::Codex);
    assert_eq!(resp.messages.len(), 2);
    assert_eq!(resp.messages[0].text, "Codex user msg");
    assert_eq!(resp.messages[1].text, "Codex reply");
    assert!(resp.messages[0].at <= resp.messages[1].at);
    assert!(!resp.truncated);
    assert!(resp.session_path.is_some());
}

#[test]
fn test_peek_partner_auto_mode_infers_partner() {
    let cwd = PathBuf::from("/tmp/fake-project-2");
    let (_tmp, home) = build_fake_home(&cwd);

    let req = PeekRequest {
        tool: Tool::Auto,
        limit: 30,
        since: None,
        cwd,
        caller_tool: Some(Tool::Claude),
        home_override: Some(home),
    };
    let resp = peek_partner(req).expect("peek");

    assert_eq!(resp.partner_tool, Tool::Codex);
    assert_eq!(resp.messages.len(), 2);
}

#[test]
fn test_peek_partner_auto_mode_errors_without_client_info() {
    let cwd = PathBuf::from("/tmp/fake-project-3");
    let (_tmp, home) = build_fake_home(&cwd);

    let req = PeekRequest {
        tool: Tool::Auto,
        limit: 30,
        since: None,
        cwd,
        caller_tool: None,
        home_override: Some(home),
    };
    let err = peek_partner(req).unwrap_err();
    assert!(matches!(err, PeekError::CannotInferPartner));
}

#[test]
fn test_peek_partner_reports_inactive_session() {
    let cwd = PathBuf::from("/tmp/fake-project-4");
    let (tmp, home) = build_fake_home(&cwd);

    // Backdate the Codex jsonl via `touch -t`.
    let codex_path = tmp
        .path()
        .join(".codex/sessions/2026/04/13/rollout-2026-04-13T12-00-00-x.jsonl");
    // 198001010000 = Jan 1 1980, well over 30 min ago.
    Command::new("touch")
        .arg("-t")
        .arg("198001010000")
        .arg(&codex_path)
        .status()
        .expect("touch");

    let req = PeekRequest {
        tool: Tool::Codex,
        limit: 30,
        since: None,
        cwd,
        caller_tool: Some(Tool::Claude),
        home_override: Some(home),
    };
    let resp = peek_partner(req).expect("peek");
    assert!(!resp.partner_active);
    assert!(resp.messages.len() > 0);
}

#[test]
fn test_peek_partner_filters_by_project_cwd() {
    let cwd_a = PathBuf::from("/tmp/project-a-xyz");
    let (_tmp, home) = build_fake_home(&cwd_a);

    // Add a second codex session for a different cwd, newer by touch.
    let other_dir = home.join(".codex/sessions/2026/04/14");
    fs::create_dir_all(&other_dir).unwrap();
    fs::write(
        other_dir.join("rollout-2026-04-14T12-00-00-other.jsonl"),
        r#"{"timestamp":"2026-04-14T12:00:00Z","type":"session_meta","payload":{"id":"other","timestamp":"2026-04-14T12:00:00Z","cwd":"/tmp/project-b-xyz","originator":"codex-tui"}}
{"timestamp":"2026-04-14T12:00:10Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"should not appear"}]}}
"#,
    )
    .unwrap();

    let req = PeekRequest {
        tool: Tool::Codex,
        limit: 30,
        since: None,
        cwd: cwd_a,
        caller_tool: Some(Tool::Claude),
        home_override: Some(home),
    };
    let resp = peek_partner(req).expect("peek");
    let path_str = resp.session_path.unwrap();
    assert!(path_str.contains("project-a"));
    for m in &resp.messages {
        assert!(!m.text.contains("should not appear"));
    }
}

#[test]
fn test_peek_partner_has_no_mempal_side_effects() {
    // Invariant by construction: the peek_partner import path doesn't touch
    // Database at all. Running peek multiple times cannot affect mempal state.
    let cwd = PathBuf::from("/tmp/fake-project-5");
    let (_tmp, home) = build_fake_home(&cwd);

    let req = PeekRequest {
        tool: Tool::Codex,
        limit: 30,
        since: None,
        cwd,
        caller_tool: Some(Tool::Claude),
        home_override: Some(home),
    };
    for _ in 0..3 {
        let _ = peek_partner(req.clone()).expect("peek");
    }
}

#[test]
fn test_peek_partner_returns_empty_when_no_session() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().to_path_buf();

    let req = PeekRequest {
        tool: Tool::Claude,
        limit: 30,
        since: None,
        cwd: PathBuf::from("/tmp/no-session-project"),
        caller_tool: Some(Tool::Codex),
        home_override: Some(home),
    };
    let resp = peek_partner(req).expect("peek");
    assert_eq!(resp.messages.len(), 0);
    assert!(!resp.partner_active);
    assert!(resp.session_path.is_none());
}
```

- [ ] **Step 2: 运行集成测试**

```bash
cargo test --test cowork_peek --no-default-features --features model2vec -- --test-threads=1 2>&1 | tail -40
```
Expected: 7 tests pass. If `touch -t 198001010000` fails, replace with `touch -t 200001010000` or similar early-epoch date.

- [ ] **Step 3: Commit**

```bash
git add tests/cowork_peek.rs
git commit -m "test(cowork): integration tests for peek_partner end-to-end (P6 task 8)"
```

---

## Task 9: agent-spec verify P6 合约

**Files:** none (verification only)

- [ ] **Step 1: 运行 agent-spec lifecycle**

```bash
agent-spec lifecycle specs/p6-cowork-peek-and-decide.spec.md --min-score 0.7 2>&1 | tail -40
```
Expected: lint 100%; verify matches scenario `Test.Filter` to actual tests.

- [ ] **Step 2: 调整 scenario test filter 名字（如有必要）**

如果某个 scenario 的 `Filter` 找不到匹配的测试，候选调整点：

- `test_peek_partner_honors_limit` (spec line 86) 对应单元测试的实际名字可能是 `honors_limit_by_taking_tail_and_sets_truncated`（Claude adapter）或 `honors_limit_by_tail_and_sets_truncated_codex`（Codex adapter）。如果 agent-spec verify 找不到，编辑 spec 把 `Filter` 改成实际存在的测试名，或在 Task 3/4 的单元测试上加一个同名的 wrapper。
- `test_peek_partner_filters_tool_use_entries` (spec line 148) 对应 `filters_tool_use_blocks_and_is_meta_entries`（Claude）+ `parses_codex_messages_filtering_event_and_reasoning`（Codex）。同上调整。

**Preferred:** 调整 spec 里的 `Filter` 字段指向真实测试名，而不是改测试名（测试名和它们要验证的单元语义更紧）。

- [ ] **Step 3: 最终 lint**

```bash
agent-spec lint specs/p6-cowork-peek-and-decide.spec.md --min-score 0.7
```
Expected: 100%.

- [ ] **Step 4: Commit（如有调整）**

```bash
git add specs/p6-cowork-peek-and-decide.spec.md
git commit -m "chore(spec): align P6 scenario test filters with actual test names"
```

---

## Task 10: 最终状态更新

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: 更新 MCP 工具表**

Modify `CLAUDE.md`:
- Change `自描述协议：MEMORY_PROTOCOL 嵌入 MCP ServerInfo.instructions，7 条规则` → `9 条规则`
- Change `## MCP 工具（7 个）` → `## MCP 工具（8 个）`
- Append to the tool table: `| \`mempal_peek_partner\` | 读 partner agent 当前 session（live，不存储） |`

- [ ] **Step 2: 把 P6 挪到"已完成"**

In `CLAUDE.md`:
- Delete the "当前 Spec（P6 — Cowork peek-and-decide）" section and replace with "（无，P6 已完成）"
- 在 "已完成的 Spec" 表格末尾加行：`| \`specs/p6-cowork-peek-and-decide.spec.md\` | 完成 | Cowork peek-and-decide（live session peek + Rule 8/9） |`
- Update table heading from `已完成的 Spec（P0-P5）` → `已完成的 Spec（P0-P6）`
- 实现计划章节里 P6 行改为 "已完成"

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md
git commit -m "docs(p6): mark P6 complete, update tool count to 8, protocol to 9 rules"
```

---

## Post-Implementation Verification

```bash
cargo test --no-default-features --features model2vec 2>&1 | tail -20
cargo clippy --no-default-features --features model2vec -- -D warnings 2>&1 | tail -20
```

Expected:
- All tests pass
- No clippy warnings

---

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| rmcp 1.3 `initialize` signature different from plan | Task 6 says to look up real signature via `cargo doc -p rmcp`. If impossible to override, fall back to "no auto-infer" behavior — `auto` errors, explicit `tool=claude/codex` still works. |
| Real Claude jsonl has shapes not in fixture (e.g. different content block types) | Parser is lenient — unknown block types are skipped, malformed lines skipped. Post-release, add fixtures capturing unforeseen cases as bugs emerge. |
| `touch -t` format varies BSD vs GNU | Use a widely-supported early date like `198001010000`. Fallback: add `filetime = "0.2"` to dev-deps. |
| Large jsonl (8 MB real session) full-read cost | Current impl reads entire file once. Acceptable for v1. If slow, optimize to tail-reading in a follow-up. |
| `HOME` not set in test environment | `home_override` injection bypasses env entirely. |

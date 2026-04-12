spec: task
name: "P6: Cowork peek-and-decide — live session peek + decision-only ingest"
tags: [feature, cowork, mcp, protocol]
estimate: 1d
---

## Intent

为 mempal 实现 Claude Code ↔ Codex 两个 coding agent 的协作通路：**live 上下文通过直接读对方的
session .jsonl 文件获取，不落盘到 mempal；只有决策定论才通过 `mempal_ingest` 沉淀**。架构对应
CoALA 的 L0 working memory（session 文件，瞬时）与 L1 semantic memory（mempal drawer，持久）分层。

新增一个 MCP 工具 `mempal_peek_partner`，并往 MEMORY_PROTOCOL 加两条规则（Rule 8 PARTNER AWARENESS、
Rule 9 DECISION CAPTURE）。设计文档见 `docs/specs/2026-04-13-cowork-peek-and-decide.md`。

## Decisions

- 新增 MCP 工具 `mempal_peek_partner`，参数 `tool: "claude" | "codex" | "auto"`、`limit?: usize` 默认 30、`since?: RFC3339 String`
- 工具返回 JSON：`partner_tool`、`session_path`、`session_mtime` (RFC3339)、`partner_active: bool`、`messages: [{role, at, text}]` 升序、`truncated: bool`
- `partner_active = true` 当且仅当 `session_mtime` 在调用时刻 30 分钟以内
- 项目隔离由调用方 cwd（`std::env::current_dir`）决定，不暴露独立的 project 参数；每次调用都以当前 cwd 为准
- Claude session adapter 路径：`~/.claude/projects/<encoded_cwd>/*.jsonl`，`encoded_cwd = cwd.replace('/', "-")`；选 mtime 最新的 `.jsonl`
- Claude jsonl 真实 schema：每行是 `{"type":"user"|"assistant","message":{"role":...,"content": string | [{type:text|tool_use|tool_result,...}]}, "isMeta":bool, "timestamp":..., "cwd":...}`；adapter 跳过 `isMeta:true`，从 `message.content` 提取 text 块（拼接），忽略 tool_use / tool_result 块
- Codex session adapter 路径：`~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-*.jsonl`；Codex 用 LOCAL 日期命名目录，所以 adapter 扫 `[UTC_today - 7, UTC_today + 1]` 共 9 个 date 目录以覆盖"最近 7 local 天"在任意时区偏移下的情况（正偏移如 +08 的早晨，live session 在 `UTC_today + 1`；负偏移如 -08 的晚间，7 local 天前是 `UTC_today - 7`）。按 mtime 倒排，读每个文件首行 `session_meta.payload.cwd` 过滤出当前项目，取第一个命中
- Codex jsonl 真实 schema：只处理 `type:"response_item"` 且 `payload.type:"message"` 的条目；`payload.role` 为 user/assistant；text 来自 `payload.content[].text`（其中 `type:"input_text"` 或 `"output_text"`）；跳过 `reasoning` payload 和 `event_msg` 条目
- adapter 单次扫描返回 `(messages, truncated)`；`truncated=true` 当消息总数大于 `limit`；成功提取的 user/assistant 消息按升序返回，超出 `limit` 时取尾部
- `tool = "auto"` 时使用 MCP `InitializeRequest.client_info.name`：含 "claude" → peek Codex；含 "codex" → peek Claude；其余 → 返回错误 "cannot infer partner; pass `tool` explicitly"
- 调用方和目标相同（self-peek，例如 Claude 调用时 `tool="claude"`）返回错误 "cannot peek your own session"
- `palace.db` schema 保持 v4，**不做**任何迁移
- `mempal_peek_partner` 实现必须是**纯读**：执行期间不得调用 `mempal_ingest` 或任何写库路径
- MEMORY_PROTOCOL 常量（`src/mcp/server.rs`）新增 Rule 8 (PARTNER AWARENESS) 与 Rule 9 (DECISION CAPTURE)；现有 Rule 4 的描述细化为"commit 后 ingest，如本轮调用过 peek 则 body 内含 partner 关键点"
- 新代码放在 `src/cowork/` 子模块（`mod.rs`、`claude.rs`、`codex.rs`、`peek.rs`）
- 不新增 CLI 子命令，`mempal_peek_partner` 只通过 MCP 暴露

## Boundaries

### Allowed
- src/cowork/**（新模块，所有文件）
- src/mcp/tools.rs（注册新 tool schema + 类型定义）
- src/mcp/server.rs（新 tool handler + MEMORY_PROTOCOL 常量修改）
- src/lib.rs（若需 `pub mod cowork`）
- tests/cowork_peek.rs（新增集成测试文件）
- tests/fixtures/cowork/**（新增 jsonl fixture 数据）
- Cargo.toml（只允许加 `walkdir` 如尚未存在）

### Forbidden
- 不要修改 `drawers`、`drawer_vectors`、`triples` 等任何表
- 不要 bump `CURRENT_SCHEMA_VERSION`
- 不要在 peek 路径里调用 `Database::insert_drawer` 或任何写操作
- 不要新增 CLI 子命令（`src/main.rs` 不得新增命令枝叶）
- 不要把 session 文件内容落盘到 mempal.db 或任何磁盘缓存
- 不要缓存 peek 结果跨调用（每次调用都要重新读文件）
- 不要引入新的运行时依赖（如 tokio-fs、async-walkdir 等）
- 不要修改现有 6 个 MCP 工具的 schema 或行为

## Out of Scope

- CLI `mempal peek` 子命令（v1 仅 MCP 接口）
- 第三方 client（Cursor、Continue 等）的 session adapter
- 访问控制 / 多租户 / 权限校验
- 流式增量 peek（长轮询、订阅）
- 对 session 消息做语义过滤或摘要（agent 自己基于返回的原文处理）
- `mempal_peek_partner` 返回值中的 tool-use 元数据（仅 user/assistant 文本）
- 任务 slug、task lifecycle、inbox/claim/dispatch 任何结构
- 跨 agent 的 write-to-inbox 通讯（只做 read-through peek）

## Completion Criteria

Scenario: Claude 客户端 peek Codex 最新 session 返回近期消息
  Test:
    Filter: test_peek_partner_claude_reads_codex_session
    Level: integration
    Test Double: fixture_jsonl
    Targets: src/cowork/codex.rs, src/cowork/peek.rs
  Given fixture 目录有一个 rollout jsonl，首行 `session_meta.payload.cwd` 为 "/tmp/fake-project"，后续 10 条是交替的 `response_item` user/assistant 消息
  And 当前 cwd 被测试桩设为 "/tmp/fake-project"
  When 以 `tool="codex"`、`limit=30` 调用 `mempal_peek_partner`
  Then 返回的 `partner_tool` 为 "codex"
  And 返回的 `messages` 长度为 10
  And `messages[0].at` 早于 `messages[9].at`（升序排列）
  And `truncated` 为 false
  And `session_path` 指向该 fixture jsonl 的绝对路径

Scenario: auto 模式通过 ClientInfo 推断 partner
  Test: test_peek_partner_auto_mode_infers_partner
  Given MCP 握手时 `InitializeRequest.client_info.name` 被设为 "claude-code"
  And fixture 目录 `tests/fixtures/cowork/codex/` 有一个匹配当前 cwd 的 rollout jsonl
  When 以 `tool="auto"` 调用 `mempal_peek_partner`
  Then 返回的 `partner_tool` 为 "codex"
  And `messages` 来自该 Codex fixture

Scenario: 消息超过 limit 时按尾部截断
  Test: honors_limit_by_taking_tail_and_sets_truncated
  Given fixture jsonl 有 "100" 条 user+assistant 消息
  When 以 `limit=5` 调用 `mempal_peek_partner`
  Then 返回的 `messages` 长度为 5
  And `messages` 全部来自 jsonl 尾部最后 5 条
  And `truncated` 为 true

Scenario: MEMORY_PROTOCOL 字符串包含 Rule 8 和 Rule 9
  Test:
    Filter: contains_rule_8_partner_awareness
    Level: unit
    Targets: src/core/protocol.rs
  Given mempal MCP server 的 `ServerInfo.instructions` 常量
  When 读取该字符串
  Then 包含子串 "Rule 8" 或 "PARTNER AWARENESS"
  And 包含子串 "Rule 9" 或 "DECISION CAPTURE"
  And 包含工具名 "mempal_peek_partner"

Scenario: auto 模式在 ClientInfo 缺失时返回错误
  Test: test_peek_partner_auto_mode_errors_without_client_info
  Given MCP 握手时 `InitializeRequest.client_info.name` 为 None 或空字符串
  When 以 `tool="auto"` 调用 `mempal_peek_partner`
  Then 返回 error 且 message 包含子串 "cannot infer partner"

Scenario: 调用方 peek 自己的 session 被拒绝
  Test: rejects_self_peek_when_caller_is_same_tool
  Given MCP 握手时 `InitializeRequest.client_info.name` 被设为 "codex"
  When 以 `tool="codex"` 显式调用 `mempal_peek_partner`
  Then 返回 error 且 message 包含子串 "cannot peek your own session"

Scenario: 最近 session 超过 30 分钟未活跃时 partner_active 为 false
  Test: test_peek_partner_reports_inactive_session
  Given fixture jsonl 的文件 mtime 被设为 "45" 分钟之前
  When 以 `tool="codex"` 调用 `mempal_peek_partner`
  Then 返回 `partner_active` 为 false
  And 返回 `messages` 长度大于 0（仍返回最近一次的内容）

Scenario: 跨项目 session 被 cwd 过滤排除
  Test:
    Filter: test_peek_partner_filters_by_project_cwd
    Level: integration
    Test Double: fixture_jsonl
    Targets: src/cowork/codex.rs
  Given fixture 目录里两个 Codex jsonl：一个 `session_meta.payload.cwd` 为 "/tmp/project-a"，另一个为 "/tmp/project-b"，后者 mtime 更新
  And 当前 cwd 被测试桩设为 "/tmp/project-a"
  When 以 `tool="codex"` 调用 `mempal_peek_partner`
  Then 返回的 `session_path` 指向 `project-a` 对应的 jsonl
  And 返回的 `messages` 不包含 `project-b` jsonl 里的内容

Scenario: peek 操作不产生任何 mempal 写入副作用
  Test: test_peek_partner_has_no_mempal_side_effects
  Given mempal 数据库中 `drawers` 表行数为 "N"
  And `palace.db` 的 `schema_version` 为 4
  When 执行 3 次任意参数的 `mempal_peek_partner` 调用
  Then `drawers` 表行数仍为 "N"
  And `schema_version` 仍为 4
  And 最后一次调用之后 `triples` 表行数未变化

Scenario: 项目目录下没有任何 session 文件时返回空消息
  Test:
    Filter: test_peek_partner_returns_empty_when_no_session
    Level: integration
    Test Double: empty_fixture_dir
    Targets: src/cowork/claude.rs
  Given fixture 目录 `tests/fixtures/cowork/claude/` 为空
  And 当前 cwd 被测试桩设为一个没有对应 encoded 目录的路径 "/tmp/no-session-project"
  When 以 `tool="claude"` 调用 `mempal_peek_partner`
  Then 返回 `messages` 长度为 0
  And `partner_active` 为 false
  And `session_path` 为 null 或空字符串

Scenario: tool-use 内部结构被 adapter 过滤，只返回 user/assistant 文本
  Test:
    Filter: filters_tool_use_blocks_and_is_meta_entries
    Level: unit
    Test Double: fixture_jsonl
    Targets: src/cowork/claude.rs, src/cowork/codex.rs
  Given fixture jsonl 混合了 "3" 条 user 消息、"3" 条 assistant 消息、"5" 条 tool_use 或 tool_result 条目
  When 以对应 `tool` 调用 `mempal_peek_partner`
  Then 返回的 `messages` 长度为 6
  And 每条 `messages[i].role` 为 "user" 或 "assistant"
  And 返回的 `messages` 不包含任何 tool_use id 或 tool_result 的序列化结构

spec: task
name: "P9: Progressive disclosure in mempal_search + mempal_read_drawer MCP tool"
tags: [feature, search, mcp, context-budget]
estimate: 1d
---

## Intent

给 `mempal_search` 增加**渐进披露模式**（default off）：开启时返回每条结果的 `content` 字段截断到 `preview_chars`（默认 120 字符，word-boundary 对齐），同时暴露新 MCP 工具 `mempal_read_drawer(drawer_id)` / `mempal_read_drawers(drawer_ids)` 按需拿原文。

**动机**：mempal raw verbatim 存储的副作用——`mempal_search` 一次 top_k=10 的查询可能返回数万 token 进 agent 上下文。对 context 预算敏感的 CLI agent（长 session、多项目切换），一次检索就能撑爆剩余预算。claude-mem `plugin/skills/mem-search/SKILL.md:32-75` 的 3-Layer Workflow 已实证"10x token savings by filtering before fetching"。

**Default off**：保持当前 verbatim 返回行为不变，opt-in 激活。

**v3 判决依据**：v2 确认 claude-mem 实际落地（`SearchOrchestrator.ts:251-274`），mempal 吸收是刚需——因为 raw 存储让 token 爆仓问题比 claude-mem 的摘要式更严重。

## Decisions

- 配置：
  ```toml
  [search]
  progressive_disclosure = false
  preview_chars = 120
  ```
- `preview_chars` 截断规则：
  - 若 `content.len() <= preview_chars`，返回原 content 不加后缀
  - 否则截到最近的 word boundary（空格 / 标点 / CJK 字符边界），≤ `preview_chars`，追加 `"…"`（单字符 U+2026，不是三点 ...）
  - 截断必须符合 UTF-8 char boundary（不切半个码点）
- 新增 MCP 工具 `mempal_read_drawer`：
  - input schema: `{ drawer_id: String }`
  - output schema: `{ drawer_id, content, wing, room, source_file, importance_stars, created_at, updated_at, merge_count }`
  - `content` 永远是完整 raw verbatim（不受 `progressive_disclosure` 开关影响）
  - 找不到 drawer_id 返回 MCP error（`code=404-like`）
- 新增 MCP 工具 `mempal_read_drawers`：
  - input schema: `{ drawer_ids: Vec<String>, max_count: Option<u32> }`（默认 max 20）
  - output: `{ drawers: Vec<DrawerDto>, not_found: Vec<String> }`
  - 批量读取，超过 `max_count` 截断并返回 warn 字段
- `SearchResultDto.content` 在 `progressive_disclosure = true` 时是 preview；为区分，**新增 bool 字段 `content_truncated: bool`**（只在 progressive 模式下可能为 true）
- P7 的 5 个 AAAK signal 字段（`entities` / `topics` / `flags` / `emotions` / `importance_stars`）**基于完整 content 计算**，即使 progressive 模式下 `content` 被截断，signals 仍反映原文（在截断前调用 `aaak::signals::analyze(&full_content)`）
- MCP ServerInfo.instructions 新增 workflow rule（编号 `10.` 续 9 条后）：
  ```
  RULE 10 (progressive disclosure): When progressive mode is active (server announces
  `progressive_disclosure_active=true` in ServerInfo), mempal_search returns truncated previews.
  Use mempal_read_drawer(drawer_id) to fetch full content for specific drawers you decide to keep.
  For narrow queries (expecting 1-3 results), set `disable_progressive=true` in the search request
  to get verbatim directly.
  ```
- `mempal_search` input schema 增加可选 `disable_progressive: Option<bool>`（per-call override）
- `ServerInfo.instructions` 动态检测 `[search] progressive_disclosure` 开关，active 时注入 RULE 10，否则不注入
- 向 `ServerInfo` 扩展一个 `progressive_disclosure_active: bool` 字段（也可以挂在 `caps`）
- Preview 生成是 `O(content.len())` 纯字符串切片，不触发任何 db / embedding / LLM 调用
- 无 schema 变更（不 bump `CURRENT_SCHEMA_VERSION`）

## Boundaries

### Allowed
- `crates/mempal-search/src/preview.rs`（新建：word-boundary truncation 算法）
- `crates/mempal-search/src/lib.rs`（`pub mod preview`）
- `crates/mempal-mcp/src/tools.rs`（`SearchResultDto` 加 `content_truncated` 字段；新增 `DrawerDto`、`ReadDrawersResponse`）
- `crates/mempal-mcp/src/server.rs`（新 tool handlers `mempal_read_drawer` / `mempal_read_drawers`；`mempal_search` handler 按开关走 preview；`ServerInfo.instructions` 条件注入 RULE 10）
- `crates/mempal-mcp/src/lib.rs`（export 新 types）
- `crates/mempal-core/src/config.rs`（`SearchConfig { progressive_disclosure, preview_chars }`）
- `tests/progressive_disclosure.rs`, `tests/read_drawer_tool.rs`（新建）

### Forbidden
- 不要修改 `drawers` / `drawer_vectors` / `triples` schema
- 不要 bump `CURRENT_SCHEMA_VERSION`
- 不要让 progressive mode 影响 `mempal_read_drawer` 的返回（永远 full）
- 不要在 preview 逻辑里调 AAAK compress（P7 证明 AAAK 非 byte-level 压缩，不减 token）
- 不要把 preview 字符串缓存到 db——每次 search 都现算
- 不要对 `mempal_peek_partner` / `mempal_kg` / `mempal_tunnels` / `mempal_ingest` 等 MCP 工具加 preview 逻辑
- 不要在 preview 中切半个 emoji / CJK 码点（UTF-8 boundary 必须对齐）
- 不要给 `mempal_search` 加 `context_budget_tokens` 参数自动裁剪（Out of Scope）
- 不要在 progressive 模式下省略 `drawer_id` / `source_file` / `importance_stars` / AAAK signals——这些是 preview 的关键 metadata

## Out of Scope

- Agent 上下文预算自感知（`context_budget_tokens` 参数）
- 基于历史查询模式的自适应 preview_chars 调优
- AAAK-based preview（独立未来工作）
- Timeline MCP 工具（claude-mem 的第二层；mempal 用 `mempal_tunnels` + `mempal_search` 已覆盖类似需求，无需新增）
- Embedding-space 再排（preview 仅截断，不改排序）
- `mempal_read_drawer` 的 content 解压 / 格式转换
- 按 wing 的 preview_chars 差异化配置

## Completion Criteria

Scenario: progressive_disclosure=false 时 content 字段原文不变
  Test:
    Filter: test_disabled_returns_verbatim_content
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given drawer content "A long string of 500 characters..."（500 字符）
  And `progressive_disclosure = false`
  When 调 `mempal_search` 命中该 drawer
  Then 返回 `SearchResultDto.content` byte-level 等于原文
  And `content_truncated == false`

Scenario: progressive_disclosure=true 时 content 被截到 preview_chars
  Test:
    Filter: test_enabled_truncates_content
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs, crates/mempal-search/src/preview.rs
  Given drawer content 500 字符
  And `preview_chars = 120`
  When 调 `mempal_search` 命中该 drawer
  Then `content.chars().count() <= 120 + 1`（+ 1 for `…`）
  And `content` 以 `"…"` 结尾
  And `content_truncated == true`

Scenario: 短 content 不被截断也不加省略号
  Test:
    Filter: test_short_content_not_truncated
    Level: unit
    Targets: crates/mempal-search/src/preview.rs
  Given `preview_chars = 120` 和 50 字符的 content
  When 调 `preview::truncate(&content, 120)`
  Then 返回值 == content（byte-level）
  And 不以 `"…"` 结尾

Scenario: 截断在 word boundary 对齐
  Test:
    Filter: test_truncation_aligns_to_word_boundary
    Level: unit
    Targets: crates/mempal-search/src/preview.rs
  Given content "The quick brown fox jumps over the lazy dog"
  And `preview_chars = 20`
  When 调 `preview::truncate`
  Then 返回值不切到单词中间
  And 返回值长度（chars） <= 20 + 1

Scenario: CJK 内容截断对齐 UTF-8 char boundary
  Test:
    Filter: test_cjk_truncation_utf8_safe
    Level: unit
    Targets: crates/mempal-search/src/preview.rs
  Given content "系统决策：采用共享内存同步机制解决状态漂移问题的根本原因是并发安全"（27 字符）
  And `preview_chars = 10`
  When 调 `preview::truncate`
  Then 返回值是合法 UTF-8
  And 返回值 `chars().count() <= 11`
  And 返回值以 `"…"` 结尾
  And 返回值不切半个汉字

Scenario: AAAK signals 基于完整 content 计算，不受截断影响
  Test:
    Filter: test_signals_computed_from_full_content
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given drawer content 含 "Decision: ..."（在 300-400 字符区间，超过 preview_chars=120）
  And progressive 开启
  When 调 `mempal_search` 命中该 drawer
  Then `content` 被截断
  And `flags` 字段含 "DECISION"（signal 来自完整原文）

Scenario: mempal_read_drawer 返回完整 raw content
  Test:
    Filter: test_read_drawer_returns_full_verbatim
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given drawer 原文 500 字符
  And progressive 开启
  When 调 `mempal_read_drawer({ drawer_id })`
  Then 返回 `DrawerDto.content` byte-level 等于 500 字符原文
  And 返回不含 `content_truncated` 字段（或 always false）

Scenario: mempal_read_drawer 对不存在的 id 返回 error
  Test:
    Filter: test_read_drawer_not_found
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given palace 中不存在 id "nonexistent"
  When 调 `mempal_read_drawer({ drawer_id: "nonexistent" })`
  Then MCP 返回 error response（非 success）
  And error 含 "not found" 或等价消息

Scenario: mempal_read_drawers 批量返回 + not_found 列表
  Test:
    Filter: test_read_drawers_batch_with_not_found
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given drawers A、B 存在，C 不存在
  When 调 `mempal_read_drawers({ drawer_ids: [A, B, C] })`
  Then `drawers` 数组含 A 和 B
  And `not_found` 数组含 "C"

Scenario: disable_progressive 参数 per-call 覆盖全局开关
  Test:
    Filter: test_per_call_disable_progressive
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given `progressive_disclosure = true` 全局
  When 调 `mempal_search({ query: "foo", disable_progressive: true })`
  Then 返回 `content` 是 full verbatim（未截断）
  And `content_truncated == false`

Scenario: ServerInfo.instructions 条件注入 RULE 10
  Test:
    Filter: test_server_info_injects_rule_10_when_active
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given `progressive_disclosure = true`
  When 客户端请求 MCP ServerInfo
  Then `instructions` 字符串含 "RULE 10"
  And 含 "mempal_read_drawer"

Scenario: progressive=false 时 ServerInfo 不含 RULE 10
  Test:
    Filter: test_server_info_omits_rule_10_when_inactive
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given `progressive_disclosure = false`
  When 客户端请求 MCP ServerInfo
  Then `instructions` 不含 "RULE 10"
  And 客户端按 verbatim 协议工作

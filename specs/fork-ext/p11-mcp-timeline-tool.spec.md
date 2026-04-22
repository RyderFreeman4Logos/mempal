spec: task
name: "P11: `mempal_timeline` MCP tool (project narrative aggregator)"
tags: [feature, mcp, narrative, claude-mem-parity]
estimate: 0.5d
---

## Intent

为 MCP 层补齐 claude-mem 的 `timeline` 工具语义：agent 在 session 中途调一次 `mempal_timeline` 就能拿到"本 project 按 importance + 时间排序的叙事视图"，不需要构造搜索 query。

**动机**：
- `mempal prime`（p11-session-priming-hook）是 SessionStart 的一次性快照；session 中途 agent 想"再看一眼最近的项目记忆"得有 MCP 入口
- `mempal_search` 是 query-driven，`mempal_tail/timeline/stats`（p10-cli-dashboard）是 CLI 操作员视角；agent 通过 MCP 只能走 search，缺 aggregate view
- claude-mem 有 `timeline` MCP tool；此 spec 做同样语义的 mempal 版本

**与 `mempal prime` 的差异**：
- `prime` 是 CLI，单向输出 formatted 文本，供 hook 注入 session
- `mempal_timeline` 是 MCP tool，返回结构化 `TimelineResponse` DTO，agent 在 session 中 on-demand 调用

## Decisions

- 新 MCP tool `mempal_timeline`（第 11 个 MCP 工具），走 `#[tool]` 宏注册到 `MempalMcpServer`
- Request schema:
  ```rust
  #[derive(Deserialize, JsonSchema)]
  pub struct TimelineRequest {
      pub project_id: Option<String>,       // absent → infer from client roots
      pub since: Option<String>,            // ISO8601 or relative ("7d", "24h"); default "30d"
      pub until: Option<String>,            // ISO8601; default now
      pub top_k: Option<usize>,             // default 20, max 100
      pub min_importance: Option<u8>,       // default 1, i.e., 全返回
      pub wing: Option<String>,             // optional filter
      pub room: Option<String>,             // optional filter
  }
  ```
- Response schema:
  ```rust
  #[derive(Serialize, JsonSchema)]
  pub struct TimelineResponse {
      pub project_id: Option<String>,
      pub generated_at: String,
      pub window: TimelineWindow { since: String, until: String },
      pub entries: Vec<TimelineEntry>,
      pub stats: TimelineStats { total_in_window: u64, returned: usize, top_wings: Vec<WingCount> },
      pub system_warnings: Vec<SystemWarning>,
  }
  pub struct TimelineEntry {
      pub drawer_id: String,
      pub added_at: String,
      pub importance_stars: u8,
      pub wing: String,
      pub room: Option<String>,
      pub preview: String,           // 200-char UTF-8 safe truncation
      pub preview_truncated: bool,
      pub original_content_bytes: u64,  // 对齐 p9-progressive-disclosure `content_truncated` 协议
  }
  ```
- 服务端实现：
  - 走 `ProjectSearchScope::from_request(project_id, include_global=false, all_projects=false, strict=true)` 做 project 硬过滤（贯彻 p10-project-vector-isolation）
  - SQL: `SELECT ... FROM drawers WHERE project_id = ? AND added_at >= ? AND added_at < ? AND importance >= ? [AND wing = ?] [AND room = ?] ORDER BY importance DESC, added_at DESC LIMIT ?`
  - preview 截断走已有 `src/search/preview.rs::truncate`（p9 产物）
  - `original_content_bytes` 填 raw content 字节数，便于 agent 判断是否该 `mempal_read_drawers` 拿全文
- **不调 embedder**（aggregate view 不需要 query vector，degraded-safe）
- tunnel resolver **不参与**（timeline 是 single-project 视图；tunnel hints 只在 search 结果里）
- 协议注入：把 `mempal_timeline` 列入 `MEMORY_PROTOCOL.instructions` 里"11 条规则"的适当位置（新增一条 rule："Use `mempal_timeline` instead of broad-match `mempal_search` when you want project state overview without a specific question in mind."）

## Boundaries

### Allowed
- `src/mcp/server.rs`（加 `#[tool] mempal_timeline` handler）
- `src/mcp/timeline.rs`（新 module，handler 主体）
- `src/core/timeline.rs`（SQL 查询 + DTO 组装；与 `p11-session-priming-hook` 的 `priming.rs` 不 merge——两者 query 相似但消费方不同，硬共享反而 coupling）
- `tests/mcp_timeline.rs`（新建集成测试）
- `src/mcp/protocol.rs`（`MEMORY_PROTOCOL` 新增规则 + 把 timeline tool 列入 ServerInfo.tools 描述）

### Forbidden
- 不改 schema / 不 bump `fork_ext_version`
- 不做 vector 检索
- 不调 tunnel resolver
- 不返回完整 `content`（只 preview + `original_content_bytes`）
- 不做 AAAK 结构化信号注入（那是 search 结果的特性，p7）
- 不在 CLI 侧暴露新子命令（`mempal timeline` 已由 p10-cli-dashboard 占据；此 spec 纯 MCP）
- 不允许 `all_projects=true`（timeline 是 single-project 语义；跨 project 概览是 dashboard 的事）
- 不缓存（每次全量 SQL；palace.db local 查询成本极低）

## Out of Scope

- `mempal_search` 和 `mempal_timeline` 的合并入口（两者语义独立，agent 自己选用）
- LLM narrative 生成（只返回 raw entries）
- 基于 KG triples 的 timeline（现有 `mempal_kg timeline` action 已覆盖 entity-based timeline）
- 历史 diff / "自上次 timeline 以来的新增"（stateless tool，不维护 client cursor）
- 配置化字段子集返回（总是返回完整 entry shape）

## Completion Criteria

Scenario: 基本调用返回按 importance + recency 排序
  Test:
    Filter: test_timeline_default_ordering
    Level: integration
    Targets: src/mcp/timeline.rs
  Given 10 条 drawer, 混合 importance 1-5, 随机 added_at
  When 调 `mempal_timeline` 无 args（但 MCP peer 声明了 root）
  Then `entries[0].importance_stars` == 5
  And `entries[0].added_at` 是 5 星中最新
  And `entries.len() == 10`（默认 top_k=20, 但只有 10 条）

Scenario: project scope 硬过滤
  Test:
    Filter: test_timeline_enforces_project_scope
    Level: integration
    Targets: src/mcp/timeline.rs
  Given drawer A 属于 project `foo`, drawer B 属于 `bar`
  And MCP peer root 推导得到 `foo`
  When 调 `mempal_timeline`
  Then `entries` 只含 drawer A
  And `project_id == "foo"`

Scenario: `since: "7d"` 过滤窗口
  Test:
    Filter: test_timeline_since_filter_7d
    Level: integration
    Targets: src/core/timeline.rs
  Given 5 条 drawer 在 7 天内, 5 条 30 天前
  When 调 `mempal_timeline { since: "7d" }`
  Then `entries.len() == 5`
  And `stats.total_in_window == 5`

Scenario: preview 截断信号
  Test:
    Filter: test_timeline_preview_truncation_signal
    Level: integration
    Targets: src/mcp/timeline.rs
  Given drawer content 长 500 字节
  When 调 `mempal_timeline`
  Then `entries[0].preview.len() <= 200`（UTF-8 char 粒度）
  And `entries[0].preview_truncated == true`
  And `entries[0].original_content_bytes == 500`

Scenario: degraded embedder 下 timeline 仍工作
  Test:
    Filter: test_timeline_degraded_embedder_still_works
    Level: integration
    Targets: src/mcp/timeline.rs
  Given `global_embed_status()` degraded
  When 调 `mempal_timeline`
  Then 返回 200 OK + entries
  And `system_warnings` 含 embedder degraded warning（信息性，不是 error）

Scenario: 参数校验 `top_k` 上限
  Test:
    Filter: test_timeline_top_k_upper_bound
    Level: unit
    Targets: src/mcp/timeline.rs
  When 调 `mempal_timeline { top_k: 500 }`
  Then 返回 `invalid_params` error 含 "top_k exceeds max 100"

Scenario: 拒绝 `all_projects`
  Test:
    Filter: test_timeline_rejects_all_projects
    Level: unit
    Targets: src/mcp/timeline.rs
  When 调 `mempal_timeline { all_projects: true }`（即使 request schema 不含此字段，服务端也应在 extra 字段时 ignore + 不意外 cross-project）
  Then 行为等同不传 —— 仍然 single-project
  And 不返回其他 project 的 drawer

Scenario: 协议头注入
  Test:
    Filter: test_memory_protocol_mentions_timeline
    Level: unit
    Targets: src/mcp/protocol.rs
  When 读 `MempalMcpServer::get_info().instructions`
  Then 含 "mempal_timeline" 字样
  And 含 "project state overview without a specific question" 近似表述

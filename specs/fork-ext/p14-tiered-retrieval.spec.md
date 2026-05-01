spec: task
name: "P14: Tiered retrieval for mempal_context (T1/T2/T3 assembly with budget allocation)"
tags: [feature, context, retrieval, mcp, ranking]
estimate: 1.5d
---

## Intent

mempal 目前只有一个检索入口 `mempal_search`，所有 drawer 均以同一策略（BM25 + vector + RRF）召回。`mempal_context` 虽已按 dao/shu/qi 分层组装知识，但底层检索依然扁平——dao_tian（决策/规则）和 shu（证据）使用相同的相关度评分，没有反映二者不同的使用场景。

**核心设计**：模仿 MemOS V7 三层检索策略，为 `mempal_context` 引入**分层装配**：

| Tier | mempal 映射 | 内容特征 | 评分策略 |
|------|------------|---------|---------|
| T1 | dao_tian（决策/规则/feedback） | 高 importance + type=decision/feedback | importance（或 effective_importance）× recency |
| T2 | shu（evidence） | query 相关的原始 drawer | hybrid search（BM25 + vector + RRF） |
| T3 | qi（operational） | 最近 session 上下文、active tunnels、KG 邻居 | pure recency + graph proximity |

三层之间有**可配 token budget 分配**，并支持 `trigger` 参数（session_start / on_demand / repair）在不同使用场景下动态调整各层权重。

**动机**：issue #117；MemOS V7 three-tier retrieval。本 spec 为**纯逻辑增强**，无 schema 迁移；受益于 p13-importance-decay 的 `effective_importance` 字段（若已实现则 T1 使用 effective_importance，否则 fallback 到 importance）。

## Decisions

### 三层评分策略

**T1 — dao_tian 层**：
- 候选来源：`WHERE drawer.type IN ('decision', 'feedback', 'rule')` 且 `importance >= min_t1_importance`（默认 3）
- 排序：`score = (effective_importance OR CAST(importance AS REAL)) × recency_weight`
  - `recency_weight = exp(-λ × days_since_ingest)`，默认 `λ = 0.01`（缓慢衰减，决策记忆保持长期相关）
- Budget：按 token 估算（字符数 / 4），取前 K 条直到 T1 budget 耗尽
- Active patterns（p13-pattern-induction 已实现时）也在 T1 中注入 `recurring_themes`

**T2 — shu 层**：
- 候选来源：现有 `mempal_search` 混合检索（BM25 + vector + RRF）
- 排序：原 RRF 分数（不改）；若 p13-importance-decay 已实现，以 `effective_importance` 做后处理二次排序
- Budget：token budget 中最大的一层（默认 50%）

**T3 — qi 层**：
- 候选来源：
  1. `WHERE drawer.created_at >= now - recency_window_days`（默认 3 天内）
  2. KG 邻居：query drawer 的一度 KG triples 对端 entity 关联的 drawer
  3. Active tunnels：`mempal_tunnels` 中指向当前 wing 的 tunnel target
- 排序：`created_at DESC`（纯时间序）

### Budget 分配

```toml
[context.budget]
total_chars = 8000           # 总 token 预算（字符估算）
t1_ratio = 0.30              # T1 分配 30%
t2_ratio = 0.50              # T2 分配 50%
t3_ratio = 0.20              # T3 分配 20%
overflow_to_t2 = true        # T1/T3 未用完的预算转入 T2
```

各层独立截断：先按 ratio 分配 chars，各层取前 K 条不超过分配量；`overflow_to_t2 = true` 时将剩余 budget 追加给 T2。

### Trigger 参数

`mempal_context` 新增可选 `trigger` 字段：

| Trigger | T1 weight | T2 weight | T3 weight | 说明 |
|---------|-----------|-----------|-----------|------|
| `session_start`（默认）| 1.0 | 0.8 | 1.2 | 重视近期上下文和决策规则 |
| `on_demand` | 0.7 | 1.3 | 0.5 | 重视 query 相关性（深度任务中） |
| `repair` | 1.5 | 0.8 | 0.5 | 重视决策记忆（出错修复场景）|

Tier budget 按 weight 比例动态调整（权重归一化后再乘以 total_chars × tier_ratio）。

### 输出结构变化

`mempal_context` 响应（JSON）结构增加分层标记：

```json
{
  "t1_dao_tian": [ { "drawer_id": "...", "content": "...", "type": "decision", ... } ],
  "t2_shu": [ { "drawer_id": "...", "content": "...", "matched_pattern_id": null, ... } ],
  "t3_qi": [ { "drawer_id": "...", "content": "...", "source": "recency|kg|tunnel", ... } ],
  "recurring_themes": [...],   // from p13-pattern-induction if available
  "system_warnings": [...],
  "budget_used": { "t1_chars": 2100, "t2_chars": 3800, "t3_chars": 1200, "total": 7100 }
}
```

现有消费方兼容：原有 `dao_tian`/`shu`/`qi` 字段**保留**（与 t1/t2/t3 别名共存），避免破坏现有 agent 的 context 解析。

### Config

`[context]` 子段，全部属于热重载白名单：
- `tiered_retrieval_enabled: bool = true`（false 时退化为现有平铺行为，向下兼容）
- budget 子段（见上）
- `min_t1_importance: u8 = 3`
- `t3_recency_window_days: u64 = 3`
- `t1_recency_lambda: f64 = 0.01`

## Boundaries

### Allowed
- `crates/mempal-core/src/config.rs` — 新增 `[context]` 子段 `ContextConfig`（含 budget、trigger 权重）
- `crates/mempal-mcp/src/tools/context.rs`（或等价）— 三层装配逻辑、trigger 参数处理、budget 分配
- `crates/mempal-search/src/tiered.rs` — 新建：T1/T2/T3 各层检索策略实现
- `crates/mempal-search/src/hybrid.rs` — T2 调用路径复用现有混合检索
- `crates/mempal-mcp/src/tools.rs` — `mempal_context` input schema 新增 `trigger` 字段，output schema 新增 t1/t2/t3/budget_used
- `crates/mempal-cli/src/context.rs`（或等价）— CLI `mempal context` 子命令支持 `--trigger` 参数（若 CLI context 已存在）
- `tests/tiered_retrieval.rs` — 新建集成测试

### Forbidden
- 不要改 `mempal_search` 工具的签名（tiered retrieval 仅影响 `mempal_context`；`mempal_search` 保持独立）
- 不要引入 schema migration（本 spec 为纯逻辑增强，无新表/新列）
- 不要删除现有 `dao_tian`/`shu`/`qi` 字段（新的 `t1`/`t2`/`t3` 别名并存，避免 breaking change）
- 不要在 T3 中执行向量 ANN 搜索（T3 是 recency + graph，不是 embedding 相关性）
- 不要让 trigger 参数影响 T2 的 RRF 融合权重（trigger 只影响 budget 分配比例，不改 RRF）
- 不要在 `tiered_retrieval_enabled = false` 路径引入新行为（fallback 必须与现有 `mempal_context` 行为完全一致）

## Out of Scope

- 跨 MCP 工具的检索策略统一（`mempal_search` 与 `mempal_context` 各自独立，不合并入口）
- T1 type 字段自动分类（drawer type 仍由 ingest 时 agent 显式提供，不做自动推断）
- 自适应 budget 学习（根据历史 session 动态调 ratio；留未来）
- Web UI（违反 CLI-first 约束）
- schema 迁移（本 spec 不涉及）

## Completion Criteria

Scenario: session_start trigger 按默认权重分层装配 context
  Test:
    Filter: test_tiered_context_session_start_default_weights
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given palace.db 含若干 decision/feedback type drawer（importance >= 3）、普通 drawer、近 3 天 drawer
  When 调用 `mempal_context({trigger: "session_start"})`
  Then 响应含 `t1_dao_tian`、`t2_shu`、`t3_qi` 三个非空数组（数据充足时）
  And `t1_dao_tian` 中每条 drawer 的 `type` 为 `"decision"` 或 `"feedback"` 或 `"rule"`
  And `budget_used.total_chars` <= `total_chars` 配置值

Scenario: repair trigger 提升 T1 权重，T1 获得更多 budget
  Test:
    Filter: test_tiered_context_repair_trigger_boosts_t1
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given 相同 palace.db
  When 分别调用 `mempal_context({trigger: "session_start"})` 和 `mempal_context({trigger: "repair"})`
  Then repair 响应的 `budget_used.t1_chars` 大于 session_start 响应的 `budget_used.t1_chars`
  And repair 响应的 `t1_dao_tian` 数组长度 >= session_start 响应的同数组长度（相同数据集下）

Scenario: on_demand trigger 向 T2 倾斜 budget
  Test:
    Filter: test_tiered_context_on_demand_boosts_t2
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given 相同 palace.db
  When 分别调用 `mempal_context({trigger: "on_demand"})` 和 `mempal_context({trigger: "session_start"})`
  Then on_demand 响应的 `budget_used.t2_chars` 大于 session_start 响应的 `budget_used.t2_chars`

Scenario: budget 分配不超过 total_chars 上限
  Test:
    Filter: test_tiered_context_budget_does_not_exceed_total
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given palace.db 含大量 drawer（各层足以超出 budget）
  When 调用 `mempal_context({trigger: "session_start"})`
  Then `budget_used.total_chars <= total_chars`（不溢出）
  And `budget_used.t1_chars + budget_used.t2_chars + budget_used.t3_chars == budget_used.total_chars`

Scenario: overflow_to_t2=true 时 T1 未用完的 budget 转给 T2
  Test:
    Filter: test_tiered_context_overflow_budget_to_t2
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given `overflow_to_t2 = true`，T1 只有 1 条 drawer（远小于 t1 分配 budget）
  When 调用 `mempal_context`，T2 数据充足
  Then `budget_used.t2_chars` 超过 `total_chars * t2_ratio`（接收了 T1 溢出）

Scenario: T3 只包含 recency_window_days 内的 drawer
  Test:
    Filter: test_tiered_t3_respects_recency_window
    Level: integration
    Targets: crates/mempal-search/src/tiered.rs
  Given drawer-old 在 10 天前创建，drawer-new 在今天创建，均含 query 词
  When 调用 `mempal_context`（`t3_recency_window_days = 3`）
  Then `t3_qi` 含 drawer-new
  And `t3_qi` 不含 drawer-old

Scenario: tiered_retrieval_enabled=false 时退化为现有行为
  Test:
    Filter: test_tiered_context_disabled_falls_back
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given config `tiered_retrieval_enabled = false`
  When 调用 `mempal_context`
  Then 响应结构与旧有 `mempal_context` 兼容（含 `dao_tian`、`shu`、`qi` 字段）
  And 响应**不含** `budget_used` 字段

Scenario: 现有 dao_tian/shu/qi 字段在 tiered_retrieval 启用时仍保留
  Test:
    Filter: test_tiered_context_preserves_legacy_fields
    Level: integration
    Targets: crates/mempal-mcp/src/tools/context.rs
  Given `tiered_retrieval_enabled = true`
  When 调用 `mempal_context`
  Then 响应同时含 `dao_tian`（等于 `t1_dao_tian` 别名）、`shu`（等于 `t2_shu` 别名）、`qi`（等于 `t3_qi` 别名）
  And 别名数组内容与对应 tier 数组相同

Scenario: mempal_search 不受 tiered_retrieval 影响
  Test:
    Filter: test_search_unaffected_by_tiered_context_config
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given `tiered_retrieval_enabled = false` 或 `true`
  When 调用 `mempal_search({query: "foo"})`
  Then 返回结果格式与 tiered_retrieval 配置无关（search 工具独立）

Scenario: T1 recency_lambda 影响 decision drawer 排序
  Test:
    Filter: test_t1_recency_lambda_affects_ordering
    Level: unit
    Targets: crates/mempal-search/src/tiered.rs
  Given 2 条 decision drawer：drawer-old（30 天前 ingest，importance=5）和 drawer-new（今天 ingest，importance=3）
  And `t1_recency_lambda = 0.1`（相对大，近期偏好明显）
  When 计算 T1 scores
  Then drawer-new 的 score > drawer-old 的 score（recency 压过 importance 差距）

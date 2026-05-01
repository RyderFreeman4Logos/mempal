spec: task
name: "P13: Cross-session pattern induction from recurring semantic similarity"
tags: [feature, patterns, dedup, knowledge, schema, mcp]
estimate: 2d
---

## Intent

mempal 的语义去重（p5-semantic-dedup）在新内容与已有 drawer 相似时只发出警告。重复出现在多个 session 中的模式被当作噪声处理——但反复出现本身就是信号：**多个 session 独立地产生了相似记忆，说明这是一个值得关注的固定模式**。

**核心设计**：模仿 MemOS V7 "L2 Policy induction"，但**完全 LLM-free**——以向量质心 + 关键词重叠作为"归纳"机制，agent 自己完成从 exemplar 到规则的认知工作：
1. **Pattern detection**：扩展语义去重；当跨 N ≥ 3 个不同 session 的 drawer 余弦相似度 ≥ threshold，将其标记为"pattern candidate"
2. **Pattern storage**：新 `patterns` 表，记录 `signature`（topic/tag 向量质心）、exemplar 列表、session 计数、生命周期状态
3. **Pattern surfacing**：`mempal_search` 对命中 active pattern 的 exemplar 集群给予 score boost；`mempal_context` 包含 "recurring themes" 段
4. **CLI**：`mempal patterns list/show/retire` 手动管理生命周期
5. **MCP**：patterns 注入 `mempal_context` 的 T1/dao_tian 层（按 p14-tiered-retrieval 的分层设计）

**动机**：issue #116；MemOS V7 L2 policy induction。p5-semantic-dedup 是直接前驱（复用相似度计算路径）。

## Decisions

- fork-ext `fork_ext_version` `6 → 7` migration：
  - 新建 `patterns` 表：
    ```sql
    CREATE TABLE IF NOT EXISTS patterns (
        pattern_id    TEXT PRIMARY KEY,     -- UUID v4
        signature     BLOB NOT NULL,        -- centroid embedding (same dim as drawer_vectors)
        exemplar_ids  TEXT NOT NULL,        -- JSON array of drawer_id strings
        session_ids   TEXT NOT NULL,        -- JSON array of session_id strings (dedup)
        session_count INTEGER NOT NULL DEFAULT 0,
        topic_tags    TEXT,                 -- JSON array of top-N keywords (overlap heuristic)
        status        TEXT NOT NULL DEFAULT 'candidate', -- candidate | active | retired
        first_seen_at INTEGER NOT NULL,     -- unix epoch ms
        updated_at    INTEGER NOT NULL,
        project_id    TEXT                  -- optional, links to p10-project-vector-isolation
    );
    CREATE INDEX IF NOT EXISTS idx_patterns_status ON patterns(status);
    CREATE INDEX IF NOT EXISTS idx_patterns_project ON patterns(project_id);
    ```

- **Pattern detection** — 扩展现有 dedup 路径（`mempal-ingest` 中的语义去重检查）：
  - 去重警告触发（cosine ≥ `similarity_threshold`）时，额外检查命中的已有 drawer 的 `session_id`
  - 如果已有 drawer 集中，来自 ≥ `min_sessions` 个**不同** session 的相似 drawer 数量 ≥ `min_exemplars`，则满足 pattern candidate 条件
  - 创建 pattern：
    - `signature` = exemplar embeddings 的向量质心（逐元素算术均值）
    - `topic_tags` = exemplar drawers 中 TF-IDF top-5 词（基于 FTS5 已有词频，不新增依赖）
    - `exemplar_ids` = 满足条件的 drawer id 列表
    - `session_ids` = 对应 session id 去重列表

- **Pattern 生命周期**：
  - `candidate`（初始）→ `active`（当 `session_count >= promote_threshold`，默认 5）→ `retired`（手动或 `session_count` 无新增超过 `retire_after_days`）
  - 每次新的相似 ingest 命中已有 pattern 时：更新 `session_ids`（追加，去重）、更新质心（滑动平均）、检查升级条件

- **Config** `[patterns]` 子段（可热重载）：
  - `enabled: bool = true`
  - `similarity_threshold: f64 = 0.82`（与 p5-semantic-dedup 的阈值分开，允许独立调参）
  - `min_sessions: usize = 3`（触发 candidate 所需不同 session 数）
  - `min_exemplars: usize = 3`
  - `promote_threshold: usize = 5`（升为 active 所需 session_count）
  - `retire_after_days: u64 = 90`（无新增超过此天数自动 retire）

- **Pattern surfacing in `mempal_search`**：
  - 为每个 active pattern，计算 query embedding 与 `signature`（质心）的余弦相似度
  - 若相似度 ≥ `surfacing_threshold`（默认 0.75），pattern 匹配的 exemplar drawers 在 RRF 后处理阶段获得 `pattern_boost`（默认 +0.2，叠加在 effective_importance 上）
  - 搜索结果 DTO 新增可选字段 `matched_pattern_id: Option<String>`

- **`mempal_context` 集成**：
  - 响应新增 `recurring_themes: Vec<PatternSummary>` 段
  - `PatternSummary` = `{ pattern_id, topic_tags, session_count, exemplar_preview }`（截取第一条 exemplar 的 preview）
  - 只包含 `status = active` 且 `project_id` 匹配当前请求的 pattern（或 NULL）

- **Session ID 来源**：
  - 从 drawer 的 `source_file` 中提取（按现有 session_id 字段）
  - 若 drawer 无 session_id，用 `ingest_batch_id`（一次 ingest CLI 调用的 UUID）作为代理 session_id

- **CLI 命令**：
  - `mempal patterns list [--status candidate|active|retired] [--project <id>]` — 表格输出 pattern_id / topic_tags / session_count / status
  - `mempal patterns show <pattern_id>` — 详情：exemplar drawer 摘要列表 + signature 维度
  - `mempal patterns retire <pattern_id>` — 手动 retire

## Boundaries

### Allowed
- `crates/mempal-core/src/db/schema.rs` — fork_ext_version 6 → 7，创建 `patterns` 表
- `crates/mempal-core/src/config.rs` — 新增 `[patterns]` config 子段 `PatternsConfig`
- `crates/mempal-core/src/patterns.rs` — 新建：pattern CRUD、质心计算、lifecycle 逻辑
- `crates/mempal-core/src/lib.rs` — `pub mod patterns`
- `crates/mempal-ingest/src/dedup.rs` (或等价) — 扩展 dedup 检查，触发 pattern candidate 创建
- `crates/mempal-search/src/hybrid.rs` — active pattern 匹配 + exemplar boost
- `crates/mempal-mcp/src/tools.rs` — `mempal_context` 新增 `recurring_themes`；`mempal_search` DTO 新增 `matched_pattern_id`
- `crates/mempal-cli/src/patterns.rs` — 新建：`mempal patterns` 子命令实现
- `crates/mempal-cli/src/main.rs` — `patterns` 子命令注册
- `tests/pattern_induction.rs` — 新建集成测试

### Forbidden
- 不要用 LLM 生成 pattern 描述或 topic_tags（违反 LLM-free 约束；topic_tags 来自 FTS5 词频）
- 不要在 ingest 关键路径上同步计算质心（embedding 维度可能达 4096d，批量算术需异步）
- 不要把 `patterns` 表的 `signature` 列声明为 `vec0` virtual table（质心是元数据，不做 ANN 检索）
- 不要自动生成"规则文本"或"policy 语句"（agent 负责认知工作；mempal 只存 exemplars + tags）
- 不要让 pattern 检测阻塞 ingest 返回（失败时仅 warn，不 fail ingest）
- 不要在 `patterns` 表中存储 embedding 维度以外的 BLOB（`topic_tags` 用 JSON TEXT，不用 BLOB）

## Out of Scope

- 自动文本生成 policy/rule（需要 LLM）
- Pattern 的自动 skill 提升（见 p15-skill-crystallization）
- Pattern 跨 project 合并（不同 project 的 pattern 相互独立）
- Pattern 版本化历史（lifecycle 状态变更不记录 changelog）
- Web UI（违反 CLI-first 约束）

## Completion Criteria

Scenario: fork-ext migration 6 → 7 创建 patterns 表
  Test:
    Filter: test_fork_ext_migration_v6_to_v7_creates_patterns_table
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db `fork_ext_version == "6"`
  When 启动 mempal
  Then `fork_ext_version == "7"`
  And sqlite_master 中存在 table `patterns`，含 `pattern_id`, `signature`, `exemplar_ids`, `session_ids`, `session_count`, `status` 列
  And 存在索引 `idx_patterns_status`

Scenario: 来自 3 个不同 session 的相似 drawer 触发 pattern candidate 创建
  Test:
    Filter: test_pattern_candidate_created_from_three_sessions
    Level: integration
    Targets: crates/mempal-ingest/src/dedup.rs, crates/mempal-core/src/patterns.rs
  Given 3 条已有 drawer，来自 3 个不同 session_id，两两余弦相似度 >= 0.82
  When ingest 第 4 条与上述 3 条相似的 drawer（第 4 个不同 session）
  Then `patterns` 表新增 1 条 `status = "candidate"` 的记录
  And `session_count >= 3`
  And `exemplar_ids` JSON 数组长度 >= 3
  And `signature` blob 非空（质心已计算）

Scenario: 来自同一 session 的多条相似 drawer 不触发 pattern
  Test:
    Filter: test_same_session_drawers_dont_create_pattern
    Level: integration
    Targets: crates/mempal-ingest/src/dedup.rs
  Given 3 条相似 drawer，全部来自同一 session_id
  When ingest 第 4 条（同 session）
  Then `patterns` 表**不**新增记录（需要跨 session，非同 session 重复）

Scenario: session_count 达到 promote_threshold 后 pattern 升为 active
  Test:
    Filter: test_pattern_promotes_to_active_at_threshold
    Level: integration
    Targets: crates/mempal-core/src/patterns.rs
  Given `promote_threshold = 5`，已有 candidate pattern，session_count = 4
  When ingest 1 条新 session 的相似 drawer（session_count → 5）
  Then 该 pattern `status == "active"`

Scenario: active pattern 的 exemplar 在搜索中获得 boost
  Test:
    Filter: test_active_pattern_boosts_exemplar_results
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given 1 个 `status = "active"` pattern，query embedding 与其 signature 余弦相似度 >= 0.75
  When 调用 `mempal_search({query: "..."})`
  Then pattern 关联的 exemplar drawer 在结果中 `matched_pattern_id` 字段非 NULL
  And 该 drawer 排名高于相同 base effective_importance 但无 pattern 匹配的 drawer

Scenario: mempal_context 包含 active patterns 的 recurring_themes
  Test:
    Filter: test_context_includes_recurring_themes
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given 2 个 `status = "active"` pattern（含当前 project_id 或 NULL）
  When 调用 `mempal_context`
  Then 响应含 `recurring_themes` 数组，长度 == 2
  And 每条含 `pattern_id`、`topic_tags`、`session_count`

Scenario: candidate pattern 不出现在 mempal_context
  Test:
    Filter: test_context_excludes_candidate_patterns
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given 仅有 `status = "candidate"` pattern
  When 调用 `mempal_context`
  Then 响应 `recurring_themes` 为空数组

Scenario: mempal patterns list 输出 active 状态的 pattern
  Test:
    Filter: test_patterns_list_cli_shows_active
    Level: integration
    Targets: crates/mempal-cli/src/patterns.rs
  Given 1 个 active pattern，1 个 candidate pattern
  When 执行 `mempal patterns list --status active`
  Then stdout 含 active pattern 的 pattern_id 和 topic_tags
  And 不含 candidate pattern

Scenario: mempal patterns retire 手动 retire pattern
  Test:
    Filter: test_patterns_retire_cli
    Level: integration
    Targets: crates/mempal-cli/src/patterns.rs
  Given 1 个 `status = "active"` pattern
  When 执行 `mempal patterns retire <pattern_id>`
  Then 该 pattern `status == "retired"`
  And 后续 `mempal_search` 不再对其 exemplar 施加 boost

Scenario: patterns disabled 时 ingest 不进行 pattern 检查
  Test:
    Filter: test_patterns_disabled_skips_detection
    Level: integration
    Targets: crates/mempal-ingest/src/dedup.rs
  Given config `[patterns] enabled = false`
  When ingest 若干高相似 drawer
  Then `patterns` 表始终为空
  And ingest 正常完成（无 error）

Scenario: pattern 检测失败不阻塞 ingest
  Test:
    Filter: test_pattern_detection_failure_does_not_fail_ingest
    Level: integration
    Targets: crates/mempal-ingest/src/dedup.rs
  Given 模拟 pattern 质心计算 panic（如 embedding 维度为 0）
  When ingest 触发 pattern detection 路径
  Then ingest 成功返回（drawer 被写入）
  And 日志含 warn 级别的 pattern detection 错误
  And `patterns` 表无新增记录

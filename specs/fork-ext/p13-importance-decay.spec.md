spec: task
name: "P13: Importance decay & retrieval-driven value backpropagation"
tags: [feature, importance, decay, ranking, schema]
estimate: 1.5d
---

## Intent

mempal 的 `importance` 是 ingest 时写死的 1-5 静态标量——经过验证有用的 drawer 和事后证明误导性的 drawer 重要度相同，没有从任务结果向记忆质量反馈的回路。

**核心设计**：模仿 MemOS V7 "Reflect2Evolve" 的价值回传思想，但**完全 LLM-free**——纯 SQLite 算术实现 heuristic 衰减：
1. **时间衰减**：`effective_importance = base_importance × decay(days_since_last_hit)` — 长时间未命中的记忆自然下沉
2. **检索提升**：drawer 出现在搜索结果中且 agent 随后在同一 session 写入新内容 → 正向信号（"这条记忆是有用上下文"），轻微提升 effective_importance
3. **陈旧惩罚**：关联 KG triple 被 invalidate 的 drawer 按系数扣分
4. **存储**：新列 `last_accessed_at`、`access_count`、`effective_importance`；后者在访问时异步重算写回（查询路径不同步阻塞）

**动机**：issue #115；MemOS V7 reward backpropagation 概念验证。p5-wake-up-importance 提供基础 importance 字段，本 spec 在其之上增加动态层。

## Decisions

- fork-ext `fork_ext_version` `5 → 6` migration：
  - `drawers` 表加四列（`ALTER TABLE ADD COLUMN`，regular table 支持）：
    - `last_accessed_at INTEGER` — unix epoch ms，NULL 表示从未命中；**NULL → 使用 `added_at`（drawer 创建时间戳）作为 fallback**
    - `access_count INTEGER NOT NULL DEFAULT 0`
    - `accumulated_boost REAL DEFAULT 0.0` — 累计 session ingest boost，持久化存储
    - `effective_importance REAL NOT NULL DEFAULT 0.0`
  - 存量 drawer migration 时 `effective_importance = CAST(importance AS REAL)`（与原始重要度一致）
  - `CREATE INDEX idx_drawers_eff_importance ON drawers(effective_importance DESC)`

- **衰减公式**（参数均可热重载）：
  ```
  let days = (now_ms - last_accessed_at.unwrap_or(added_at)) / 86_400_000.0
  //         NULL fallback: use added_at (drawer creation timestamp)
  let decay = (-decay_rate * days).exp().max(floor)
  effective_importance = base_importance as f64 * decay + accumulated_boost.min(boost_cap)
  ```
  `[importance]` config 子段（default 值）：
  - `decay_rate: f64 = 0.01`（半衰期约 69 天；旧对数衰减 0.05 约需 60 年减半）
  - `floor: f64 = 0.1`（防止有用 drawer 被完全压制）
  - `boost_per_access: f64 = 0.15`（每次 session ingest boost）
  - `boost_cap: f64 = 2.0`（防止无限膨胀）
  - `stale_penalty: f64 = 0.5`（KG invalidated 乘数）

- **命中信号**：`mempal_search` 返回结果时，对每个命中的 drawer 异步执行单条 SQL UPDATE（**禁止先 SELECT 再 UPDATE 的 read-modify-write，整个计算和写回必须在一条 SQL UPDATE 语句内完成**）：
  ```sql
  UPDATE drawers SET
    last_accessed_at = :now_ms,
    access_count = access_count + 1,
    effective_importance = (
      CAST(importance AS REAL)
      * MAX(EXP(-:decay_rate * (:now_ms - COALESCE(last_accessed_at, added_at)) / 86400000.0), :floor)
      + MIN(accumulated_boost, :boost_cap)
    )
  WHERE id = :id
  ```
  此写操作在 tokio task 中批量 commit，不阻塞 search 响应路径

- **Session ingest boost**：MCP server 维护 per-session 命中 drawer id 集合（内存，不持久化）；同一 session 调用 `mempal_ingest` 时对命中集合内的 drawer 在单条 SQL UPDATE 内原子增加 `accumulated_boost` 并重算 `effective_importance`，然后清空集合
  - 结构：`MempalMcpContext` 加 `session_hit_drawers: HashSet<DrawerId>`
  - boost 触发条件：集合非空 AND `mempal_ingest` 在同 session 内被调用
  - **boost 必须原子写入**：`accumulated_boost = accumulated_boost + :boost_per_access` 与 `effective_importance` 重算在同一 SQL UPDATE 语句内执行

- **陈旧惩罚**：`mempal_fact_check` 发现 `StaleFact` 时，对关联 drawer 执行 `effective_importance *= stale_penalty`（同步，低频）

- **搜索排序**：RRF 融合产生候选集后，以 `effective_importance` 做**后处理二次排序**（不改 RRF 权重公式）

- **Config 热重载**：`decay_rate`、`floor`、`boost_per_access`、`boost_cap`、`stale_penalty` 全部属于 hot-reload 白名单

- **CLI 命令**：
  - `mempal audit --stale [--threshold 0.5]` — 列出 `effective_importance < threshold` 的 drawer，展示 id / wing / room / effective_importance / access_count / last_accessed_at
  - `mempal recompute-importance` — 全库批量重算 `effective_importance`（用于参数调整后校正）

- `mempal_search` 结果 DTO 新增 `effective_importance: f64` 字段（只读，让 agent 可观察衰减状态）

## Boundaries

### Allowed
- `crates/mempal-core/src/db/schema.rs` — fork_ext_version 5 → 6 migration
- `crates/mempal-core/src/config.rs` — 新增 `[importance]` config 子段 `ImportanceConfig`
- `crates/mempal-core/src/decay.rs` — 新建：衰减/提升算法（纯函数，便于单元测试）
- `crates/mempal-core/src/lib.rs` — `pub mod decay`
- `crates/mempal-search/src/hybrid.rs` — 后处理二次排序，异步 access update 分发
- `crates/mempal-mcp/src/server.rs` — `session_hit_drawers` 维护
- `crates/mempal-mcp/src/tools.rs` — search DTO 新增 `effective_importance`，ingest 触发 boost
- `crates/mempal-cli/src/audit.rs` — 新建：`mempal audit --stale` 实现
- `crates/mempal-cli/src/main.rs` — `audit` 子命令注册，`recompute-importance` 子命令
- `tests/importance_decay.rs` — 新建集成测试

### Forbidden
- 不要在 search 关键路径（返回响应之前）同步 `UPDATE drawers`（影响 p99 latency）
- 不要修改 RRF 权重公式（只改后处理排序步骤）
- 不要把 `effective_importance` 写入 AAAK signal 字段（内部排序信号，非对外内容信号）
- 不要允许 `mempal_ingest` 调用方显式设置 `effective_importance`（由系统计算）
- 不要对异步 access update 使用 `BEGIN IMMEDIATE`（高并发 search 时会产生锁竞争；WAL + deferred transaction）
- 不要自动删除低 effective_importance 的 drawer（`audit --stale` 仅展示，决策权在用户）

## Out of Scope

- LLM 评估记忆质量（违反 LLM-free 硬约束）
- 显式任务 episode 打分协议（需 agent-side 扩展，留未来）
- `effective_importance` 参与 FTS5 内部 BM25 分数（FTS5 分数轴独立，不支持自定义权重注入）
- Web UI 可视化（违反 CLI-first 约束）

## Completion Criteria

Scenario: fork-ext migration 5 → 6 添加重要度相关列
  Test:
    Filter: test_fork_ext_migration_v5_to_v6_adds_importance_columns
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db `fork_ext_version == "5"`
  When 启动 mempal
  Then `fork_ext_version == "6"`
  And `drawers` 表含 `last_accessed_at`、`access_count`、`effective_importance` 三列
  And 存量 drawer `effective_importance == CAST(importance AS REAL)`，`access_count == 0`，`last_accessed_at IS NULL`
  And 存在索引 `idx_drawers_eff_importance`

Scenario: 长时间未访问的 drawer effective_importance 低于原始 importance
  Test:
    Filter: test_decay_reduces_importance_over_time
    Level: unit
    Targets: crates/mempal-core/src/decay.rs
  Given `base_importance = 3.0`, `decay_rate = 0.05`, `floor = 0.1`, `days_since_last_hit = 30.0`, `access_boost = 0.0`
  When 调 `compute_effective_importance(...)`
  Then 返回值 < 3.0
  And 返回值 >= 0.1（floor 生效，不归零）

Scenario: floor 确保 effective_importance 不低于下限
  Test:
    Filter: test_decay_floor_prevents_zero
    Level: unit
    Targets: crates/mempal-core/src/decay.rs
  Given `base_importance = 1.0`, `floor = 0.1`, `days_since_last_hit = 10000.0`（极端天数）
  When 调 `compute_effective_importance(...)`
  Then 返回值 == 0.1

Scenario: 累计访问提升 effective_importance
  Test:
    Filter: test_access_boost_increases_effective_importance
    Level: unit
    Targets: crates/mempal-core/src/decay.rs
  Given `base_importance = 2.0`, `days = 0.0`, `access_boost = 0.3`（2 次 session boost）
  When 调 `compute_effective_importance(...)`
  Then 返回值 > 2.0

Scenario: access boost 不超过 boost_cap
  Test:
    Filter: test_access_boost_capped_at_max
    Level: unit
    Targets: crates/mempal-core/src/decay.rs
  Given `boost_per_access = 0.15`, `boost_cap = 2.0`，累计 `access_boost = 3.0`（超过 cap）
  When 调 `compute_effective_importance(base=3.0, days=0.0, access_boost=3.0, config)`
  Then 返回值 == 3.0 + 2.0（boost 被 cap 在 2.0）

Scenario: search 命中后异步更新 access 字段
  Test:
    Filter: test_search_hit_updates_access_fields_async
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given palace.db 含 1 条 drawer，`access_count = 0`，`last_accessed_at IS NULL`
  When `mempal_search` 返回结果中含该 drawer，等待异步 write flush
  Then 该 drawer `access_count == 1`
  And `last_accessed_at IS NOT NULL`

Scenario: session 内 ingest 触发命中 drawer 的 boost
  Test:
    Filter: test_session_ingest_boosts_hit_drawers
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs, crates/mempal-mcp/src/tools.rs
  Given MCP session 内 `mempal_search` 返回结果含 drawer-A，记录到 session_hit_drawers
  When 同 session 内调用 `mempal_ingest` 写入新内容
  Then drawer-A 的 `effective_importance` 增加了 `boost_per_access`
  And session_hit_drawers 清空

Scenario: KG triple 被 invalidate 后 effective_importance 下降
  Test:
    Filter: test_stale_kg_triple_penalizes_importance
    Level: integration
    Targets: crates/mempal-core/src/decay.rs（stale 惩罚调用路径）
  Given drawer-A `effective_importance = 3.0`，关联 KG triple 状态 = invalidated
  When 执行 `mempal_fact_check`（触发 StaleFact 检测）
  Then drawer-A `effective_importance == 1.5`（3.0 × 0.5 stale_penalty）

Scenario: mempal audit --stale 列出低 effective_importance 的 drawer
  Test:
    Filter: test_audit_stale_surfaces_decayed_drawers
    Level: integration
    Targets: crates/mempal-cli/src/audit.rs
  Given drawer-A `effective_importance = 0.4`，drawer-B `effective_importance = 3.0`
  When 执行 `mempal audit --stale --threshold 0.5`
  Then stdout 含 drawer-A 的 id 和 `effective_importance`
  And stdout 不含 drawer-B

Scenario: mempal_search 结果 DTO 含 effective_importance 字段
  Test:
    Filter: test_search_result_dto_includes_effective_importance
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given palace.db 含有若干 drawer（fork_ext_version >= 6）
  When 调用 `mempal_search({query: "foo"})`
  Then 每条结果 JSON 含 `"effective_importance": <number>` 字段

Scenario: config hot-reload 改变 decay_rate 立即生效
  Test:
    Filter: test_importance_config_hot_reload_decay_rate
    Level: integration
    Targets: crates/mempal-core/src/config.rs, crates/mempal-core/src/decay.rs
  Given daemon 运行中，`decay_rate = 0.05`
  When 修改 config 文件将 `decay_rate` 改为 `0.0`（零衰减）
  And config hot-reload 触发
  Then 下一次 `compute_effective_importance` 使用新 `decay_rate = 0.0`（decay 项为零）
  And 无需重启 daemon

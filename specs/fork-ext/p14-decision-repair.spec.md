spec: task
name: "P14: Decision repair via anti-pattern detection"
tags: [feature, repair, anti-patterns, fact-check, schema, mcp]
estimate: 2d
---

## Intent

mempal 的 `mempal_fact_check` 检测存储知识中的矛盾（SimilarNameConflict / RelationContradiction / StaleFact），但不检测**行为反模式**——agent 反复犯同一错误的情况。每次失败都被当作独立事件入库，失败集群对未来 session 没有预警作用。

**核心设计**：模仿 MemOS V7 "Decision Repair" 的"不阻塞当前步骤、修复下一步"哲学，扩展 `mempal_fact_check` 引入新检测类型 `RepeatedFailurePattern`：
1. **检测**：ingest 时检查内容中的 failure 关键词；在同 wing/room 集群内，若同一 topic 下 ≥ 3 次失败事件出现在 `window_days` 内，标记为 anti-pattern candidate
2. **证据装配**：收集该 topic 下的失败 drawer + 成功 drawer，产出结构化的 "do/avoid" 对比（不使用 LLM 生成描述——agent 收到证据，自行归纳）
3. **预警注入**：`mempal_context` 响应新增 `repair_warnings` 段，在 T1 层最高优先级注入，确保 agent 在 session 开始时就看到
4. **存储**：新 `failure_events` 表记录失败事件，`anti_patterns` 标签打在相关 drawer 上

**动机**：issue #118；MemOS V7 Decision Repair 机制。`p9-fact-checker` 是扩展基础（复用其检测触发逻辑）；`p14-tiered-retrieval` 的 repair trigger 是主要消费场景。

## Decisions

- fork-ext `fork_ext_version` `7 → 8` migration：
  - 新建 `failure_events` 表：
    ```sql
    CREATE TABLE IF NOT EXISTS failure_events (
        event_id      TEXT PRIMARY KEY,       -- UUID v4
        drawer_id     TEXT NOT NULL,          -- 关联失败 drawer
        wing          TEXT NOT NULL,
        room          TEXT,
        topic_sig     TEXT NOT NULL,          -- 归一化 topic 签名（FTS5 top keywords hash）
        failure_type  TEXT NOT NULL,          -- error | reverted | rolled_back | failed | custom
        detected_at   INTEGER NOT NULL,       -- unix epoch ms
        project_id    TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_failure_events_topic ON failure_events(topic_sig, detected_at);
    CREATE INDEX IF NOT EXISTS idx_failure_events_wing  ON failure_events(wing, room, detected_at);
    ```
  - `drawers` 表新增可选 tag `anti_pattern=true` 通过现有 metadata / flags 机制标记（不加新列）

- **Failure 关键词检测**（ingest pipeline 新增步骤，在 dedup 之后）：
  - 内置关键词列表（可在 config 扩展）：`["error", "failed", "failure", "reverted", "rolled back", "exception", "panic", "aborted", "wrong", "mistake", "incorrect"]`
  - 检测策略：case-insensitive 全词匹配（`\bword\b` 正则），命中任意关键词即视为 failure 事件
  - 写入 `failure_events` 表（异步，不阻塞 ingest 关键路径）

- **Topic 签名**（`topic_sig`）：
  - 取 drawer content 经 FTS5 已有词频计算的 top-5 TF-IDF 词（与 p13-pattern-induction 的 `topic_tags` 同一机制，可共用实现）
  - `topic_sig = sha256(sorted_top5_words)[:32]`（固定长度字符串，便于精确匹配；32 字符提供足够的碰撞抵抗性，16 字符在大规模 failure 事件集中碰撞概率不可接受）

- **RepeatedFailurePattern 检测** — 触发时机：
  1. 每次 `mempal_ingest` 写入 failure 事件后
  2. 每次 `mempal_fact_check` 调用时（全量扫描）
  - 检测逻辑：
    ```sql
    SELECT topic_sig, COUNT(*) as cnt, MIN(detected_at) as first, MAX(detected_at) as last
    FROM failure_events
    WHERE detected_at >= :window_start  -- now - window_days * 86400000
      AND (project_id = :project_id OR project_id IS NULL)
    GROUP BY topic_sig
    HAVING cnt >= :min_failures          -- 默认 3
    ```
  - 命中的 `topic_sig` 组成 anti-pattern 候选集

- **证据装配**（`RepairPackage`）：
  - 失败方：`WHERE drawer_id IN (SELECT drawer_id FROM failure_events WHERE topic_sig = :sig AND detected_at >= :window_start) LIMIT 5`
  - 成功方：语义相近（vector cosine >= 0.75）但**不含 failure 关键词**的 drawer，同 wing/room，`LIMIT 5`
  - `RepairPackage` 结构：
    ```json
    {
      "topic_sig": "...",
      "failure_count": 4,
      "window_days": 7,
      "failure_drawers": [ { "drawer_id": "...", "preview": "..." } ],
      "success_drawers": [ { "drawer_id": "...", "preview": "..." } ]
    }
    ```

- **`mempal_fact_check` 扩展**：
  - 新增 `check_type: "RepeatedFailurePattern"` 的检测结果
  - 结果格式：`{ check_type: "RepeatedFailurePattern", repair_packages: [RepairPackage], confidence: "heuristic" }`
  - 现有三种 check type 不受影响（SimilarNameConflict / RelationContradiction / StaleFact）

- **`mempal_context` `repair_warnings` 注入**：
  - 若存在未解决的 anti-pattern（最近 `window_days` 内 failure_count >= `alert_threshold`），在 `mempal_context` 响应中注入：
    ```json
    "repair_warnings": [
      {
        "severity": "warn",
        "message": "Repeated failure pattern detected in wing={W} room={R}: {topic_preview}",
        "repair_package_id": "..."
      }
    ]
    ```
  - `repair trigger` 时（p14-tiered-retrieval）repair_warnings 也注入 T1 层顶部

- **Config** `[repair]` 子段（可热重载）：
  - `enabled: bool = true`
  - `failure_keywords: Vec<String> = [...]`（追加扩展，不覆盖内置）
  - `window_days: u64 = 7`
  - `min_failures: usize = 3`（触发 RepeatedFailurePattern 的最小次数）
  - `alert_threshold: usize = 3`（注入 repair_warnings 的阈值，默认等于 min_failures）

- **CLI 命令**：
  - `mempal repair list [--wing <W>] [--since <days>]` — 列出检测到的反模式，输出 topic_sig / failure_count / wing / preview
  - `mempal repair show <topic_sig>` — 详情：失败 drawer 列表 + 成功对比 drawer

## Boundaries

### Allowed
- `crates/mempal-core/src/db/schema.rs` — fork_ext_version 7 → 8，创建 `failure_events` 表
- `crates/mempal-core/src/config.rs` — 新增 `[repair]` config 子段 `RepairConfig`
- `crates/mempal-core/src/repair.rs` — 新建：failure keyword 检测、topic_sig 计算、RepairPackage 装配
- `crates/mempal-core/src/lib.rs` — `pub mod repair`
- `crates/mempal-ingest/src/pipeline.rs` — ingest 后 failure 事件异步写入
- `crates/mempal-search/src/lib.rs`（或等价）— success drawer 语义检索（复用现有 vector search）
- `crates/mempal-mcp/src/tools.rs` — `mempal_fact_check` 新增 RepeatedFailurePattern；`mempal_context` 新增 repair_warnings
- `crates/mempal-cli/src/repair.rs` — 新建：`mempal repair` 子命令实现
- `crates/mempal-cli/src/main.rs` — `repair` 子命令注册
- `tests/decision_repair.rs` — 新建集成测试

### Forbidden
- 不要用 LLM 生成 repair 建议文本（违反 LLM-free 约束；agent 基于 RepairPackage 中的 exemplar drawers 自行归纳）
- 不要在 ingest 关键路径上同步检测（failure event 写入 + anti-pattern 扫描必须异步）
- 不要在 `failure_events` 表中存储 drawer 全文内容（只存 drawer_id 引用，内容从 `drawers` 表查）
- 不要让 failure keyword 匹配大小写敏感（必须 case-insensitive）
- 不要让 `mempal_fact_check` 在 `[repair] enabled = false` 时运行 RepeatedFailurePattern 检测
- 不要删改 `mempal_fact_check` 现有三种 check type（向下兼容）

## Out of Scope

- LLM 驱动的根因分析（无 LLM 依赖）
- 自动 apply repair（repair_warnings 是建议，agent 自主决策；mempal 不自动修改任何 drawer）
- Failure event 的细粒度 severity 分级（目前只有 binary：failure / success）
- Anti-pattern 跨 wing 聚合（每个 topic_sig 在 wing 内独立检测）
- Web UI（违反 CLI-first 约束）

## Completion Criteria

Scenario: fork-ext migration 7 → 8 创建 failure_events 表
  Test:
    Filter: test_fork_ext_migration_v7_to_v8_creates_failure_events
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db `fork_ext_version == "7"`
  When 启动 mempal
  Then `fork_ext_version == "8"`
  And sqlite_master 中存在 table `failure_events`，含 `event_id`、`drawer_id`、`topic_sig`、`failure_type`、`detected_at` 列
  And 存在索引 `idx_failure_events_topic`

Scenario: ingest 含 failure 关键词的 drawer 触发 failure_events 写入
  Test:
    Filter: test_ingest_failure_keyword_creates_event
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs, crates/mempal-core/src/repair.rs
  Given palace.db 空
  When ingest 内容含 "the migration failed with SQLITE_ERROR" 的 drawer（wing=code-memory）
  And 等待异步 failure event 写入
  Then `failure_events` 表含 1 条记录，`failure_type = "failed"`，`wing = "code-memory"`
  And `topic_sig` 非空

Scenario: ingest 不含 failure 关键词的 drawer 不写入 failure_events
  Test:
    Filter: test_ingest_no_failure_keyword_skips_event
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given palace.db 空
  When ingest 普通内容 drawer（无 failure 关键词）
  Then `failure_events` 表为空

Scenario: 同 topic_sig 3 次失败后 fact_check 检测到 RepeatedFailurePattern
  Test:
    Filter: test_fact_check_detects_repeated_failure_pattern
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs, crates/mempal-core/src/repair.rs
  Given `min_failures = 3`，`window_days = 7`
  And failure_events 表含同 `topic_sig` 的 3 条记录，均在 7 天内
  When 调用 `mempal_fact_check`
  Then 响应含 `check_type = "RepeatedFailurePattern"` 的检测结果
  And `repair_packages[0].failure_count == 3`
  And `repair_packages[0].failure_drawers` 数组长度 == 3

Scenario: RepairPackage 包含失败和成功 drawer 的对比证据
  Test:
    Filter: test_repair_package_assembles_evidence
    Level: integration
    Targets: crates/mempal-core/src/repair.rs
  Given 失败 drawer 3 条（同 topic_sig），同 wing 还有 2 条相似但无 failure 关键词的成功 drawer
  When 装配 RepairPackage
  Then `failure_drawers` 列表长度 == 3
  And `success_drawers` 列表长度 >= 1（至少找到 1 条成功对比 drawer）

Scenario: repair_warnings 注入 mempal_context
  Test:
    Filter: test_repair_warnings_injected_in_context
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given 存在触发 RepeatedFailurePattern 的 failure_events（>=3 次同 topic）
  When 调用 `mempal_context`
  Then 响应含 `repair_warnings` 数组，至少 1 条含 `severity = "warn"`
  And 警告 message 提及检测到的 wing

Scenario: window_days 外的 failure_events 不触发检测
  Test:
    Filter: test_repair_window_excludes_old_events
    Level: integration
    Targets: crates/mempal-core/src/repair.rs
  Given `window_days = 7`，failure_events 表含同 topic_sig 的 3 条记录，均在 10 天前
  When 调用 `mempal_fact_check`
  Then 响应**不含** `RepeatedFailurePattern` 检测结果（时间窗外）

Scenario: repair disabled 时 fact_check 不运行 RepeatedFailurePattern
  Test:
    Filter: test_repair_disabled_skips_detection
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given config `[repair] enabled = false`
  When 调用 `mempal_fact_check`
  Then 响应不含 `RepeatedFailurePattern` 类型（其他 check type 正常运行）

Scenario: mempal repair list 列出反模式
  Test:
    Filter: test_repair_list_cli_shows_patterns
    Level: integration
    Targets: crates/mempal-cli/src/repair.rs
  Given 1 个 RepeatedFailurePattern（wing=code-memory，4 次失败）
  When 执行 `mempal repair list --wing code-memory`
  Then stdout 含该 topic_sig、`failure_count = 4`、wing 信息

Scenario: ingest 失败检测不阻塞 ingest 成功响应
  Test:
    Filter: test_ingest_failure_detection_is_nonblocking
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given failure event 写入路径模拟慢速（delay 200ms）
  When ingest 含 failure 关键词的 drawer
  Then ingest 响应在 200ms 内返回（不等待 failure_events 写入完成）
  And failure_events 最终写入成功（eventual consistency）

spec: task
name: "P15: Skill crystallization from validated recurring patterns"
tags: [feature, skills, patterns, knowledge, schema, mcp]
estimate: 2d
---

## Intent

mempal 存储原始记忆并让 agent 检索，但没有将频繁验证的知识**结晶**为更高层次、可直接行动的格式的机制。Agent 每个 session 都需从原始 drawer 重新推导同样的经验，造成认知重复劳动。

**核心设计**：模仿 MemOS V7 "Skill Crystallization"，将 active pattern 提升为结构化可调用单元——`Skill`。关键设计选择：
- **LLM-free**：`trigger_description`（何时使用）和 skill 结构体由 **agent 在提升时提供**，mempal 只存储和检索；mempal 不生成文本
- **依赖 p13-pattern-induction**：pattern 基础设施是前驱；技能从 active pattern 提升，不从裸 drawer 直接提升
- **显式生命周期**：probationary → active → retired，需人工或 agent 显式触发，不自动提升
- **反馈回路**：agent 通过 `mempal_skill adopt/reject` 信号更新 `eta`（采用率），驱动生命周期决策

**动机**：issue #119；MemOS V7 skill crystallization。`p13-pattern-induction` 是硬依赖（需 patterns 表和 active pattern 基础设施）。

## Decisions

- fork-ext `fork_ext_version` `8 → 9` migration：
  - 新建 `skills` 表：
    ```sql
    CREATE TABLE IF NOT EXISTS skills (
        skill_id             TEXT PRIMARY KEY,   -- UUID v4
        name                 TEXT NOT NULL,       -- human-readable name (provided by agent at promote)
        trigger_description  TEXT NOT NULL,       -- when to invoke (provided by agent, NOT generated)
        pattern_id           TEXT NOT NULL,       -- FK to patterns.pattern_id
        exemplar_ids         TEXT NOT NULL,       -- JSON array (inherited from pattern at promote time)
        adoption_count       INTEGER NOT NULL DEFAULT 0,
        rejection_count      INTEGER NOT NULL DEFAULT 0,
        status               TEXT NOT NULL DEFAULT 'probationary', -- probationary | active | retired
        promoted_at          INTEGER NOT NULL,    -- unix epoch ms
        updated_at           INTEGER NOT NULL,
        project_id           TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_skills_status ON skills(status);
    CREATE INDEX IF NOT EXISTS idx_skills_pattern ON skills(pattern_id);
    ```

- **Promotion gate** — 满足所有条件时 pattern 才可被提升为 skill：
  1. `patterns.status == "active"`
  2. `patterns.session_count >= min_supporting_sessions`（默认 5，与 patterns promote_threshold 一致）
  3. `patterns.session_count >= skill_min_sessions`（config，默认 5，可独立于 pattern threshold 调参）
  4. 不存在已有 `status IN ('probationary', 'active')` 的 skill 关联同一 `pattern_id`（防止重复提升）

- **Promote 操作**（仅通过显式调用触发，不自动）：
  - MCP 工具 `mempal_skill promote` — 需提供 `pattern_id`、`name`、`trigger_description`（agent 必须提供描述，mempal 不生成）
  - CLI `mempal skills promote <pattern_id> --name "..." --trigger "..."`
  - 提升时：创建 `skills` 行，`status = "probationary"`

- **Probationary → Active** 升级条件：`adoption_count >= active_threshold`（默认 3）

- **Feedback 信号** — agent 显式调用：
  - `mempal_skill adopt <skill_id>` / CLI `mempal skills adopt <skill_id>`：`adoption_count += 1`，检查是否升为 active
  - `mempal_skill reject <skill_id>` / CLI `mempal skills reject <skill_id>`：`rejection_count += 1`；若 `rejection_count >= retire_threshold`（默认 3）且 `adoption_count == 0`，自动 retire
  - **Eta 计算**（展示用）：`eta = adoption_count / (adoption_count + rejection_count + 1.0)`（Laplace smoothed，不影响状态机）

- **`mempal_context` T1 集成**：
  - Active skills 注入 T1 层（p14-tiered-retrieval），优先级高于普通 decision drawer
  - 每条 skill 在 T1 中以简洁格式展示：`{ skill_id, name, trigger_description, eta, exemplar_count }`
  - 匹配当前 query 向量（query embedding 与 pattern signature 余弦相似度 >= `skill_surfacing_threshold`，默认 0.70）的 skill 优先注入

- **MCP 工具 `mempal_skill`**（新建，独立工具）：
  - `list` — 列出 skill（可按 status / project_id 过滤），返回 `[{skill_id, name, trigger_description, eta, status, adoption_count, rejection_count}]`
  - `show <skill_id>` — 完整详情 + exemplar drawer previews
  - `promote` — 从 pattern 提升（见 Promote 操作）
  - `adopt <skill_id>` — 正向反馈
  - `reject <skill_id>` — 负向反馈
  - `retire <skill_id>` — 手动 retire

- **CLI 命令**：`mempal skills list/show/promote/adopt/reject/retire`（镜像 MCP 工具）

- **Skill 与 pattern 的关系**：
  - pattern retire 不自动 retire 关联 skill（skill 独立生命周期）
  - skill 被 retire 后，pattern 可再次被提升为新 skill（允许 reset cycle）
  - skill 的 `exemplar_ids` 在 promote 时快照（不随 pattern 后续更新变化）

## Boundaries

### Allowed
- `crates/mempal-core/src/db/schema.rs` — fork_ext_version 8 → 9，创建 `skills` 表
- `crates/mempal-core/src/config.rs` — 新增 `[skills]` config 子段 `SkillsConfig`
- `crates/mempal-core/src/skills.rs` — 新建：skill CRUD、promotion gate、feedback、eta 计算
- `crates/mempal-core/src/lib.rs` — `pub mod skills`
- `crates/mempal-mcp/src/tools.rs` — 新建 `mempal_skill` MCP 工具（6 actions）；`mempal_context` T1 集成
- `crates/mempal-mcp/src/server.rs` — 注册 `mempal_skill` 工具
- `crates/mempal-search/src/tiered.rs` — T1 中 skill query-matching 逻辑（复用 pattern.signature 相似度）
- `crates/mempal-cli/src/skills.rs` — 新建：`mempal skills` 子命令实现
- `crates/mempal-cli/src/main.rs` — `skills` 子命令注册
- `tests/skill_crystallization.rs` — 新建集成测试

### Forbidden
- 不要让 mempal 自动生成 `name` 或 `trigger_description`（必须由 agent 在 promote 时提供；违反 LLM-free 约束）
- 不要自动触发 pattern → skill 提升（必须显式调用 promote；防止未审查的 skill 污染 T1）
- 不要让 pattern retire 连带 retire skill（skill 生命周期独立）
- 不要把 skill 内容写入 `drawers` 表（skill 存在 `skills` 表，不是普通记忆）
- 不要在 `mempal_context` 注入 probationary skill（只有 `status = "active"` 的 skill 进入 T1 注入）
- 不要对 adoption/rejection signal 添加速率限制（agent 应可自由发信号，限制留未来）
- 不要让 skill 的 exemplar_ids 随 pattern 后续更新而变化（promote 时快照，之后独立）

## Out of Scope

- 自动文本生成 `trigger_description`（需要 LLM）
- Skill 版本化（每次 promote 创建新 skill，旧的 retire；不维护 revision history）
- Skill 组合（multi-skill 工作流编排，留未来）
- Skill 跨项目共享（skills 与 project_id 绑定，不跨 project 传播）
- Skill 导入/导出（留未来）
- Web UI（违反 CLI-first 约束）
- 自动 skill 重新提升（pattern 可再次 promote 为新 skill，但需显式操作）

## Completion Criteria

Scenario: fork-ext migration 8 → 9 创建 skills 表
  Test:
    Filter: test_fork_ext_migration_v8_to_v9_creates_skills_table
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db `fork_ext_version == "8"`
  When 启动 mempal
  Then `fork_ext_version == "9"`
  And sqlite_master 中存在 table `skills`，含 `skill_id`、`name`、`trigger_description`、`pattern_id`、`adoption_count`、`rejection_count`、`status`、`eta` 相关列
  And 存在索引 `idx_skills_status`

Scenario: active pattern 满足条件时可被提升为 probationary skill
  Test:
    Filter: test_pattern_promote_creates_probationary_skill
    Level: integration
    Targets: crates/mempal-core/src/skills.rs, crates/mempal-mcp/src/tools.rs
  Given 1 个 `status = "active"` pattern，`session_count = 6`（>= skill_min_sessions=5）
  When 调用 `mempal_skill promote({ pattern_id: "...", name: "Deploy guard", trigger_description: "When about to deploy, verify tests pass first" })`
  Then `skills` 表新增 1 条 `status = "probationary"` 记录
  And `name = "Deploy guard"`，`trigger_description` 按原文存储（不被修改）
  And `adoption_count == 0`，`rejection_count == 0`

Scenario: candidate pattern 不可被提升为 skill
  Test:
    Filter: test_candidate_pattern_cannot_be_promoted
    Level: integration
    Targets: crates/mempal-core/src/skills.rs
  Given `status = "candidate"` pattern
  When 调用 `mempal_skill promote({ pattern_id: "...", name: "...", trigger_description: "..." })`
  Then 返回错误（`PromotionError::PatternNotActive`）
  And `skills` 表无新记录

Scenario: 同一 pattern 不可重复提升（已有 active/probationary skill）
  Test:
    Filter: test_duplicate_promotion_rejected
    Level: integration
    Targets: crates/mempal-core/src/skills.rs
  Given 已有 `status = "probationary"` skill 关联 pattern-A
  When 再次对 pattern-A 调用 promote
  Then 返回错误（`PromotionError::SkillAlreadyExists`）
  And 不创建第二条 skill 记录

Scenario: adopt 信号累计到 active_threshold 后 skill 升为 active
  Test:
    Filter: test_skill_adopts_to_active_at_threshold
    Level: integration
    Targets: crates/mempal-core/src/skills.rs
  Given `active_threshold = 3`，probationary skill，`adoption_count = 2`
  When 调用 `mempal_skill adopt <skill_id>`（第 3 次）
  Then skill `status == "active"`，`adoption_count == 3`

Scenario: 足量 reject 信号且无 adoption 触发自动 retire
  Test:
    Filter: test_skill_auto_retires_on_rejection
    Level: integration
    Targets: crates/mempal-core/src/skills.rs
  Given `retire_threshold = 3`，probationary skill，`adoption_count = 0`，`rejection_count = 2`
  When 调用 `mempal_skill reject <skill_id>`（第 3 次）
  Then skill `status == "retired"`

Scenario: eta 反映 adoption/rejection 比例
  Test:
    Filter: test_skill_eta_calculation
    Level: unit
    Targets: crates/mempal-core/src/skills.rs
  Given `adoption_count = 3`，`rejection_count = 1`
  When 调用 `compute_eta(adoption=3, rejection=1)`
  Then 返回值 == 3.0 / (3 + 1 + 1.0) = 0.6（Laplace smoothed）

Scenario: active skill 注入 mempal_context T1 层
  Test:
    Filter: test_active_skill_injected_in_t1
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs, crates/mempal-search/src/tiered.rs
  Given 1 个 `status = "active"` skill，其关联 pattern signature 与 query embedding 余弦相似度 >= 0.70
  When 调用 `mempal_context({trigger: "session_start"})`
  Then `t1_dao_tian` 数组首部含该 skill 的 `{ skill_id, name, trigger_description, eta }`
  And skill 条目的 rank 高于普通 decision drawer（skill 优先级更高）

Scenario: probationary skill 不注入 mempal_context
  Test:
    Filter: test_probationary_skill_excluded_from_context
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given 1 个 `status = "probationary"` skill
  When 调用 `mempal_context`
  Then `t1_dao_tian` 不含该 skill 的 skill_id

Scenario: pattern retire 不连带 retire 关联 skill
  Test:
    Filter: test_pattern_retire_does_not_cascade_to_skill
    Level: integration
    Targets: crates/mempal-core/src/skills.rs, crates/mempal-core/src/patterns.rs
  Given active pattern-A 关联 active skill-B
  When 执行 `mempal patterns retire pattern-A`
  Then pattern-A `status == "retired"`
  And skill-B `status` **仍为** `"active"`（不受影响）

Scenario: mempal_skill list 仅显示当前 project 的 skill
  Test:
    Filter: test_skill_list_filters_by_project
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs
  Given skill-A.project_id = "proj-X"，skill-B.project_id = "proj-Y"
  When 调用 `mempal_skill list({project_id: "proj-X"})`
  Then 结果含 skill-A，不含 skill-B

Scenario: mempal skills promote CLI 提升 pattern 为 skill
  Test:
    Filter: test_skills_promote_cli
    Level: integration
    Targets: crates/mempal-cli/src/skills.rs
  Given 1 个 active pattern（session_count >= 5）
  When 执行 `mempal skills promote <pattern_id> --name "Test guard" --trigger "Run tests before deploy"`
  Then stdout 含成功提示和新 skill_id
  And `skills` 表含对应记录，`name = "Test guard"`，`trigger_description = "Run tests before deploy"`

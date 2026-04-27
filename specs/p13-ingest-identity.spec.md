spec: task
name: "P13B: bootstrap ingest drawer identity parity"
inherits: project
tags: [memory, ingest, identity, bootstrap, parity]
estimate: 0.5d
---

## Intent

P12 引入 typed/bootstrap drawer metadata 后，MCP `mempal_ingest` 已经使用
`build_bootstrap_drawer_id(...)`，但 REST ingest 和文件/CLI ingest 仍使用旧的
`build_drawer_id(...)`。这会导致同一逻辑 bootstrap drawer 通过不同入口写入时得到
不同 `drawer_id`，进而影响 dedup、citation 和 knowledge refs。

本任务只统一 **bootstrap/evidence identity** 的生成规则：以当前 MCP bootstrap identity
算法为 canon，让 MCP、REST、文件/CLI ingest 在各自已暴露的 metadata 范围内使用同一套
identity components。不迁移旧 drawer，不做 REST typed-field parity。

## Decisions

- **canonical identity 保持当前 MCP 语义**：继续使用 `build_bootstrap_drawer_id(wing, room, content, identity_components)`
- **旧 `build_drawer_id(...)` 保留**，但不再用于 bootstrap drawer ingest 的新写入路径
- **identity components 必须从实际要写入的 drawer metadata 派生**，而不是每个入口手写一套 hash 输入
- bootstrap identity 至少覆盖：
  - `memory_kind`
  - `domain`
  - `field`
  - `anchor_kind`
  - `anchor_id`
  - `parent_anchor_id`
  - `provenance`
  - `statement`
  - `tier`
  - `status`
  - `scope_constraints`
  - `supporting_refs`
  - `counterexample_refs`
  - `teaching_refs`
  - `verification_refs`
  - `trigger_hints.intent_tags`
  - `trigger_hints.workflow_bias`
  - `trigger_hints.tool_needs`
- role refs 和 `trigger_hints` 中的数组在 identity 中保持顺序无关，继续先 normalize/sort 再 hash
- `source_file`、`chunk_index`、`added_at`、`importance` 不进入 bootstrap identity
- `MCP mempal_ingest` 的现有 drawer id 不允许变化；本任务是抽出/复用 canon，不是改 canon
- `REST POST /api/ingest` 在本任务中仍是 evidence-only：
  - 不新增 `memory_kind` / `statement` / `tier` / `status` 等 typed request 字段
  - 其默认 evidence identity 必须与省略 P12 字段的 MCP ingest 一致
- 文件/CLI ingest 在本任务中仍是 evidence-only：
  - 不新增 CLI typed drawer flags
  - 但新写入的 evidence drawer id 必须由 bootstrap identity components 生成
  - `source_type_for(format)` 仍决定 provenance，因此 plain project text 与 conversation transcript 可以因为 provenance 不同得到不同 id
- 同一 `content/wing/room` 但不同 anchor 或不同 typed metadata 的 drawer 仍必须保持不同 `drawer_id`
- 既有数据库中的旧 drawer id 不迁移、不重写、不 backfill 新 id
- search / wake-up / MCP search / REST search 的 `content` raw 语义保持不变

## Boundaries

### Allowed Changes
- `src/core/utils.rs`
- `src/core/types.rs`
- `src/core/anchor.rs`
- `src/ingest/**`
- `src/api/handlers.rs`
- `src/mcp/server.rs`
- `src/main.rs`
- `tests/**`
- `AGENTS.md`
- `CLAUDE.md`

### Forbidden
- 不要改 `drawers` schema
- 不要迁移或重写已有 drawer id
- 不要删除 `build_drawer_id(...)`
- 不要为 REST ingest 增加 typed drawer request fields
- 不要为 CLI/file ingest 增加 typed drawer flags
- 不要改 semantic dedup 阈值或算法
- 不要改 search ranking
- 不要改变 `SearchResult.content` / REST search / MCP search 的 raw 内容语义
- 不要引入新依赖

## Out of Scope

- P13C REST typed drawer parity
- REST search typed filter/metadata parity
- `mempal_context` / reasoning pack
- `publish_anchor` / promote / demote lifecycle
- Phase-2 `knowledge_cards`
- 旧 drawer id 迁移工具

## Completion Criteria

Scenario: MCP default ingest keeps canonical bootstrap drawer id
  Test:
    Filter: test_mcp_ingest_default_drawer_id_matches_bootstrap_identity
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，只提供 `content/wing/room`
  Then 返回的 `drawer_id` 等于 `build_bootstrap_drawer_id(...)` 使用默认 evidence identity components 的结果
  And 不等于旧的 content-only `build_drawer_id(...)` 结果
  And 写入的 drawer metadata 仍为 `memory_kind="evidence"`, `domain="project"`, `field="general"`, `provenance="human"`

Scenario: REST default evidence ingest matches MCP default identity
  Test:
    Filter: test_rest_ingest_default_evidence_drawer_id_matches_mcp
    Level: integration
  Given 一个空数据库
  And `content == "Shared evidence"`
  And `wing == "mempal"`
  And `room == "identity"`
  When 先通过 MCP `mempal_ingest` dry-run 预览该 `content/wing/room`
  And 再通过 REST `POST /api/ingest` 写入相同 `content/wing/room`
  Then REST 返回的 `drawer_id` 与 MCP dry-run 返回值完全相同
  And 数据库中只存在一个该 `drawer_id` 对应的 drawer

Scenario: REST duplicate after MCP write skips by shared bootstrap id
  Test:
    Filter: test_rest_after_mcp_default_ingest_reuses_existing_bootstrap_drawer
    Level: integration
  Given MCP `mempal_ingest` 已经写入一个默认 evidence drawer
  When REST `POST /api/ingest` 使用相同 `content/wing/room`
  Then REST 返回同一个 `drawer_id`
  And drawer count 不增加
  And vector count 不增加

Scenario: file ingest uses bootstrap identity for evidence drawers
  Test:
    Filter: test_file_ingest_uses_bootstrap_identity_for_evidence_drawer
    Level: integration
  Given 一个临时目录中存在一个 plain text 文件
  When 调用文件/CLI ingest 路径写入该文件
  Then 新 drawer 的 `drawer_id` 等于 `build_bootstrap_drawer_id(...)` 使用实际写入 metadata 的结果
  And 其 `provenance` 由 `source_type_for(format)` 决定
  And `source_file` 与 `chunk_index` 不参与 identity

Scenario: same content with different explicit anchors remains distinct
  Test:
    Filter: test_bootstrap_identity_separates_same_content_with_different_anchors
    Level: integration
  Given 一个空数据库
  When MCP `mempal_ingest` 写入两条相同 `content/wing/room` 的 evidence drawer
  And 两次请求使用不同的显式 `anchor_kind/anchor_id`
  Then 两次返回的 `drawer_id` 不同
  And 两条 drawer 都可共存

Scenario: role refs and trigger hints remain order-insensitive
  Test:
    Filter: test_bootstrap_identity_ignores_ref_and_hint_order
    Level: integration
  Given 两个 knowledge ingest 请求只有 `supporting_refs` 或 `trigger_hints` 数组顺序不同
  When 两次请求都 dry-run
  Then 返回的 `drawer_id` 完全相同

Scenario: all knowledge governance components participate in identity
  Test:
    Filter: test_knowledge_bootstrap_identity_changes_when_governance_component_changes
    Level: integration
  Given 一个 baseline knowledge ingest 请求包含 `parent_anchor_id`, `tier`, `status`, `scope_constraints`, all role refs, and all trigger hint arrays
  When 对每个 governance component 分别改动一个值并 dry-run
  Then 每个改动后的请求都得到不同于 baseline 的 `drawer_id`
  And 这覆盖 `counterexample_refs`, `teaching_refs`, `verification_refs`, `trigger_hints.intent_tags`, `trigger_hints.workflow_bias`, and `trigger_hints.tool_needs`

Scenario: invalid explicit anchor is rejected before identity generation
  Test:
    Filter: test_mcp_ingest_rejects_malformed_explicit_anchor
    Level: integration
  Given 一个 MCP `mempal_ingest` 请求包含 `anchor_kind="repo"`
  And 该请求包含 malformed `anchor_id="worktree://wrong-prefix"`
  When 通过 `src/mcp/server.rs` handler 执行该请求
  Then 返回 invalid params error
  And 不返回 `drawer_id`
  And 数据库不写入 drawer

Scenario: existing legacy drawer ids are not rewritten
  Test:
    Filter: test_p13b_does_not_rewrite_existing_drawer_ids
    Level: integration
  Given 一个 schema v4 数据库中已有 `drawer_legacy_001`
  When 打开数据库并执行 migration
  Then 原 drawer id 仍然是 `drawer_legacy_001`
  And 不会额外创建一个 bootstrap-id 版本的 duplicate drawer

Scenario: REST typed fields remain out of scope
  Test:
    Filter: test_rest_ingest_does_not_claim_typed_field_parity
    Level: integration
  Given REST `POST /api/ingest` 当前 contract 只支持 `content/wing/room/source`
  And `memory_kind == "knowledge"`
  When 请求体包含 P12 typed fields such as that `memory_kind` 或 `statement`
  Then 本任务不要求 REST 写入 knowledge drawer
  And REST 不得因为这些字段改变 bootstrap evidence identity
  And typed REST parity 留给 P13C

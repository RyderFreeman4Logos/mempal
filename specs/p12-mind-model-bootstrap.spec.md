spec: task
name: "P12: stage-1 mind-model bootstrap — typed drawers + anchor metadata"
inherits: project
tags: [memory, schema, bootstrap, anchor, search, mcp]
estimate: 1.5d
---

## Intent

把前面讨论的 mind-model 落成一个**可存、可查、可区分**的 bootstrap 版本：在不引入 Phase-2 `knowledge_cards` / `knowledge_events` 的前提下，让现有 `drawers` 能显式区分 `evidence` 和 `knowledge`，并记录 `dao/shu/qi` 所需的最小治理元数据，以及 `global/repo/worktree` 锚点信息。

核心用户价值有两点：

1. agent 不再把“看到的证据”和“提炼后的知识”混成同一种 drawer
2. 分支 / worktree 的局部记忆不再天然污染 repo 级共享记忆

这是一版 bootstrap schema，不是最终架构；最终仍会演进到独立的 `knowledge_cards` 层。

## Decisions

- **Phase 1 继续复用 `drawers` 表**；本 spec 不引入 `knowledge_cards`、`knowledge_events`、`knowledge_evidence_links` 新表
- **`drawers.content` 的 raw 语义保持**：search/MCP 返回的 `content` 仍然是 drawer 自身 raw text
  - 对 `evidence drawer`：`content` 是原始证据正文
  - 对 `knowledge drawer`：`content` 是 rationale / explanatory body
- **`knowledge drawer.statement` 是独立列**，用于短句唤醒；它不替代 `content`
- **新增共享列**（bootstrap 最小集）：
  - `memory_kind TEXT NOT NULL CHECK ('evidence','knowledge') DEFAULT 'evidence'`
  - `domain TEXT NOT NULL CHECK ('project','agent','skill','global') DEFAULT 'project'`
  - `field TEXT NOT NULL DEFAULT 'general'`
  - `anchor_kind TEXT NOT NULL CHECK ('global','repo','worktree') DEFAULT 'repo'`
  - `anchor_id TEXT NOT NULL DEFAULT 'repo://legacy'`
  - `parent_anchor_id TEXT`
- **新增 evidence-only 列**：
  - `provenance TEXT CHECK ('runtime','research','human')`
- **新增 knowledge-only 列**：
  - `statement TEXT`
  - `tier TEXT CHECK ('qi','shu','dao_ren','dao_tian')`
  - `status TEXT CHECK ('candidate','promoted','canonical','demoted','retired')`
  - `supporting_refs TEXT`
  - `counterexample_refs TEXT`
  - `teaching_refs TEXT`
  - `verification_refs TEXT`
  - `scope_constraints TEXT`
  - `trigger_hints TEXT`
- **evidence refs 从第一版就分角色**，不用单一 `evidence_refs`
  - `supporting_refs`
  - `counterexample_refs`
  - `teaching_refs`
  - `verification_refs`
- **`trigger_hints` 只允许最小 JSON 结构**：
  - `intent_tags: string[]`
  - `workflow_bias: string[]`
  - `tool_needs: string[]`
  - 不允许 hard-coded `skill_id`
- **按 `memory_kind` 做写入验证**
  - `evidence` 必须有 `provenance`
  - `evidence` 不允许写 `statement` / `tier` / `status` / refs / `trigger_hints`
  - `knowledge` 必须有 `statement` / `tier` / `status` / `supporting_refs`
  - `knowledge` 的 `supporting_refs` 至少 1 个 drawer id
- **`statement` / `content` 分工固定**
  - `statement`: 单句命题，供 runtime wake-up
  - `content`: rationale / 边界 / 解释正文
  - `statement` 不应承载长解释；`content` 不应只是重复 `statement`
- **自然状态分布约束写进文档和验证层**
  - `dao_tian` 仅允许 `canonical | demoted`
  - `dao_ren` 仅允许 `candidate | promoted | demoted | retired`
  - `shu` 仅允许 `promoted | demoted | retired`
  - `qi` 仅允许 `candidate | promoted | demoted | retired`
- **source_file 约定**
  - `evidence drawer` 延续现有真实来源语义（文件路径 / URL / session ref）
  - `knowledge drawer` 使用 synthetic URI，例如 `knowledge://global/epistemics/dao_tian/evidence-precedes-assertion`
- **锚点生成规则**
  - 显式传入 `anchor_kind` / `anchor_id` 时，按请求写入并校验
  - 未显式传入时，如存在本地 `cwd` 或 source root 且位于 git 仓库：
    - `repo` 身份基于 `git rev-parse --git-common-dir`
    - `worktree` 身份基于 `git rev-parse --show-toplevel`
    - 默认写 `anchor_kind='worktree'`
    - `parent_anchor_id` 写所属 `repo_anchor`
  - 未显式传入且不在 git 仓库：
    - 默认写 `anchor_kind='worktree'`
    - `anchor_id` 为规范化本地路径
    - `parent_anchor_id = NULL`
  - `anchor_kind='global'` 只允许 `domain='global'`
- **兼容性回填（migration）**
  - 既有 drawer 回填为 `memory_kind='evidence'`
  - 既有 drawer 回填 `domain='project'`, `field='general'`
  - `source_type='project'` → `provenance='research'`
  - `source_type='conversation' | 'manual'` → `provenance='human'`
  - 既有 drawer 回填 `anchor_kind='repo'`, `anchor_id='repo://legacy'`, `parent_anchor_id=NULL`
- **`mempal_ingest` MCP 请求扩展**
  - 在现有 `content/wing/room/source/dry_run/importance` 基础上，追加可选：
    - `memory_kind`
    - `domain`
    - `field`
    - `provenance`
    - `statement`
    - `tier`
    - `status`
    - `supporting_refs`
    - `counterexample_refs`
    - `teaching_refs`
    - `verification_refs`
    - `scope_constraints`
    - `trigger_hints`
    - `anchor_kind`
    - `anchor_id`
    - `parent_anchor_id`
    - `cwd`
- **向后兼容默认值**
  - 省略所有新字段时，`mempal_ingest` 仍写入 `evidence drawer`
  - 默认 `domain='project'`, `field='general'`, `provenance='human'`
- **search / MCP search 最小扩展**
  - `SearchRequest` / CLI `search` 增加可选过滤：
    - `memory_kind`
    - `domain`
    - `field`
    - `tier`
    - `status`
    - `anchor_kind`
  - `SearchResult` / `SearchResultDto` 追加返回：
    - `memory_kind`
    - `domain`
    - `field`
    - `statement: Option<String>`
    - `tier: Option<String>`
    - `status: Option<String>`
    - `anchor_kind`
    - `anchor_id`
    - `parent_anchor_id: Option<String>`
- **search ranking 逻辑不变**
  - 过滤只缩小候选集
  - BM25 + vector + RRF 不改
  - P7 的 `content` raw 语义保持不变

## Boundaries

### Allowed Changes
- `src/core/db.rs`
- `src/core/types.rs`
- `src/ingest/**`
- `src/search/**`
- `src/mcp/tools.rs`
- `src/mcp/server.rs`
- `src/main.rs`
- `tests/**`
- `docs/MIND-MODEL-DESIGN.md`

### Forbidden
- 不要引入 `knowledge_cards` / `knowledge_events` / `knowledge_evidence_links` 新表
- 不要实现自动 `promote` / `demote` / `publish_anchor` 工作流
- 不要新增 `mempal_context` / `mempal_reasoning_pack` 之类 runtime assembler
- 不要改现有 BM25 + vector + RRF ranking
- 不要改变 `SearchResult.content` 的 raw 语义
- 不要移除 `wing/room` 路由、taxonomy 或 tunnel 逻辑
- 不要引入新依赖

## Out of Scope

- Phase 2 `knowledge_cards` 抽离
- `publish_anchor(worktree -> repo)` 的独立 API / CLI
- skill trigger orchestration
- runtime `dao_tian -> dao_ren -> shu -> qi -> evidence` context assembler
- 自动 evaluator gate
- `repo -> global` 或 `worktree -> repo` 的自动晋升策略

## Completion Criteria

Scenario: MCP ingest omission remains backward-compatible and writes evidence drawer defaults
  Test:
    Filter: test_mcp_ingest_defaults_to_evidence_drawer_bootstrap_metadata
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，只提供现有字段 `content/wing/room/source/dry_run/importance`
  Then 新 drawer 的 `memory_kind == "evidence"`
  And `domain == "project"`
  And `field == "general"`
  And `provenance == "human"`
  And `statement IS NULL`
  And `tier IS NULL`
  And `status IS NULL`

Scenario: knowledge drawer stores statement separately from content body
  Test:
    Filter: test_knowledge_drawer_keeps_statement_separate_from_content
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，参数包含:
    | 字段             | 值 |
    | memory_kind      | knowledge |
    | domain           | skill |
    | field            | debugging |
    | statement        | Debug by reproducing before patching. |
    | tier             | shu |
    | status           | promoted |
    | supporting_refs  | ["drawer_ev_001"] |
    | content          | Start from a concrete reproduction, then isolate scope before patching. |
  Then 新 drawer 的 `memory_kind == "knowledge"`
  And `statement == "Debug by reproducing before patching."`
  And `content == "Start from a concrete reproduction, then isolate scope before patching."`
  And `tier == "shu"`
  And `status == "promoted"`
  And `supporting_refs` 保存为 JSON array

Scenario: knowledge drawer search result exposes metadata while preserving raw content
  Test:
    Filter: test_search_result_exposes_knowledge_metadata_without_rewriting_content
    Level: integration
  Given 数据库中存在一个 knowledge drawer，`statement="Evidence precedes assertion."`
  And 其 `content` 为 "Use source-backed verification before making load-bearing claims."
  When `search(query="evidence assertion", memory_kind="knowledge")`
  Then 返回结果包含 `memory_kind == "knowledge"`
  And `statement == "Evidence precedes assertion."`
  And `tier` 与 `status` 非空
  And 返回的 `content` 字段字节级等于原始 `"Use source-backed verification before making load-bearing claims."`

Scenario: evidence drawer cannot set knowledge-only fields
  Test:
    Filter: test_evidence_drawer_rejects_knowledge_only_fields
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，设置 `memory_kind="evidence"` 且同时传 `tier="qi"`
  Then 请求失败
  And 错误消息指出 `evidence` drawer 不允许 knowledge-only 字段

Scenario: knowledge drawer requires statement and supporting refs
  Test:
    Filter: test_knowledge_drawer_requires_statement_and_supporting_refs
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，设置 `memory_kind="knowledge"` 但省略 `statement` 或 `supporting_refs`
  Then 请求失败
  And 错误消息指出缺失必填 knowledge 字段

Scenario: dao_tian rejects non-canonical status
  Test:
    Filter: test_dao_tian_rejects_noncanonical_status
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，设置 `memory_kind="knowledge"`, `tier="dao_tian"`, `status="candidate"`
  Then 请求失败
  And 错误消息指出 `dao_tian` 只允许 `canonical` 或 `demoted`

Scenario: global anchor requires global domain
  Test:
    Filter: test_global_anchor_rejected_for_non_global_domain
    Level: integration
  Given 一个空数据库
  When MCP 客户端调用 `mempal_ingest`，设置 `anchor_kind="global"` 且 `domain="project"`
  Then 请求失败
  And 错误消息指出 `global` anchor 只允许 `domain="global"`

Scenario: linked git worktree derives worktree anchor with repo parent
  Test:
    Filter: test_git_worktree_derives_worktree_anchor_and_repo_parent
    Level: integration
  Given 一个 linked git worktree 路径作为 `cwd`
  And 调用 `mempal_ingest` 时未显式提供 `anchor_kind` / `anchor_id`
  When 写入一条 evidence drawer
  Then 新 drawer 的 `anchor_kind == "worktree"`
  And `anchor_id` 基于 `git rev-parse --show-toplevel`
  And `parent_anchor_id` 基于 `git rev-parse --git-common-dir`

Scenario: non-git cwd falls back to standalone worktree anchor
  Test:
    Filter: test_non_git_cwd_falls_back_to_standalone_worktree_anchor
    Level: integration
  Given 一个不在 git 仓库中的本地目录作为 `cwd`
  When 写入一条 evidence drawer 且未显式提供 anchor
  Then 新 drawer 的 `anchor_kind == "worktree"`
  And `anchor_id` 等于规范化后的目录路径编码
  And `parent_anchor_id IS NULL`

Scenario: migration backfills legacy drawers as evidence with bootstrap defaults
  Test:
    Filter: test_migration_backfills_legacy_drawers_with_bootstrap_defaults
    Level: integration
  Given 一个旧 schema 数据库，drawer 只含旧列
  When 打开数据库并触发 migration
  Then 既有 drawer 的 `memory_kind == "evidence"`
  And `domain == "project"`
  And `field == "general"`
  And `anchor_kind == "repo"`
  And `anchor_id == "repo://legacy"`
  And `source_type="project"` 的 drawer 被回填为 `provenance="research"`
  And `source_type="conversation"` 的 drawer 被回填为 `provenance="human"`

Scenario: search filters by memory_kind and tier without changing ranking semantics
  Test:
    Filter: test_search_filters_by_memory_kind_and_tier_without_rerank_changes
    Level: integration
  Given 数据库中同时存在 evidence drawer 与 knowledge drawer
  And 至少两条 knowledge drawer 分别为 `tier="qi"` 和 `tier="shu"`
  When `search(query="debug", memory_kind="knowledge", tier="shu")`
  Then 返回结果只包含 `memory_kind == "knowledge"` 且 `tier == "shu"` 的 drawer
  And 结果仍按现有 `similarity` / RRF 排序规则返回

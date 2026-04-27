spec: task
name: "P13A: wake-up consumes knowledge statement"
inherits: project
tags: [memory, wake-up, knowledge, statement]
estimate: 0.5d
---

## Intent

P12 已经把 `knowledge drawer.statement` 定义成短句唤醒载荷，但当前
`mempal wake-up` 和 `mempal wake-up --format aaak` 仍然只消费
`drawer.content`。这会让 knowledge drawer 在 runtime wake-up 阶段继续
暴露长 rationale body，而不是短 canonical proposition。

本任务只把 wake-up 输出切换到“knowledge 优先用 `statement`，evidence 继续
用 `content`”这一条最小行为闭环，不引入更大的 runtime assembler。

## Decisions

- 定义一个 **effective wake-up text**：
  - `memory_kind='knowledge'` 且 `statement` 非空时，使用 `statement`
  - 其它情况使用 `content`
- plain `mempal wake-up` 的 L1 summary 行使用 effective wake-up text
- `mempal wake-up --format aaak` 的聚合文本使用 effective wake-up text
- `estimated_tokens` 必须基于 effective wake-up text 估算，而不是始终基于 `content`
- wake-up 的排序保持不变：
  - 继续使用现有 `top_drawers(5)` 行为
  - 不改 importance / added_at 的排序语义
- `source_file`、`drawer_id`、`wing/room`、Memory Protocol 输出形状保持不变
- knowledge drawer 若 `statement` 为 `NULL` 或空白，wake-up 回退到 `content`
- search / MCP search / CLI search 的 `content` raw 语义完全不变
- 本任务只覆盖 CLI wake-up surface：
  - `mempal wake-up`
  - `mempal wake-up --format aaak`
  - `mempal wake-up --format protocol`

## Boundaries

### Allowed Changes
- `src/main.rs`
- `tests/**`
- `docs/MIND-MODEL-DESIGN.md`
- `AGENTS.md`
- `CLAUDE.md`

### Forbidden
- 不要改 `drawers` schema
- 不要改 `mempal_search` / MCP search / REST search 返回的 `content`
- 不要新增 wake-up flags 或新的输出格式
- 不要实现 `mempal_context` / reasoning-pack / runtime assembler
- 不要改 wake-up 排序逻辑

## Out of Scope

- `dao_tian -> dao_ren -> shu -> qi -> evidence` 的完整组装器
- skill trigger orchestration
- REST API parity
- ingest identity 统一
- `knowledge_cards` / lifecycle events

## Completion Criteria

Scenario: plain wake-up prefers knowledge statement over rationale body
  Test:
    Filter: test_wake_up_prefers_knowledge_statement_in_plain_output
    Level: integration
  Given 数据库中存在一个 knowledge drawer
  And `statement == "Debug by reproducing before patching."`
  And `content == "Start from a concrete reproduction, then isolate scope before patching."`
  When 执行 `mempal wake-up`
  Then L1 summary 包含 `"Debug by reproducing before patching."`
  And L1 summary 不包含原始 rationale body 的长句

Scenario: AAAK wake-up aggregates knowledge statement instead of content body
  Test:
    Filter: test_wake_up_aaak_prefers_knowledge_statement
    Level: integration
  Given 数据库中存在一个 knowledge drawer
  And 其 `statement` 与 `content` 不同
  When 执行 `mempal wake-up --format aaak`
  Then 生成的 AAAK document 基于 `statement`
  And 不要求 `content` 逐字出现在输出中

Scenario: evidence drawer still wakes by content
  Test:
    Filter: test_wake_up_evidence_drawer_still_uses_content
    Level: integration
  Given 数据库中存在一个 evidence drawer
  And 其 `content == "Observed that tests failed after the patch."`
  When 执行 `mempal wake-up`
  Then L1 summary 包含 `"Observed that tests failed after the patch."`

Scenario: knowledge drawer without statement falls back to content
  Test:
    Filter: test_wake_up_knowledge_without_statement_falls_back_to_content
    Level: integration
  Given 数据库中存在一个 knowledge drawer
  And `statement IS NULL` 或仅包含空白
  And `content == "Fallback to the rationale body when statement is missing."`
  When 执行 `mempal wake-up`
  Then L1 summary 包含 `"Fallback to the rationale body when statement is missing."`

Scenario: wake-up preserves existing top-drawer ordering
  Test:
    Filter: test_wake_up_preserves_existing_top_drawer_order
    Level: integration
  Given 数据库中存在两个 drawer
  And 现有 `top_drawers(5)` 语义会让高 importance 或更新的 drawer 排在前面
  When 执行 `mempal wake-up`
  Then 输出中的 drawer 顺序与既有 wake-up 排序一致
  And 切换到 `statement` 不会改变 drawer 的相对次序

Scenario: estimated tokens use effective wake-up text
  Test:
    Filter: test_wake_up_estimated_tokens_use_effective_text
    Level: integration
  Given 数据库中存在一个 knowledge drawer
  And 其 `statement` 明显短于 `content`
  When 执行 `mempal wake-up`
  Then `estimated_tokens` 基于 effective wake-up text
  And 不是始终按原始 `content` 词数累加

Scenario: protocol output remains unchanged
  Test:
    Filter: test_wake_up_protocol_output_unchanged
    Level: integration
  Given 任意数据库状态
  When 执行 `mempal wake-up --format protocol`
  Then 输出仍然等于 `MEMORY_PROTOCOL`
  And 不注入 drawer summary

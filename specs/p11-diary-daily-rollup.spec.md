spec: task
name: "P11: agent-diary day-based rollup — one upserted drawer per day to tame chatty agents"
tags: [feature, diary, ingest, agent-diary, optional]
estimate: 0.5d
---

## Intent

借鉴 mempalace f935e85 (`diary_ingest.py`) 的"ONE drawer per day, upserted as the day grows"模式。

当前问题：P5 `wing="agent-diary"` 约定对 chatty agent（一天 ingest 20+ 条 OBSERVATION/LESSON/PATTERN）产生 drawer 爆炸。search 结果列表挤满同类条目，重要信号被稀释。

mempalace 的方案：天粒度 roll-up，每天一个 drawer，当天新增内容 **upsert append** 到同一 drawer，而不是每条产一个 row。

核心用户价值：**agent-diary 的信号密度提升**，每日 drawer 作为一天行为总结单位，既方便 search 也方便人工 review；chatty agent 不再淹没 palace。

**Optional 原因**：当前 agent-diary 才 3 个 drawer（见 status），尚未爆炸；本 spec 是预防性能力，用户可自行选择启用。

## Decisions

- **新 flag `--diary-rollup`** 在 `mempal_ingest` MCP 工具和 `mempal ingest` CLI：
  - `true` → 触发 diary-rollup 路径
  - `false` / omit → 走常规 ingest（一 ingest 一 drawer）
- **触发条件收紧**：`diary_rollup=true` 必须配合 `wing="agent-diary"`，否则返回 `IngestError::DiaryRollupWrongWing`
- **日期 key**：`rollup_day = Utc::now().format("%Y-%m-%d")`（UTC 基准；**不**按 local tz，避免跨机器行为漂移）
- **Drawer id 约定**：`drawer_id = format!("drawer_agent-diary_{room}_day_{rollup_day}")`（固定 pattern，保证同 room 同天唯一）
- **Upsert 语义**：
  1. 查 `SELECT content FROM drawers WHERE id = ?`
  2. 若存在：`new_content = old_content + "\n\n---\n\n" + incoming_content`（追加分隔符）
  3. 若不存在：`new_content = incoming_content`
  4. `INSERT OR REPLACE INTO drawers (...)` 写回；**重新 embed 整条** content（保证向量和最新文本一致）
  5. 旧 `drawer_vectors` row 随 REPLACE 级联（外键 ON DELETE CASCADE 已有）
- **P9 lock 复用**：`source_key = format!("diary_rollup_{room}_{rollup_day}")`，不同天 / 不同 room 并发不阻塞，同一 (room, day) 串行化
- **内容大小硬上限**：单个 rollup drawer 的 `content.len() > 32 KB` 时拒绝继续 append，返回 `IngestError::DailyRollupFull`；agent 应改日或换 room。不自动 split（保证 id 唯一性）
- **search 表现**：rollup drawer 作为普通 drawer 参与检索；P7 signals 在每次 upsert 时**重新计算**（AAAK entity/topic 聚合整段 content）
- **Protocol Rule 5a 更新**：`src/core/protocol.rs` 的 Rule 5a "KEEP A DIARY" 末尾追加"若一天内多次记录，可以 `diary_rollup=true` 合并到当日单条 drawer 以降低噪声"
- **CLI**：`mempal ingest --diary-rollup --wing agent-diary --room claude -f entry.txt`
- **Status 展示**：`mempal_status` response 追加 `diary_rollup_days: u32`（有 rollup drawer 的天数），可选字段
- **不改 schema**：upsert 复用现有 `INSERT OR REPLACE` 语义，`drawer_id` 主键独一无二即可；不 bump schema version

## Boundaries

### Allowed
- `src/ingest/mod.rs`（`IngestOptions` 加 `diary_rollup: bool`；upsert 分支）
- `src/mcp/tools.rs`（`IngestRequest` 加 `diary_rollup: Option<bool>`）
- `src/mcp/server.rs`（handler 路由）
- `src/cli.rs`（`--diary-rollup` flag）
- `src/core/protocol.rs`（Rule 5a 补充）
- `src/core/db.rs`（`upsert_diary_rollup_drawer(...)` 薄 helper，或直接复用 insert_drawer）
- `tests/diary_rollup.rs`（新增）

### Forbidden
- 不改 `drawers` / `drawer_vectors` / `triples` 表 schema
- 不破坏 P5 agent-diary 约定（wing="agent-diary" 下默认仍是 per-entry drawer）
- 不在 `wing != "agent-diary"` 下允许 diary_rollup
- 不自动 split 超长 rollup drawer（硬上限 + 报错）
- 不引新 dep
- 不改 P7 signals 抽取逻辑的 public API

## Out of Scope

- 跨月 / 跨周 rollup
- 自动把历史多条 per-entry drawer 合并为 daily rollup（需要 migration 工具，独立 spec）
- Search 时按天聚合展示（search 返回 raw drawer 即可）
- Rollup 的 AAAK 压缩
- 按 local timezone 分天（UTC 统一避免不确定）
- 非 agent-diary wing 的 rollup

## Completion Criteria

Scenario: diary-rollup 首次 ingest 产生 day drawer
  Test:
    Filter: test_first_rollup_creates_day_drawer
    Level: integration
  Given 空 palace.db
  When `ingest --diary-rollup --wing agent-diary --room claude --content "OBSERVATION: foo"`
  Then drawer_count == 1
  And drawer id 形如 `drawer_agent-diary_claude_day_<today>`

Scenario: 同一天第二次 ingest upsert 到同一 drawer
  Test:
    Filter: test_second_rollup_same_day_appends
    Level: integration
  Given 已有今天的 rollup drawer with content "A"
  When ingest with content "B" + diary_rollup=true
  Then drawer_count 仍为 1
  And 该 drawer.content 等于 "A\n\n---\n\nB"

Scenario: 不同天的 ingest 产生新 drawer
  Test:
    Filter: test_different_day_creates_new_rollup
    Level: integration
  Given 有 2026-04-16 的 rollup drawer
  When mock 时间为 2026-04-17 并 ingest diary_rollup
  Then drawer_count == 2
  And 第二个 drawer id 含 "2026-04-17"

Scenario: 不同 room 的同一天互不干扰
  Test:
    Filter: test_different_room_separate_rollup
    Level: integration
  Given 今天 claude room 和 codex room 各自 diary_rollup ingest
  When 查 drawer_count
  Then drawer_count == 2
  And 两个 drawer id 分别含 "claude" 和 "codex"

Scenario: diary_rollup 在非 agent-diary wing 被拒绝
  Test:
    Filter: test_rollup_wrong_wing_rejected
    Level: unit
  When ingest with `wing="mempal"` 和 `diary_rollup=true`
  Then 返回 `Err(IngestError::DiaryRollupWrongWing)`
  And drawer_count 不变

Scenario: rollup drawer 超过 32KB 继续 append 被拒绝
  Test:
    Filter: test_rollup_over_limit_rejected
    Level: integration
  Given 今天 rollup drawer content 已达 31.9KB
  When ingest 一条 200B content + diary_rollup
  Then 返回 `Err(IngestError::DailyRollupFull)`
  And drawer.content 不变

Scenario: rollup 的 vector 在 upsert 后反映最新 content
  Test:
    Filter: test_rollup_vector_refreshed_on_upsert
    Level: integration
  Given 今天 rollup drawer 有 content "A"，嵌入向量 V1
  When 再 ingest "B" upsert 后
  Then `drawer_vectors` 中该 drawer 的 embedding 不等于 V1（内容已变）
  And search query 匹配 "B" 时能命中该 drawer

Scenario: 并发两个 diary_rollup 同 (room, day) 串行化且内容不丢
  Test:
    Filter: test_concurrent_rollup_same_day_serialized
    Level: integration
  Given 空 palace.db
  When spawn 两个 task 同时 ingest diary_rollup + content ("X", "Y")
  Then drawer_count == 1
  And drawer.content 含 "X" 且含 "Y"（两者都被 append，顺序不约束）

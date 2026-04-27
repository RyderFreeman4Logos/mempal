spec: task
name: "P10: drawer normalize_version — detect and auto-rebuild drawers made by stale normalize logic"
tags: [infra, ingest, reindex, migration, content-version]
estimate: 0.5d
---

## Intent

借鉴 mempalace 7e5eeda (`NORMALIZE_VERSION` schema gate) 的思路。

当前问题：mempal 的 `src/ingest/normalize.rs`（格式检测 + 内容规范化）是 drawer **内容层**的逻辑。当这段逻辑升级（例如 Slack DM / Codex CLI 格式支持改进、AAAK entity 抽取规则优化），**历史 drawer 仍是旧逻辑产出的 content**——search/KG 都基于旧语义。DB schema 版本（`CURRENT_SCHEMA_VERSION`）不能覆盖这种**内容版本**的漂移，因为 schema 没变。

mempalace 的方案：每个 drawer row 存一个 `normalize_version`，当 `CURRENT_NORMALIZE_VERSION` bump 后，`mempal reindex --stale` 只重处理 `drawer.normalize_version < CURRENT_NORMALIZE_VERSION` 的 rows。

核心用户价值：**未来改内容规范化逻辑后，不用清空整个 palace 重 ingest**——只重新处理过时 drawer，成本由 stale fraction 决定。

## Decisions

- **Schema v7**：`drawers` 表加列 `normalize_version INTEGER NOT NULL DEFAULT 1`；`CURRENT_SCHEMA_VERSION: 6 → 7`（P10 explicit tunnels 使用 schema v6）
- **Migration 语义**：v6 → v7 的 ALTER TABLE 把所有历史 drawer 设为 `normalize_version = 1`（baseline）
- **新常量 `CURRENT_NORMALIZE_VERSION`** 在 `src/ingest/normalize.rs`，**初值 = 1**（本 spec 不 bump，只建立机制）
- **Ingest pipeline 改造**：`ingest_file_with_options` 写入 drawer 时记入 `CURRENT_NORMALIZE_VERSION`
- **`mempal reindex` 子命令扩展**（假设现已存在；若无则新增）：
  - 新增 flag `--stale`：只处理 `normalize_version < CURRENT_NORMALIZE_VERSION` 的 drawer
  - 新增 flag `--force`：处理全部 drawer（忽略 version）
  - 默认（无 flag）=== 等价 `--stale`
  - reindex 流程：per-drawer 拉出 `source_file`，**复用 ingest_file_with_options（带 P9 lock）**走完整 pipeline，而不是 in-place re-run normalize（避免 normalize 和 chunker 不一致）
- **Status MCP 工具扩展**：`mempal_status` response 追加 `normalize_version_current: u32` 和 `stale_drawer_count: u64`，agent 可以在 session 开始时看出"这个 palace 有 X 条 drawer 过时"
- **Search 行为不变**：不做 "skip stale drawers" 的过滤（那会让 reindex 阻塞搜索）；stale drawer 正常参与检索
- **Dry-run support**：`mempal reindex --stale --dry-run` 输出将要处理的 drawer 数和估计时间，不实际重写
- **Reindex 并发安全**：复用 P9 的 per-source lock（`acquire_source_lock`）——多个 drawer 共享同一 source_file 时只处理一次
- **不 bump 实际 NORMALIZE_VERSION**：本 spec 建立机制，不改变任何现有 normalize 行为；首次 bump 留给后续 spec（比如 P11 格式支持改进）

## Boundaries

### Allowed
- `src/core/schema.sql`（ALTER / CREATE 新列）
- `src/core/db.rs`（migration v6→v7；`stale_drawer_count()` 查询；`drawer_count_by_normalize_version()`；insert 路径带入新列）
- `src/ingest/normalize.rs`（pub const `CURRENT_NORMALIZE_VERSION: u32 = 1`）
- `src/ingest/mod.rs`（insert 时带 CURRENT_NORMALIZE_VERSION）
- `src/main.rs`（reindex --stale / --force / --dry-run 子命令 flag）
- `src/mcp/tools.rs`（`StatusResponse` 加两个字段）
- `src/mcp/server.rs`（`mempal_status` handler 填新字段）
- `tests/normalize_version.rs`（新增）

### Forbidden
- 不改 normalize 实际逻辑（本 spec 只建机制）
- 不 bump `CURRENT_NORMALIZE_VERSION` 超过 1
- 不改 `drawer_vectors` / `triples` / `tunnels` 表
- 不让 search 按 normalize_version 过滤
- 不破坏现有 `tests/ingest_test.rs`
- 不引新 dep
- 不改 MCP ingest / search / kg / tunnels / peek / push / delete 工具的 schema（除 status）

## Out of Scope

- 自动触发 reindex（手动 / CI-gated，不在 ingest 里自动 race）
- Normalize version 的 semver / 多维版本（单一 u32 足够）
- 内容 diff（reindex 直接重走 pipeline，不 compare）
- Vector 版本（vector 维度变更走 schema migration，不是 normalize version）
- Drawer 粒度的部分重算（整个 source 重走 pipeline）

## Completion Criteria

Scenario: v6 → v7 migration 把历史 drawer 设为 normalize_version=1
  Test:
    Filter: test_migration_v6_to_v7_stamps_normalize_version_1
    Level: integration
  Given schema v6 palace.db with 20 drawers
  When 打开 db（触发 migration）
  Then schema_version == 7
  And `SELECT COUNT(*) FROM drawers WHERE normalize_version = 1` == 20
  And drawer_count 不变

Scenario: 新 ingest drawer 带 CURRENT_NORMALIZE_VERSION
  Test:
    Filter: test_new_ingest_writes_current_normalize_version
    Level: unit
  Given schema v7，CURRENT_NORMALIZE_VERSION == 1
  When ingest 一个新文件产生 3 chunks
  Then `SELECT DISTINCT normalize_version FROM drawers WHERE source_file = ?` == [1]

Scenario: mempal_status 暴露 stale_drawer_count
  Test:
    Filter: test_status_exposes_stale_count
    Level: integration
  Given 20 drawers 带 normalize_version=1
  And 手动 UPDATE 5 drawer 为 normalize_version=0（模拟 stale）
  When 调用 `mempal_status`
  Then response.stale_drawer_count == 5
  And response.normalize_version_current == 1

Scenario: reindex --stale 只处理过期 drawer
  Test:
    Filter: test_reindex_stale_only_reprocesses_outdated
    Level: integration
  Given 20 drawer, 5 个 normalize_version=0, 15 个 = 1
  When `mempal reindex --stale` 执行完
  Then 所有 drawer.normalize_version == 1
  And reindex 只触发了 5 个 drawer 对应的 source_file 的 ingest（log capture）

Scenario: reindex --dry-run 不改写
  Test:
    Filter: test_reindex_dry_run_no_writes
    Level: integration
  Given 5 stale drawer
  When `mempal reindex --stale --dry-run`
  Then stdout 含 "would reprocess 5 drawers"
  And 所有 drawer.normalize_version 不变
  And 无新 audit log entry

Scenario: reindex --force 处理所有 drawer
  Test:
    Filter: test_reindex_force_reprocesses_all
    Level: integration
  Given 20 drawer（混合 version）
  When `mempal reindex --force` 执行
  Then 所有 drawer 被重新 ingest（drawer id 可能变；count 不变或变大）
  And 所有 drawer.normalize_version == CURRENT_NORMALIZE_VERSION

Scenario: reindex 和并发 ingest 不 race（复用 P9 lock）
  Test:
    Filter: test_reindex_respects_per_source_lock
    Level: integration
  Given stale drawer for source `/tmp/doc.md`
  When 同时 spawn task A: `reindex --stale` + task B: `ingest_file /tmp/doc.md`
  Then 两个 task 串行化（B 等 A 释放锁 or 反之）
  And 最终 drawer_count 一致（无 duplicate）

Scenario: 缺失 source_file 的 stale drawer 被 skip 且报告
  Test:
    Filter: test_reindex_skips_missing_source_file
    Level: integration
  Given stale drawer 对应的 source_file 已被删
  When `mempal reindex --stale`
  Then stderr 含 warning "skipped 1 drawer: source file missing"
  And drawer 原样保留（不删除，不更新 normalize_version）

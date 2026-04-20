spec: task
name: "P10: Hard project_id filter on hybrid search to prevent cross-repo noise"
tags: [feature, search, isolation, multi-project]
estimate: 1d
---

## Intent

在 `mempal-search` 的混合检索（BM25 + 向量 + RRF）中，**硬隔离 project_id**：搜索请求带 `project_id` 时，向量 top-k 和 FTS5 查询都在 SQL `WHERE project_id = ?` 子句层直接过滤，而非后过滤。保证跨 repo / 跨项目使用 mempal 时，小项目的召回不被大项目的噪声挤占 top-N 槽位。

**动机**：claude-mem `src/services/worker/SearchManager.ts:246-249` 的关键实战发现——高噪音大项目库在向量 top-N 召回中会完全压制小项目的相关结果，即使后过滤也来不及。必须把 `project_id` 推到向量搜索的 `where` 子句（对应 sqlite-vec 的 `knn_rowids` filter）。

**v3 判决依据**：v2 分析 "claude-mem 值得吸收但 7 个 issue 没覆盖" 第 2 项。独立对 mempal 的多项目部署场景价值高。

## Decisions

- fork-ext `fork_ext_version` `4 → 5` 新 migration：
  - `drawers` 表加 `project_id TEXT`（默认 NULL 兼容既有 drawer） —— 走 `ALTER TABLE ADD COLUMN`（regular table 支持）
  - 索引 `CREATE INDEX idx_drawers_project_id ON drawers(project_id)`
  - `drawer_vectors` 表加 `project_id TEXT` —— **不能用 `ALTER TABLE`**（gemini 2026-04-20 策略审查 Part 3 #1 critical finding）：sqlite-vec `vec0` 是 SQLite virtual table，**不支持 `ALTER TABLE ADD COLUMN`**（也不支持 `ALTER TABLE ADD` 任何形式的列修改，只支持 `RENAME`）。必须走 **DROP + CREATE + 全量 reinsert** 流程，且整个过程必须把既有向量数据**逐行读回内存再写回新表**，否则会静默丢失全部历史向量（导致 search 只剩 BM25 但不报错）。具体步骤见下条"vec0 recreate 迁移步骤"
  - `triples` 表加 `project_id TEXT`（未来 KG 隔离用，本 spec 不用） —— regular table 走 `ALTER TABLE ADD COLUMN`
- **vec0 recreate 迁移步骤**（保数据零丢失）：
  ```
  BEGIN IMMEDIATE;
  -- 1. 把既有向量全量读出到一个临时 regular table（不是 vec0）
  CREATE TEMP TABLE _drawer_vectors_backup (id TEXT PRIMARY KEY, embedding BLOB);
  INSERT INTO _drawer_vectors_backup (id, embedding)
    SELECT id, embedding FROM drawer_vectors;
  -- 2. DROP 老 vec0 表
  DROP TABLE drawer_vectors;
  -- 3. 重建 vec0，声明 auxiliary column `+project_id TEXT`（vec0 语法要求 `+` 前缀声明非索引 metadata 列）
  CREATE VIRTUAL TABLE drawer_vectors USING vec0(
    id TEXT PRIMARY KEY,
    embedding FLOAT[{DIM}],  -- DIM 取迁移前 SELECT vec_length(embedding) FROM _drawer_vectors_backup LIMIT 1
    +project_id TEXT
  );
  -- 4. 把 backup 灌回新表，project_id 填 NULL（后续用 `mempal project migrate` 显式 backfill）
  INSERT INTO drawer_vectors (id, embedding, project_id)
    SELECT id, embedding, NULL FROM _drawer_vectors_backup;
  -- 5. 断言行数一致
  -- （代码侧检查：SELECT COUNT(*) FROM drawer_vectors 必须等于迁移前 COUNT(*)；不等则 ROLLBACK）
  DROP TABLE _drawer_vectors_backup;
  COMMIT;
  ```
  **关键不变量**：
  - 整个迁移包在单个 `BEGIN IMMEDIATE ... COMMIT` 里——失败 `ROLLBACK` 后 `drawer_vectors` 恢复到迁移前状态（SQLite DDL 本身也支持事务）
  - 行数 before/after 必须相等，否则立即 `ROLLBACK` + 返回 `MigrationError::VectorLossDetected { expected, got }` + fork_ext_version 不 bump
  - 迁移期间 daemon / MCP 必须 pause 所有 ingest / search 写入（`mempal status` 显示 `migrating=true`，写入类 MCP 返回 `system_warnings` 要求 agent 重试）。读路径（search）在迁移期间 BM25-only，vector 部分静默跳过且在 `system_warnings` 标记
  - 如果迁移前 `drawer_vectors` 表不存在（fresh DB 或 lazy-create 未触发），跳过 backup 步骤，直接 step 3 CREATE（带 `+project_id`）
  - lazy-create 路径（`db.rs` `ensure_vector_table`）在 fork_ext_version >= 5 之后 MUST 带 `+project_id TEXT` 声明；schema drift 检测见 "`test_lazy_create_after_v5_has_project_id_column`" 新 scenario
- `project_id` 识别：
  - CLI: `mempal ingest ... --project <id>`；默认从 `git rev-parse --show-toplevel` basename 推断（`mempal-dev` → `"mempal"`），或配置 `[project] id = "..."`
  - MCP: `mempal_ingest` / `mempal_search` input schema 加可选 `project_id` 字段；`mempal_search` 额外加可选 `include_global: bool`（默认 `false`）；未提供 `project_id` 时从 `[project]` config 回退
  - `mempal-hook` / `daemon`: 从 payload `workspace_path` 或 env（`MEMPAL_PROJECT_ID`）推断
- Search filter 语义：
  - `mempal_search(project_id: Some("X"), include_global: false)` → **硬过滤**：只返回 `drawers.project_id == "X"` 的结果（default；与 Intent "硬隔离" 对齐，彻底消除 NULL 记录在 top-k 的 crowd-out）
  - `mempal_search(project_id: Some("X"), include_global: true)` → 返回 `drawers.project_id == "X" OR drawers.project_id IS NULL`，并在结果 DTO 标 `source: "project" | "global"` 让调用方看清命中来源（透明 opt-in，不是默认 soft pass-through）
  - `mempal_search(project_id: None)` → 全库搜（向下兼容），结果同样带 `source` 标记（避免调用方无法判别命中来自哪项目）
  - 新配置 `[search] strict_project_isolation = false`；`true` 时 `project_id: None` 也只返回 `IS NULL` 记录，禁止跨项目
  - `include_global` 默认 `false` 是 Intent "硬隔离" 的正确默认值——默认放行 NULL 会让既有 palace.db（migration 后所有记录 `project_id=NULL`）继续污染每一次 project-filtered 查询，crowd-out 不会真正消除
- BM25 (FTS5) 侧过滤：
  - FTS5 contentless table 无法直接 join —— 用 `drawers` 外部表 JOIN FTS5 的 rowid，JOIN 条件加 `project_id`
  - 或预先查出 `project_id=X` 的 drawer id 列表，传给 FTS5 查询做 `rowid IN (...)` 子句（性能在数千 drawer 规模 OK）
- Vector 侧过滤：
  - `sqlite-vec` 的 `vec0` 虚拟表 KNN 查询支持 auxiliary column filter——`drawer_vectors` 表新增 `project_id` 列，KNN 子句里加 `WHERE project_id = ?`
- `project_id` 向量填值：`mempal_ingest` 写 `drawer_vectors` 时同步写 `project_id`
- 现有 drawer 的 backfill：
  - migration 时自动把所有 `project_id` 设为 NULL（兼容）
  - 提供 `mempal project migrate --project <id> [--wing <W>]` 子命令把指定 wing 下 drawer 的 `project_id` 批量 UPDATE
  - **Batched UPDATE 避免长锁**（CSA debate 2026-04-20 R7 validated）：一次性 `UPDATE drawers SET project_id = ? WHERE ...` 对十万行大库会拿 SQLite 写锁数分钟，阻塞 daemon ingest 和 MCP search。改用**分批循环**：
    ```sql
    BEGIN IMMEDIATE;
    UPDATE drawers SET project_id = ? WHERE id IN (
      SELECT id FROM drawers WHERE project_id IS NULL AND wing = ? LIMIT 1000
    );
    COMMIT;
    ```
    循环直到 `changes() == 0`，每批 commit 后让出锁 ~10ms 让其它 writer 插入
    - `BEGIN IMMEDIATE` fail-fast：若此时有其它 writer 持锁，立刻返回 `SQLITE_BUSY` 而非等待，migrate 子命令可选重试 backoff（500ms / 1s / 2s）或输出进度让用户看到"暂时让路"
    - 并发 ingest 可能在 migrate 期间继续插入 `project_id IS NULL` 的新行——LIMIT loop 会在后续批次把它们也捕获，最终收敛（`changes() == 0` 时结束）
    - 同步更新 `drawer_vectors` 表的 `project_id` 冗余列时，采用同样分批策略（对每个已 UPDATE 的 drawer id）
  - `mempal project migrate` 子命令 stdout 逐批打印进度 `"batch 3: 1000 drawers updated, 42000 remaining"`，让用户能判断是否卡住
- `project_id` 不参与 AAAK signal 提取（project 是元数据轴，非内容轴）
- `mempal status` 加 "project breakdown" 行：`drawers per project: {proj-A:42, proj-B:18, NULL:7}`
- `mempal tail` / `mempal timeline`（P10 CLI）支持 `--project <id>` 过滤
- 严格 project 隔离是**默认关闭**的（保持 P0-P9 语义），用户显式开才生效
- **Tunnel resolver 豁免**（CSA design review 2026-04-20 识别的交互）：upstream `specs/p10-explicit-tunnels.spec.md`（未 fork-ext，是 upstream 提出的 wing-to-wing 链接 schema）的 tunnel-follow 路径**必须豁免** project hard-filter——tunnels 的存在本身就是"跨 wing / 跨 project 的显式桥"，如果 tunnel 的 target drawer 属于另一 project，严格过滤会把 follow 变成 404。具体实现：
  - `mempal_tunnels follow <tunnel_id>` / `mempal_search tunnel_hints` 解析目标 drawer 时，走 **bypass 版** `drawers::get_by_id()` 不带 `project_id` 筛选
  - Tunnel row 存储时必须保存 target drawer 的 `project_id`（若有），让调用方看清"这个 tunnel 把我引向另一 project"
  - Tunnel-resolved 的结果 DTO 明确标 `source: "tunnel_cross_project"`（与正常搜索结果的 `source: "project" | "global"` 并列），让 agent 知道命中是"故意越界"
  - 这是 **唯一** 允许绕过 project filter 的读路径；ingest / direct search / KG query 全部尊重 project filter

## Boundaries

### Allowed
- `crates/mempal-core/src/db/schema.rs`（fork_ext_version `4 → 5` migration）
- `crates/mempal-core/src/project.rs`（新建：project_id 解析工具）
- `crates/mempal-core/src/config.rs`（`[project] id`、`[search] strict_project_isolation`）
- `crates/mempal-core/src/lib.rs`（`pub mod project`）
- `crates/mempal-ingest/src/pipeline.rs`（ingest 写入时填 `project_id`）
- `crates/mempal-search/src/hybrid.rs` 或等价（查询时加 project filter）
- `crates/mempal-mcp/src/tools.rs` / `server.rs`（input schema 加可选 `project_id`）
- `crates/mempal-cli/src/main.rs`（`--project` 参数、`mempal project migrate` 子命令）
- `crates/mempal-cli/src/project_migrate.rs`（新建）
- `tests/project_isolation.rs`（新建）

### Forbidden
- 不要让 `project_id = NULL` 的既有 drawer 在 P8 升级后消失——migration 必须保留
- 不要把 `project_id` 作为复合主键的一部分（drawer id 依然全局唯一）
- 不要让 `project_id` 绑死到 git（支持非 git 环境：config 文件、env、CLI 参数）
- 不要让 `project_id` 格式校验过严（允许任意 UTF-8 非空字符串，只禁 `/`、`\0`、空白首尾）
- 不要在 `mempal_peek_partner` 加 project filter（跨项目协同是其本义）
- 不要让 project_id 参与 AAAK 编码
- 不要改 RRF 权重策略（仅改候选集合过滤，不改排序公式）
- 不要在 search 结果 DTO 里 expose 其他项目的 `project_id` 字符串（避免跨项目侧漏）；允许返回 `source: "project" | "global"` 二值标记——用户隐式知道自己查的是哪个项目，不需暴露具体 id 字符串，但需要知道命中来自本项目还是跨项目共享记忆

## Out of Scope

- 多 project 的独立权限 / ACL
- project 级 policy override（不同 project 用不同 gating / novelty 阈值）
- project_id 的命名空间校验（是否跨设备唯一）
- 按 project 备份 / 恢复（单 db）
- project 重命名工具（drawer 层面数据转移——走 `mempal project migrate --from X --to Y`，但本 spec 不实现 rename）
- 跨 project 显式引用机制（如 `mempal_search(project_id="other-X")` 穿透本项目 scope——默认不允许，需另一 spec）
- KG triples 的 project 隔离（schema 已 reserve 列，逻辑留未来）
- Web UI（违反 CLI-first feedback）

## Completion Criteria

Scenario: fork-ext 迁移 v4 → v5 添加 project_id 列
  Test:
    Filter: test_fork_ext_migration_v4_to_v5_adds_project_id
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db `fork_ext_version == "4"`
  When 启动 mempal
  Then `fork_ext_version == "5"`
  And `drawers`, `drawer_vectors`, `triples` 三表均有 `project_id` 列
  And 存在索引 `idx_drawers_project_id`
  And 既有 drawer 的 `project_id` 全部为 NULL

Scenario: v4 → v5 迁移时 drawer_vectors DROP+CREATE 全程保留向量（零丢失）
  Test:
    Filter: test_v4_to_v5_migration_preserves_vectors_during_recreation
    Level: integration
    Targets: crates/mempal-core/src/db.rs, crates/mempal-core/src/db_fork_ext.rs
  Given palace.db `fork_ext_version == "4"` 且 `drawer_vectors` 含 50 条向量（由过往 `mempal ingest` 真实写入，非 mock）
  And 迁移前记录 `before_count = SELECT COUNT(*) FROM drawer_vectors`
  And 迁移前为每条向量取 `(id, vec_length(embedding), vec_to_raw(embedding))` 元组存到测试内存
  When 启动 mempal（触发 v4 → v5 migration）
  Then `fork_ext_version == "5"`
  And `SELECT COUNT(*) FROM drawer_vectors == before_count`（必须等于 50，否则 MigrationError::VectorLossDetected）
  And 随机抽 5 个 drawer id，每个的 `(vec_length, vec_to_raw)` 字节级匹配迁移前记录（verbatim 不能丢精度）
  And 新表 schema 含 `+project_id TEXT` auxiliary column（`PRAGMA table_info(drawer_vectors)` 或等价 vec0 metadata 检查）
  And 所有 project_id 为 NULL（未 backfill）
  And 迁移失败/ROLLBACK 时（模拟：mid-migration 写入权限被撤销）`drawer_vectors` **完整恢复**到迁移前状态（`COUNT` / 内容全部相同），`fork_ext_version` **不** bump

Scenario: fresh DB 或 drawer_vectors 未 lazy-create 时 v4 → v5 跳过 backup 步骤
  Test:
    Filter: test_v4_to_v5_migration_skips_backup_when_table_absent
    Level: integration
    Targets: crates/mempal-core/src/db_fork_ext.rs
  Given palace.db `fork_ext_version == "4"` 且 `drawer_vectors` 表不存在（`SELECT * FROM sqlite_master WHERE name='drawer_vectors'` 为空，即还没任何 ingest 触发 lazy-create）
  When 启动 mempal
  Then 迁移**不**尝试 `CREATE TEMP TABLE _drawer_vectors_backup`（空表 SELECT 会正常 no-op，但 spec 明确这一路径不依赖表存在）
  And `fork_ext_version == "5"`
  And 后续首次 ingest 触发 `ensure_vector_table` 时创建的 vec0 表**必须**含 `+project_id TEXT`（见下条 scenario）

Scenario: lazy-create 路径在 fork_ext_version ≥ 5 后带 project_id auxiliary column
  Test:
    Filter: test_lazy_create_after_v5_has_project_id_column
    Level: integration
    Targets: crates/mempal-core/src/db.rs (ensure_vector_table)
  Given palace.db `fork_ext_version == "5"` 且 `drawer_vectors` 表未建（fresh 或迁移后仍 zero ingest）
  When 执行 `mempal ingest any.md --project proj-X`（触发首次 lazy-create + insert）
  Then `drawer_vectors` schema 中**必须**含 auxiliary column `+project_id TEXT`（否则后续 KNN filter 会 planner 错误或静默全表扫）
  And `SELECT project_id FROM drawer_vectors WHERE id=?` 返回 `"proj-X"`

Scenario: ingest 带 --project 时 project_id 被持久化
  Test:
    Filter: test_ingest_with_project_id_persists
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs, crates/mempal-core/src/project.rs
  Given 空 palace.db
  When 执行 `mempal ingest test.md --project proj-A`
  Then `drawers` 新增行 `project_id == "proj-A"`
  And `drawer_vectors` 对应行 `project_id == "proj-A"`

Scenario: search with project_id 硬过滤（默认 include_global=false）只返回同项目 drawer
  Test:
    Filter: test_search_hard_filters_by_project_id
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given drawer A.project_id="proj-A"、B.project_id="proj-B"、C.project_id=NULL（全部含 query 词）
  When 调 `mempal_search({query: "foo", project_id: "proj-A"})`（include_global 未传，默认 false）
  Then 返回结果仅含 A
  And 不含 B、不含 C
  And 每条结果 DTO `source == "project"`

Scenario: search with project_id + include_global=true 返回同项目 + NULL 并标 source
  Test:
    Filter: test_search_include_global_returns_project_and_null
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given drawer A.project_id="proj-A"、B.project_id="proj-B"、C.project_id=NULL（全部含 query 词）
  When 调 `mempal_search({query: "foo", project_id: "proj-A", include_global: true})`
  Then 返回结果含 A 和 C
  And 不含 B
  And A 的 DTO `source == "project"`，C 的 DTO `source == "global"`

Scenario: search without project_id 默认全库（非严格模式）
  Test:
    Filter: test_search_without_project_id_returns_all
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given 配置 `strict_project_isolation = false`
  And drawer A.project_id="proj-A"、B.project_id="proj-B"
  When 调 `mempal_search({query: "foo"})` 无 project_id 参数
  Then 返回结果含 A 和 B

Scenario: strict_project_isolation=true 时无 project_id 查询只返回 NULL 项目
  Test:
    Filter: test_strict_isolation_without_project_id_returns_null_only
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given 配置 `strict_project_isolation = true`
  And drawer A.project_id="proj-A"、C.project_id=NULL
  When 调 `mempal_search({query: "foo"})` 无 project_id 参数
  Then 返回结果仅含 C
  And 不含 A

Scenario: 大项目不挤占小项目召回槽位
  Test:
    Filter: test_large_project_does_not_crowd_out_small
    Level: integration
    Targets: crates/mempal-search/src/hybrid.rs
  Given proj-A 含 "1000" 条 drawer（高噪音），proj-B 含 "5" 条 drawer（含 query 精确匹配）
  When 调 `mempal_search({query: "精确匹配词", project_id: "proj-B", top_k: 5})`
  Then 返回结果长度 <= 5
  And 所有结果 `project_id == "proj-B"`（默认硬过滤，NULL 记录不参与）
  And 若 proj-B 中有精确匹配 drawer，其排名 <= 2（即高排位被保护）

Scenario: project_id 从 git repo 自动推断
  Test:
    Filter: test_project_id_auto_inferred_from_git
    Level: unit
    Targets: crates/mempal-core/src/project.rs
  Given 当前目录是 git repo，`git rev-parse --show-toplevel` 返回 `/path/to/my-awesome-proj`
  When 调 `project::infer_current()`
  Then 返回 `Some("my-awesome-proj")`

Scenario: config [project] id 覆盖自动推断
  Test:
    Filter: test_config_project_id_overrides_git
    Level: unit
    Targets: crates/mempal-core/src/project.rs
  Given 当前目录是 git repo name "foo"，config `[project] id = "bar"`
  When 调 `project::resolve(&config)`
  Then 返回 `"bar"`

Scenario: CLI --project 参数覆盖 config
  Test:
    Filter: test_cli_project_overrides_config
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given config `[project] id = "bar"`
  When 执行 `mempal ingest test.md --project baz`
  Then 新 drawer `project_id == "baz"`

Scenario: mempal project migrate 批量迁移既有 drawer
  Test:
    Filter: test_project_migrate_command
    Level: integration
    Targets: crates/mempal-cli/src/project_migrate.rs
  Given palace.db 含 20 条 `project_id IS NULL` 的 drawer（wing=code-memory）
  When 执行 `mempal project migrate --project proj-A --wing code-memory`
  Then 该 wing 的 20 条 drawer `project_id == "proj-A"`
  And 其他 wing 的 drawer 不变
  And `drawer_vectors` 同步更新

Scenario: project migrate 用分批 UPDATE 不长时阻塞 ingest
  Test:
    Filter: test_project_migrate_batched_does_not_block_ingest
    Level: integration
    Targets: crates/mempal-cli/src/project_migrate.rs
  Given palace.db 含 **5000** 条 `project_id IS NULL` 的 drawer（wing=code-memory）
  And 后台开启一个轻量 writer 线程，每 50ms 往 wing=logs 写一条 drawer（与 migrate 目标 wing 不同，模拟并发 ingest）
  When 执行 `mempal project migrate --project proj-A --wing code-memory`
  Then 命令在合理时间内完成（例如 < 10s）
  And 后台 writer 期间的每次 ingest latency p99 < 200ms（未被长锁阻塞到秒级）
  And 命令结束时 wing=code-memory 全部 5000 条 `project_id == "proj-A"`
  And stdout 至少打印了 5 条 batch progress 行（5000 / 1000 批 = 5 批）

Scenario: project migrate 遇到繁忙写锁时 BEGIN IMMEDIATE fail-fast 并重试
  Test:
    Filter: test_project_migrate_begin_immediate_fails_fast
    Level: integration
    Targets: crates/mempal-cli/src/project_migrate.rs
  Given 另一进程持有写锁（模拟长事务）1 秒
  When 执行 `mempal project migrate --project proj-A --wing code-memory`
  Then 第一批 `BEGIN IMMEDIATE` 立即（< 100ms）返回 `SQLITE_BUSY`
  And migrate 输出 `"batch busy, retrying in 500ms"` 类提示
  And 持锁进程释放后 migrate 成功完成
  And 没有死锁、没有线程悬挂

Scenario: mempal status 显示 project breakdown
  Test:
    Filter: test_status_shows_project_breakdown
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given drawer 分布 `{proj-A: 10, proj-B: 5, NULL: 3}`
  When 运行 `mempal status`
  Then stdout 含 `drawers per project:` 行
  And 含 `proj-A=10`、`proj-B=5`、`NULL=3`

Scenario: mempal_peek_partner 不应用 project filter
  Test:
    Filter: test_peek_partner_unaffected_by_project_filter
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given `strict_project_isolation = true`
  When 调 `mempal_peek_partner({partner: "codex"})`
  Then 返回结果不受 project_id 过滤影响（跨项目 peek 是其本义）

# Fork-ext P8 Implementation Plan — Config Hot-Reload + Queue + Privacy + Embed + Hook

> **For agentic workers:** follow the Task checklist order. Every Task ends with green tests before the next begins. Spec cross-reference lines use "§" to point at exact Decisions in the source spec.

**Goal**: 落地 fork-ext P8 的 5 个 spec，顺序执行（每个 spec 一个 feature branch + PR），总估时 8d。

**Baseline Commit**: `8d82aff` (main after PR #11 merge)

**Source Specs** (all under `specs/fork-ext/`):

| # | Spec | 估时 | 上游依赖 | 本 plan 阶段 |
|---|------|------|----------|-------------|
| 1 | `p8-config-hot-reload.spec.md` | 1d | 无 | Phase 1（基础设施） |
| 2 | `p8-pending-message-store.spec.md` | 2d | config-hot-reload | Phase 2 |
| 3 | `p8-privacy-scrubbing.spec.md` | 1d | config-hot-reload | Phase 3 |
| 4 | `p8-embed-qwen3-backend.spec.md` | 2d | config-hot-reload + queue | Phase 4 |
| 5 | `p8-hook-passive-capture.spec.md` | 2d | queue + privacy + embed | Phase 5 |

---

## D0. Upstream-Sync 摩擦最小化约束（写代码前必读）

**来源**：gemini-cli 2026-04-20 tier-3-complex 策略审查（session `01KPP36SKKNQ5T3C5JEFR9VN61`）。用户目标：`claude-mem` 执行 `sync-upstream` 时尽可能少花 token，fork 与 upstream 共存**永久化**。

以下三条是 fork-ext 所有源码落点的**硬约束**，任何 PR 提审前自查：

- **R1 — `src/core/config.rs` 保持单文件**。不拆 `config/{mod,schema,hot_reload}.rs` 子目录。upstream 每加一个 `Config` 字段会落在 `config.rs`，子目录拆分让 "加字段" 放大成树重构级冲突。fork 的热重载逻辑单独放新文件 `src/core/hot_reload.rs`。
- **R2 — `src/core/db.rs` 只改一行**。所有 fork-ext DDL 常量 (`FORK_EXT_V*_SCHEMA_SQL`)、迁移数组、runner / reader / setter 全部落在**新文件** `src/core/db_fork_ext.rs`。`db.rs` 仅加 `mod db_fork_ext;` 和 `Database::open` 里一行 `db_fork_ext::apply_fork_ext_migrations(&conn)?;`。理由：`db.rs` 是 upstream 高频变更区，每次 schema 升级必改，fork 的大段 SQL 常量插在中间会让每次 merge 都爆冲突。
- **R3 — fork-only MCP handler 只追加到末尾**。`src/mcp/server.rs` / `src/mcp/tools.rs` 里新 handler / 新工具枚举项必须放在**文件/match arm 最底部**，禁插在既有 upstream handler 之间。upstream 新增工具通常加在自己顺序位，fork 放底部让 git 能自动 3-way-merge。

pre-flight 自查 checklist：
- [ ] `grep -rn "config/\(mod\|schema\|hot_reload\)\.rs" src/` 空 → R1 ✅
- [ ] `git diff main -- src/core/db.rs` 只含 `mod db_fork_ext;` 和一行 `apply_fork_ext_migrations` 调用 → R2 ✅
- [ ] fork 新增 MCP handler 全部 `git diff main -- src/mcp/` 显示在既有 handler 之后（尾部）→ R3 ✅

---

## Key Decisions（开工前已定）

### D1. 实际代码结构 ≠ spec 路径

**事实**：仓库是**单 crate + 子模块**布局，不是 spec 里写的 workspace。

```
src/
├── aaak/
├── api/
├── core/       # spec 里的 "crates/mempal-core/src/"
├── cowork/
├── embed/      # spec 里的 "crates/mempal-embed/src/"
├── factcheck/
├── ingest/     # spec 里的 "crates/mempal-ingest/src/"
├── mcp/        # spec 里的 "crates/mempal-mcp/src/"
└── search/
```

**路径映射规则**（每次读 spec 时代入）：

| Spec 写法 | 实际路径 |
|-----------|---------|
| `crates/mempal-core/src/config.rs` | `src/core/config.rs` |
| `crates/mempal-core/src/db/schema.rs` | `src/core/db.rs`（schema 在同文件） |
| `crates/mempal-ingest/src/pipeline.rs` | `src/ingest/mod.rs` |
| `crates/mempal-ingest/src/privacy.rs` | `src/ingest/privacy.rs`（新建） |
| `crates/mempal-ingest/src/gating/*` | `src/ingest/gating/*`（新建） |
| `crates/mempal-mcp/src/tools.rs` | `src/mcp/tools.rs` |
| `crates/mempal-mcp/src/server.rs` | `src/mcp/server.rs` |
| `crates/mempal-cli/src/main.rs` | `src/main.rs` |
| `crates/mempal-cli/src/hook.rs`（新建） | `src/hook.rs` 或 `src/hooks/mod.rs`（看分量） |
| `crates/mempal-cli/src/daemon.rs`（新建） | `src/daemon.rs` 或 `src/daemon/mod.rs` |
| `crates/mempal-embed/src/` | `src/embed/` |
| `tests/{name}.rs` | `tests/{name}.rs`（根目录 integration tests，不变） |

**不拆 workspace**：splitting to workspace 是独立 refactor（估时 1-2d 单独起 spec），不在 P8 范围。

### D2. Schema 版本冲突——采用 Fork Namespace 方案

**事实**（已 verify `src/core/db.rs`，2026-04-20）：
- fork main 和 upstream main 都在 `const CURRENT_SCHEMA_VERSION: u32 = 4;`（`src/core/db.rs` line 11）
- upstream 版本控制用的是 **SQLite 内置 `PRAGMA user_version`（单 u32 槽位）**，**没有 `schema_meta` 表**。`read_user_version` / `set_user_version` 位于 `db.rs` line 727/732
- 迁移链线性：V1（全量 schema）→ V2（`deleted_at`）→ V3（FTS5 + triggers）→ V4（`importance` 列）
- upstream `specs/p10-explicit-tunnels.spec.md` 计划 v5，`specs/p10-normalize-version.spec.md` 计划 v6——但**都是 draft 未 ship**
- fork-ext chain 计划 ext-v1(queue) → ext-v2(embed-qwen3) → ext-v3(gating) → ext-v4(novelty) → ext-v5(vector-iso)

**选定方案：独立版本号命名空间 + 新建 fork_ext_meta K-V 表**（其他方案见下方 "Rejected Alternatives"）：

```rust
// src/core/db.rs
const CURRENT_SCHEMA_VERSION: u32 = 4;        // upstream axis —— 不改，继续由 PRAGMA user_version 管
const FORK_EXT_SCHEMA_VERSION: u32 = 0;       // fork-ext axis，bootstrap 落在 fork_ext_meta
                                               // ext-v1=queue (pending_messages)
                                               // ext-v2=embed-qwen3 (reindex_progress)
                                               // ext-v3=gating (gating_audit)
                                               // ext-v4=novelty (merge_count col + novelty_audit + FTS5 trigger)
                                               // ext-v5=vector-isolation (drawers.project_id)
```

- 新建 `fork_ext_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)` K-V 表，作为 **fork-ext 轴的 bootstrap**
  - 读写 `fork_ext_version`：`SELECT value FROM fork_ext_meta WHERE key='fork_ext_version'`（缺失视为 0）
  - **`fork_ext_meta` 表本身不占用版本号**——`apply_fork_ext_migrations` 在遍历 `fork_ext_migrations()` 数组**之前**先幂等 `CREATE TABLE IF NOT EXISTS fork_ext_meta`。Phase 0 里数组为空，bootstrap 完 `fork_ext_version` 仍为 `0`。queue 的 `pending_messages` 才是真正的 **ext-v1**
  - 未来 fork 独有的元数据（比如 degraded 状态时间戳、config_version 持久化等）也能塞这张 K-V 表，无需 ALTER
- migration runner **两套独立函数**：
  - `apply_migrations(conn)` —— 既有，读写 `PRAGMA user_version`，upstream 链
  - `apply_fork_ext_migrations(conn)` —— 新增，读写 `fork_ext_meta`，fork-ext 链
  - `open()` 里先调 upstream，再调 fork-ext（或反之，顺序不重要——两者无交叉依赖，idempotent）
- Upstream 将来 ship v5/v6 时，fork sync 过来**零冲突**（独立 axis，`PRAGMA user_version` 完全不动）
- `drawers` / `drawer_vectors` / `triples` / `taxonomy` 等既有表由 upstream axis 管理；pending_messages / gating_audit / novelty_audit / fork_ext_meta 由 fork_ext axis 管理
  - 例外：`drawers.project_id` 是对 upstream 表的 `ALTER TABLE ADD COLUMN`——由 fork_ext ext-v4 执行，独立于 upstream 链（`ALTER ADD COLUMN` 天然向前兼容，不会阻挡 upstream 将来 v5/v6）

**Rejected Alternatives**：

- (A) 等 upstream 先 ship v5/v6 — fork 无限期阻塞，不可接受
- (B) 插 v5/v6 placeholder no-op migration — 当 upstream 真 ship v5/v6 时，已 passed-through 的 fork db 永远拿不到 upstream 的真实 DDL（除非再写 retrofit 脚本），复杂且脆弱
- (C) renumber fork chain 到 v9/v10/v11/v12 留出 upstream 空间 — upstream 未来的 v7/v8 还是可能再撞
- (D-naive) 复用 `PRAGMA user_version` 高低位（前 16 位 fork、后 16 位 upstream）— 脆弱、对 upstream 不透明，一旦 upstream 直接写 `PRAGMA user_version = 5` 就把 fork 位覆盖
- (E, 选定) 独立 axis + 新建 `fork_ext_meta` K-V 表 — 一次性根除整类冲突，infrastructure 成本 ~50 LoC，下一节 Phase 0 交付

### D3. 实现顺序：`config-hot-reload` 最先

**理由**：
- 当前 `src/core/config.rs` 只有 2.1KB，功能极简
- 后续每个 spec 都要往 `Config` 加字段
- 先把 `ArcSwap<Config>` + fs-watch + 请求级 snapshot 做好，后续 spec 只需声明 "字段属于 hot-reload / restart-required"
- 如果反过来先做 queue / embed，后期改 Config 会搅动每个已写好的模块

**后续顺序**：queue → privacy → embed-qwen3 → hook（严格按 spec 依赖图）。

### D4. 新依赖审计

本 plan 引入新的 workspace dependencies：

| Crate | 版本 | 用途 | 阶段 |
|-------|------|------|------|
| `arc-swap` | `1` | `ArcSwap<Config>` lock-free atomic replacement（当前 1.9.1） | 1 |
| `notify` | `8` | fs-watch for config.toml（当前 stable 8.2.0，9.x 仍 RC） | 1 |
| `blake3` | `1` | config_version hash（当前 1.8.4） | 1 |
| `daemonize` | `0.5` | fork + setsid + fd redirect（Unix only；Windows 分支 cfg-off） | 5 |
| `libc` | `0.2` | daemonize 用到 sigaction；既有 `flock` 使用路径需重新声明（目前通过 rusqlite 间接取） | 5 |

Cargo.toml 当前是**单 crate 结构**（`[dependencies]` 直接列，无 `[workspace.dependencies]`）。新依赖直接加到 `[dependencies]`。

**Resolution 验证**（`cargo add --dry-run arc-swap@1 notify@8 blake3@1 daemonize@0.5`，2026-04-20）：4 个 crate 与现有依赖（rusqlite 0.37、serde 1、tokio 1、reqwest 0.12、rmcp 1.3、model2vec-rs 0.1、sqlite-vec 0.1.9 等）无 version 冲突，MSRV 1.85 满足。

**Features 默认取值**：
- `notify = "8"` → 默认启用 `fsevent-sys` + `macos_fsevent`（macOS），Linux 走 `inotify` 无额外 feature；**不启 `serde`**（配置 parse 独立走 `toml`）；考虑是否启 `crossbeam-channel` 代替默认 `std::sync::mpsc`（留 T1.5 实现时决定，默认不启）
- `blake3 = "1"` → 默认启 `std`；**不启 `rayon`**（配置文件 < 10KB，并行 overhead 不值）
- `arc-swap = "1"` → 默认无 feature
- `daemonize = "0.5"` → 无 feature 开关

### D5. TDD 节奏 = 沿用 P5-P9 upstream

- **每 Task** 顺序：失败测试写 → 验证确实 fail → impl → 验证 pass → `cargo clippy --all-targets --all-features -- -D warnings` → `cargo fmt` → commit
- **每 Phase** 结束：集中跑一次 `cargo test --all-features`，open PR，等 `/gemini review` 或本地 `csa review --branch` 通过再 merge
- **不做** spec-wide 一次性 impl 再跑全量测试——太痛

### D6. 分支策略

- `feat/fork-ext/foundation-schema-axis` → Phase 0
- `feat/fork-ext/p8-config-hot-reload` → Phase 1 → merge → 下一条
- 1（Phase 0）+ 5（Phase 1-5 specs）= 6 个 PR，顺序线性 merge
- **不开 epic 分支**（epic branch 会让 reviewer 看不到增量，且 schema 迁移在 epic 里容易出乱）

---

## Phase 0 — Fork-ext 版本轴 Foundation（0.5d）

**Source**: 本 plan D2（不对应单独 spec——纯 infrastructure）
**Branch**: `feat/fork-ext/foundation-schema-axis`
**Rationale**: 先把 fork-ext 迁移轴的脚手架搭好，Phase 2 起每个 spec 只需往 `fork_ext_migrations()` 数组塞一行 + 一段 SQL，不再碰 infrastructure。单独开 PR 让 reviewer 看到纯净的版本轴变更，与功能变更解耦。

### File Structure

> **Upstream-sync 摩擦约束**（gemini 2026-04-20 策略审查 R2）：fork-ext DDL 常量 / 迁移数组 / runner / reader / setter 全部落在**新文件 `src/core/db_fork_ext.rs`**，不内联到 `src/core/db.rs`。理由：`db.rs` 是 upstream 高频变更区（每次 schema 升级都要改），把 fork 的 `FORK_EXT_V*_SCHEMA_SQL` 大段 SQL 常量插在中间会让每次 merge 都炸。`db.rs` 只做一件事：`mod db_fork_ext;` + `Database::open` 里调 `apply_fork_ext_migrations` 一行。

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/core/db.rs` | modify（极小） | 顶部加 `mod db_fork_ext;`；`Database::open` 在 upstream `apply_migrations` 之后调 `db_fork_ext::apply_fork_ext_migrations(&conn)?;`——**仅此两行改动** |
| `src/core/db_fork_ext.rs` (new) | create | `FORK_EXT_SCHEMA_VERSION` 常量、`Migration` struct、`fork_ext_migrations()` 返回数组（Phase 0 为空）、`read_fork_ext_version`、`set_fork_ext_version`、`apply_fork_ext_migrations`、`FORK_EXT_META_DDL`（`CREATE TABLE IF NOT EXISTS fork_ext_meta ...`）|
| `src/core/mod.rs` | unchanged | `db_fork_ext` 是 `db` 的子模块，不用 re-export |
| `tests/fork_ext_schema_axis.rs` (new) | create | 3 scenarios：fresh DB → ext_v=0，bootstrap → ext_v=0，独立于 user_version |

### Tasks

- [ ] **T0.1** 写失败测试 `test_fork_ext_version_is_zero_on_fresh_db`（fresh DB → `apply_fork_ext_migrations` 跑完 → `fork_ext_version=0`，因为 Phase 0 只建表不注册业务 migration）
- [ ] **T0.2** 在**新文件** `src/core/db_fork_ext.rs` 实现 `fork_ext_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)` 表创建 + `read_fork_ext_version` / `set_fork_ext_version` helpers（表不存在时视为 0，便于幂等首次 bootstrap）
- [ ] **T0.3** 在 `db_fork_ext.rs` 实现 `apply_fork_ext_migrations(conn)`：建 `fork_ext_meta`（IF NOT EXISTS）→ 读 `fork_ext_version` → 遍历 `fork_ext_migrations()` 数组跑未应用的 → 写回 `fork_ext_version`；Phase 0 里数组是**空的**，`fork_ext_version` 保持 0
- [ ] **T0.4** 在 `src/core/db.rs` 顶部加 `mod db_fork_ext;`；`Database::open` 在 upstream `apply_migrations` 之后一行调用 `db_fork_ext::apply_fork_ext_migrations(&conn)?;`
- [ ] **T0.5** 写测试 `test_apply_fork_ext_migrations_idempotent`（调两次，`fork_ext_meta` 不重复建、`fork_ext_version` 不错误升）
- [ ] **T0.6** 写测试 `test_upstream_user_version_unchanged_after_fork_ext_bootstrap`（`PRAGMA user_version` 仍 = 4）
- [ ] **T0.7** `cargo clippy --all-targets --all-features -- -D warnings` + `cargo fmt`
- [ ] **T0.8** commit + push + PR：`feat(fork-ext): introduce independent schema version axis`

**Done-when**：
- `fork_ext_meta` 表存在，fresh DB + 既有 ingest/search tests 全绿（upstream 行为零回归）
- `SELECT value FROM fork_ext_meta WHERE key='fork_ext_version'` → `'0'`（Phase 0 内无业务 migration）
- `PRAGMA user_version` 仍 = 4
- Phase 2 起每个 spec 在 `fork_ext_migrations()` 数组追加一行 `Migration { version, sql }` 即完成 schema 部分，不再动 runner / reader / setter

**此后 Phase 2-5 的版本号映射**：
- Phase 2 `p8-queue`：**ext_v1**（pending_messages 表）
- Phase 3 `p8-privacy`：无 schema 变更
- Phase 4 `p8-embed-qwen3`：**ext_v2**（reindex_progress 表——支持 `mempal reindex --resume`）
- Phase 5 `p8-hook`：无 schema 变更
- 未来 `p9-gating`：ext_v3；`p9-novelty`：ext_v4；`p10-vector-iso`：ext_v5
- fork-ext spec 已同步改写：原 `schema_version == "7"/"8"/"9"/"10"` 断言全部改为 `fork_ext_version == "1"/"2"/"3"/"4"/"5"`（见 `p8-pending-message-store.spec.md` / `p8-embed-qwen3-backend.spec.md` / `p9-judge-gating-local.spec.md` / `p9-novelty-filter.spec.md` / `p10-project-vector-isolation.spec.md`），spec 和 plan / impl 三者一致

---

## Phase 1 — `p8-config-hot-reload`（1d）

**Source**: `specs/fork-ext/p8-config-hot-reload.spec.md`（10 scenarios）
**Branch**: `feat/fork-ext/p8-config-hot-reload`

### File Structure

> **Upstream-sync 摩擦约束**（gemini 2026-04-20 策略审查 R1）：**不拆** `src/core/config.rs` 成 `config/{mod,schema,hot_reload}.rs` 子目录。理由：upstream 未来每加一个 `Config` 字段都会落在 `config.rs`，子目录拆分会把 "加字段" 放大成 "删除旧文件 + 创建新文件" 级的树结构冲突。保留单文件 + 独立 `hot_reload.rs` 是冲突最小的结构。

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/core/config.rs` | modify | **保持单文件**；`Config` struct 在此 + 字段属性 `#[hot_reload]` / `#[restart_required]` marker（宏或 attribute 常量表）；新增 `pub struct ConfigHandle` 对外 API |
| `src/core/hot_reload.rs` (new) | create | `ArcSwap<Config>` + `notify::Watcher` + 250ms debounce 任务 + blake3 hash + fallback poll；不包含 `Config` schema 定义 |
| `src/core/mod.rs` | modify | `pub mod hot_reload;`（`config` 已存在） |
| `src/main.rs` | modify | 启动时 `ConfigHandle::bootstrap(path)`；`mempal status` 打印 `config: version=... loaded=...` |
| `src/mcp/tools.rs` | modify | `StatusResponse` 加 `config_version: String`、`config_loaded_at_unix_ms: u64` |
| `src/mcp/server.rs` | modify | 每个 handler 入口 `let cfg = ConfigHandle::current();`；新 handler 追加**末尾**，禁插在既有 upstream handler 之间（R3） |
| `Cargo.toml` | modify | 加 `arc-swap`, `notify`, `blake3` |
| `tests/config_hot_reload.rs` (new) | create | 10 scenarios |

### Tasks

- [ ] **T1.1** 加依赖 + `cargo check`（`arc-swap` / `notify` / `blake3`）
- [ ] **T1.2** 在 `src/core/config.rs` 单文件内扩充 `Config` struct + `ConfigHandle` 公开 API（**不拆目录**——R1 约束），新建 `src/core/hot_reload.rs` 空骨架；`cargo test` 全绿
- [ ] **T1.3** 写失败测试 `test_privacy_pattern_hot_reload_applies_on_next_ingest`（scenario §Completion Criteria 第 1 条）
- [ ] **T1.4** 实现 `ArcSwap<Config>` + `ConfigHandle::current()`，T1.3 转绿
- [ ] **T1.5** 实现 `notify::RecommendedWatcher` + 250ms debounce task
- [ ] **T1.6** 实现 blake3 hash + `config_version` + `loaded_at`
- [ ] **T1.7** 实现 parse-fail keep-previous（scenario §"parse 失败时保留上一版"）
- [ ] **T1.8** 实现 restart-required blacklist（scenario §"embedder backend 变更触发 restart-required warning"）
- [ ] **T1.9** 实现 MCP `mempal_status` 暴露 `config_version` + `loaded_at`
- [ ] **T1.10** 实现 CLI `mempal status` 打印 config 行
- [ ] **T1.11** 实现 `enabled=false` 完全不启 watcher（scenario §"enabled=false 时完全不启动 watcher"）
- [ ] **T1.12** 实现 notify-crash fallback poll（scenario §"notify watcher 死掉后 fallback poll"）
- [ ] **T1.13** 跑全部 10 scenarios `cargo test --test config_hot_reload`
- [ ] **T1.14** `cargo clippy -- -D warnings` + `cargo fmt` + commit + push + PR

**Done when**: `tests/config_hot_reload.rs` 10 条全绿、`cargo test --all-features` 其他测试无回归、`mempal status` CLI 输出新 config 行。

---

## Phase 2 — `p8-pending-message-store`（2d）

**Source**: `specs/fork-ext/p8-pending-message-store.spec.md`
**Branch**: `feat/fork-ext/p8-pending-message-store`
**Depends**: Phase 1 merged

### File Structure

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/core/queue.rs` (new) | create | `PendingMessageStore` + `enqueue` / `claim_next` / `confirm` / `mark_failed` / `refresh_heartbeat` / `reclaim_stale` |
| `src/core/db_fork_ext.rs` | modify | 定义 `FORK_EXT_V1_SCHEMA_SQL` 常量（`pending_messages` 表 + indexes DDL）；往 `fork_ext_migrations()` 数组追加 `Migration { version: 1, sql: FORK_EXT_V1_SCHEMA_SQL }`——`fork_ext_meta` 表 / runner / reader / setter 已由 Phase 0 就位，T2 这里**不再碰 infra**；**`src/core/db.rs` 不变**（R2 约束） |
| `src/core/mod.rs` | modify | `pub mod queue;` |
| `Cargo.toml` | modify | 无新 dep（rusqlite 已有） |
| `tests/queue_claim_confirm.rs` (new) | create | scenarios from spec §Completion Criteria |
| `tests/queue_heartbeat.rs` (new) | create | heartbeat + reclaim_stale scenarios |

### Tasks

- [ ] **T2.1** 在 `src/core/db_fork_ext.rs` 定义 `FORK_EXT_V1_SCHEMA_SQL` 常量（pending_messages DDL），往 `fork_ext_migrations()` 数组追加 `Migration { version: 1, sql: FORK_EXT_V1_SCHEMA_SQL }`——runner / reader / setter 已由 Phase 0 就位，本任务**不碰 `fork_ext_meta` 表结构**，**也不碰 `src/core/db.rs`**（R2）
- [ ] **T2.2** 写失败测试 `test_enqueue_claim_confirm_basic`
- [ ] **T2.3** 写 v0→v1 migration（CREATE TABLE pending_messages + indexes）
- [ ] **T2.4** 实现 `PendingMessageStore::enqueue` / `claim_next` / `confirm` / `mark_failed`
- [ ] **T2.5** 实现 `refresh_heartbeat` + `reclaim_stale`（heartbeat 条件而非 claimed_at 条件）
- [ ] **T2.6** 实现指数退避 retry（`retry_backoff_ms` 字段）
- [ ] **T2.7** integration test：并发 claim winner-takes-all
- [ ] **T2.8** integration test：crash 模拟（kill -9 claim holder → reclaim_stale 回收）
- [ ] **T2.9** clippy / fmt / PR

**Done when**: `tests/queue_*` 全绿；`mempal status` 显示 `fork_ext_version=1`；db 中 `pending_messages` 表存在。

---

## Phase 3 — `p8-privacy-scrubbing`（1d）

**Source**: `specs/fork-ext/p8-privacy-scrubbing.spec.md`
**Branch**: `feat/fork-ext/p8-privacy-scrubbing`
**Depends**: Phase 1 merged

### File Structure

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/ingest/privacy.rs` (new) | create | `scrub(text, cfg) -> (String, ScrubStats)` + 默认 pattern 库 |
| `src/ingest/mod.rs` | modify | pipeline 里 normalize 之后、chunk 之前调 privacy::scrub |
| `src/core/config.rs` | modify | 加 `PrivacyConfig` struct（`#[hot_reload]`） |
| `Cargo.toml` | modify | 加 `regex = "1"`（若 baseline 未依赖；如已在 sha2 / jieba 依赖链中可传递，verify first） |
| `tests/privacy_scrubbing.rs` (new) | create | 9 scenarios from spec |

### Tasks

- [ ] **T3.1** verify `regex` crate is available（`cargo tree | grep regex`）；如无则加到 `Cargo.toml`
- [ ] **T3.2** 写失败测试 `test_privacy_disabled_preserves_content_byte_identical`
- [ ] **T3.3** 实现 `PrivacyConfig` 在 `src/core/config.rs`（单文件，R1）+ 标记 hot-reload
- [ ] **T3.4** 实现 `privacy::scrub` + 默认 pattern 库
- [ ] **T3.5** 挂到 ingest pipeline（**关键顺序**：normalize → scrub → chunk）
- [ ] **T3.6** 跑新增 `test_scrub_catches_cross_chunk_secret`（CSA R1 新 scenario）确认 pre-chunk 时机正确
- [ ] **T3.7** 走完 9 scenarios
- [ ] **T3.8** clippy / fmt / PR

**Done when**: 9 scenarios 全绿；ingest 管道 privacy disabled 时 byte-identical；enabled 时跨 chunk 边界 secret 被 scrub。

---

## Phase 4 — `p8-embed-qwen3-backend`（2d）

**Source**: `specs/fork-ext/p8-embed-qwen3-backend.spec.md`
**Branch**: `feat/fork-ext/p8-embed-qwen3-backend`
**Depends**: Phase 1 + 2 merged（queue for heartbeat protocol）

### File Structure

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/embed/openai_compat.rs` (new) | create | `OpenAiCompatibleEmbedder` impl `Embedder` trait |
| `src/embed/retry.rs` (new) | create | 2s 固定间隔 retry loop + heartbeat callback |
| `src/embed/alerting.rs` (new) | create | 阈值告警 + 脚本执行（热重载脚本路径） |
| `src/embed/mod.rs` | modify | 默认改为 `OpenAiCompatibleEmbedder`；`model2vec-rs` 保留为 offline fallback |
| `src/core/config.rs` | modify | 加 `EmbedderConfig`（restart-required）+ `AlertingConfig`（hot-reload）；**R1 单文件约束** |
| `src/core/db_fork_ext.rs` | modify | 定义 `FORK_EXT_V2_SCHEMA_SQL`（`reindex_progress` 表 DDL）；往 `fork_ext_migrations()` 追加 `Migration { version: 2, sql: FORK_EXT_V2_SCHEMA_SQL }`；**`src/core/db.rs` 不变**（R2） |
| `src/core/reindex.rs` (new) | create | `ReindexProgressStore` CRUD（resume checkpoint） |
| `src/main.rs` | modify | `mempal reindex --embedder <name> [--resume]` 子命令 |
| `tests/openai_compat_embedder.rs` (new) | create | scenarios |
| `tests/embedder_retry_heartbeat.rs` (new) | create | 重试 + heartbeat 协议测试 |
| `tests/reindex_progress.rs` (new) | create | reindex `--resume` 断点恢复 scenario |

### Tasks

- [ ] **T4.1** 加 `reqwest` client 适配（已在 deps）
- [ ] **T4.2** 写失败测试 `test_openai_compat_embed_happy_path`（mock server）
- [ ] **T4.3** 实现 `OpenAiCompatibleEmbedder`（`/v1/embeddings` POST + `Qwen/Qwen3-Embedding-8B` model name）
- [ ] **T4.4** 在 `src/core/db_fork_ext.rs` 定义 `FORK_EXT_V2_SCHEMA_SQL` + `fork_ext_migrations()` 追加 ext-v2 entry（`reindex_progress` 表 DDL）；写 migration 测试；**不碰 `db.rs`**（R2）
- [ ] **T4.5** 实现 2s 固定重试 + 每轮调 heartbeat callback（从 queue store 注入）
- [ ] **T4.6** 实现 degraded 状态 + MCP `system_warnings` 注入
- [ ] **T4.7** 实现告警阈值 + 脚本执行 + 路径热重载（消费 Phase 1 机制）
- [ ] **T4.8** 实现 `mempal reindex --embedder <name> [--resume]` 全库 re-embed；checkpoint 写 `reindex_progress`
- [ ] **T4.9** integration test：LAN 不可达时 fallback model2vec
- [ ] **T4.10** integration test：切后端前后 `drawer_vectors` dim 不一致检测
- [ ] **T4.11** integration test：reindex 20/50 时 SIGINT → `--resume` 从 20 继续；最终 `fork_ext_version == "2"`
- [ ] **T4.12** clippy / fmt / PR

**Done when**: 默认走 `OpenAiCompatibleEmbedder`；LAN 不可用时 fallback model2vec；`mempal reindex --resume` 可用；fork-ext axis 升到 ext_v2。

---

## Phase 5 — `p8-hook-passive-capture`（2d）

**Source**: `specs/fork-ext/p8-hook-passive-capture.spec.md`
**Branch**: `feat/fork-ext/p8-hook-passive-capture`
**Depends**: Phase 1 + 2 + 3 + 4 merged

### File Structure

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/hook.rs` (new) | create | `mempal hook <event>` 子命令（stdin → queue enqueue） |
| `src/daemon.rs` (new) | create | `mempal daemon` worker loop |
| `src/daemon_bootstrap.rs` (new) | create | `DaemonContext::bootstrap()`（daemonize → runtime → db → tracing） |
| `src/hook_install.rs` (new) | create | `mempal hook install --target <claude-code\|gemini-cli\|codex>` |
| `src/main.rs` | modify | 顶层**禁用** `#[tokio::main]`；手动 `block_on` per-handler |
| `src/core/config.rs` | modify | `HooksConfig` + `HooksSessionEndConfig` |
| `Cargo.toml` | modify | 加 `daemonize = "0.5"`；`libc`, `nix` verify 已在 deps |
| `tests/hook_enqueue.rs` (new) | create | hook payload envelope + enqueue scenarios |
| `tests/daemon_lifecycle.rs` (new) | create | start/stop/SIGTERM/reclaim_stale/DaemonContext 启动序检查 |
| `tests/hook_install.rs` (new) | create | settings.json merge scenarios |

### Tasks

- [ ] **T5.1** 移除 `src/main.rs` 顶层的 `#[tokio::main]`，改手动 `block_on`（**关键前置**，否则 daemonize 无法工作）—— 这步单独 commit，跑一遍 `cargo test --all-features` 确保无回归
- [ ] **T5.2** 写失败测试 `test_hook_post_tool_enqueues_to_queue`
- [ ] **T5.3** 实现 `mempal hook <event>` stdin 读 + envelope-wrap（>10MB）
- [ ] **T5.4** 写失败测试 `test_daemon_context_bootstrap_ordering`（R2 新 scenario）
- [ ] **T5.5** 实现 `DaemonContext::bootstrap()`——严格 daemonize → runtime → db → tracing → fd 重定向
- [ ] **T5.6** 实现 `mempal daemon` worker loop + handler 映射
- [ ] **T5.7** 实现 truncated envelope → marker drawer path（不走重试）
- [ ] **T5.8** 实现 privacy scrub 对 envelope preview 生效（验证跨 spec 集成）
- [ ] **T5.9** 实现 `mempal hook install --target claude-code`——**关键三点**（gemini 2026-04-20 Part 1 + Part 3 #3）：
  - **本地优先**：若 `<cwd>/.claude/settings.json` 存在（无论是 regular file 还是 symlink），写入本地；否则 fallback 写 `~/.claude/settings.json`（原设计）。理由：upstream `215b62f` 已将本地 `.claude/settings.json` 作为 tracked blob commit 到仓库，本地优先会直接被 Claude Code pick up 且不被 global shadow
  - **symlink 感知**：若目标是 symlink，`canonicalize()` 到真实文件再读写。尊重用户习惯（用户常用 `git config core.excludesfile ~/.gitignore_noai` + `.claude/settings.json` 软链到独立 tracked 仓库）
  - **append-to-array**：`hooks.UserPromptSubmit` / `hooks.PostToolUse` 等是数组，**push** fork 的 entry，**不覆盖** upstream 的 `mempal cowork-drain` entry
- [ ] **T5.10** 实现 install --dry-run / uninstall（可选，stretch）
- [ ] **T5.11** integration test：daemon crash → reclaim_stale
- [ ] **T5.12** integration test：SIGTERM 优雅退出
- [ ] **T5.13** integration test `test_hook_install_respects_project_local_settings`：在 repo 里有 `.claude/settings.json`（tracked）时，install 写本地不写全局
- [ ] **T5.13b** integration test `test_hook_install_follows_symlink_target`：`.claude/settings.json` 是 symlink 到 `drafts/claude-settings.json`，install 编辑目标文件不破坏 symlink
- [ ] **T5.13c** integration test `test_hook_install_coexists_with_upstream_cowork_entry`：已含 upstream `user-prompt-submit.sh` 的 UserPromptSubmit 数组，install 后既有 entry 保留 + fork entry 追加
- [ ] **T5.14** clippy / fmt / PR

**Done when**: `mempal hook hook_post_tool < payload.json` enqueue；`mempal daemon --foreground` 消费并写 drawer；`mempal hook install --target claude-code` 注入不覆盖。

---

## Pre-Flight Facts（开工前最后核对）

> 开工前对照这些事实。任一条和当前源码不符就**立即停下**。

**`src/core/db.rs`** (baseline `8d82aff`)：
- line 11: `const CURRENT_SCHEMA_VERSION: u32 = 4;`
- **没有 `schema_meta` 表**（已 verify `src/core/db.rs`）：upstream 轴只用 SQLite 内置 `PRAGMA user_version`；fork-ext 轴由 Phase 0 新建的 `fork_ext_meta(key, value)` K-V 表承担
- v1-v4 migration SQL 以 `const V<N>_SCHEMA_SQL: &str = r#"..."#;` 形式存在——fork_ext 链用独立常量 `const FORK_EXT_V<N>_SCHEMA_SQL: &str`（Phase 0 先落 runner，Phase 2 起引入 `FORK_EXT_V1_SCHEMA_SQL`）

**`src/main.rs`** (baseline `8d82aff`)：
- line 30+：顶层用 `#[tokio::main]` 宏—— Phase 5 T5.1 先把它改掉，单独 commit，防止 Phase 1-4 的 hot-reload watcher / embedder retry 也被这个架构约束
- `enum Commands` 当前含 `Init` / `Ingest` / `Search` / `WakeUp` / `Compress` / `Bench` / `Delete` / `Purge` / `Reindex` / `Kg` / `Tunnels` / `Taxonomy` / `CoworkDrain` / `CoworkStatus` / `CoworkInstallHooks` / `FactCheck` ——Phase 1/2/4/5 各自追加子命令

**`src/core/config.rs`** (baseline `8d82aff`)：
- 极简，只 2.1KB—— Phase 1 T1.2 扩为目录模式时无现有字段冲突

**`src/ingest/mod.rs`** (baseline)：
- `ingest_file_with_options` 已含 P9 `lock_wait_ms`；Phase 3 privacy scrub 插在 `normalize` 后 + `chunk` 前，要新增一步 `scrub_pipeline`，不动 lock/dedup 顺序

**`Cargo.toml`** (baseline)：
- 单 crate，无 workspace 节
- 已有：`reqwest`（rustls-tls）/ `rusqlite 0.37`（bundled）/ `tokio 1`（full）/ `sha2` / `thiserror 2` / `anyhow 1` / `clap 4` / `serde` / `serde_json`
- 新增清单见 D4

---

## Post-P8 预告（不在本 plan 实施范围，记档）

P8 完成后下一阶段是 fork-ext P9：`p9-judge-gating-local`（ext_v3）→ `p9-novelty-filter`（ext_v4）→ `p9-progressive-disclosure`（**不 bump**，纯 MCP / 响应侧，无 schema 变更）→ `p9-session-self-review`（**不 bump**，复用 drawer 表末尾 sentinel）。

P10 阶段：`p10-project-vector-isolation`（ext_v5）→ `p10-cli-dashboard`（**不 bump**，只读 CLI）。

合计 fork-ext chain：Phase 0 bootstrap（ext_v0，runner + 空 migration array） → queue（ext_v1） → embed-qwen3（ext_v2，reindex_progress） → gating（ext_v3） → novelty（ext_v4） → vector-isolation（ext_v5）。与 D2 映射表、fork-ext spec 断言三者一致。

---

## References

- All specs: `specs/fork-ext/p8-*.spec.md`
- CSA debate session that validated this chain: `01KPNVMWSCD6HSGCWVRXCKSNMY`（2026-04-20 tier-4-critical）
- Related memory: `~/.claude/projects/-home-obj-project-github-RyderFreeman4Logos-mempal/memory/`
- Upstream baseline: upstream/main @ `215b62f`（同样 schema v4）

**问题 / 阻塞点就地 surface**，不绕过 spec。

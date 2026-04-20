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

**事实**：
- fork main 和 upstream main 当前都在 `CURRENT_SCHEMA_VERSION = 4`
- upstream `specs/p10-explicit-tunnels.spec.md` 计划 v5，`specs/p10-normalize-version.spec.md` 计划 v6——但**都是 draft 未 ship**
- fork-ext chain 计划 v7(queue) → v8(gating) → v9(novelty) → v10(vector-iso)

**选定方案：独立版本号命名空间**（其他方案见下方 "Rejected Alternatives"）：

```rust
// src/core/db.rs
const UPSTREAM_SCHEMA_VERSION: u32 = 4;       // upstream's axis
const FORK_EXT_SCHEMA_VERSION: u32 = 0;       // fork-ext's axis, starts at 0 (= no fork migrations applied)
                                               // P8-queue bumps to 1; P9-gating to 2; P9-novelty to 3; P10-vector-iso to 4
```

- `schema_meta` 表加 `fork_ext_version INTEGER NOT NULL DEFAULT 0` 列（在一个 v4→v5 无副作用的 upstream 兼容 migration 里）
  - 或更干脆：复用既有 `schema_meta.key='fork_ext_version'` K-V 存（mempal 现有结构允许）
- migration runner 分两条链：upstream 链走 `schema_version`，fork-ext 链走 `fork_ext_version`；两者独立 idempotent 升
- Upstream 将来 ship v5 `explicit-tunnels`、v6 `normalize-version` 时，fork sync 过来**无冲突**（独立 axis）
- 当前 `drawers` / `drawer_vectors` 等既有表由 upstream axis 管理；pending_messages / gating_audit / novelty_audit / `drawers.project_id` 列由 fork_ext axis 管理
  - 例外：`drawers.project_id` 是对 upstream 表的列添加——这一条归 fork_ext 链，加列时 `ALTER TABLE drawers ADD COLUMN project_id TEXT` 独立于 upstream 链

**Rejected Alternatives**：

- (A) 等 upstream 先 ship v5/v6 — fork 无限期阻塞，不可接受
- (B) 插 v5/v6 placeholder no-op migration — 当 upstream 真 ship v5/v6 时，已 passed-through 的 fork db 永远拿不到 upstream 的真实 DDL（除非再写 retrofit 脚本），复杂且脆弱
- (C) renumber fork chain 到 v9/v10/v11/v12 留出 upstream 空间 — upstream 未来的 v7/v8 还是可能再撞
- (D, 选定) 独立 axis — 一次性根除整类冲突，infrastructure 成本 ~40 LoC

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
| `arc-swap` | `1.7` | `ArcSwap<Config>` lock-free atomic replacement | 1 |
| `notify` | `6` | fs-watch for config.toml | 1 |
| `blake3` | `1.5` | config_version hash | 1 |
| `daemonize` | `0.5` | fork + setsid + fd redirect（Unix only，Windows fallback no-op） | 5 |
| `libc` | `0.2` | flock 已用（p9-ingest-lock），daemonize 用到 sigaction | 5（已间接依赖） |

Cargo.toml 当前是**单 crate 结构**（`[dependencies]` 直接列，无 `[workspace.dependencies]`）。新依赖直接加到 `[dependencies]`。

### D5. TDD 节奏 = 沿用 P5-P9 upstream

- **每 Task** 顺序：失败测试写 → 验证确实 fail → impl → 验证 pass → `cargo clippy --all-targets --all-features -- -D warnings` → `cargo fmt` → commit
- **每 Phase** 结束：集中跑一次 `cargo test --all-features`，open PR，等 `/gemini review` 或本地 `csa review --branch` 通过再 merge
- **不做** spec-wide 一次性 impl 再跑全量测试——太痛

### D6. 分支策略

- `feat/fork-ext/p8-config-hot-reload` → PR → merge → 下一条
- 5 个 spec 5 个 PR，顺序线性 merge
- **不开 epic 分支**（epic branch 会让 reviewer 看不到增量，且 schema 迁移在 epic 里容易出乱）

---

## Phase 1 — `p8-config-hot-reload`（1d）

**Source**: `specs/fork-ext/p8-config-hot-reload.spec.md`（10 scenarios）
**Branch**: `feat/fork-ext/p8-config-hot-reload`

### File Structure

| 文件 | 动作 | 职责 |
|------|------|------|
| `src/core/config.rs` | **重写 + 拆模块** | 从 2.1KB 扩充，拆为 `config/{mod,schema,hot_reload}.rs` |
| `src/core/config/mod.rs` (new) | create | `pub use` + `ConfigHandle` 公开 API |
| `src/core/config/schema.rs` (new) | create | `Config` struct + `serde::Deserialize` + 字段属性 `#[hot_reload]` / `#[restart_required]` |
| `src/core/config/hot_reload.rs` (new) | create | `ArcSwap<Config>` + `notify::Watcher` + debounce 任务 + blake3 hash + fallback poll |
| `src/core/mod.rs` | modify | `pub mod config;`（目录模式） |
| `src/main.rs` | modify | 启动时 `ConfigHandle::bootstrap(path)`；`mempal status` 打印 `config: version=... loaded=...` |
| `src/mcp/tools.rs` | modify | `StatusResponse` 加 `config_version: String`、`config_loaded_at_unix_ms: u64` |
| `src/mcp/server.rs` | modify | 每个 handler 入口 `let cfg = ConfigHandle::current();` |
| `Cargo.toml` | modify | 加 `arc-swap`, `notify`, `blake3` |
| `tests/config_hot_reload.rs` (new) | create | 10 scenarios |

### Tasks

- [ ] **T1.1** 加依赖 + `cargo check`（`arc-swap` / `notify` / `blake3`）
- [ ] **T1.2** 拆 `src/core/config.rs` → `config/{mod,schema,hot_reload}.rs`（先**不动功能**，纯 move），`cargo test` 全绿
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
| `src/core/db.rs` | modify | `FORK_EXT_SCHEMA_VERSION` 常量 + `schema_meta.fork_ext_version` 列 + v0→v1 migration DDL（`pending_messages` 表 + indexes） |
| `src/core/mod.rs` | modify | `pub mod queue;` |
| `Cargo.toml` | modify | 无新 dep（rusqlite 已有） |
| `tests/queue_claim_confirm.rs` (new) | create | scenarios from spec §Completion Criteria |
| `tests/queue_heartbeat.rs` (new) | create | heartbeat + reclaim_stale scenarios |

### Tasks

- [ ] **T2.1** 在 `db.rs` 加 fork_ext axis infra（`FORK_EXT_SCHEMA_VERSION: u32 = 0` → `1`, `fork_ext_version` K-V row in `schema_meta`, `run_fork_ext_migrations()` entry）
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
| `src/core/config/schema.rs` | modify | 加 `PrivacyConfig` struct（`#[hot_reload]`） |
| `Cargo.toml` | modify | 加 `regex = "1"`（若 baseline 未依赖；如已在 sha2 / jieba 依赖链中可传递，verify first） |
| `tests/privacy_scrubbing.rs` (new) | create | 9 scenarios from spec |

### Tasks

- [ ] **T3.1** verify `regex` crate is available（`cargo tree | grep regex`）；如无则加到 `Cargo.toml`
- [ ] **T3.2** 写失败测试 `test_privacy_disabled_preserves_content_byte_identical`
- [ ] **T3.3** 实现 `PrivacyConfig` 在 `config/schema.rs` + 标记 hot-reload
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
| `src/core/config/schema.rs` | modify | 加 `EmbedderConfig`（restart-required）+ `AlertingConfig`（hot-reload） |
| `src/main.rs` | modify | `mempal reindex --embedder <name>` 子命令 |
| `tests/openai_compat_embedder.rs` (new) | create | scenarios |
| `tests/embedder_retry_heartbeat.rs` (new) | create | 重试 + heartbeat 协议测试 |

### Tasks

- [ ] **T4.1** 加 `reqwest` client 适配（已在 deps）
- [ ] **T4.2** 写失败测试 `test_openai_compat_embed_happy_path`（mock server）
- [ ] **T4.3** 实现 `OpenAiCompatibleEmbedder`（`/v1/embeddings` POST + `Qwen/Qwen3-Embedding-8B` model name）
- [ ] **T4.4** 实现 2s 固定重试 + 每轮调 heartbeat callback（从 queue store 注入）
- [ ] **T4.5** 实现 degraded 状态 + MCP `system_warnings` 注入
- [ ] **T4.6** 实现告警阈值 + 脚本执行 + 路径热重载（消费 Phase 1 机制）
- [ ] **T4.7** 实现 `mempal reindex --embedder <name>` 全库 re-embed
- [ ] **T4.8** integration test：LAN 不可达时 fallback model2vec
- [ ] **T4.9** integration test：切后端前后 `drawer_vectors` dim 不一致检测
- [ ] **T4.10** clippy / fmt / PR

**Done when**: 默认走 `OpenAiCompatibleEmbedder`；LAN 不可用时 fallback model2vec；`mempal reindex` 可用。

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
| `src/core/config/schema.rs` | modify | `HooksConfig` + `HooksSessionEndConfig` |
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
- [ ] **T5.9** 实现 `mempal hook install --target claude-code`（**关键**：append-to-array，**不**覆盖 upstream cowork hook）
- [ ] **T5.10** 实现 install --dry-run / uninstall（可选，stretch）
- [ ] **T5.11** integration test：daemon crash → reclaim_stale
- [ ] **T5.12** integration test：SIGTERM 优雅退出
- [ ] **T5.13** integration test：hook install 合并 existing settings 且与 `mempal cowork-install-hooks` 共存
- [ ] **T5.14** clippy / fmt / PR

**Done when**: `mempal hook hook_post_tool < payload.json` enqueue；`mempal daemon --foreground` 消费并写 drawer；`mempal hook install --target claude-code` 注入不覆盖。

---

## Pre-Flight Facts（开工前最后核对）

> 开工前对照这些事实。任一条和当前源码不符就**立即停下**。

**`src/core/db.rs`** (baseline `8d82aff`)：
- line 11: `const CURRENT_SCHEMA_VERSION: u32 = 4;`
- `schema_meta` 表存在（K-V store for version + misc metadata）—— **D2 方案用这个存 `fork_ext_version`**，无需改表结构
- v1-v4 migration SQL 以 `const V<N>_SCHEMA_SQL: &str = r#"..."#;` 形式存在——fork_ext 链用独立常量 `const FORK_EXT_V1_SCHEMA_SQL: &str`

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

P8 完成后下一阶段是 fork-ext P9：`p9-judge-gating-local` → `p9-novelty-filter` → `p9-progressive-disclosure` → `p9-session-self-review`。schema_version 走 fork_ext axis v1 → v2 (gating) → v3 (novelty) → v4 (progressive-disclosure 不 bump) → v4 (session-self-review 不 bump，复用 drawer 表末尾 sentinel)。

P10 阶段：`p10-project-vector-isolation`（fork_ext v5） → `p10-cli-dashboard`（不 bump）。

---

## References

- All specs: `specs/fork-ext/p8-*.spec.md`
- CSA debate session that validated this chain: `01KPNVMWSCD6HSGCWVRXCKSNMY`（2026-04-20 tier-4-critical）
- Related memory: `~/.claude/projects/-home-obj-project-github-RyderFreeman4Logos-mempal/memory/`
- Upstream baseline: upstream/main @ `215b62f`（同样 schema v4）

**问题 / 阻塞点就地 surface**，不绕过 spec。

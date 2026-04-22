spec: task
name: "P11: SessionStart semantic priming (`mempal prime` command + agent hook glue)"
tags: [feature, cli, hooks, priming, claude-code-parity]
estimate: 1d
---

## Intent

复现 claude-mem 最有感的 UX：**session 开始时自动喂一段"你过去在这个 repo 做过什么"的高价值摘要**，让 agent 不用主动调 `mempal_search` 就带着项目上下文开工。

本 spec 提供 `mempal prime` 命令 —— agent hook 脚本（`p11-integrations-layer` 铺的路径）调用它拿到 formatted priming 文本，通过各 agent 的 SessionStart 注入协议送给模型。

**动机**：
- claude-mem SessionStart 注入是观测到最显著的产品感差距；没这个切过去会明显退步（用户亲口认证）
- Mempal 当前已有 `mempal_status` 能返回协议 + counts，但无 "recent/important drawer narrative" 渲染；`mempal tail/timeline` 是 CLI 操作员视角，不是 agent 摘要视角
- Prime 要 embedder-free：embedder degraded 时 SessionStart 仍能 fire，不阻塞 agent 启动

**关键决策**：priming **不走向量检索**，纯 SQL `ORDER BY importance DESC, added_at DESC`。理由：(a) 无 query 上下文（session 刚开，不知道用户要做什么），(b) 必须 degraded-safe。

## Decisions

- 新 CLI 子命令 `mempal prime`：
  - 默认无 args：按 CWD 推导 `project_id`（复用 p10-project-vector-isolation 的 infer 逻辑），按默认 budget 输出 priming 文本
  - `--project-id <id>`: override CWD 推导
  - `--format {text|json}`：默认 text（Claude Code / Codex 直接注入），json 用于 CSA 或未来 machine-consumer
  - `--token-budget <n>`: 输出内容 token 上限（默认 2048，最小 512，最大 8192）
  - `--include-stats`: 追加统计块（默认 on），`--no-stats` 关
  - `--since <duration>`: 只考虑最近一段时间的 drawer（默认 `30d`；`all` 表示不限）
- 输出 text 格式（三块，固定顺序）：
  1. **Legend header**：单行，说明 icon / importance stars 语义
  2. **Timeline block**：`<added_at> <importance_stars> <wing>/<room> <id> — <preview[120]>` 逐行，最多 N 条（受 token budget 控制）
  3. **Stats block** (opt)：drawer 总数 / 最近 7d ingest 数 / top 3 wings + counts / embedder status（`healthy` 或 `degraded`）
- 排序策略：`ORDER BY importance DESC, added_at DESC`，不分页
- Token budgeting：
  - 用 `tiktoken-rs` 或轻量估算器（4 chars ≈ 1 token）
  - 超限时按 importance 降序截断；保证至少 1 条 entry + stats block 能 fit
- 空 DB / 无 drawer：输出空字符串 + exit 0（让 SessionStart hook 静默不干扰）
- DB 不存在：输出空字符串 + exit 0 + stderr 一行 warning（hook 吞掉 stderr）
- `--format json` schema：
  ```json
  {
    "project_id": "...",
    "generated_at": "ISO8601",
    "legend": "...",
    "drawers": [{"id":"...", "added_at":"...", "importance_stars":N, "wing":"...", "room":"...", "preview":"..."}, ...],
    "stats": {"total":N, "recent_7d":N, "top_wings":[{"wing":"...","count":N}], "embedder_status":"healthy|degraded"},
    "budget_used_tokens": N,
    "truncated": bool
  }
  ```
- 实现在 `cli/prime.rs`（新 module）+ `src/core/priming.rs`（查询 + budgeting）
- 直连 `palace.db`（不走 MCP stdio，省 fork + IPC），通过 `Database::open(&db_path)` + 预编译 SQL
- 输出**必须**是纯文本/JSON，无 ANSI 颜色码（下游是 agent 模型而非 TTY）

## Boundaries

### Allowed
- `cli/prime.rs`（新 module）
- `main.rs`（注册 `prime` 子命令）
- `src/core/priming.rs`（查询 + budgeting）
- `src/core/lib.rs`（re-export priming mod）
- `Cargo.toml`（引入 `tiktoken-rs` 或 OK 用 4-char 估算）
- `tests/priming.rs`（新建集成测试）
- `tests/fixtures/priming/`（已知 drawer 集 + expected output）

### Forbidden
- 不 embedder call / 不向量检索（degraded-safe 硬约束）
- 不修改 drawer / 不写 audit（纯读操作）
- 不触发 session-self-review / auto-ingest（prime 不 side-effect）
- 不 touch MCP tool（此命令 CLI-only，MCP 暂不暴露 `mempal_prime`；未来可在独立 spec 加）
- 不读 hook settings / 不知道自己被谁调（prime 是无状态 CLI）
- 不做 "自动清理陈旧 drawer"（importance + recency 排序，不是 GC 入口）
- 不改 schema / 不 bump `fork_ext_version`
- 不依赖 p11-integrations-layer（prime 是数据产物，hook 集成是消费方；本 spec 可独立落地测试）

## Out of Scope

- hook 脚本的实际安装（`p11-integrations-layer` 负责）
- MCP 工具 `mempal_prime`（保留未来 spec）
- Query-aware priming（session 中途用户说了什么后再 prime —— 那是 `mempal_search`，不是 prime）
- 跨 session 增量 priming（每次全量输出；session context 是模型侧的事）
- Multi-project aggregated priming（当前 scope 死锁在 CWD 推导的单 project；cross-project 场景走 `--all-projects` 是未来需求）
- LLM 摘要改写（prime 只返回 drawer 片段，不二次生成文字）

## Completion Criteria

Scenario: 有 drawer 的 project, 默认输出三块
  Test:
    Filter: test_prime_default_output_has_three_blocks
    Level: integration
    Targets: cli/prime.rs
  Given CWD 是一个已知 project，`palace.db` 含 5 条 drawer, 混合 importance 1-5
  When 运行 `mempal prime` 无 args
  Then stdout 按顺序包含：legend 行、timeline 多行、stats 块
  And timeline 最前一行是 importance 最高的 drawer
  And exit 0

Scenario: 空 DB 输出空字符串 + exit 0
  Test:
    Filter: test_prime_empty_db_silent
    Level: integration
    Targets: cli/prime.rs
  Given `palace.db` 存在但无 drawer
  When 运行 `mempal prime`
  Then stdout 为空
  And stderr 为空
  And exit 0

Scenario: DB 不存在也 exit 0
  Test:
    Filter: test_prime_missing_db_exits_zero
    Level: integration
    Targets: cli/prime.rs
  Given `~/.mempal/palace.db` 不存在
  When 运行 `mempal prime`
  Then stdout 为空
  And stderr 含 "mempal: palace.db not found; skipping priming" 单行
  And exit 0

Scenario: `--format json` 输出合法 JSON
  Test:
    Filter: test_prime_json_format_valid_schema
    Level: integration
    Targets: cli/prime.rs
  Given 非空 DB
  When 运行 `mempal prime --format json`
  Then stdout 是合法 JSON
  And 顶层 keys 包含 `project_id, generated_at, legend, drawers, stats, budget_used_tokens, truncated`
  And `drawers[].preview` 长度 <= 120 UTF-8 chars

Scenario: Token budget 超限时按 importance 降序截断
  Test:
    Filter: test_prime_token_budget_truncates_by_importance
    Level: integration
    Targets: src/core/priming.rs
  Given 50 条 drawer, 含 5 颗星 + 4 颗星 + 3 颗星各若干
  And 总 preview tokens 远超 512
  When 运行 `mempal prime --token-budget 512 --format json`
  Then 返回 `drawers` 全是 >= 4 星（3 星被截断）
  And `truncated: true`
  And `budget_used_tokens <= 512`

Scenario: Degraded embedder 不影响 prime
  Test:
    Filter: test_prime_runs_when_embedder_degraded
    Level: integration
    Targets: cli/prime.rs
  Given `palace.db` 有 drawer
  And `global_embed_status()` 处于 `degraded`
  When 运行 `mempal prime`
  Then stdout 正常输出 timeline + stats
  And `stats.embedder_status == "degraded"`（text 或 json 对等信号）
  And exit 0

Scenario: `--project-id` override CWD 推导
  Test:
    Filter: test_prime_project_id_overrides_cwd
    Level: integration
    Targets: cli/prime.rs
  Given CWD 推导得到 project `foo`
  And DB 里 `foo` 有 1 条 drawer, `bar` 有 3 条
  When 运行 `mempal prime --project-id bar`
  Then timeline 只含 `bar` 的 drawer
  And drawer count == 3

Scenario: `--since 7d` 过滤窗口
  Test:
    Filter: test_prime_since_filter
    Level: integration
    Targets: src/core/priming.rs
  Given 10 条 drawer, 5 条在 7 天内, 5 条在 30 天前
  When 运行 `mempal prime --since 7d`
  Then timeline 只含最近 7 天的 5 条
  And `stats.recent_7d == 5`

Scenario: 输出不含 ANSI 颜色码
  Test:
    Filter: test_prime_output_no_ansi_escapes
    Level: integration
    Targets: cli/prime.rs
  Given 任意 DB 状态
  When 运行 `mempal prime`（即使在 TTY 环境下）
  Then stdout 不含 `\x1b[` 开头的 ANSI escape 序列

stdout 不含 `\x1b[` 开头的 ANSI escape 序列


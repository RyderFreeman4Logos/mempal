spec: task
name: "P10: CLI observability subcommands — tail, timeline, stats, view, audit (replaces Web UI)"
tags: [feature, cli, observability, audit]
estimate: 1.5d
---

## Intent

给 mempal 增加一组**纯 CLI 子命令**替代原 Issue #5 提议的 Web UI：让人类用户在终端内审计记忆系统状态（新增 drawer 流、时间线、统计面板、单 drawer 内容、去重/gating 审计日志）。

5 个新子命令：
1. `mempal tail [--follow] [--wing <W>] [--room <R>] [--since <dur>]` — 按时间倒序打印最近 drawer（follow 模式持续刷新）
2. `mempal timeline [--wing <W>] [--since <dur>] [--format text|json]` — 按时间线展开 drawer 摘要视图
3. `mempal stats [--verbose]` — 一屏面板：总 drawer / 按 wing 分组 / 向量维度 / 队列 stats / gating 命中 / novelty 去重 / FTS 大小 / 最近 24h 增长
4. `mempal view <drawer_id> [--raw]` — 打印单 drawer 完整信息，默认带 AAAK signal 高亮；`--raw` 纯原文
5. `mempal audit [--kind novelty|gating] [--since <dur>]` — 查 gating_audit、novelty_audit 命中历史（privacy 累计汇总见 `mempal stats`，不作为 `--kind`——Decisions L54-58 解释了数据源缺失的理由）

**动机**：claude-mem `src/ui/viewer/` 的 React + SSE Dashboard 对人类审计 auto-capture 效果极好，但硬依赖 HTTP Server + 前端构建链。mempal 坚守纯 CLI 单二进制哲学，用子命令覆盖相同审计场景——零 HTTP 端口、零前端资产、零构建链膨胀。

**v3 判决依据**：用户 2026-04-16 明确："web ui可以改成子命令, 在命令行查状态(降低开发成本)"（`feedback_cli_over_web_ui.md`）。工作量从 Web UI 的 3d 降到 1.5d，且维护成本接近零。

## Decisions

- 所有新子命令都是 `mempal <verb>`，注册在 `crates/mempal-cli/src/main.rs` 的 `clap` 派生结构
- 共用一个 `mempal-observability` 子模块：`crates/mempal-cli/src/observability/{tail,timeline,stats,view,audit}.rs`
- **不引入 TUI 框架**（no ratatui / crossterm）——纯打印足够。--follow 模式用 `tokio::time::interval` + ANSI 光标控制简单刷屏
- **不引入任何 HTTP Server**（no axum / hyper / warp）——没有 bind 端口
- 输出默认 ANSI-colored text；`--format json` 切 JSON lines（ndjson）；`--no-color` 禁色
- `mempal tail`：
  - 默认展示最近 20 条（按 `created_at DESC`）
  - `--follow` 每 2s 轮询 `SELECT ... WHERE created_at > last_seen_ts`，append 新行
  - 每行格式 `<timestamp> <flag>  <wing>/<room>  <drawer_id[:8]>  <preview(120 chars)>`
  - `--wing` / `--room` / `--since "7d"` 精细过滤
  - SIGINT 优雅退出
- `mempal timeline`：
  - 类似 `tail` 但不 follow，扫指定时间窗内所有 drawer
  - 分组显示按天（`=== 2026-04-16 ===`），drawer 按 importance 降序 + created_at 升序
  - `--format json` 输出 `{timestamp, drawer_id, wing, room, importance_stars, flags, preview}` 数组
- `mempal stats`：
  - 顶部显示 schema_version + db 大小
  - 主体：drawers total / per-wing count / per-room count / avg importance
  - `pending_messages` queue stats（pending / claimed / failed / oldest_pending_age）
  - `gating_audit` 7d stats（tier1_kept/skipped/tier2_kept/skipped）
  - `novelty_audit` 7d stats（inserted / merged / dropped）
  - privacy scrub stats（按 pattern 7d 命中）
  - FTS5 index 大小
  - 向量 dim + 数量
  - `--verbose` 加时间序列（7d 逐日 ingest rate）
- `mempal view <drawer_id>`：
  - header 区：`drawer_id` / `wing/room` / `created_at` / `updated_at` / `merge_count` / `importance_stars` / `flags` / `entities` / `topics` / `source_file`
  - body 区：`content`（默认 UTF-8 pretty print，AAAK signal 词汇高亮彩显；`--raw` 不高亮不截断）
  - 底部 hint：`linked_drawer_ids`（若 content 中含 session_metadata sentinel 段）
  - `--json` 输出结构化 JSON
- `mempal audit`：
  - `--kind novelty` 读 `novelty_audit` 表最近 `--since` 时长（数据源：P9 novelty filter 产生的 per-event 审计表）
  - `--kind gating` 读 `gating_audit`（数据源：P9 gating 产生的 per-event 审计表）
  - **不**提供 `--kind privacy`：P8 `p8-privacy-scrubbing.spec.md` 仅存累计 `ScrubStats`（per-pattern 计数）且显式把独立审计表列为 out-of-scope（L61-64）；P10 又禁止改 schema。两者相互锁定使时间窗 privacy 审计无合法数据源。若未来需要 privacy 时间窗审计，需独立 spec（如 `p11-privacy-event-log`）扩 schema 加 `privacy_scrub_events` 表
  - 默认 kind = all，两类分段输出（novelty + gating）
  - privacy 命中的累计汇总通过 `mempal stats` 输出（见 L45 `privacy scrub stats（按 pattern 7d 命中）`）
- `mempal_peek_partner` MCP 工具保留，但 CLI `mempal peek` 子命令以 human-readable 方式展示（简单 print，避免 agent 独占）——**本 spec 不新增 peek 子命令**，仅列出方向，由未来 spec 承担
- 所有 CLI 子命令只读 palace.db（`SELECT` only），绝不写入
- 子命令若 palace.db 不存在 → 友好错误："run mempal init first"
- `mempal tail --follow` 默认用 `inotify` / `fsevents` 监听 db 文件变化（更高效）；不可用时 fallback poll（2s interval）
- **不**启动 daemon / service；每次子命令独立进程、独立退出

## Boundaries

### Allowed
- `crates/mempal-cli/src/observability/` 子目录（新建）
- `crates/mempal-cli/src/main.rs`（注册 5 个新子命令）
- `crates/mempal-cli/Cargo.toml`（可选 `notify` crate for fs watch，workspace 已有则复用）
- `crates/mempal-search/src/preview.rs`（P9 spec 已新建，此处复用 `truncate`）
- `tests/cli_tail.rs`、`tests/cli_stats.rs`、`tests/cli_view.rs`（新建）

### Forbidden
- 不要引入 axum / warp / hyper / actix-web 或任何 HTTP server
- 不要引入 ratatui / crossterm（纯 print + ANSI escape 即可）
- 不要新增 MCP 工具（本 spec 是 CLI-only）
- 不要修改 db schema
- 不要让任何子命令写 db（只读！）
- 不要 bundle HTML/JS 资源
- 不要启动 socket / bind 端口
- 不要依赖 `mempal daemon` 运行（子命令独立）
- 不要给子命令加 agent auth（localhost CLI 不需要）
- 不要在 `tail --follow` 内用 `while true { sleep }` 无 backoff（用 `tokio::time::interval` + fs watch）
- 不要输出绝对路径到 stdout（可能泄漏 HOME）——用 `~` 替换

## Out of Scope

- TUI 交互（ratatui）——留给未来 `mempal tui` spec 需要时再加
- Web UI / HTTP Dashboard（用户明确拒绝，`feedback_cli_over_web_ui.md`）
- Remote monitoring（用户自己 SSH 上去跑）
- KG 三元组可视化（如有需求用 `mempal kg query` MCP 输出，不做 ASCII graph）
- 统计数据持久化到时间序列 db（Prometheus / InfluxDB）
- 跨 palace 聚合（单 db）
- 多用户权限（localhost 无此需求）
- 交互式 REPL（`mempal shell`）

## Completion Criteria

Scenario: mempal tail 默认打印最近 20 条 drawer
  Test:
    Filter: test_tail_default_prints_recent_20
    Level: integration
    Targets: crates/mempal-cli/src/observability/tail.rs
  Given palace.db 中 50 条 drawer，按 created_at 分布
  When 运行 `mempal tail`
  Then stdout 含 20 行 drawer 记录
  And 第一行是最新（按 created_at DESC）
  And 每行含 timestamp / wing/room / drawer_id[:8] / preview

Scenario: mempal tail --wing 过滤
  Test:
    Filter: test_tail_wing_filter
    Level: integration
    Targets: crates/mempal-cli/src/observability/tail.rs
  Given drawer 分布在 wing={A,B,C} 各 10 条
  When 运行 `mempal tail --wing A`
  Then stdout 只含 wing=A 的记录
  And 行数 <= 10

Scenario: mempal tail --follow 响应新增 drawer
  Test:
    Filter: test_tail_follow_sees_new_drawers
    Level: integration
    Targets: crates/mempal-cli/src/observability/tail.rs
  Given `mempal tail --follow` 启动（后台）
  When 主进程 `mempal_ingest` 新增一条 drawer
  Then follow 进程 5s 内 stdout 新增一行对应该 drawer
  And follow 进程仍在运行

Scenario: mempal timeline 按天分组
  Test:
    Filter: test_timeline_groups_by_day
    Level: integration
    Targets: crates/mempal-cli/src/observability/timeline.rs
  Given drawer 跨 3 天存在
  When 运行 `mempal timeline --since 7d`
  Then stdout 含 3 个 `=== <YYYY-MM-DD> ===` 日期分组标题
  And 每组下列出当日 drawer

Scenario: mempal stats 显示全量统计
  Test:
    Filter: test_stats_shows_all_sections
    Level: integration
    Targets: crates/mempal-cli/src/observability/stats.rs
  Given palace.db 含 drawers / pending_messages / gating_audit / novelty_audit 数据
  When 运行 `mempal stats`
  Then stdout 含 "schema_version" 行
  And 含 "drawers total" 行
  And 含 "queue" section
  And 含 "gating" section
  And 含 "novelty" section
  And 含 "privacy scrub" section

Scenario: mempal view <id> 完整打印单 drawer
  Test:
    Filter: test_view_prints_full_drawer
    Level: integration
    Targets: crates/mempal-cli/src/observability/view.rs
  Given palace.db 含 drawer A content "Decision: use Arc<Mutex<>>"
  When 运行 `mempal view <A.id>`
  Then stdout 含 A.id
  And stdout 含 A.wing / A.room
  And stdout 含 "Decision: use Arc<Mutex<>>"
  And stdout 含 flags（含 "DECISION"）

Scenario: mempal view --raw 不高亮不截断
  Test:
    Filter: test_view_raw_is_verbatim
    Level: integration
    Targets: crates/mempal-cli/src/observability/view.rs
  Given drawer A content 2000 字符
  When 运行 `mempal view <A.id> --raw`
  Then stdout body 部分 byte-level 含完整 2000 字符
  And 无 ANSI color escape

Scenario: mempal audit --kind novelty 列出最近决策
  Test:
    Filter: test_audit_novelty_lists_decisions
    Level: integration
    Targets: crates/mempal-cli/src/observability/audit.rs
  Given `novelty_audit` 表含 3 drop / 2 merge / 5 insert
  When 运行 `mempal audit --kind novelty --since 7d`
  Then stdout 行数 == 10
  And 含 "drop" "merge" "insert" 各对应关键字

Scenario: 子命令对只读 db 不写
  Test:
    Filter: test_observability_subcommands_readonly
    Level: integration
    Targets: crates/mempal-cli/src/observability/*
  Given palace.db 被以只读 (`-R`) 打开或文件权限 0444
  When 依次运行 `tail` / `timeline` / `stats` / `view <id>` / `audit`
  Then 所有命令退出码 == 0
  And palace.db 的 mtime 未变化

Scenario: 无 HTTP 端口被 bind
  Test:
    Filter: test_no_http_port_bound
    Level: integration
    Targets: crates/mempal-cli/src/observability/*
  Given 任一 observability 子命令运行
  When 扫描进程的 TCP LISTEN sockets（`ss -ltnp` 或等价）
  Then 该进程无任何 LISTEN socket

Scenario: palace.db 不存在时友好错误
  Test:
    Filter: test_missing_palace_db_friendly_error
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/observability/*
  Given `~/.mempal/palace.db` 不存在
  When 运行 `mempal tail`
  Then stderr 含 "run mempal init first" 或等价提示
  And 退出码 != 0
  And 不 panic

Scenario: --format json 产出合法 ndjson
  Test:
    Filter: test_timeline_json_format_is_valid_ndjson
    Level: integration
    Targets: crates/mempal-cli/src/observability/timeline.rs
  Given palace.db 含 5 drawer
  When 运行 `mempal timeline --format json`
  Then stdout 每行是独立合法 JSON object
  And 每 object 含 `{timestamp, drawer_id, wing, room, importance_stars}`

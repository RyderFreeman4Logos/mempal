spec: task
name: "P8: Hook-based passive capture — mempal hook subcommand + daemon + installers (default off)"
tags: [feature, hooks, capture, cli, daemon]
estimate: 2d
---

## Intent

给 mempal 增加一个**可选的、默认关闭**的 hook 桥接层：让 Claude Code / Gemini CLI / Codex 等终端的 lifecycle 事件（`SessionStart`, `UserPromptSubmit`, `PostToolUse`, `SessionEnd`）被**被动捕获**并写入 mempal——无需 agent 主动调 `mempal_ingest`。

三个子能力：
1. `mempal hook <event>` 子命令：读 stdin 上的 hook 事件 JSON，快速 `enqueue` 到 `pending_messages` 队列（亚毫秒级返回，不阻塞主 agent）
2. `mempal daemon` 子命令：后台 worker，从队列 `claim_next` 消息、走 ingest 管道（含 privacy scrub）、`confirm` 或 `mark_failed`
3. `mempal hook install --target <claude-code|gemini-cli|codex>`：自动把 hook 配置写入目标工具的 settings 文件

**Default off**：`[hooks] enabled = false`，保持 mempal 当前的"自愿合规"行为不变。

**v3 判决依据**：v2 分析确认 claude-mem `src/hooks/hook-response.ts:7-10` 返回 `{ continue: true, suppressOutput: true }` 做到亚毫秒级 hook 响应，mempal 吸收后有相同可靠性保证。

## Decisions

- 新建 `crates/mempal-hook/` crate，含 hook 解析 + enqueue 逻辑（小而独立，方便未来独立分发）
- 或者作为 `crates/mempal-cli` 的子模块 `src/hook.rs`——**本 spec 选后者**，理由：避免分裂二进制，维持 `cargo install mempal` 单入口；如果未来需要独立 `mempal-hook` 二进制再拆
- CLI 新增子命令：
  - `mempal hook <event-type>` — 从 stdin 读 JSON，enqueue 后退出（退出码永远 0 除非 db 不可达）
  - `mempal daemon [--foreground]` — 启动 worker；默认 detach（fork + setsid + 写 pid 到 `~/.mempal/daemon.pid`）
  - `mempal daemon stop` — 读 pid 文件发 SIGTERM
  - `mempal daemon status` — 看 pid 文件存活 + 读 `pending_messages` stats 报告
  - `mempal hook install --target <T> [--dry-run]` — 写对应工具的 settings 文件注入 hook 配置
- Hook payload 格式：透传 agent 发来的 raw JSON，不做 schema 约束（容错优先）；`kind` 字段映射为 queue 的 `kind` 列（`hook_session_start`, `hook_pre_tool`, `hook_post_tool`, `hook_user_prompt`, `hook_session_end`）
- `mempal hook` 执行路径：
  1. parse CLI args 得到 `event_type`
  2. `read_to_string(stdin)` 得 payload（最多 10MB，超过截断并记 warn）
  3. open 或 reuse db connection
  4. `PendingMessageStore::enqueue(event_kind, payload)`
  5. flush, exit 0
  6. 目标总耗时 < 50ms（naive SLO，不阻塞 agent）
- `mempal daemon` 执行循环：
  1. 初始化 `PendingMessageStore` + `reclaim_stale(claim_ttl_secs)` 回收崩溃 claim
  2. 循环 `claim_next("mempal-daemon", claim_ttl=120)`
  3. 无消息时 sleep 500ms（可配 `[hooks] daemon_poll_interval_ms`）
  4. 有消息时 dispatch 到 handler（按 `kind` 分发，详见下）
  5. handler 成功 → `confirm(id)`；失败（含 panic catch） → `mark_failed(id, error)`
  6. 接 SIGTERM 时停止 claim 新消息，等已 claim 的 handler 完成（最长 30s）后退出
- Handler 映射（P8 最小可行）：
  - `hook_post_tool` → 提取 `tool_name`、`input`、`output`、`exit_code`，组装 drawer（wing=`hooks-raw`，room=`<tool_name>`）经 `ingest::pipeline` 写入
  - `hook_user_prompt` → drawer（wing=`hooks-raw`，room=`user-prompt`）
  - `hook_session_start` / `hook_session_end` → drawer（wing=`hooks-raw`，room=`session-lifecycle`）
  - **不**处理 `hook_pre_tool`（仅 audit 无新信息，skip）
- 所有 handler 都走完整 ingest 管道，自动应用 P8 privacy scrub；gating（P9）未到时自动禁用
- `mempal hook install --target claude-code` 写入 `~/.claude/settings.json` 的 `hooks` key：
  ```json
  {
    "hooks": {
      "PostToolUse": [{ "hooks": [{ "type": "command", "command": "mempal hook hook_post_tool" }] }],
      "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "mempal hook hook_user_prompt" }] }]
    }
  }
  ```
  非侵入式合并：读既有 JSON，merge `hooks` key，保留其他配置；若已有相同命令则跳过。
- `--dry-run` 打印 diff 不写入
- 配置项在 `[hooks]`：`enabled`, `capture` (Vec<String>), `wing`, `daemon_poll_interval_ms`, `daemon_claim_ttl_secs`
- `enabled = false` 时 `mempal hook <event>` **仍然** enqueue（让用户可以先写 hook 配置，再翻开关）；`mempal daemon` 检查到 `enabled = false` 直接退出并打印 warning
- 所有 daemon 生命周期日志走 `tracing` 到 `~/.mempal/daemon.log`
- 进程 RAII：daemon 启动时写 pid 文件，接 SIGTERM/SIGINT 触发 `Drop` 清理 pid 文件
- 崩溃恢复：`reclaim_stale` 在启动时被调 + 周期性（每 60s）调用一次防御长 stuck claim

## Boundaries

### Allowed
- `crates/mempal-cli/src/hook.rs`（新建：`mempal hook <event>` 实现）
- `crates/mempal-cli/src/daemon.rs`（新建：`mempal daemon` worker loop）
- `crates/mempal-cli/src/hook_install.rs`（新建：各 target 的 settings 文件 patching）
- `crates/mempal-cli/src/main.rs`（注册新子命令）
- `crates/mempal-cli/Cargo.toml`（添加 `nix`/`libc` 仅用于 fork/setsid、signal 处理；`serde_json` 已有）
- `crates/mempal-core/src/config.rs`（`HooksConfig` struct + `[hooks]` parsing）
- `tests/hook_enqueue.rs`、`tests/daemon_lifecycle.rs`、`tests/hook_install.rs`（新建）

### Forbidden
- 不要把 hook event 直接写入 `drawers` 表——**必须**经过 `PendingMessageStore` 队列
- 不要用 `systemd` 集成或 launchd plist——daemon 由 `mempal daemon` 自管生命周期
- 不要让 `mempal hook <event>` 直接调 `ingest::pipeline::ingest`——会阻塞 agent
- 不要新增 HTTP / socket 端口给 hook——stdin 透传
- 不要改 `drawers` / `drawer_vectors` schema（新表已在 P8 queue spec 中独立 bump）
- 不要在 `mempal-mcp` / `mempal-api` / `mempal-search` 任何 crate 引用 `mempal-cli::hook` 模块
- 不要强制 hook 注入 auth token 或 HMAC 验证——目标是 localhost 工具链协同，非外网
- 不要覆盖用户已有的 `~/.claude/settings.json` 内容；只 merge `hooks` 子树
- 不要做 systemd timer / cron 自动启动 daemon——用户显式调 `mempal daemon`
- 不要在 `mempal hook <event>` 里输出 stdout（只能 stderr），避免干扰 agent 的 stdout 管道

## Out of Scope

- 多 daemon 竞争协作（只支持单机单 daemon）
- Windows 支持的 hook install（先 macOS / Linux）
- 以 plugin 形式注入 Claude Code 扩展（走 settings.json 足够）
- `mempal hook uninstall` 逆操作（P8 之后可加）
- Daemon 远程管理（SSH 过去直接跑即可）
- Hook 事件的 schema 验证（容错优先）
- 把 hook payload 签名（防伪造）——不做
- Daemon 与 hook 间的非 SQLite IPC（共享队列已足够）
- Gating / Novelty / Progressive 这些对 hook 产物的后续处理（独立 P9 spec）

## Completion Criteria

Scenario: mempal hook PostToolUse 正确 enqueue
  Test:
    Filter: test_hook_post_tool_enqueues_to_queue
    Level: integration
    Targets: crates/mempal-cli/src/hook.rs
  Given stdin 输入 `{"tool_name":"Bash","input":"ls","exit_code":0,"output":"..."}`
  When 执行 `mempal hook hook_post_tool`
  Then 命令退出码为 "0"
  And `pending_messages` 表新增 1 行，`kind == "hook_post_tool"`
  And 该行 `payload` 含 `"tool_name":"Bash"`
  And 该命令耗时 < 500ms（`time` 测量）

Scenario: hooks.enabled=false 时 daemon 直接退出
  Test:
    Filter: test_daemon_exits_when_disabled
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs
  Given `[hooks] enabled = false`
  When 运行 `mempal daemon --foreground`
  Then 进程 3 秒内退出且退出码非 0
  And stderr 含 "hooks not enabled" 或等价消息
  And `~/.mempal/daemon.pid` 不存在

Scenario: daemon 正确处理 hook_post_tool 生成 drawer
  Test:
    Filter: test_daemon_processes_hook_post_tool_to_drawer
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs
  Given `enabled = true`，队列中 1 条 `kind=hook_post_tool`，payload 含 Bash 工具输出
  When 启动 `mempal daemon --foreground` 并等 5s
  Then `drawers` 表新增 1 行，`wing == "hooks-raw"`，`room == "Bash"`
  And 该 drawer `content` 含 payload 的 tool output 文本
  And `pending_messages` 表该行不存在（已 confirm）

Scenario: daemon 崩溃后 reclaim_stale 恢复 claim
  Test:
    Filter: test_daemon_crash_reclaim_stale
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs, crates/mempal-core/src/queue.rs
  Given 队列 1 条消息被 daemon-A claim 后模拟崩溃（`kill -9 pid`），`claimed_at` 停留
  When 启动新 `mempal daemon` 并等 65s（超过 claim_ttl=60）
  Then 该消息被 daemon-B 重新 claim 并处理
  And 最终 `drawers` 表有对应 drawer，`pending_messages` 无该行

Scenario: mempal hook install --target claude-code 写入 settings.json
  Test:
    Filter: test_hook_install_writes_claude_code_settings
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hook_install.rs
  Given 临时 HOME 无 `.claude/settings.json`
  When 执行 `mempal hook install --target claude-code`
  Then `~/.claude/settings.json` 存在
  And 解析后 JSON 含 `hooks.PostToolUse` 数组
  And 数组中有一项 `command == "mempal hook hook_post_tool"`

Scenario: hook install 合并而非覆盖已有 settings
  Test:
    Filter: test_hook_install_merges_existing_settings
    Level: integration
    Test Double: tempfile_home_with_settings
    Targets: crates/mempal-cli/src/hook_install.rs
  Given `~/.claude/settings.json` 已有 `{ "theme": "dark", "hooks": { "Stop": [...] } }`
  When 执行 `mempal hook install --target claude-code`
  Then `~/.claude/settings.json` 依然含 `theme == "dark"`
  And `hooks.Stop` 未被动
  And `hooks.PostToolUse` 已新增

Scenario: hook install --dry-run 不写文件
  Test:
    Filter: test_hook_install_dry_run_does_not_write
    Level: integration
    Targets: crates/mempal-cli/src/hook_install.rs
  Given 临时 HOME 无 `.claude/settings.json`
  When 执行 `mempal hook install --target claude-code --dry-run`
  Then stdout 输出 diff 预览
  And `~/.claude/settings.json` 仍不存在

Scenario: daemon status 报告运行状态和队列 stats
  Test:
    Filter: test_daemon_status_reports_state_and_queue
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs
  Given `mempal daemon` 后台运行 + 队列含 2 pending
  When 运行 `mempal daemon status`
  Then stdout 含 `running=true`
  And stdout 含 `pending=2 claimed=0 failed=0`

Scenario: daemon SIGTERM 优雅退出
  Test:
    Filter: test_daemon_sigterm_graceful_shutdown
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs
  Given 队列有 5 pending 且 `mempal daemon` 正在处理
  When 发 SIGTERM 给 daemon 进程
  Then daemon 在 30 秒内退出
  And `~/.mempal/daemon.pid` 被清理
  And 无任何 pending_messages 卡在 `claimed` 状态（都 confirm 或回退到 pending）

Scenario: mempal hook 对 >10MB payload 截断并 warn
  Test:
    Filter: test_hook_truncates_oversized_payload
    Level: integration
    Targets: crates/mempal-cli/src/hook.rs
  Given stdin 输入 11MB JSON payload
  When 执行 `mempal hook hook_post_tool`
  Then 命令仍退出码 "0"
  And stderr 含 "payload truncated" 或等价 warning
  And 队列中该消息 payload 长度 <= 10MB + 元数据开销

Scenario: hook 子命令不污染 stdout
  Test:
    Filter: test_hook_writes_nothing_to_stdout
    Level: integration
    Targets: crates/mempal-cli/src/hook.rs
  Given 任意合法 hook payload
  When 执行 `mempal hook hook_post_tool` 并捕获 stdout + stderr
  Then stdout 长度 == 0
  And stderr 可含 info/warn 日志但不 panic

spec: task
name: "P8: Config hot-reload — fs watch + atomic swap + request-scoped snapshot (zero restart for most fields)"
tags: [feature, config, reload, observability]
estimate: 1d
---

## Intent

让修改 `~/.mempal/config.toml` 不需要重启 `mempal daemon` / MCP server / CLI 长进程。通过 **fs-watch（`notify` crate）+ 原子替换读取 + 请求级 `Arc<Config>` snapshot** 实现：

- 告警阈值、告警脚本路径、路由关键词、privacy regex、gating 规则、novelty 阈值、preview 截断长度、dashboard debounce、ingest 锁超时、strict_project_isolation、progressive_disclosure 等字段**实时生效**
- embedder 后端、SQLite path、MCP server 路径等**需重启**字段被显式 blacklist
- 解析失败不 crash，保留上一版 `Arc<Config>` 继续服务，stderr 打印警告
- `mempal_status` / `mempal status` 输出 `config_version`（blake3 hash）+ `loaded_at` 时间戳，agent 和人类都能看到当前生效版本

**动机**：当前 mempal 改任何一个字段（比如调 gating threshold 或加一条 privacy pattern）都要重启 daemon / 让 Claude Code 重拉 MCP 子进程，开发反馈循环被拖长到分钟级。热重载把这个降到秒级——保存 config.toml → 下一个 MCP 请求自动看到新配置。

**v3 判决依据**：用户 2026-04-20 明确要求"配置文件尽可能热重载，修改配置不需要重启服务 / claude code"。CSA debate 2026-04-20 tier-4-critical session 确认：`notify` crate 是唯一对 daemon / MCP stdio 两种运行模式都兼容的机制（SIGHUP 送不到 Claude Code spawn 的 stdio 子进程），`Arc<Config>` 请求级 snapshot 是 mid-flight 不一致的唯一正确解法。

**Default on**：`[config_hot_reload] enabled = true`，纯读文件变更，无副作用，默认开。用户可显式 `enabled = false` 回退到 "config 一加载到进程结束不变" 的旧语义。

## Decisions

- 新建 `crates/mempal-core/src/config/hot_reload.rs`（原 `config.rs` 按模块拆分：`config/mod.rs` + `config/schema.rs` + `config/hot_reload.rs`）
- 全局状态用 `Arc<ArcSwap<Config>>`（`arc-swap` crate，workspace-dep）包裹 Config，提供 `ConfigHandle::current() -> Arc<Config>` 获取当前快照
- 加载流程：
  1. 启动时 parse `config.toml` → `Config` → 存入 `ArcSwap::new(Arc::new(cfg))`
  2. spawn `notify::RecommendedWatcher` watch **config.toml 所在目录**（单文件 watch 被 editor rename/atomic-save 击穿，与 P10 dashboard 同理）+ 按文件名过滤 `config.toml`
  3. 收到 `Create` / `Modify` / `Rename` 事件 → **250ms debounce**（editor 写多段文件时合并）→ 重读文件 → parse
  4. parse 成功 → `ArcSwap::store(Arc::new(new_cfg))` 原子替换；旧 `Arc` 被读者释放后自动回收
  5. parse 失败 → 保留 `ArcSwap` 当前内容不变，stderr `warn!("config hot-reload: parse failed, keeping previous version: {err}")`
  6. `config_version` = blake3 hash of `config.toml` 字节（hex 前 12 位），每次成功加载都更新 `ConfigHandle.version`
  7. `loaded_at` = Unix epoch millis，每次成功加载更新
- 请求级 snapshot：
  - MCP server 每个 tool handler 在入口处 `let cfg = ConfigHandle::current();` 拿一个 `Arc<Config>` 快照，之后整个 request 用这个 cfg
  - `ingest::pipeline::ingest` 同理：入口拿 snapshot，整个 pipeline（privacy scrub → gating → novelty → store）共用
  - 这消除 mid-flight 不一致：一个 ingest 请求不会看到 "privacy.enabled=true 开始 scrub，中途被热重载成 false 跳过 scrub" 的断层
  - `cfg.clone()` 是廉价的（`Arc::clone`）
- 可热重载字段白名单（`#[hot_reload(allowed)]` attribute 或 runtime 检查 enum）：
  - `[alerting]` 全部（threshold / script_path / cooldown_secs）
  - `[privacy]` 全部（enabled / strip_tags / scrub_patterns）
  - `[taxonomy]`（wing/room 路由关键词）
  - `[ingest_gating.rules]`、`[ingest_gating.embedding_classifier.threshold]`、`[ingest_gating.embedding_classifier.prototypes]`（改 prototype 会触发 daemon "下一次重启时重新 embed"——参见 "受限热重载" 下方）
  - `[ingest_gating.novelty]` 全部（enabled / duplicate_threshold / merge_threshold / wing_scope / top_k_candidates / max_merges_per_drawer / max_content_bytes_per_drawer）
  - `[progressive_disclosure]` 全部（preview_bytes / enabled）
  - `[search] strict_project_isolation`
  - `[hooks.session_end]` 全部（extract_self_review / trailing_messages / min_length / wing）
  - `[hooks] daemon_poll_interval_ms` / `daemon_claim_ttl_secs`（下一轮循环生效）
  - `[cli_dashboard] tail_debounce_ms` / `tail_poll_fallback_secs`
  - `[pending_messages] claim_ttl_secs` / `reclaim_interval_secs` / `retry_backoff_ms`
  - `[project] id`（下一个请求生效；**不**触发 backfill UPDATE）
- 受限热重载字段（变更被接受但实际生效需 daemon 重启或显式命令）：
  - `[ingest_gating.embedding_classifier.prototypes]` 增删 prototype 后，daemon 下次重启才重新 pre-compute；hot-reload 只更新 `Arc<Config>` 内的 prototype 文本，但实际使用的 `prototype_vectors: Vec<Vec<f32>>` 需 daemon 重启；stderr `warn!("prototype change detected, effective after daemon restart")`
- 重启硬需字段 blacklist（热重载检测到变更 → stderr 报 `error!("field X requires restart, change ignored in current session")` 并保持旧值）：
  - `[embedder]` 全部（`backend`、`base_url`、`model`、`dim` 等）——切后端要 `mempal reindex`，不是热重载能解决
  - `[database] path`
  - `[mcp] server_name` / 子进程启动时 MCP SDK 已拿走的 ServerInfo
  - `[cli] binary_path` / 编译期决定的路径
- 可观测性：
  - `ConfigHandle` 暴露 `.version() -> String`, `.loaded_at() -> SystemTime`
  - `mempal_status` MCP 工具 response 多两个字段：`config_version: String`, `config_loaded_at_unix_ms: u64`
  - `mempal status` CLI 输出 `"config: version=a1b2c3d4e5f6 loaded=2026-04-20 12:34:56"`
  - daemon log 记录每次热重载成功 / 失败 / 忽略（restart-required 字段）事件
- 错误处理：
  - `ConfigError`（`thiserror`）区分 `ParseFailed` / `IoFailed` / `RestartRequired { field }`
  - hot_reload 回路**永不 panic**——任何 parse / io 错误都 catch 下来，回到上一版
  - `notify` crate 本身死掉（watcher thread panic）→ stderr warn + fallback 到每 5s poll `stat(config.toml)`（mtime + size 对比），保证降级可用
- 原子性：
  - 读侧用 `ArcSwap::load()` 原子读取（一次 load 返回一致的 Arc）
  - 写侧 `ArcSwap::store()` 原子替换
  - 文件侧：假设 editor 用 `rename(tmp → config.toml)` 原子保存（vim / emacs / 所有主流 editor 默认）；直接 open+truncate+write 的编辑方式会产生短暂 parse 失败，但我们的"保留上一版"策略会兜住
- 线程模型：
  - `notify::RecommendedWatcher` 在独立 tokio task（`tokio::task::spawn_blocking`，因为 notify 是同步 API）
  - 事件通过 `tokio::sync::mpsc::channel(64)` 送到 debounce task（tokio::task::spawn）
  - debounce task 调 `parse → ArcSwap::store`
  - 不需要任何 Mutex——`ArcSwap` 内部用 seqlock，读侧 lock-free
- 集成顺序依赖：
  - 本 spec 需要在 `p8-embed-qwen3-backend.spec.md` 引入的"告警脚本路径热重载"字段存在时落地——本 spec 把它泛化为通用机制，`p8-embed-qwen3-backend` 的告警路径是第一个消费者
  - `p9-judge-gating-local` / `p9-novelty-filter` / `p10-cli-dashboard` / `p10-project-vector-isolation` 的配置字段上线时直接声明"可热重载 / 需重启"属性，由本 spec 机制落实

## Boundaries

### Allowed
- `crates/mempal-core/src/config/hot_reload.rs`（新建）
- `crates/mempal-core/src/config/mod.rs`（拆分旧 `config.rs`）
- `crates/mempal-core/src/config/schema.rs`（拆分）
- `crates/mempal-core/Cargo.toml`（添加 `arc-swap` + `notify` + `blake3`——workspace dep 可复用）
- `crates/mempal-mcp/src/tools.rs`（`mempal_status` response schema 加 `config_version` / `config_loaded_at_unix_ms`）
- `crates/mempal-mcp/src/server.rs`（tool handler 入口拿 `ConfigHandle::current()`）
- `crates/mempal-ingest/src/pipeline.rs`（入口拿 snapshot）
- `crates/mempal-cli/src/main.rs`（`mempal status` 打印 config 行）
- `crates/mempal-cli/src/daemon.rs`（daemon 启动时初始化 ConfigHandle + watcher）
- `tests/config_hot_reload.rs`（新建）

### Forbidden
- 不要用 SIGHUP（对 Claude Code spawn 的 stdio MCP 子进程送不到；`notify` 覆盖所有模式）
- 不要用轮询作为主路径（`notify` 可用时延迟 < 500ms；poll 是 degraded fallback）
- 不要让 parse 失败导致 daemon / MCP server 退出——保留上一版
- 不要在 `ConfigHandle::current()` 内部加任何 blocking（热路径）
- 不要对 embedder backend 字段做 "partial hot-reload"（重建 Embedder trait object）——这破坏 dim 不变式，必须整进程重启 + `mempal reindex`
- 不要把热重载的"新配置"写回到 `ArcSwap` 之外的其他缓存（会产生多个真源）
- 不要为每个字段单独起 notify watcher（单一 watcher + 整 Config 替换）
- 不要在 parse 成功后立刻把 `Arc<Config>` 的字段 move 到 static——读者可能还持着旧 Arc
- 不要让 Config 有 `Deserialize` 之外的 side-effect（比如"加载后自动 `fs::create_dir_all`"）——加载纯函数
- 不要让 `mempal status` 为了读 config_version 去读文件（从 `ConfigHandle` 拿即可）
- 不要暴露 "force reload" MCP 工具让 agent 主动触发重载（violates principle of least surprise；fs-watch 自动触发，用户编辑文件就是意图信号）
- 不要修改 db schema

## Out of Scope

- Per-request config override（agent 在 MCP 请求里传"用这个 config 跑这次"——YAGNI，未来有需求再独立 spec）
- 配置版本历史 / rollback（ArcSwap 只保留当前版；用户自己 git 管理 config.toml 历史）
- 加密 config（凭证存 config.toml 已经是 "the config is in the user's home"——不是本项目责任）
- 配置 schema 版本迁移（字段加减直接 config.toml 字面编辑；工具辅助是未来 `mempal config migrate` 独立 spec）
- Web UI 编辑 config（纯 CLI / 编辑器）
- 向 agent 暴露 "reload failed" 错误（agent 无行动权；人类看 stderr / log）
- 热重载 `[logging]` 级别（大多数 tracing subscriber 初始化后不支持动态 level；独立 spec）
- multi-config 文件 merge（单 `~/.mempal/config.toml` 够用）
- `[embedder]` 的"下次重启生效"自动存储（变更被 warn 掉，用户自己重启）

## Completion Criteria

Scenario: 修改 privacy.scrub_patterns 后下一次 ingest 立即应用新 pattern
  Test:
    Filter: test_privacy_pattern_hot_reload_applies_on_next_ingest
    Level: integration
    Targets: crates/mempal-core/src/config/hot_reload.rs, crates/mempal-ingest/src/pipeline.rs
  Given `mempal daemon --foreground` 运行，config.toml 含 `openai_key` pattern，不含 `custom_token` pattern
  When 执行一次 ingest（含 custom_token 字串 `"CT-1234567890abcdef"`），verify content 原样（未被 scrub）
  And 随后编辑 config.toml append 一条 `custom_token` pattern（正则 `CT-[a-f0-9]{16}`）并保存（atomic rename）
  And 等待 1s（debounce + parse）
  And 执行第二次 ingest 同样内容
  Then 第一次 drawer.content 含原 `CT-1234567890abcdef` 字面（彼时未开启）
  And 第二次 drawer.content 含 `[REDACTED:custom_token]`，不含原字面
  And daemon log 含一行 "config hot-reload: version changed from ... to ..."

Scenario: parse 失败时保留上一版继续服务
  Test:
    Filter: test_parse_failure_preserves_previous_config
    Level: integration
    Targets: crates/mempal-core/src/config/hot_reload.rs
  Given daemon 运行，config.toml 合法、privacy.enabled=true
  When 编辑 config.toml 故意写入**非法 TOML**（比如 `privacy.enabled = ***`）并保存
  And 等 1s
  Then daemon 不退出
  And stderr 含 "config hot-reload: parse failed, keeping previous version" 或等价
  And 此时执行 ingest 仍走 privacy scrub（old cfg 生效）
  When 再把 config.toml 改回合法（保存）
  And 等 1s
  Then ingest 走新版 privacy 规则
  And daemon 恢复日志中的 config_version 更新

Scenario: 请求级 Arc<Config> snapshot 防 mid-flight 不一致
  Test:
    Filter: test_request_scoped_snapshot_prevents_mid_flight_mutation
    Level: integration
    Test Double: slow_embedder_fixture
    Targets: crates/mempal-ingest/src/pipeline.rs, crates/mempal-core/src/config/hot_reload.rs
  Given daemon 运行，`privacy.enabled = true` + 一个人工 slow_embedder（每次 embed 2s）
  When 发起一次 ingest（含 `sk-abcdef1234567890abcdef1234567890abcd`）
  And 在该 ingest 的 embed 阶段（pipeline 已经 scrub 过但还没 embed 完）**热替换 config 把 privacy.enabled 改为 false**
  Then 该次 ingest 的 drawer.content 仍含 `[REDACTED:openai_key]`（因为 pipeline 入口已 snapshot 当时的 cfg）
  And embedder 收到的输入是 scrubbed 版本
  And 随后的下一次 ingest（新请求）走 disabled 版本（不 scrub）

Scenario: embedder backend 变更触发 restart-required warning 但不 reload
  Test:
    Filter: test_embedder_backend_change_warns_and_ignores
    Level: integration
    Targets: crates/mempal-core/src/config/hot_reload.rs
  Given daemon 运行，`[embedder] backend = "openai-compat"`、`base_url = "http://gb10:18002/v1/"`
  When 编辑 config.toml 把 `base_url` 改成 `http://localhost:9000/v1/` 保存
  And 等 1s
  Then daemon stderr 含 `"embedder.base_url requires restart, change ignored"` 类 error 级日志
  And 运行时 embedder 仍调旧 `http://gb10:18002/v1/`（通过一次 ingest + 抓网络请求 target 或 embedder mock 验证）
  And `config_version` **未**更新（因为 blacklist 字段变更不触发版本号变——防止 `mempal_status` 误报生效）
  And 编辑 config.toml 其它（非 blacklist）字段再保存 → `config_version` 正常更新

Scenario: notify watcher 死掉后 fallback poll 仍能捕获变更
  Test:
    Filter: test_notify_watcher_crash_falls_back_to_poll
    Level: integration
    Test Double: notify_kill_fixture
    Targets: crates/mempal-core/src/config/hot_reload.rs
  Given daemon 运行，人工 kill `notify::Watcher` 后台 thread（模拟 crash）
  When 编辑 config.toml 改 `privacy.enabled = false`（atomic rename 保存）
  And 等 6s（超过 fallback poll 间隔 5s）
  Then daemon 依然捕获变更，`config_version` 更新
  And 新 ingest 生效新 cfg
  And daemon log 含 `"notify watcher crashed, falling back to poll"` 类 warn

Scenario: mempal status 输出 config_version 和 loaded_at
  Test:
    Filter: test_status_prints_config_version_and_loaded_at
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given daemon 运行，config 已加载
  When 运行 `mempal status`
  Then stdout 含形如 `"config: version=<12-char-hex> loaded=<ISO-8601 timestamp>"` 的行
  And version 长度 == 12
  And timestamp 在最近 10 分钟内

Scenario: mempal_status MCP 工具返回 config_version 字段
  Test:
    Filter: test_mcp_status_returns_config_version
    Level: integration
    Targets: crates/mempal-mcp/src/tools.rs, crates/mempal-mcp/src/server.rs
  Given mempal MCP server 运行
  When agent 调 `mempal_status({})`
  Then response JSON 含 `config_version: String` 字段（12 hex chars）
  And 含 `config_loaded_at_unix_ms: number` 字段
  And 修改 config.toml + 等 1s 后再次调 → `config_version` 值不同、`config_loaded_at_unix_ms` 更大

Scenario: 一秒内多次保存只触发一次 parse（debounce 生效）
  Test:
    Filter: test_rapid_edits_coalesced_by_debounce
    Level: integration
    Test Double: parse_counter
    Targets: crates/mempal-core/src/config/hot_reload.rs
  Given daemon 运行
  When 在 500ms 内连续保存 config.toml 5 次（每次 atomic rename）
  And 等 1s
  Then parse 被调用次数 <= 2 次（允许 1 或 2，视 debounce 时序窗口）
  And 最终 `config_version` 对应最后一次保存的内容
  And 日志无重复 warn

Scenario: enabled=false 时完全不启动 watcher
  Test:
    Filter: test_hot_reload_disabled_no_watcher
    Level: integration
    Targets: crates/mempal-core/src/config/hot_reload.rs
  Given `[config_hot_reload] enabled = false`
  When daemon 启动并运行
  Then 进程无 inotify watcher fd（`ls /proc/<pid>/fd | xargs -I{} readlink /proc/<pid>/fd/{}` 不含 `anon_inode:inotify`）
  And 编辑 config.toml 不影响运行中的 Config
  And `mempal status` 的 `config_loaded_at` 等于 daemon 启动时间

Scenario: 新 prototype 被热重载但实际 embedding 直到下次 daemon 重启才生效
  Test:
    Filter: test_prototype_hot_reload_deferred_to_restart
    Level: integration
    Targets: crates/mempal-core/src/config/hot_reload.rs, crates/mempal-ingest/src/gating/prototypes.rs
  Given daemon 运行，gating.prototypes 含 3 个 prototype A/B/C
  When 编辑 config.toml 加一个新 prototype D 保存
  And 等 1s
  Then `ConfigHandle::current().ingest_gating.embedding_classifier.prototypes` 已含 D
  And daemon 仍用旧 `prototype_vectors` 数组（只 embed 过 A/B/C）做判决
  And 日志含 "prototype change detected, effective after daemon restart"
  When 重启 daemon
  Then prototype D 被新 daemon 启动时 embed 并加入 `prototype_vectors`

Scenario: MCP stdio 子进程（被 Claude Code spawn）也能热重载
  Test:
    Filter: test_mcp_stdio_child_hot_reloads
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs, crates/mempal-core/src/config/hot_reload.rs
  Given Claude Code 把 mempal MCP server 作为 stdio 子进程启动（`mempal mcp stdio` 或等价）
  When 编辑 config.toml 改 `[search] strict_project_isolation = true` 保存
  And 等 1s
  Then 对该 stdio 子进程发 `mempal_status` 返回的 config_version 已更新
  And 下一次 `mempal_search` 请求走 strict isolation 语义（通过 fixture 数据验证）

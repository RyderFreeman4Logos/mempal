spec: task
name: "P8: Pluggable OpenAI-compatible Embedder + fixed-interval retry + hot-reloadable alert + agent degradation signal"
tags: [feature, embed, backend, reliability, alerting, mcp]
estimate: 2d
---

## Intent

增加 `OpenAiCompatibleEmbedder`：任何 OpenAI-style `/v1/embeddings` HTTP 后端都能作为 `Embedder` trait 的实现。**推荐配置顺序**：(1) **LAN 自部署**（首选，如 `http://gb10:18002/v1/` 跑 `Qwen/Qwen3-Embedding-8B` 4096d——mempal 存在的核心价值主张就是把 claude-mem 的好特性搬到这档 LAN 硬件上、**消除云端 generative LLM 的 quota 花费**）；(2) **localhost 监听端口**（用户自起 ollama / vllm / TEI 等本地 embedding server）；(3) **云端商业 embedding API**（OpenAI text-embedding-3-*、Cohere、Voyage AI、SiliconFlow 等）——协议原生支持、可用作 LAN 不可达时的后备，成本比 generative chat 低约 100×，但**不是 mempal 的推荐主路径**：agent session 累积几千次 embedding 调用仍有显著成本，且与 "mempal = 零云端依赖的 claude-mem 替代" 的定位不一致。`model2vec-rs` 进程内 fallback 保留（offline / LAN 全断时的最后一档）。**URL / model 完全 config-driven**，不在代码里硬编码任何主机名。

**失败语义**：
- **Ingest 路径**：**固定 2 秒间隔** 无限重试（**不做指数退避**，用户明确偏好——压力可预测、故障恢复可观测），数据永不丢
- **Search 路径**：5s deadline 后 fallback 到 BM25-only，不阻塞用户

**与 `pending-message-store` 的协议**（CSA design review 2026-04-20 blocker 1 修复）：embedder ingest 重试循环**必须**在每次 sleep(2s) 前后调用 `store.refresh_heartbeat(msg_id, worker_id)`——这是 "2s 无限重试 + 数据永不丢" 与 "claim TTL + reclaim_stale" 能并存的唯一正确协议。heartbeat 活着的 claim 不会被 reclaim_stale 回滚；只有 worker 进程真正崩溃（heartbeat 静默 > `stale_secs`，默认 10s）才触发回滚。这消除了 CSA 识别的 "worker 永久持有 claim → TTL 到期 → 第二 worker 双派发" 的死锁路径。
- **告警**：累计失败每 `alert_every_n_failures` 次（默认 100，**配置文件可配且热重载**）调用用户预配置的绝对路径脚本（e.g., Telegram / Slack / email 推送）
- **Agent 通知**：当累计失败超过 `degrade_after_n_failures`（默认 10），mempal 进入 **degraded 状态**，对所有 MCP tool response 注入 `system_warnings` 字段并**拒绝**写入类 MCP 工具（`mempal_ingest` / `mempal_kg` add|invalidate / `mempal_tunnels` add|invalidate），强制 agent 暂停写入；读类正常。一次成功 embed 退出 degraded 并清零计数

**设计理由**：
- 固定 2s 间隔：用户明确要求，且比指数退避更可预测——LAN 服务 OOM 时 2s 节拍正好给 GPU 回复时间，运维看日志更易判断（每 2s 一条 warn 有规律）
- 热重载告警阈值：把运维 knob 从"改完重启"降级为"改完生效"
- Degraded 状态同时推给外部（脚本）+ agent（MCP response）：外部是异步告警（人类可稍后看），agent 是同步门控（立即暂停，保证系统一致性）

**与 feedback 的对应**：
- 符合 `feedback_no_llm_api_dependency.md`（"generative LLM 云端 API 禁令不含 embedding 云端服务"，embedding 成本 ≈ generative chat 的 1%）。本 spec 的立场：**LAN 自部署为默认首选**（与项目价值主张一致），云端 embedding API 作为**可配置的后备**（协议原生支持、用户显式指向时不阻塞），model2vec 进程内作**最后一档**（offline / 全部 HTTP 后端不可用时）。推荐顺序不是硬约束而是运维建议：单 agent session 的 embedding 调用会累积，即使按 $0.02/1M tokens 的低价也非完全可忽略。
- 符合 `feedback_cli_over_web_ui.md`：无 UI，告警走脚本、状态走 MCP field

## Decisions

### Embedder 后端

- 新建 `crates/mempal-embed/src/openai_compat.rs`：`OpenAiCompatibleEmbedder` struct 实现 `Embedder` trait
- 新建 `crates/mempal-embed/src/retry.rs`：固定间隔重试 loop + deadline 支持
- 新建 `crates/mempal-embed/src/status.rs`：`EmbedStatus` 全局单例（`Arc<EmbedStatus>`）
- 新建 `crates/mempal-embed/src/alert.rs`：告警脚本调用 + 节流计数
- 新建 `crates/mempal-embed/src/config_watcher.rs`：`notify` crate watch config 热重载
- 配置：
  ```toml
  [embed]
  backend = "openai_compat"   # "openai_compat" | "model2vec" | "onnx"
  
  [embed.openai_compat]
  base_url = "http://gb10:18002/v1"   # user-configurable; LAN / localhost:<port> / https://api.openai.com/v1
  model = "Qwen/Qwen3-Embedding-8B"   # 必填，无默认
  api_key_env = ""                    # 推荐：环境变量名（e.g., "OPENAI_API_KEY"）；留空跳过 Authorization
  request_timeout_secs = 30
  dim = 4096                          # 可选，未填则首次 embed 推断
  
  [embed.retry]                       # ↓ 热重载
  interval_secs = 2                   # 固定间隔，无退避
  search_deadline_secs = 5
  
  [embed.alert]                       # ↓ 热重载
  enabled = false
  script_path = ""                    # 绝对路径，例：/home/obj/bin/mempal-telegram-alert.sh
  alert_every_n_failures = 100        # 每 N 次失败告警一次（固定节流，无阈值退避）
  
  [embed.degradation]                 # ↓ 热重载
  degrade_after_n_failures = 10
  block_writes_when_degraded = true   # degraded 时 MCP 写入工具拒绝
  ```
- `api_key` 不直接写配置文件——仅支持 `api_key_env = "VAR_NAME"` 从 env 读，避免 secret 入 config
- 所有 secrets 不进日志（即便 debug）——保留前 4 字符 + `…`

### 重试语义（固定 2s 间隔，**无退避**）

- Ingest 路径 loop：
  ```
  loop:
    match embedder.embed(text) {
      Ok(v) => break v,
      Err(e) => {
        status.record_failure(e);
        tokio::time::sleep(interval_secs).await;  // 固定 2s
      }
    }
  ```
- Search 路径：`tokio::select! { embed => ..., sleep(deadline) => return BM25Only }`
- `interval_secs` 热重载：每次 sleep 前读 `status.retry_interval.load()`；变化立即生效
- 无 jitter（用户未要求；2s 规律更易运维观察）
- 无 max_retries 上限（无限重试）

### Degraded 状态机

- `EmbedStatus`:
  ```rust
  pub struct EmbedStatus {
      fail_count: AtomicU64,
      degraded: AtomicBool,
      retry_interval_secs: ArcSwap<u64>,        // hot-reload
      alert_threshold: ArcSwap<u64>,            // hot-reload
      degrade_threshold: ArcSwap<u64>,          // hot-reload
      alert_script: ArcSwap<Option<PathBuf>>,   // hot-reload
      block_writes: ArcSwap<bool>,              // hot-reload
      last_error: ArcSwap<Option<String>>,
      last_success_at: ArcSwap<Option<Instant>>,
  }
  ```
- `record_failure(err)`：
  - `fail_count += 1`
  - 若 `fail_count >= degrade_threshold` 且 `!degraded` → `degraded = true`，log error "entering degraded state"
  - 若 `fail_count % alert_threshold == 0` → spawn `alert::fire(fail_count, err)`
- `record_success()`：
  - `fail_count = 0`
  - 若 `degraded` → `degraded = false`，log info "recovered from degraded state"

### MCP 集成（agent 通知）

- `mempal-mcp/src/tools.rs` 给每个 response DTO 加统一字段：
  ```rust
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub system_warnings: Vec<SystemWarning>,
  ```
  `SystemWarning { level: "error"|"warn"|"info", message: String, source: "embed"|... }`
- 所有 MCP handler 在构造 response 前调 `EmbedStatus::collect_warnings()`，填入字段
- **写入类 MCP 工具**（在 `block_writes_when_degraded = true` 且 `degraded = true` 时）：
  - `mempal_ingest`、`mempal_kg` action=add|invalidate、`mempal_tunnels` action=add|invalidate
  - 直接返回 MCP error：
    ```
    { code: -32000, message: "mempal embed backend degraded (N failures since last success at T).
      Writes are paused to preserve data integrity. Please pause your write
      workflow until recovery. Read operations (search, peek, status) remain
      available with BM25-only fallback." }
    ```
  - error 含 instructional hint，agent 读到就会暂停
- **读类 MCP 工具**（`mempal_search`、`mempal_read_drawer`、`mempal_status`、`mempal_peek_partner`、`mempal_taxonomy` show、`mempal_kg` query、`mempal_tunnels` query）：
  - 正常工作
  - `mempal_search` 在 degraded 下 fallback BM25-only，response `system_warnings` 含 `"vector unavailable, BM25 fallback"`
  - `mempal_status` 响应专门有 `embed_status: { healthy|degraded, fail_count, last_error, last_success_at }` 字段
- MCP `ServerInfo.instructions` 动态注入 RULE 11 (degraded-behavior)：当前 degraded 时加一段引导

### 配置热重载

- `mempal daemon` 启动时 `notify::RecommendedWatcher` watch `~/.mempal/` **目录**并在 event handler 里 filter `event.paths` 是否等于 `~/.mempal/config.toml`——不能直接 watch 单文件，因为许多编辑器走 atomic save（写新文件 + rename 覆盖），inode 变化让文件级 watcher 失联（macOS `FSEvents`、Linux `inotify IN_MOVE_SELF` 表现各异，目录级 watch 是跨平台稳妥选）
- 变化事件 debounce 500ms 后 re-parse 整个 config
- **只** apply 以下 section 的热重载变化：
  - `[embed.retry]`
  - `[embed.alert]`
  - `[embed.degradation]`
- 其他 section 变化 → log info "config key X changed, restart required to apply"，**不崩溃**
- parse 失败 → log error，保留旧值，不崩溃
- 对 non-daemon 场景（CLI 单命令）不启用 watcher（无 long-lived 进程）
- 热重载也支持 `SIGHUP`（同步触发 reload）：对 shell 交互 / systemd 单元友好

### 维度处理（保持 P8 v1 决策）

- `drawer_vectors` 表动态 dim
- 切后端 dim 不一致 → 拒绝启动 + 提示 `mempal reindex --embedder <name>`
- fork-ext `fork_ext_version` `1 → 2`：加 `reindex_progress` 表（fork-ext 独立版本轴；queue 先占 ext_v1）
- reindex 也走固定 2s 重试策略
- **reindex 失败不触发 degraded**（degraded 只针对常规 ingest / search 路径）——reindex 是一次性批处理

### 默认值

- `backend = "openai_compat"` 但 `base_url` 是 placeholder，**首次启动**若未显式配置 → 拒绝启动 + 打印示例配置
- 用户必须显式填 `base_url` 和 `model`，防止 mempal 默认指向不存在的 host 卡住首次用户

## Boundaries

### Allowed
- `crates/mempal-embed/src/openai_compat.rs`（新建）
- `crates/mempal-embed/src/retry.rs`（新建）
- `crates/mempal-embed/src/status.rs`（新建）
- `crates/mempal-embed/src/alert.rs`（新建）
- `crates/mempal-embed/src/config_watcher.rs`（新建）
- `crates/mempal-embed/src/lib.rs`（re-exports + backend dispatch）
- `crates/mempal-embed/Cargo.toml`（新增 `reqwest`、`notify`、`arc-swap`——workspace deps）
- `crates/mempal-core/src/config.rs`（新 section struct）
- `crates/mempal-core/src/db/schema.rs`（fork_ext_version `1 → 2`）
- `crates/mempal-mcp/src/tools.rs`（`SystemWarning` type + 每个 DTO 加字段）
- `crates/mempal-mcp/src/server.rs`（handler 填字段 + 写类工具 degraded 拒绝 + RULE 11）
- `crates/mempal-cli/src/main.rs`（`mempal reindex` + SIGHUP handler）
- `crates/mempal-cli/src/reindex.rs`（新建）
- `crates/mempal-cli/src/daemon.rs`（config watcher 启动）
- `tests/openai_compat_embedder.rs`、`tests/fixed_retry.rs`、`tests/alert_hot_reload.rs`、`tests/degraded_state.rs`、`tests/reindex.rs`（新建）

### Forbidden
- 不要做指数退避（用户明确要求固定间隔）
- 不要在 `api_key` 直接写配置文件——必须通过 `api_key_env`
- 不要在日志 / stderr / stdout 任何地方打印 `Authorization` header 或 `api_key` 原值
- 不要在 degraded + `block_writes_when_degraded=true` 时允许 `mempal_ingest` / `mempal_kg add|invalidate` / `mempal_tunnels add|invalidate` 成功
- 不要在 degraded 状态下 block 读操作（search / peek / status 必须可用）
- 不要做阈值退避（`alert_every_n_failures` 是固定节流，每 N 次告警一次）
- 不要硬编码 `http://gb10:18002` 或任何主机名进代码——default 放示例，require user 显式配置
- 不要让 config watcher 崩溃影响主流程（panic 隔离）
- 不要在 non-daemon 场景启动 watcher
- 不要 panic 在 `notify` 注册失败——warn 继续运行
- 不要在 `[embed.openai_compat]` section 加热重载（切 embedder 后端必须重启，避免中途切换导致 dim mismatch）
- 不要给 `mempal_peek_partner` 走 embedder 路径（peek 不需要 embed）
- 不要让 reindex 进度影响 `fail_count` / degraded 判定（reindex 独立统计）
- 不要新增独立 LLM SDK 依赖（`async-openai` 等）——原始 reqwest POST 足够

## Out of Scope

- 流式 embed response（`stream=true`）
- Token usage 计费
- LAN 服务发现（mDNS）
- Embedder 热切换（backend 变更必须重启）
- 加密的 API key keyring 集成
- 告警渠道内置（永远走脚本）
- Embedding cache
- MCP `notifications/message` 主动 push（留未来，`system_warnings` 响应字段已满足目标）
- `mempal embed-server` 子命令（把 model2vec 包装成 HTTP server 统一访问协议——P11+ optional）
- 单独的 embedder healthcheck 端点（走 `mempal status`）
- reindex 增量策略（只支持全量 + 中断续跑）

## Completion Criteria

Scenario: OpenAiCompatibleEmbedder 正常调用返回 embedding
  Test:
    Filter: test_openai_compat_happy_path
    Level: integration
    Test Double: mock_http_server
    Targets: crates/mempal-embed/src/openai_compat.rs
  Given mock server 对 POST `/v1/embeddings` 返回 `{"data":[{"embedding":[<4096 floats>]}]}`
  And config `base_url = mock.url, model = "Qwen/Qwen3-Embedding-8B"`
  When 调 `embedder.embed("hello")`
  Then 返回 `Ok(Vec<f32>)` 长度 4096

Scenario: api_key_env 从环境变量读取
  Test:
    Filter: test_api_key_from_env_var
    Level: integration
    Test Double: mock_http_server
    Targets: crates/mempal-embed/src/openai_compat.rs
  Given env `MEMPAL_TEST_KEY = "sk-test123"`
  And config `api_key_env = "MEMPAL_TEST_KEY"`
  When 调 embed
  Then mock server 收到请求的 `Authorization` 头 == `Bearer sk-test123`

Scenario: 固定 2 秒重试间隔（无退避）
  Test:
    Filter: test_fixed_two_second_retry_interval
    Level: integration
    Test Double: timestamped_failing_server
    Targets: crates/mempal-embed/src/retry.rs
  Given server 前 3 次失败，第 4 次成功
  And `interval_secs = 2`
  When 触发 ingest embed
  Then 4 次请求的时间戳间隔近似 `[t0, t0+2, t0+4, t0+6]`（±200ms 容忍）
  And 没有任何间隔 >= 3s（排除指数退避）

Scenario: search 路径 deadline 后 fallback BM25
  Test:
    Filter: test_search_deadline_bm25_fallback
    Level: integration
    Test Double: slow_http_server
    Targets: crates/mempal-embed/src/retry.rs, crates/mempal-search/src/hybrid.rs
  Given server 每次响应延迟 10s，`search_deadline_secs = 5`
  When 调 `mempal_search({query:"foo"})`
  Then 命令在 <= 6 秒内返回
  And response.system_warnings 含 `"vector unavailable"`（BM25 fallback 标记）

Scenario: 失败累计达 degrade_threshold 进入 degraded 状态
  Test:
    Filter: test_enters_degraded_after_threshold
    Level: integration
    Test Double: always_failing_server
    Targets: crates/mempal-embed/src/status.rs
  Given server 永远失败，`degrade_after_n_failures = 10`
  When 触发 15 次 ingest embed
  Then `EmbedStatus::is_degraded() == true`
  And log 含 "entering degraded state"

Scenario: degraded + block_writes=true 时 mempal_ingest 被拒绝
  Test:
    Filter: test_ingest_rejected_when_degraded
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs, crates/mempal-embed/src/status.rs
  Given `degraded = true`，`block_writes_when_degraded = true`
  When 调 `mempal_ingest({wing:"x", content:"y"})`
  Then 返回 MCP error code -32000
  And error message 含 "embed backend degraded"
  And error message 含 "pause your write workflow"
  And error message 含当前 fail_count 数值

Scenario: degraded 状态下 mempal_search 正常返回 + system_warnings
  Test:
    Filter: test_search_works_when_degraded_with_warning
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given `degraded = true`，palace.db 有 drawer
  When 调 `mempal_search({query:"foo"})`
  Then 返回 success（非 error）
  And `response.system_warnings` 非空
  And warning 含 `level="warn"` 且 `source="embed"` 的条目

Scenario: 一次成功 embed 后 degraded 状态退出 + fail_count 清零
  Test:
    Filter: test_successful_embed_exits_degraded
    Level: integration
    Test Double: flaky_then_healthy_server
    Targets: crates/mempal-embed/src/status.rs
  Given server 前 12 次失败（触发 degraded），第 13 次成功
  When ingest 驱动该序列
  Then 第 13 次成功后 `EmbedStatus::is_degraded() == false`
  And `fail_count == 0`
  And log 含 "recovered from degraded state"

Scenario: block_writes_when_degraded=false 时 ingest 不被拒绝（但阻塞重试）
  Test:
    Filter: test_ingest_blocks_but_not_rejected_when_block_writes_false
    Level: integration
    Test Double: always_failing_server
    Targets: crates/mempal-mcp/src/server.rs
  Given `degraded = true`，`block_writes_when_degraded = false`
  When 调 `mempal_ingest(...)`
  Then 命令不返回 error
  And 命令持续阻塞在 embed 重试（测试用 2s timeout 判断 "still running"）

Scenario: alert 阈值热重载生效（不重启进程）
  Test:
    Filter: test_alert_threshold_hot_reload
    Level: integration
    Test Double: always_failing_server, recording_alert_script, tempfile_config
    Targets: crates/mempal-embed/src/config_watcher.rs, crates/mempal-embed/src/alert.rs
  Given `mempal daemon` 运行，`alert_every_n_failures = 100`
  And 已累积 99 次失败（未触发 alert）
  When 修改 `config.toml` 把 `alert_every_n_failures` 改为 50
  And 等待 debounce（1s）
  And 再触发 1 次失败（累计 100）
  Then alert script 被调用（新阈值下 100 % 50 == 0 触发）
  And daemon 未重启

Scenario: alert 每 N 次触发（固定节流，无退避）
  Test:
    Filter: test_alert_fixed_throttle
    Level: integration
    Test Double: always_failing_server, recording_alert_script
    Targets: crates/mempal-embed/src/alert.rs
  Given `alert_every_n_failures = 50`
  When 累积 200 次失败
  Then alert script 被调用恰好 4 次（在 50、100、150、200）
  And 4 次调用间隔大致相同（每 ~50 次失败一次，无退避）

Scenario: alert 脚本不存在时 warn 不 panic
  Test:
    Filter: test_missing_alert_script_warns_only
    Level: integration
    Targets: crates/mempal-embed/src/alert.rs
  Given `script_path = "/nonexistent/path"`
  When alert 触发
  Then mempal 进程继续运行
  And stderr 含 "alert script not found"

Scenario: alert 脚本接收参数含 fail_count 和 last_error
  Test:
    Filter: test_alert_script_args
    Level: integration
    Test Double: recording_alert_script
    Targets: crates/mempal-embed/src/alert.rs
  Given `script_path = <tmp script>`，`alert_every_n_failures = 10`
  When 累积 10 次失败（每次 error message = "connection refused"）
  Then script 被调用 1 次
  And argv 含 "10"（fail_count）
  And argv 含 "connection refused"（last error）
  And argv 含 "Qwen/Qwen3-Embedding-8B"（model name）

Scenario: retry interval 热重载生效
  Test:
    Filter: test_retry_interval_hot_reload
    Level: integration
    Test Double: tempfile_config
    Targets: crates/mempal-embed/src/config_watcher.rs, crates/mempal-embed/src/retry.rs
  Given `interval_secs = 2` 且正在无限重试中
  When 修改 config 改为 `interval_secs = 1`
  And 等 debounce
  Then 下一次重试间隔约 1s（±200ms）
  And daemon 未重启

Scenario: [embed.openai_compat] section 变化不热重载（需重启）
  Test:
    Filter: test_openai_compat_section_requires_restart
    Level: integration
    Test Double: tempfile_config
    Targets: crates/mempal-embed/src/config_watcher.rs
  Given mempal daemon 运行，`base_url = "http://localhost:18002/v1"`
  When 修改 config `base_url = "http://other-host/v1"`
  Then daemon 实际使用的 base_url 未变
  And stderr / log 含 `"config key [embed.openai_compat] changed, restart required"`

Scenario: SIGHUP 触发手动 reload
  Test:
    Filter: test_sighup_triggers_reload
    Level: integration
    Test Double: tempfile_config
    Targets: crates/mempal-cli/src/daemon.rs
  Given `mempal daemon` 运行
  When 修改 config 并立即发 SIGHUP
  Then config 立即 re-parse（不等 fs-watch debounce）

Scenario: 维度 mismatch 拒绝启动
  Test:
    Filter: test_dim_mismatch_fail_fast
    Level: integration
    Targets: crates/mempal-embed/src/lib.rs
  Given `drawer_vectors` dim == 256（旧 model2vec）
  And config `backend = "openai_compat", dim = 4096`
  When 启动 mempal
  Then 退出码 != 0
  And stderr 含 `"run 'mempal reindex --embedder openai_compat'"`

Scenario: `mempal reindex` 全库 re-embed + resume
  Test:
    Filter: test_reindex_with_resume
    Level: integration
    Test Double: mock_http_server
    Targets: crates/mempal-cli/src/reindex.rs
  Given 50 条旧 drawer (dim=256)
  When 执行 `mempal reindex --embedder openai_compat`，在 20 条时 SIGINT
  And 再执行 `mempal reindex --embedder openai_compat --resume`
  Then 全 50 条 drawer 完成 re-embed，dim=4096
  And `fork_ext_version == "2"`

Scenario: 配置首次启动若缺 base_url/model 则 fail-fast
  Test:
    Filter: test_missing_base_url_fails_fast_with_example
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given config `[embed.openai_compat]` section 缺 `base_url` 或 `model`
  When 启动 mempal
  Then 退出码 != 0
  And stderr 含示例 `base_url = "http://localhost:18002/v1"` 配置片段

Scenario: api_key 不泄漏到任何日志
  Test:
    Filter: test_api_key_never_in_logs
    Level: integration
    Test Double: always_failing_server
    Targets: crates/mempal-embed/src/openai_compat.rs
  Given env `TESTKEY = "sk-supersecret1234567890"`，config `api_key_env = "TESTKEY"`
  When 触发 20 次失败（debug 日志）
  Then 所有 log / stderr / stdout / tracing 输出不含 "sk-supersecret1234567890"
  And 若引用，前 4 字符 + "…" 格式（如 `"sk-s…"`）

Scenario: mempal_status 输出 embed_status 字段
  Test:
    Filter: test_status_exposes_embed_status
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given degraded = true，fail_count = 42
  When 调 `mempal_status`
  Then response 含 `embed_status.healthy == false`
  And `embed_status.fail_count == 42`
  And `embed_status.last_error` 非空

Scenario: ServerInfo.instructions 在 degraded 时注入 RULE 11
  Test:
    Filter: test_server_info_injects_rule_11_when_degraded
    Level: integration
    Targets: crates/mempal-mcp/src/server.rs
  Given degraded = true
  When 客户端请求 MCP ServerInfo
  Then instructions 字符串含 "RULE 11"
  And 含 "pause" 或等价引导词

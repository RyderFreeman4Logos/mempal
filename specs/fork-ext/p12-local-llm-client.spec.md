spec: task
name: "P12: Local LLM client infrastructure (OpenAI-compat, crash-safe queue, concurrency limiter)"
tags: [feature, llm, queue, concurrency, infrastructure, local-only]
estimate: 3d
---

## Intent

为 mempal 提供**可选的本地 LLM 客户端基础设施**，通过 OpenAI-compatible API 端点对接用户自部署的本地/LAN 模型（如 `qwen35-35b-a3b` 跑在 vllm 上）。用于增强 ingest gating（Tier 3 judge）、knowledge distill、compress 等场景。

**核心不变量**：
- **Local-only by design**：不走云端 generative LLM API（成本禁令不变）
- **Optional**：不配 `[llm]` 段 = mempal 行为与现在完全一致
- **Crash-safe**：先持久化 SQLite，重启后从 SQLite 恢复未处理任务
- **无限重试不退避**：跟 embedder 一致，固定间隔重试，本地 LLM 恢复即继续
- **并发控制**：可配 `max_concurrent`，热重载生效

**动机**：cn-llm-censor-research 实验验证了 GPT-5.4-mini 云端 judge 成本 ~$4.5/2000 items，本地 72B+ 模型 $0 且 gb10 128GB 统一内存可同时跑目标模型和 judge。用户 2026-04-29 确认本地 LLM 成本为零，可接入 mempal。Issue #102。

**与 p9-judge-gating-local 的关系**：该 spec 的 Tier 3 LLM judge 被标注"推迟到 P11+ 独立 spec"——本 spec 就是那个独立 spec。P12 提供 LLM 基础设施 + Tier 3 gating 集成；distill/compress 集成在 Boundaries 中标注为 future。

## Decisions

### Config

在 `Config` struct 新增 `pub llm: LlmConfig` 字段（`#[serde(default)]`）：

```toml
[llm]
enabled = true                          # master switch, default false
backend = "openai_compat"               # only supported backend for now
base_url = "http://localhost:8317/v1"    # OpenAI-compat endpoint
model = "qwen35-35b-a3b"               # model name for API requests
api_key_env = "LOCAL_ROUTER_API_KEY"    # env var name to read API key from (optional)
request_timeout_secs = 30               # per-request timeout
retry_interval_secs = 2                 # fixed retry interval (no backoff)
max_concurrent = 16                     # max in-flight LLM requests
enabled_for = ["gating"]                # which subsystems may use LLM; valid: "gating", "distill", "compress"
```

- `LlmConfig` 在 `config.rs` 中定义，字段均带 serde default
- `enabled = false` 时所有 LLM 相关代码路径不激活（零开销）
- `max_concurrent` 和 `enabled_for` 属于热重载白名单字段（`p8-config-hot-reload` 机制）
- `base_url` / `model` / `api_key_env` 变更需要重启（热重载黑名单）
- 移除 `config.rs:100-101` 的 `has_llm_judge_section` warning 硬编码逻辑

### LLM Client

新建 `src/llm/mod.rs` + `src/llm/client.rs`：

```rust
pub struct LlmClient {
    http: reqwest::Client,
    config: Arc<ArcSwap<LlmConfig>>,  // hot-reloadable
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl LlmClient {
    pub async fn chat_completion(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
    pub fn update_concurrency(&self, new_max: usize);
}
```

- HTTP 请求遵循 OpenAI chat completions API 格式（`POST /chat/completions`）
- 单次请求超时：`request_timeout_secs`
- **无限重试**：请求失败（网络错误/超时/5xx）时固定间隔 `retry_interval_secs` 重试，**不指数退避**，跟 embedder 一致
- 4xx 错误（除 429）不重试，直接返回错误
- 429 (rate limit) 按 `retry_interval_secs` 重试
- `api_key_env` 指定的环境变量不存在或为空时，请求不附带 `Authorization` header（部分本地 LLM 不需要 key）
- Semaphore 控制并发：`max_concurrent` permits，`chat_completion` 入口 `acquire_owned` 等 permit

### 并发热重载

`max_concurrent` 变更时（通过 `p8-config-hot-reload` 机制）：
- 新值 > 旧值：`semaphore.add_permits(diff)`
- 新值 < 旧值：创建新 Semaphore 替换（旧 Semaphore 的 in-flight 请求自然完成后 drop）
- `LlmClient::update_concurrency(&self, new_max)` 方法处理此逻辑

### Crash-safe 任务队列

新建 `src/llm/queue.rs`，复用 `pending_messages` 表的 Claim-Confirm 模式（`p8-pending-message-store`），但以 **独立 kind** 区分 LLM 任务：

```sql
-- 复用 pending_messages 表，kind='llm_task'
-- payload JSON schema:
-- {
--   "task_type": "gating" | "distill" | "compress",
--   "drawer_id": "...",           -- optional, for gating/distill
--   "input": "...",               -- prompt or content to process
--   "system_prompt": "...",       -- optional system prompt
--   "metadata": {}                -- task-specific metadata
-- }
```

- **入队**：调用方（如 gating Tier 3）把任务 `enqueue(kind="llm_task", payload=JSON)` 到 `pending_messages`
- **出队处理**：后台 worker 从 `pending_messages` claim `kind='llm_task'` 的任务，通过 LlmClient 发送请求，结果写回目标（如 `gating_audit` 表的 `llm_verdict` 字段）
- **重启恢复**：启动时 `reclaim_stale` 回滚 claimed 但未完成的 LLM 任务到 pending 状态
- **heartbeat 协议**：跟 embedder 一致——每轮重试循环调 `refresh_heartbeat`，防止 `reclaim_stale` 误判
- 如果 `pending_messages` 表尚未存在（fork-ext migrations 未跑），LLM 队列相关功能 graceful skip 并 warn

### Worker 循环

`src/llm/worker.rs`：

```rust
pub async fn run_llm_worker(
    store: Arc<PendingMessageStore>,
    client: Arc<LlmClient>,
    config: Arc<ArcSwap<LlmConfig>>,
) -> Result<(), LlmError>;
```

- 后台 tokio task，在 `mempal daemon` 中启动（仅当 `llm.enabled = true`）
- 循环：`claim_next(kind="llm_task")` → `client.chat_completion()` → 写结果 → `confirm()`
- 无任务时 sleep 500ms 再查（跟 hook worker 一致）
- LLM 请求失败时 `refresh_heartbeat` + sleep `retry_interval_secs` + 重试（无限循环）
- 成功后根据 `task_type` 路由到对应的 result handler

### Gating Tier 3 集成

修改 `p9-judge-gating-local` 的 gating 管道，在 Tier 2 之后增加 Tier 3：

```toml
[ingest_gating]
enabled = true

[ingest_gating.llm_judge]
enabled = true
system_prompt = "You are a quality judge..."
threshold = 0.6                 # LLM 返回的 score >= threshold 才 Keep
```

- Tier 3 仅在 `[llm].enabled = true` 且 `"gating"` 在 `enabled_for` 中 且 `[ingest_gating.llm_judge].enabled = true` 时激活
- Tier 1/2 已 Skip 的 → 不进 Tier 3（短路）
- Tier 1/2 已 Keep 的 → 也不进 Tier 3（已经决定保留）
- 仅 Tier 2 `Unclassified` 的候选进入 Tier 3（模糊地带由 LLM 裁决）
- Gating 中 Tier 3 是 **异步非阻塞**：先 `Keep` 放行（fail-open），同时入队 LLM 任务；LLM 结果回来后异步更新 `gating_audit` 表的 `llm_verdict`
- 后续可通过 `mempal gating stats` 查看 LLM judge 的判决分布，作为调参依据

### Degraded 状态

跟 embedder 一致的 degraded 状态机制：
- 累计连续失败 >= `degrade_after_n_failures`（默认 10）→ 进入 degraded
- degraded 状态下：MCP response 注入 `system_warnings: ["llm_degraded: ..."]`
- 成功一次即退出 degraded
- degraded **不阻止** ingest（LLM 是增强，不是必需）

### Observability

- `mempal status` 输出新增 LLM 段：`llm: enabled=true, backend=openai_compat, model=qwen35-35b-a3b, pending=3, in_flight=12/16, degraded=false`
- `mempal_status` MCP 工具同步暴露
- 日志（tracing）：每次 LLM 请求记 `info!` 含 task_type + latency_ms + token_count

## Boundaries

### Allowed
- `src/llm/mod.rs`（新建）
- `src/llm/client.rs`（新建）
- `src/llm/queue.rs`（新建）
- `src/llm/worker.rs`（新建）
- `src/core/config.rs`（新增 `LlmConfig` + 移除 `has_llm_judge_section` warning）
- `src/mcp/tools/status.rs`（新增 LLM 状态段）
- `src/cli/status.rs`（新增 LLM 状态段）
- 现有 gating 模块（新增 Tier 3 路径）
- `Cargo.toml`（如需新增依赖）

### Not Allowed
- 修改 embedder 相关代码（独立子系统）
- 修改 drawers 表 schema（LLM 结果走 gating_audit / 独立表）
- 引入云端 LLM API 硬编码端点
- 引入新的 IPC 机制（NATS/Redis/ZeroMQ）

### Future (out of scope)
- `distill` 集成：LLM 增强 knowledge distill（需新 spec）
- `compress` 集成：LLM 驱动压缩替代规则 AaakCodec（需新 spec）
- 流式响应（streaming）：当前用标准 request-response 足够
- Prompt 模板管理：当前 system_prompt 直接配在 config 里

## Scenarios

### test_llm_config_default_disabled
Config 不含 `[llm]` 段 → `LlmConfig::default()` 的 `enabled = false` → 无 LLM worker 启动 → 零开销

### test_llm_config_parse_full
完整 `[llm]` 段 → 所有字段正确解析 → `enabled = true`

### test_llm_config_missing_base_url_when_enabled
`[llm] enabled = true` 但无 `base_url` → `ConfigError::Validation` 明确报错

### test_llm_client_chat_completion_success
Mock HTTP server 返回 200 + valid JSON → `LlmResponse` 包含 content

### test_llm_client_retry_on_5xx
Mock server 前 3 次返回 500，第 4 次返回 200 → 最终成功 → 总延迟 ≈ 3 × retry_interval

### test_llm_client_no_retry_on_4xx
Mock server 返回 400 → 立即返回 `LlmError::ClientError` → 无重试

### test_llm_client_retry_on_429
Mock server 前 2 次返回 429，第 3 次返回 200 → 最终成功

### test_llm_client_semaphore_limits_concurrency
`max_concurrent = 2`，同时发 5 个请求 → 同一时刻最多 2 个 in-flight

### test_llm_concurrency_hot_reload_increase
`max_concurrent` 从 2 改到 4 → `update_concurrency` → 新请求立即可获得更多 permits

### test_llm_concurrency_hot_reload_decrease
`max_concurrent` 从 4 改到 2 → 新 Semaphore 替换 → in-flight 请求正常完成

### test_llm_queue_enqueue_and_claim
`enqueue(kind="llm_task", ...)` → `claim_next(kind="llm_task")` → 返回 claimed message

### test_llm_queue_restart_recovery
enqueue 3 个 llm_task → 模拟 crash（不 confirm）→ 重启 → `reclaim_stale` → 3 个任务回到 pending

### test_llm_queue_heartbeat_keeps_claim
claimed 任务持续 heartbeat → `reclaim_stale` 不回滚

### test_llm_worker_processes_task
enqueue 1 个 gating task → worker 循环 claim + LLM 请求 + confirm → pending_messages 为空

### test_llm_worker_retries_indefinitely
LLM 端点 down → worker 每 retry_interval 重试 + heartbeat → 端点恢复 → 成功处理

### test_gating_tier3_only_for_unclassified
Tier 1 Skip → 不入队 LLM 任务；Tier 2 Keep → 不入队 LLM 任务；Tier 2 Unclassified → 入队 LLM 任务

### test_gating_tier3_fail_open
LLM 端点 down → Tier 3 仍返回 `Keep { tier: 0, label: "llm_pending" }` → drawer 被保存（fail-open）

### test_llm_degraded_after_n_failures
连续 10 次 LLM 失败 → `is_degraded() = true` → status 显示 degraded

### test_llm_degraded_recovery
degraded 状态 → 1 次成功 → `is_degraded() = false`

### test_llm_disabled_removes_config_warning
`[llm] enabled = true` + `[ingest_gating.llm_judge] enabled = true` → **不**输出旧的 "external LLM API disabled by design" warning

### test_llm_no_api_key_omits_auth_header
`api_key_env` 未设或环境变量不存在 → 请求不含 `Authorization` header

### test_llm_status_output
`mempal status` 输出包含 `llm:` 段 + pending/in_flight/degraded 信息

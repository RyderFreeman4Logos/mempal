spec: task
name: "P8: Privacy tag stripping + credential regex scrubbing at ingest edge (default off)"
tags: [feature, privacy, ingest, security]
estimate: 1d
---

## Intent

在 `mempal-ingest` 管道前端新增一个**边缘清洗层**，在内容进入 embedding/FTS/存储之前，按配置去除 `<private>` 标签块和已知凭证正则模式（OpenAI/Anthropic API key、AWS access key、Bearer token、长 hex token 等），把占位符 `[REDACTED:<kind>]` 写回 raw 内容。

**动机**：mempal 的 raw verbatim 存储是设计强项，但也意味着 agent 偶然打出 API key / 环境变量 / session token 时会被永久索引。向量一旦算出无法回溯删除。应在**进入 vector 空间前**拦截。

**v3 判决依据**：
- claude-mem `src/utils/tag-stripping.ts:40-61` 仅做标签剥离，**没有**凭证正则嗅探。后者是用户基于 mempal 安全需求的**独立增强**，符合"强化 mempal"意图。
- Rust `regex` crate 基于 Thompson NFA，原生免疫 ReDoS，反而比 claude-mem Node 版更安全。

**Default off**：维持 mempal "verbatim + deliberate" 默认体验，opt-in 才激活。

## Decisions

- 新建 `crates/mempal-ingest/src/privacy.rs` 模块，暴露 `fn scrub(text: &str, cfg: &PrivacyConfig) -> (String, ScrubStats)`
- `PrivacyConfig` 含 `enabled: bool`、`strip_tags: Vec<String>`、`scrub_patterns: Vec<ScrubPattern>`
- `ScrubPattern { name: String, pattern: String, replacement: String }`，`pattern` 在 config 加载时编译成 `regex::Regex`，编译失败 fail-fast
- 清洗发生在 `ingest::pipeline` 的 format-detect / 归一化**之后**、chunking **之前**（order 很重要：必须在完整归一化文本上先 scrub，chunking 之后再 scrub 会让跨 chunk 边界的 secret / `<private>` 标签漏网——单 chunk 内 regex 匹配不到整条凭证或跨段标签。scrubbing 以 `[REDACTED:<kind>]` 替换并不破坏 chunking 段落结构）
- 内置默认 pattern 库（all opt-in）：`openai_key` (`sk-[A-Za-z0-9]{32,}`)、`aws_access` (`AKIA[0-9A-Z]{16}`)、`bearer_token` (`Bearer\s+[A-Za-z0-9\-_\.]{20,}`)、`hex_token` (`\b[a-f0-9]{32,}\b`)、`anthropic_key` (`sk-ant-[A-Za-z0-9_\-]{64,}`)
- `strip_tags` 默认 `["private"]`，匹配 `<private>...</private>`（greedy single-line + multi-line，flags `(?s)`）
- 清洗统计（命中 pattern 名称 + 计数）挂到返回值，由 `mempal_ingest` handler 和 CLI 汇总到 `mempal status` 输出
- 同一 `PrivacyConfig` 实例在进程内复用（Regex 编译 cache），不 per-call 重编译
- **不**把 tag 内原文加密存到任何地方——彻底丢弃。用户需要保留私密笔记走其他渠道
- **不**做 false-positive auditing——一旦被正则命中，内容直接替换，不记录原串
- `PrivacyConfig` 读取自 `~/.mempal/config.toml` 的 `[privacy]` section
- 清洗应用于 `drawer.content` + `drawer.summary`（如果存在）+ `ingest_request.metadata` 中的任何 free-text 字段
- 所有 `anyhow::Error` 转换发生在 `mempal-ingest` 边界；`privacy.rs` 内部用 `thiserror::Error` 定义 `PrivacyError`（`InvalidPattern`、`CompileFailed`）

## Boundaries

### Allowed
- `crates/mempal-ingest/src/privacy.rs`（新建）
- `crates/mempal-ingest/src/pipeline.rs`（集成 scrub 调用）
- `crates/mempal-ingest/src/lib.rs`（`pub mod privacy`）
- `crates/mempal-ingest/Cargo.toml`（无新增依赖——`regex` 已是 workspace dep）
- `crates/mempal-core/src/config.rs`（`PrivacyConfig` struct + `[privacy]` parsing）
- `crates/mempal-cli/src/main.rs`（`mempal status` 输出加 scrub stats 行）
- `tests/privacy_scrubbing.rs`（新建集成测试）

### Forbidden
- 不要给 `drawers` / `drawer_vectors` / `triples` 加字段
- 不要 bump `CURRENT_SCHEMA_VERSION`
- 不要引入 `regex-automata` 之外的新 regex 库
- 不要在 `privacy.rs` 外 import `regex::Regex` 做凭证匹配（集中管理）
- 不要在 `mempal-search` / `mempal-aaak` / `mempal-mcp` / `mempal-api` 任何 crate 里调 `privacy::scrub`——清洗只发生在 ingest 边
- 不要做 retroactive scrub（已存 drawer 的清洗是独立后续工具 `mempal sanitize`，P8+ 之外）
- 不要加 LLM-based 敏感词检测（违反 no-LLM-API feedback）
- 不要修改 `mempal_ingest` MCP tool schema（`scrub_stats` 走 response metadata，不是顶层字段）

## Out of Scope

- 对已存 drawer 的回溯清洗（将来的 `mempal sanitize --dry-run / --apply` 工具）
- 按 wing/room 的 per-scope policy override
- 加密存储敏感区块（differential privacy / k-anonymity 风格）
- 把 strip tag / pattern match 结果写入独立审计表（非目标——mempal 不做合规日志）
- 扫已有 palace.db 检测"可能泄露"内容（独立 tool）
- UI/CLI 提示用户某次 ingest 被 scrub 了多少——仅走 stats
- 配置热重载（进程内改 config 需要重启）

## Completion Criteria

Scenario: privacy.enabled=false 时行为零变化
  Test:
    Filter: test_privacy_disabled_preserves_content_byte_identical
    Level: integration
    Targets: crates/mempal-ingest/src/privacy.rs, crates/mempal-ingest/src/pipeline.rs
  Given 一段含 `sk-abcdef1234567890abcdef1234567890abcd` 的文本
  And `[privacy]` section `enabled = false`
  When 走 `ingest::pipeline::ingest` 写入 palace.db
  Then 读出的 drawer `content` 字段 byte-level 等于原文
  And 不存在 `[REDACTED:` 替换
  And embedding 输入等于原文

Scenario: `<private>` 标签被整块剥离
  Test:
    Filter: test_private_tag_block_stripped
    Level: unit
    Targets: crates/mempal-ingest/src/privacy.rs
  Given 文本 `"Here is the key: <private>sk-1234</private> done"`
  And `strip_tags = ["private"]`、`enabled = true`
  When 调 `privacy::scrub(text, &cfg)`
  Then 返回的清洗文本不含 `<private>` 也不含 `</private>` 也不含 `sk-1234`
  And `ScrubStats.tag_matches["private"]` == 1

Scenario: OpenAI sk- 格式 key 被替换为 placeholder
  Test:
    Filter: test_openai_key_scrubbed_to_placeholder
    Level: unit
    Targets: crates/mempal-ingest/src/privacy.rs
  Given 文本 `"my key is sk-abcdef1234567890abcdef1234567890abcd_more"`
  And 默认 pattern 库启用 `openai_key`
  When 调 `privacy::scrub`
  Then 返回文本含 `[REDACTED:openai_key]` 而非原 key
  And `ScrubStats.pattern_matches["openai_key"]` == 1

Scenario: AWS access key 被替换
  Test:
    Filter: test_aws_access_key_scrubbed
    Level: unit
    Targets: crates/mempal-ingest/src/privacy.rs
  Given 文本 `"access: AKIAIOSFODNN7EXAMPLE in logs"`
  When 调 `privacy::scrub` with 默认 pattern 启用
  Then 返回文本含 `[REDACTED:aws_access]`
  And 不含 `AKIAIOSFODNN7EXAMPLE`

Scenario: embedding 和 FTS 收到的是清洗后文本
  Test:
    Filter: test_embedding_receives_scrubbed_text
    Level: integration
    Test Double: recording_embedder
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given `PrivacyConfig { enabled: true, ... }` 及含 `sk-abcdef...` 的 ingest payload
  And `Embedder` 实现捕获每次 `embed` 调用的 input 参数
  When 执行完整 `ingest::pipeline::ingest`
  Then `embedder` 收到的 text 不含原 `sk-` 字串
  And `embedder` 收到的 text 含 `[REDACTED:openai_key]`

Scenario: drawer.content 存的是清洗后文本（raw 不变性保持——清洗后的文本即是 raw）
  Test:
    Filter: test_drawer_content_stores_scrubbed_text
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given `PrivacyConfig.enabled = true` 及原文 `"key=sk-abcdef1234567890abcdef1234567890abcd end"`
  When 走 ingest 并查询回 drawer
  Then `drawer.content` 等于 `"key=[REDACTED:openai_key] end"`
  And 不存在任何字段保留原始 key

Scenario: 无效 regex pattern 在 config 加载时 fail-fast
  Test:
    Filter: test_invalid_regex_pattern_fails_config_load
    Level: unit
    Targets: crates/mempal-core/src/config.rs, crates/mempal-ingest/src/privacy.rs
  Given `[privacy]` section 含 `pattern = "("`（不合法 regex）
  When 加载配置
  Then 返回 `Err(PrivacyError::CompileFailed)` 或等价错误
  And mempal 进程不启动（CLI 退出码 != 0）

Scenario: scrub stats 在 mempal status 输出
  Test:
    Filter: test_status_command_shows_scrub_stats
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given `enabled = true` 后执行 3 次 ingest，其中 2 次命中 `openai_key`、1 次命中 `private` tag
  When 运行 `mempal status`
  Then stdout 含一行 `scrub: openai_key=2 private=1` 或等价结构化汇总

Scenario: 清洗不影响 drawer 计数和 vector 维度
  Test:
    Filter: test_scrub_does_not_affect_storage_invariants
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given `enabled = true`，ingest "5" 条不同内容，其中 2 条被完全 scrub 为空字符串后的残留片段仍 >= chunking 最小长度
  When ingest 完成后查询 `drawers` 表
  Then `drawer` 条数为 "5"（scrub 不触发丢弃）
  And 每条 `drawer_vectors` 维度一致（256d for potion-multilingual-128M）

Scenario: 跨 chunk 边界的 secret 仍被 scrub（pre-chunk 顺序断言）
  Test:
    Filter: test_scrub_catches_cross_chunk_secret
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs, crates/mempal-ingest/src/privacy.rs
  Given `enabled = true` 且默认 `openai_key` pattern 启用
  And chunk 上限调小到 50 字节（测试夹具强制触发分块）
  And ingest payload：`"long preamble of ~48 bytes then sk-abcdef1234567890abcdef1234567890abcd rest..."` — 关键 secret **恰好跨越 chunk 边界**（48 字节前缀 + sk-key）
  When 走 `ingest::pipeline::ingest`
  Then 所有产生的 chunk 文本均不含 `sk-abcdef1234567890abcdef1234567890abcd` 原字面
  And 至少一个 chunk 含 `[REDACTED:openai_key]`
  And `embedder` 收到的每个 chunk 输入也不含原 secret 字面
  And `ScrubStats.pattern_matches["openai_key"]` >= 1

Scenario: 无新增外部运行时依赖
  Test:
    Filter: test_no_new_runtime_dependencies_introduced
    Level: static
    Targets: crates/mempal-ingest/Cargo.toml
  Given `crates/mempal-ingest/Cargo.toml` P7 版本作为 baseline
  When 应用 P8 privacy spec
  Then `[dependencies]` section 只新增 `regex = { workspace = true }` 或 zero 新依赖
  And 不引入 `rand`、`lazy_static`、`once_cell` 之外的 helper crate（已在 workspace）

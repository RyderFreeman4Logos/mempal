spec: task
name: "P9: Ingest gating with local-only rules + vector prototype classifier (NO external LLM API)"
tags: [feature, gating, ingest, local-ml, noise-reduction]
estimate: 2d
---

## Intent

在 hook-based passive capture（P8）和 `mempal_ingest` 之间插入一个**本地判决门控**，把低价值事件（纯 Read/Glob/Grep、亚 50 字节输出、退出码即写入的短文本）过滤掉，**不**让它们污染 `drawers` 表和向量索引。

三层设计（但**硬性只做前两层**，本地 + LAN 部署可接受）：
- **Tier 1 静态规则**：按工具名、字节长度、退出码黑白名单（零计算成本）
- **Tier 2 向量原型分类器**：把一组 user-defined 类别原型（"architectural-decision"、"bug-discovery"、"user-preference" 等）预先 embed，把候选 payload 的 embedding 与每个原型做余弦相似度，取最大值，若低于 `threshold` 直接 skip；这一层**复用** mempal 当前配置的 `Embedder`（默认 P8 新 `OpenAiCompatibleEmbedder` 指向 LAN 的 Qwen3-Embedding-8B，见 `p8-embed-qwen3-backend.spec.md`；offline 可回退 `model2vec-rs`），**零额外模型**，**零云 API**
- **Tier 3 云 LLM judge**：**永久禁用**。Issue 原文含 Tier 3 `backend = "api" | "local"`，按 `feedback_no_llm_api_dependency.md` **云 API backend 永不实现**；`local` / LAN 部署的 tiny-LLM judge 因为引入新进程管理依赖推迟到 P11+ 独立 spec

**动机**：没有 gating 的情况下，P8 hook 会把 agent 的每次 `ls`、`grep`、`cat` 都变成一条 drawer，数百 MB 的垃圾会把 BM25 和向量召回质量拖垮。Tier 1 能砍 70% 噪声，Tier 2 再砍 20%，剩下 10% 直接留给 agent 自己判断或人工审计。

**Default off**：`[ingest_gating] enabled = false`，opt-in 激活。

**v3 判决依据**：v2 REJECT 的唯一理由是"claude-mem 没做"——这不是判决依据（见 `feedback_feature_value_not_origin.md`）。独立工程价值：配合 P8 Hook 的必备保护层。

## Decisions

- 新建 `crates/mempal-ingest/src/gating/mod.rs`、`crates/mempal-ingest/src/gating/rules.rs`、`crates/mempal-ingest/src/gating/prototypes.rs`
- 暴露统一入口 `pub async fn judge(candidate: &IngestCandidate, cfg: &GatingConfig, embedder: &dyn Embedder) -> GateDecision`
- `GateDecision` 枚举：`Keep { tier, label }`、`Skip { tier, reason }`、`Unclassified`（极少见——仅当所有 tier 都 disabled）
- `IngestCandidate` 是 ingest 管道内部的归一化结构：`{ tool_name: Option<String>, content_bytes: usize, exit_code: Option<i32>, content: String }`
- Tier 1 规则定义（`GatingConfig.rules: Vec<Rule>`）：
  ```toml
  [[ingest_gating.rules]]
  match = { tool = "Read" }
  action = "skip"
  
  [[ingest_gating.rules]]
  match = { content_bytes_lt = 50 }
  action = "skip"
  ```
- `Rule.match` 支持字段：`tool: Option<String>`、`tool_in: Option<Vec<String>>`、`content_bytes_lt: Option<usize>`、`content_bytes_gt: Option<usize>`、`exit_code_eq: Option<i32>`
- `Rule.action`：`"skip"` | `"keep"` | `"continue"`（后者表示不判决，交给下一 tier）
- 多条规则**短路求值**：第一个 match 决定结果（`continue` 例外，会继续下一条）
- Tier 2 原型分类器：
  - 配置 `[ingest_gating.embedding_classifier] enabled = true, threshold = 0.35, prototypes = [...]`
  - 启动时 `Prototype { label, embed_vec }` 预计算（embed 阻塞一次）存内存
  - 判决：计算 candidate embedding → 对每个 prototype 求 cosine → 取 max
  - max >= threshold → `Keep { tier: 2, label }`
  - max < threshold → `Skip { tier: 2, reason: "below_threshold" }`
- 内置默认 prototypes（用户可覆盖）：`architectural-decision`, `bug-discovery`, `user-preference`, `experiment-result`, `anti-pattern`, `user-correction`, `library-pick`
- 向量 dim 必须和 embedder 输出 dim 一致，编译时 check
- Gating stats：`GatingStats { tier1_kept, tier1_skipped, tier2_kept, tier2_skipped, unclassified }`，暴露到 `mempal status` 和新 CLI `mempal gating stats [--since <duration>]`
- Gating 决策写入 `gating_audit` 表（`id`, `candidate_hash`, `decision`, `tier`, `label`, `created_at`, `retained_until` 默认 7d 滚动清理）供 `mempal gating stats` 查询
- schema bump v5 → v6（新增 `gating_audit` 表）
- **禁止**任何 HTTP client 调用外部 LLM API——Tier 3 的 API backend 根本不实现；`GatingConfig` 解析时若见到 `[ingest_gating.llm_judge]` section 输出 warn 并忽略
- 错误处理：`GatingError`（`thiserror`），embedder 失败时 `judge` 回退到 `Keep { tier: 0, label: "embedder_error" }`（fail-open，宁可多存也不丢）
- judge 是 async（因为 embedder trait 是 async），在 ingest pipeline 中被 `.await` 调用

## Boundaries

### Allowed
- `crates/mempal-ingest/src/gating/` 整个子目录（新建）
- `crates/mempal-ingest/src/pipeline.rs`（插入 judge 调用点：privacy scrub 之后、chunking 之前）
- `crates/mempal-ingest/src/lib.rs`（`pub mod gating`）
- `crates/mempal-core/src/config.rs`（`GatingConfig` struct + `[ingest_gating]` parsing）
- `crates/mempal-core/src/db/schema.rs`（v5 → v6 migration + `gating_audit` DDL）
- `crates/mempal-cli/src/main.rs`（`mempal gating stats` 子命令）
- `tests/gating_rules.rs`, `tests/gating_prototypes.rs`, `tests/gating_integration.rs`（新建）

### Forbidden
- 不要引入 `reqwest` / `hyper` 以外的 HTTP client 给 gating（且这两个在 gating 里也不得调外部 API）
- 不要为 Tier 3 cloud LLM backend 预留代码框架——整块砍掉
- 不要修改 `Embedder` trait（复用现有）
- 不要给 `drawers` / `drawer_vectors` 新增字段表达 gating 决策（走独立 `gating_audit` 表）
- 不要让 gating 决策进入 AAAK signal 提取结果
- 不要在 MCP 工具（`mempal_ingest` 或其他）里暴露 "force skip gating" 参数——用户想强制就调 config 临时关 gating
- 不要做 Tier 2 threshold 自适应（user-tuned 值即可，YAGNI）
- 不要在 prototypes 初始化失败时崩溃进程——若 embedder 对某个 prototype embed 失败，log warn 并 skip 该 prototype

## Out of Scope

- Tier 3 本地 LLM judge（独立 P11+ spec：`p11-llm-judge-local.spec.md`，需要 ollama/llama.cpp 集成）
- 任何外部 LLM API 客户端（`feedback_no_llm_api_dependency.md` 硬禁止）
- 基于 drawer 历史的自适应 threshold 调优
- Per-wing gating policy override
- 决策回溯（允许人工复审被 skip 的 candidate）——走审计表足够，无需 UI
- 把 gating stats 推到 Prometheus / OTel
- Novelty filter（独立 `p9-novelty-filter.spec.md`，与 gating 串联但职责分离）

## Completion Criteria

Scenario: Tier 1 规则匹配 Read 工具并 skip
  Test:
    Filter: test_tier1_skips_read_tool
    Level: unit
    Targets: crates/mempal-ingest/src/gating/rules.rs
  Given `GatingConfig.rules` 第一条 `match = { tool = "Read" }, action = "skip"`
  When 调 `judge(&candidate(tool="Read"), &cfg, &embedder)`
  Then 返回 `GateDecision::Skip { tier: 1, reason: "rule_match" }`
  And `embedder` 未被调用

Scenario: Tier 1 content_bytes_lt 规则 skip 超短内容
  Test:
    Filter: test_tier1_skips_short_content
    Level: unit
    Targets: crates/mempal-ingest/src/gating/rules.rs
  Given 规则 `match = { content_bytes_lt = 50 }, action = "skip"` 和 30 字节内容候选
  When 调 `judge`
  Then 返回 `Skip { tier: 1, reason: "rule_match" }`

Scenario: Tier 2 向量原型 cosine >= threshold 时 Keep
  Test:
    Filter: test_tier2_keeps_above_threshold
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/gating/prototypes.rs
  Given `threshold = 0.5`，prototypes 含 "architectural-decision"
  And candidate content 与 "architectural-decision" 原型 cosine == 0.7（mock）
  When 调 `judge`
  Then 返回 `Keep { tier: 2, label: "architectural-decision" }`

Scenario: Tier 2 cosine < threshold 时 Skip
  Test:
    Filter: test_tier2_skips_below_threshold
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/gating/prototypes.rs
  Given `threshold = 0.5`，所有 prototype cosine == 0.3（mock）
  When 调 `judge`
  Then 返回 `Skip { tier: 2, reason: "below_threshold" }`

Scenario: enabled=false 时 judge 短路返回 Keep
  Test:
    Filter: test_gating_disabled_short_circuits
    Level: unit
    Targets: crates/mempal-ingest/src/gating/mod.rs
  Given `GatingConfig.enabled = false`
  When 调 `judge(&any_candidate, &cfg, &embedder)`
  Then 返回 `Keep { tier: 0, label: "gating_disabled" }`
  And `embedder` 未被调用

Scenario: embedder 错误时 fail-open Keep
  Test:
    Filter: test_embedder_error_fail_open
    Level: integration
    Test Double: failing_embedder
    Targets: crates/mempal-ingest/src/gating/prototypes.rs
  Given Tier 2 enabled，embedder 对 candidate embed 时返回 `Err`
  When 调 `judge`
  Then 返回 `Keep { tier: 0, label: "embedder_error" }`
  And stderr/log 含 warn 级别错误信息

Scenario: 配置含 [ingest_gating.llm_judge] section 时 warn 并忽略
  Test:
    Filter: test_llm_judge_section_warns_and_ignores
    Level: integration
    Targets: crates/mempal-core/src/config.rs
  Given `config.toml` 含 `[ingest_gating.llm_judge] enabled = true, backend = "api"`
  When 加载 config
  Then 加载成功（不 fail-fast）
  And stderr 含 `"llm_judge tier ignored: external LLM API disabled by design"` 或等价 warn
  And `GatingConfig.llm_judge` 为 None 或 disabled

Scenario: gating_audit 表记录决策
  Test:
    Filter: test_gating_audit_records_decisions
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs, crates/mempal-core/src/db/schema.rs
  Given `enabled = true`，执行 10 次 ingest，预期 6 skip + 4 keep
  When 查询 `SELECT decision, COUNT(*) FROM gating_audit GROUP BY decision`
  Then 返回 `skip=6 keep=4`

Scenario: mempal gating stats 输出 kept/skipped 计数
  Test:
    Filter: test_gating_stats_cli_output
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given `gating_audit` 表有最近 7d 的 10 keep + 20 skip
  When 执行 `mempal gating stats --since 7d`
  Then stdout 含 `kept: 10` 和 `skipped: 20`
  And 按 tier 分解

Scenario: schema 迁移 v5 → v6 创建 gating_audit 表
  Test:
    Filter: test_migration_v5_to_v6_creates_gating_audit
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db schema_version == "5"
  When 启动 mempal
  Then schema_version == "6"
  And `gating_audit` 表存在

Scenario: gating 不影响 drawer 维度一致性
  Test:
    Filter: test_gating_preserves_vector_dim_consistency
    Level: integration
    Targets: crates/mempal-ingest/src/pipeline.rs
  Given 连续 ingest 5 条 candidate，3 条被 skip
  When 查询 `drawer_vectors` 表
  Then 存在的 2 条 vector 维度一致（256d）
  And 无 skip 的 candidate 残留在任何表

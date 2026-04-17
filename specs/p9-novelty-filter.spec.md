spec: task
name: "P9: Novelty filter — vector-similarity deduplication in ingest pipeline"
tags: [feature, dedup, ingest, vector]
estimate: 1d
---

## Intent

在 ingest 管道里加一个**向量相似度去重层**：candidate 写入前，和**同 wing** 内最相近的既有 drawer 做 cosine 对比：
- `cosine >= duplicate_threshold` → drop（skip 存储）
- `cosine >= merge_threshold && < duplicate_threshold` → merge（把 candidate content 追加到既有 drawer 的 `content` 末尾作 supplementary note，`updated_at` bump，`merge_count += 1`）
- `cosine < merge_threshold` → 正常插入新 drawer

**动机**：P8 hook + P9 gating 组合后，同一个 session 内 agent 会反复说相似的话（每次 tool call checkpoint 一条，同主题每 2-3 分钟一条）。不去重的话，palace 几个月后充满近似重复，向量召回的 top-k 全是同一件事的多个表述，BM25 也被噪声占位。

**v3 判决依据**：Issue #6 含三个 innovation，本 spec 只吸收 Novelty Filter 一项（纯本地向量计算，零 LLM，纯增强），拆分为独立 spec。Causal-chain extraction 违反 raw verbatim 推迟到 P11+，Session Self-Review 独立成 `p9-session-self-review.spec.md`。

**与 P5 `p5-semantic-dedup.spec.md` 的关系（关键区分）**：P5 是 **warning-only**——检测到语义接近的既有 drawer 时，`mempal_ingest` 只在 response 里附 warning，仍然正常写入新 drawer，用户自行决定是否手动清理。本 P9 Novelty Filter 是 **admission control**——在 passive capture 高流量场景下，判决结果真实影响存储（drop / merge / insert），无法事后撤销。两者互补：P5 服务于 explicit ingest 路径的审慎提示，P9 服务于 passive capture 路径的自动治理。**P5 warning 永远不被 P9 禁用**；P9 触发 drop/merge 的决策还会额外在 `novelty_audit` 记录一笔（P5 warning 去处不变）。

**Default off**：`[ingest_gating.novelty] enabled = false`，opt-in。

## Decisions

- 新建 `crates/mempal-ingest/src/novelty.rs`
- 暴露 `pub async fn filter(candidate: &IngestCandidate, embedding: &[f32], store: &DrawerStore, cfg: &NoveltyConfig) -> NoveltyDecision`
- `NoveltyDecision` 枚举：`Insert`（新 drawer）、`Merge { into: DrawerId, cosine: f32 }`、`Drop { near: DrawerId, cosine: f32 }`
- 配置：
  ```toml
  [ingest_gating.novelty]
  enabled = false
  duplicate_threshold = 0.95   # cosine >= → drop
  merge_threshold = 0.80       # cosine [0.80, 0.95) → merge
  wing_scope = "same_wing"     # "same_wing" | "same_room" | "global"
  top_k_candidates = 5         # 从 vec 索引取 top-k 后选最大
  ```
- 查询策略：用 candidate embedding 做 `sqlite-vec` 的 top-k vector search，限定 `wing`（依 `wing_scope`）；对返回的 k 个结果取 cosine max
- Merge 实现：
  - 既有 drawer `content` 末尾追加分隔符 `\n---\nSUPPLEMENTARY ({timestamp}):\n` + candidate content
  - `updated_at = now()`
  - `merge_count` 列 +1（schema v7 新增）
  - **重新计算** embedding（合并后内容变化，旧 embedding 不再准确）：embed new `content`，UPDATE `drawer_vectors`
  - FTS5 index 自动随 `drawers.content` UPDATE 触发同步（如现有 trigger 已覆盖）
  - **保留** drawer ID 不变（KG triples 引用稳定）
- 去重触发后 write 到 `novelty_audit` 表（schema v7 新增）：`{ id, candidate_hash, decision, near_drawer_id, cosine, created_at }`，`mempal status` 汇总
- 对 `mempal_ingest` MCP 工具调用方：返回 response metadata 含 `novelty_decision: "inserted" | "merged" | "dropped"`, `near_drawer_id`（如适用）；content 语义保持 raw，不影响字段形态
- schema v6 → v7 bump：
  - `drawers` 表加 `merge_count INTEGER NOT NULL DEFAULT 0`
  - 新建 `novelty_audit` 表
- 错误处理：novelty filter 失败（embedder/db 错误）→ fail-open，当作 `Insert`
- cosine 实现：纯 Rust f32 点积 / norm 乘积，不引入新依赖
- 对 `IngestCandidate` 结构和 `Embedder` trait 无变更

## Boundaries

### Allowed
- `crates/mempal-ingest/src/novelty.rs`（新建）
- `crates/mempal-ingest/src/pipeline.rs`（novelty filter 调用点：embedding 计算后、drawer 插入前）
- `crates/mempal-ingest/src/lib.rs`（`pub mod novelty`）
- `crates/mempal-core/src/config.rs`（`NoveltyConfig` struct）
- `crates/mempal-core/src/db/schema.rs`（v6 → v7）
- `crates/mempal-mcp/src/tools.rs`（`IngestResponseDto` 加可选 `novelty_decision` / `near_drawer_id` 字段）
- `crates/mempal-mcp/src/server.rs`（handler 传递 novelty 决策到 response）
- `tests/novelty_filter.rs`（新建）

### Forbidden
- 不要禁用或取代 P5 `p5-semantic-dedup.spec.md` 的 warning 机制——P5 是 explicit ingest 路径的审慎提示，P9 是 passive 路径的 admission control，二者并行不冲突
- 不要让 P9 novelty 决策在 `mempal_ingest` MCP response 里替代 P5 warning 字段——各自独立
- 不要引入新向量库（`sqlite-vec` + 现有 embedder 足够）
- 不要 per-ingest 对全库做 O(N) cosine——必须用 `sqlite-vec` 的 top-k
- 不要在 merge 时覆盖既有 drawer `content`——只追加
- 不要 merge 时丢弃既有 embedding——必须重新计算
- 不要让 novelty 决策破坏 drawer ID 稳定性（triples 引用不能悬空）
- 不要让 `mempal_search` 因 novelty audit 改变召回行为
- 不要把 merged supplementary content 写到 `summary` 字段（summary 不是 supplementary 堆栈）
- 不要对 `wing=agent-diary` 的 drawer 应用 novelty filter（agent diary 可能有合理的重复模式，单独策略——本 spec 直接 bypass）

## Out of Scope

- Causal-chain extraction（Issue #6 Innovation 2，推迟到 P11+ 独立 spec）
- Session Self-Review extraction（独立 `p9-session-self-review.spec.md`）
- 跨 wing 的语义合并（只做 same_wing / same_room / global 选一）
- Merge 时的 KG triple 自动迁移（既有 triples 指向 drawer_id 不变，无需迁移）
- Novelty 阈值的自适应调优
- 回溯去重（已存 drawer 的批量去重是独立工具 `mempal dedup --apply`）
- UI 预览哪些被去重了——走 `novelty_audit` 表 SQL 查询

## Completion Criteria

Scenario: 严重相似的 candidate 被 drop
  Test:
    Filter: test_high_similarity_candidate_dropped
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/novelty.rs
  Given wing="code-memory" 内既有 drawer A (content "Decision: Arc<Mutex<>>")
  And 配置 `duplicate_threshold = 0.95`
  And candidate embedding 与 A 的 cosine == 0.97
  When 走 ingest 管道
  Then `drawers` 表中 A 之外无新增
  And `novelty_audit` 表新增 1 行，`decision = "drop"`，`near_drawer_id = A.id`，`cosine == 0.97`
  And ingest MCP response 的 `novelty_decision == "dropped"`

Scenario: 中等相似度 candidate 被 merge 到既有 drawer
  Test:
    Filter: test_medium_similarity_candidate_merged
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/novelty.rs
  Given drawer A content "Decision: Arc<Mutex<>>"
  And `merge_threshold = 0.8`, `duplicate_threshold = 0.95`
  And candidate content "Also: use RwLock when reads dominate" 与 A cosine == 0.85
  When 走 ingest
  Then `drawers` 行数不变（仅 A，无新行）
  And A.content 含原文 "Decision: Arc<Mutex<>>"
  And A.content 含 supplementary 分隔符和 "Also: use RwLock when reads dominate"
  And A.merge_count == 1
  And A.updated_at 大于 A.created_at
  And `drawer_vectors` 中 A.embedding 已更新为合并后内容的 embedding

Scenario: 低相似度 candidate 正常插入新 drawer
  Test:
    Filter: test_low_similarity_candidate_inserted
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/novelty.rs
  Given drawer A in wing, candidate cosine to A == 0.6
  When 走 ingest
  Then `drawers` 新增 1 行
  And `novelty_audit` 新增 1 行，`decision = "insert"`

Scenario: enabled=false 时跳过 novelty filter
  Test:
    Filter: test_novelty_disabled_skips_filter
    Level: unit
    Targets: crates/mempal-ingest/src/novelty.rs
  Given `NoveltyConfig.enabled = false`
  When 走 ingest
  Then 所有 candidate 无条件插入为新 drawer
  And `novelty_audit` 表无新增（表可不存在或空）

Scenario: merge 保留 drawer ID 稳定（KG triples 不悬空）
  Test:
    Filter: test_merge_preserves_drawer_id_for_kg
    Level: integration
    Targets: crates/mempal-ingest/src/novelty.rs, crates/mempal-core
  Given drawer A 已存在 + 一条 triple `(subj=drawer:A, pred="implies", obj=Y)`
  When merge 另一条 candidate 到 A
  Then A.id 保持不变
  And `triples` 表中该 triple 的 subj 仍为 A.id
  And triple 未被删除

Scenario: wing_scope="same_wing" 严格限制搜索范围
  Test:
    Filter: test_wing_scope_respected
    Level: integration
    Test Double: deterministic_embedder
    Targets: crates/mempal-ingest/src/novelty.rs
  Given wing=X 中 drawer A（cosine to candidate 0.96）
  And wing=Y 中 drawer B（cosine to candidate 0.96）
  And candidate 的目标 wing == X
  And `wing_scope = "same_wing"`
  When 走 ingest
  Then candidate 被 drop 进入 A（same wing）
  And B 完全不参与 cosine 比较（audit log 无 B 痕迹）

Scenario: wing=agent-diary bypass novelty
  Test:
    Filter: test_agent_diary_bypasses_novelty
    Level: integration
    Targets: crates/mempal-ingest/src/novelty.rs
  Given `enabled = true`，agent-diary wing 中已有 drawer X
  And candidate 的 wing == "agent-diary"，cosine to X == 0.98
  When 走 ingest
  Then candidate 作为新 drawer 插入（未被 drop）
  And `novelty_audit` 无本次决策行（agent-diary bypass）

Scenario: embedder 错误时 fail-open 插入
  Test:
    Filter: test_novelty_embedder_error_fails_open
    Level: integration
    Test Double: failing_embedder
    Targets: crates/mempal-ingest/src/novelty.rs
  Given novelty enabled，embedder 对 candidate embed 成功、对 prototype embed 失败
  When 走 ingest
  Then candidate 正常插入为新 drawer
  And stderr/log 含 warn

Scenario: schema 迁移 v6 → v7 加 merge_count 和 novelty_audit
  Test:
    Filter: test_migration_v6_to_v7_schema
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given palace.db schema_version == "6"
  When 启动 mempal
  Then schema_version == "7"
  And `drawers` 表有 `merge_count` 列，默认值 0
  And `novelty_audit` 表存在

Scenario: merge 后 FTS5 搜索能命中新增内容
  Test:
    Filter: test_fts_finds_merged_supplementary
    Level: integration
    Targets: crates/mempal-ingest/src/novelty.rs, crates/mempal-search
  Given drawer A content "foo decision", merge 一条 "bar addition" 进 A
  When 用 `mempal_search` query "bar addition"
  Then 返回结果含 drawer A（FTS 索引含合并后文本）

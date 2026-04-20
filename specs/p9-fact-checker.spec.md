spec: task
name: "P9: mempal_fact_check — offline contradiction detection against entity registry + KG triples"
tags: [feature, mcp, kg, fact-check, offline]
estimate: 1.0d
---

## Intent

借鉴 mempalace 4a6147f (`fact_checker.py`) 的离线事实核查思路，基于 mempal 已有的 `triples` 表（subject/predicate/object + `valid_from`/`valid_to`）和 P7 的 AAAK entities signals，新增 MCP 工具 `mempal_fact_check`，在纯本地、零网络、零 LLM 依赖下检测文本中的三类矛盾：

1. **相似名混淆**（Bob vs Bobby）— 基于已知 entity 的 fuzzy match
2. **KG 关系对立**（KG 说 husband，文本说 brother）— 三元组 predicate/object 不一致
3. **时态失效**（KG `valid_to` 已过期，文本仍当前断言）— 基于温度验证

核心用户价值：agent 在 ingest 前或 search 返回后，可以对候选文本做一次 sanity check，避免把矛盾决策写进 drawer 或据此作答。契合 mempal 的 local-first + 验证优先于断言（Protocol Rule 2）。

为什么不做 mempalace 的 closet layer：见 `drawer_mempal_default_d1e88c52`（2026-04-11 decision，AAAK 出端 on-demand 替代）。本 spec 用 KG 路径实现等价的"可搜索索引 → 验证"能力。

## Decisions

- **新增 MCP 工具**：`mempal_fact_check`，工具总数 9 → 10
- **MCP 工具签名**：`FactCheckRequest { text: String, wing: Option<String>, room: Option<String>, now: Option<String> (RFC3339, 默认 current UTC) }` → `FactCheckResponse { issues: Vec<FactIssue>, checked_entities: Vec<String>, kg_triples_scanned: usize }`
- **`FactIssue` 枚举**：
  - `SimilarNameConflict { mentioned: String, known_entity: String, edit_distance: usize }` — 文本中的名字和 KG/drawer entities 中已知名字 Levenshtein 距离 ≤ 2 且不等
  - `RelationContradiction { subject: String, text_claim: String, kg_fact: String, triple_id: String, source_drawer: Option<String> }` — 文本断言的 predicate 和 KG 中同 subject+object 对的现存 triple 的 predicate 不兼容
  - `StaleFact { subject: String, predicate: String, object: String, valid_to: String, triple_id: String }` — 文本仍断言一个 `valid_to < now` 的 KG fact；`valid_to` 按 DB 当前约定保留 Unix seconds 字符串
- **新模块 `src/factcheck/`**：
  - `mod.rs` — pub API `check(text: &str, db: &Db, now_unix_secs: u64, scope: Option<(&str, Option<&str>)>) -> Result<FactCheckReport, FactCheckError>`；handler/CLI 在边界层把 RFC3339 `now` 解析为 Unix seconds
  - `names.rs` — 名字抽取（复用 `src/aaak/signals.rs` 的 entity extraction 逻辑，不重写）+ Levenshtein 距离（纯 std 实现，不引 `strsim` crate）
  - `relations.rs` — 文本到 (subject, predicate, object) 的启发式抽取（正则 + 常见句型：`X is Y's Z` / `X married Y` 等；**不**做 NLP 深解析）
  - `contradictions.rs` — 对抽出的三元组逐一查 KG（复用 `db.query_triples`）做 predicate 冲突判定
- **Predicate 冲突矩阵**：硬编码 `INCOMPATIBLE_PREDICATES: &[(&str, &str)]` 小表，如 `("husband", "brother")`, `("mother", "wife")`, `("employee", "founder")`。不做语义推理，只做字典对比。**可扩展**：将来可以通过配置文件覆盖
- **时态检查来源**：复用 `db.query_triples` 传 `Some(now)` 作为时间点，返回带 `valid_to` 的 rows；text 抽出的三元组如命中一个已 expire 的 triple 就是 StaleFact
- **Entity 名字来源（双路）**：
  1. KG：`SELECT DISTINCT subject FROM triples UNION SELECT DISTINCT object FROM triples WHERE object REGEXP '^[A-Z]'`
  2. Drawer signals：P7 的 `extract_entities` 复用，从指定 scope 下最近 50 个 drawer 聚合（upper-bound 限制避免 O(N) 爆炸）
- **性能预算**：单次 `check()` 处理 8 KB 文本 + 1000 triples 在 p95 100 ms 以内；超预算作为性能优化议题，不引入超时中断或部分结果返回
- **CLI 子命令**：`mempal fact-check <path|-> [--wing w] [--room r] [--now <rfc3339>]`，读文件或 stdin，输出 pretty JSON issues
- **Protocol Rule 11 新增**：在 `src/core/protocol.rs` 的 `MEMORY_PROTOCOL` 追加 Rule 11 "VERIFY BEFORE INGEST"，指引 agent 在 ingest 前对关键决策文本先跑 fact_check，发现 SimilarNameConflict 尤其要和用户确认
- **零 LLM 依赖**：不调外部 API，不依赖 onnx / model2vec；所有检测都是确定性规则
- **不改 schema**：不 bump `CURRENT_SCHEMA_VERSION`（保持 v4）

## Boundaries

### Allowed
- `src/factcheck/mod.rs` / `names.rs` / `relations.rs` / `contradictions.rs`（新增）
- `src/lib.rs`（追加 `pub mod factcheck`）
- `src/mcp/tools.rs`（追加 `FactCheckRequest` / `FactCheckResponse` / `FactIssue` DTO）
- `src/mcp/server.rs`（追加 `mempal_fact_check` handler）
- `src/main.rs`（追加 `fact-check` 子命令）
- `src/core/protocol.rs`（追加 Rule 11 + Rule 0 工具表 9→10）
- `tests/fact_check.rs`（新增集成测试文件）

### Forbidden
- 不引入新的 runtime/dev dependency（不装 `strsim` / `textdistance` / `nlp` / `regex-automata`，已有的 `regex` + `std` 足够）
- 不改 `triples` 表 schema 或 `db.query_triples` 签名
- 不改 P7 `src/aaak/signals.rs` 的 entity/topic extraction 公共 API
- 不改 `mempal_search` / `mempal_ingest` / `mempal_kg` 等现有 10 个 MCP 工具的 schema
- 不在 KG 或 drawer 上做写操作（fact_check 是纯 read）
- 不调外部 HTTP / LLM / embedding API
- 不 bump schema version

## Out of Scope

- 自动修正文本（只报 issue，不改写）
- 基于 LLM 的语义相似性（用 Levenshtein + 预置 predicate 字典足够）
- 跨语言相似名（中文名 fuzzy 不做，只 ASCII）
- 从 free text 自动抽新的三元组写回 KG（那是独立 spec 的职责）
- Ingest pipeline 的自动 fact-check 阻断（Protocol 建议 agent 做，但不强制在代码层阻断）
- 批量文本 / 流式检查
- 检查结果 cache

## Completion Criteria

Scenario: 相似名检测能识别 edit distance 2 以内的拼写错误
  Test:
    Filter: test_similar_name_conflict_detected
    Level: integration
  Given KG 中存在 triple `(Bob, husband_of, Alice)`
  When 调用 `check(text="Bobby is Alice's husband", now=...)`
  Then 返回的 issues 包含 `SimilarNameConflict { mentioned: "Bobby", known_entity: "Bob", edit_distance: 2 }`

Scenario: 关系矛盾检测识别不兼容 predicate
  Test:
    Filter: test_relation_contradiction_detected
    Level: integration
  Given KG 中存在 triple `(Bob, husband_of, Alice)`，valid_to IS NULL
  When 调用 `check(text="Bob is Alice's brother")`
  Then 返回的 issues 包含 `RelationContradiction { subject: "Bob", text_claim: "brother_of", kg_fact: "husband_of" }`

Scenario: 时态失效检测识别过期事实
  Test:
    Filter: test_stale_fact_detected
    Level: integration
  Given KG 中存在 triple `(Alice, works_at, Acme)`，valid_from / valid_to 按 DB 当前约定存为 Unix seconds
  When 调用 `check(text="Alice works at Acme", now="2026-04-17T00:00:00Z")`
  Then 返回的 issues 包含 `StaleFact { subject: "Alice", predicate: "works_at", object: "Acme", valid_to: <expired unix seconds> }`

Scenario: 无矛盾文本返回空 issues
  Test:
    Filter: test_consistent_text_no_issues
    Level: integration
  Given KG 中存在 triple `(Bob, husband_of, Alice)`
  When 调用 `check(text="Bob and Alice went hiking.")`
  Then issues.len() == 0
  And checked_entities 包含 "Bob" 和 "Alice"

Scenario: MCP 工具 round trip 保留所有 issue 类型
  Test:
    Filter: test_mcp_fact_check_round_trip
    Level: integration
  Given tempfile palace.db 预置 1 个 RelationContradiction 场景 + 1 个 StaleFact 场景
  When 调用 `mempal_fact_check` MCP 工具
  Then response.issues.len() == 2
  And serde_json 序列化反序列化后数据字节一致

Scenario: fact_check 不改写 DB 任何状态
  Test:
    Filter: test_fact_check_has_no_db_side_effects
    Level: integration
  Given palace.db 基线 drawer_count=N, triple_count=M, schema_version=4
  When 执行 10 次不同 text 的 `mempal_fact_check`
  Then drawer_count == N, triple_count == M, schema_version == 4

Scenario: CLI 子命令从 stdin 读取 + 输出格式化 issues
  Test:
    Filter: test_cli_fact_check_from_stdin
    Level: integration
  Given 预置 KG 一个 relation contradiction
  When `echo "Bob is Alice's brother" | mempal fact-check --now <rfc3339>`
  Then exit code 0
  And stdout 含 `"relation_contradiction"` 和 `"husband_of"`

Scenario: Unknown entity 不误报
  Test:
    Filter: test_unknown_entity_no_false_positive
    Level: unit
  Given 空 KG
  When 调用 `check(text="Alice and Bob went to the store.")`
  Then issues.len() == 0
  And checked_entities 仅包含文本里出现的名字（无 KG 里的对照）

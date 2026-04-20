spec: task
name: "P11: search result chunk neighbors — include prev/next chunks around a hit for better context"
tags: [feature, search, context, optional]
estimate: 0.5d
---

## Intent

借鉴 mempalace 971b92d (`drawer-grep returns best-matching chunk + neighbors`)。

当前问题：mempal 的 search 返回 chunk-level drawer（一个 drawer 就是一个 chunk）。对长源文件（README、设计文档），命中的 chunk 可能指向一个段落，但 agent 要**理解上下文**通常还要看前后段。目前 agent 要自己再发 search query 或 grep 原文件。

mempalace 的方案：`search` 返回 top chunk 时同时带上**同一 source_file 的前一个和后一个 chunk**（基于 `chunk_index` 相邻），让 agent 一次拿到连贯上下文。

核心用户价值：**单次 search 即拿齐段落上下文**，减少 round-trip 和 agent 自己拼图的工作量。长文档类 ingest（设计文档 / docs/*）收益最大。

**Optional 原因**：当前搜索质量已够用，本 spec 是**提升体验**而非必需修复。

## Decisions

- **Search 请求可选字段**：`SearchRequest` 追加 `with_neighbors: Option<bool>` (默认 `false`，向后兼容)
- **`SearchResult` 追加可选字段**：
  ```rust
  pub neighbors: Option<ChunkNeighbors>,
  
  pub struct ChunkNeighbors {
      pub prev: Option<NeighborChunk>,
      pub next: Option<NeighborChunk>,
  }
  
  pub struct NeighborChunk {
      pub drawer_id: String,
      pub content: String,
      pub chunk_index: u32,
  }
  ```
- **邻居定位算法**：
  1. 命中 drawer 有字段 `source_file` + `chunk_index`（`chunk_index` 在 `drawers` 表已有？如无则本 spec 加列）
  2. 查 `SELECT id, content, chunk_index FROM drawers WHERE source_file = ? AND chunk_index IN (hit_idx - 1, hit_idx + 1) AND deleted_at IS NULL AND wing = ? [AND room = ?]`
  3. 按 chunk_index 排序填入 prev/next（缺失的留 `None`）
- **Schema 检查**：若 `drawers` 表没 `chunk_index` 列，bump schema v6 → v7 加 `chunk_index INTEGER NOT NULL DEFAULT 0`，migration 时按 `source_file + created_at` 顺序自动回填 chunk_index（`ROW_NUMBER() OVER (PARTITION BY source_file ORDER BY created_at)`）
- **MCP tool schema**：`mempal_search` 的 request schema 更新，`with_neighbors` 字段 Optional；response `SearchResult` 追加 `neighbors`
- **Payload size 保护**：`with_neighbors=true` 只对 top_k ≤ 10 的请求生效；> 10 时 silently 不补（避免 response 爆炸）
- **Tunnel hits 不带 neighbors**：tunnel_hints 仍保持轻量，只 hit drawer 本身带 neighbors
- **Unit wing/room**：邻居查询限制在同 wing 和 room（若命中 drawer 有 room），避免跨 scope 的 "邻居" 无意义
- **Ingest 写 chunk_index**：`src/ingest/mod.rs` 的 chunker 输出带 index，insert 时写入
- **CLI**：`mempal search "query" --with-neighbors`
- **Agent protocol**：无变化；此字段是 opt-in，现有 client 不 break

## Boundaries

### Allowed
- `src/core/schema.sql`（若需 bump 则加 `chunk_index`）
- `src/core/db.rs`（migration v6→v7 若 bump；`get_neighbor_chunks(source_file, chunk_idx, scope)` 方法）
- `src/search/mod.rs`（命中后 optional 补 neighbors）
- `src/search/mod.rs` 的 `SearchResult` 追加 `neighbors` 字段
- `src/mcp/tools.rs`（`SearchRequest.with_neighbors` + `SearchResultDto.neighbors`）
- `src/mcp/server.rs`（透传）
- `src/cli.rs`（`--with-neighbors` flag）
- `src/ingest/mod.rs` / `src/ingest/chunk.rs`（写入 chunk_index）
- `tests/search_neighbors.rs`（新增）

### Forbidden
- 不改 search 的 ranking 逻辑（BM25+vector+RRF 保持）
- 不让 neighbors 算入 similarity / rerank
- 不改 `drawer_vectors` / `triples` / `tunnels` schema
- 不破坏 `with_neighbors=false` / omit 时的 response 二进制布局（`neighbors` 必须 `Option` 并在 `serde(skip_serializing_if = "Option::is_none")`）
- 不引入新 dep
- 不对 `mempal_peek_partner` / `mempal_ingest` 加邻居能力

## Out of Scope

- 多跳邻居（只 prev + next，不 prev-prev）
- 跨 source_file 的语义邻居（那是 tunnel 的职责）
- Neighbors 自己的 similarity 分数
- 对 tunnel_hints 加 neighbors
- 手动设置邻居跨度
- 取消 chunking（仍按原 chunker 规则切分）

## Completion Criteria

Scenario: with_neighbors=true 返回命中 chunk 的前后相邻
  Test:
    Filter: test_search_with_neighbors_includes_prev_next
    Level: integration
  Given 一个 source file 产生 5 个 chunk（index 0..4）
  And search 命中 chunk_index=2
  When `search(query=..., with_neighbors=true, top_k=5)`
  Then hit.neighbors.prev.chunk_index == 1
  And hit.neighbors.next.chunk_index == 3
  And prev.content 和 next.content 不为空

Scenario: with_neighbors omit 时 response 向后兼容
  Test:
    Filter: test_with_neighbors_omit_backward_compat
    Level: integration
  Given 一个匹配的 drawer
  When `search(query=...)` 不传 with_neighbors
  Then response JSON 中 `neighbors` 字段不存在（serde skip_serializing_if）
  And 其他字段原样返回

Scenario: 命中第一个 chunk（index=0）时 prev = None
  Test:
    Filter: test_first_chunk_has_no_prev
    Level: integration
  Given source 有 3 chunks，命中 chunk_index=0
  When with_neighbors=true
  Then hit.neighbors.prev IS None
  And hit.neighbors.next.chunk_index == 1

Scenario: 命中最后一个 chunk 时 next = None
  Test:
    Filter: test_last_chunk_has_no_next
    Level: integration
  Given source 有 3 chunks，命中 chunk_index=2 (最后)
  When with_neighbors=true
  Then hit.neighbors.next IS None
  And hit.neighbors.prev.chunk_index == 1

Scenario: top_k > 10 时 silently 不补 neighbors
  Test:
    Filter: test_top_k_over_10_skips_neighbors
    Level: integration
  Given 20 个匹配 drawer
  When `search(top_k=20, with_neighbors=true)`
  Then 所有 result 的 neighbors 字段都 IS None

Scenario: 邻居不跨 wing（scope 隔离）
  Test:
    Filter: test_neighbors_limited_to_same_wing
    Level: integration
  Given wing=A 中 source "doc.md" 有 3 chunks
  And 手动污染：wing=B 中也有 source="doc.md" chunk_index=3（不应当作邻居）
  When `search(wing="A", with_neighbors=true)` 命中 chunk_index=2
  Then hit.neighbors.next IS None（不把 B wing 的 chunk_index=3 当邻居）

Scenario: schema migration 自动回填 chunk_index
  Test:
    Filter: test_migration_backfills_chunk_index
    Level: integration
  Given v6 palace.db with 3 drawers same source_file（chunk_index 列不存在）
  When 打开 db（触发 migration）
  Then schema_version == 7
  And 3 个 drawer 的 chunk_index 依次为 0, 1, 2（按 created_at 排序）

Scenario: ingest 新 source 时 chunk_index 从 0 顺序写入
  Test:
    Filter: test_new_ingest_writes_chunk_index_sequentially
    Level: unit
  When ingest 一个产生 4 chunks 的文件
  Then 4 个 drawer 的 chunk_index 分别是 0, 1, 2, 3
  And 同 source_file + 不同 chunk_index 可唯一区分

# Search Latency Investigation

This note records the Issue #652 investigation for broad `mempal search`
queries that take roughly 31-44 seconds on a local database with about 4,278
drawers and 10,151 4096-dimensional vectors.

## Current Pipeline

The normal CLI and MCP search path is still quality-first hybrid retrieval:

1. Resolve the route from the query, optional wing, room, project, and typed
   filters.
2. Embed the query vector, bounded by the configured search deadline and
   eligible for BM25 fallback when the embedder is unavailable.
3. Run vector retrieval through `search_with_vector_and_scope_options`
   (`src/search/mod.rs`), then run FTS5/BM25 retrieval.
4. Merge vector and BM25 candidates with reciprocal rank fusion (RRF), apply
   typed filters, tunnel hints, temporal filtering/decay, and importance
   reranking.
5. Apply the optional local/LAN reranker to the final candidate texts.

The issue measurements already isolate query embedding to about 0.75s and
reranking 50 candidates to about 1.6s. The remaining 28-42s sits before
reranking, in the hybrid retrieval stage. In the current code shape, the broad
query case is dominated by the sqlite-vec vector query inside
`search_by_vector_scoped_knn`, specifically the `matches` CTE that evaluates:

```sql
SELECT id, distance
FROM drawer_vectors v
WHERE embedding MATCH vec_f32(?1)
  AND k = ?2
  AND (
      ?3 = 'all'
      OR (?3 = 'project' AND v.project_id = ?4)
      OR (?3 = 'project_plus_global' AND (v.project_id = ?4 OR v.project_id IS NULL))
      OR (?3 = 'null_only' AND v.project_id IS NULL)
  )
```

The later BM25 query is an FTS5 lookup over `drawers_fts`, and the reranker runs
after hybrid retrieval has already returned candidates. Those stages can add
latency, but they do not explain a 30s plateau when embedding and reranking are
measured separately.

## Dispatch Behavior

`search_by_vector_with_filters` first counts actual vector rows:

```sql
SELECT COUNT(*)
FROM drawer_vectors v
JOIN drawers d ON d.id = v.id
WHERE ...
```

It then dispatches by `EXACT_VECTOR_CANDIDATE_LIMIT = 4096`:

- `candidate_count <= 4096`: use `search_by_vector_scoped_exact`, a Rust exact
  scan that applies the full drawer/project/filter scope before scoring.
- `candidate_count > 4096`: use `search_by_vector_scoped_knn`, the sqlite-vec
  `vec0` virtual table path shown above.

For the reported database, 10,151 vector rows is above the exact-scan cap, so a
broad query enters the sqlite-vec KNN path. The KNN `k` is computed as
`top_k * 50`, clamped to `[100, 4096]`; this limits returned rows, not the amount
of vector data sqlite-vec must consider to find those rows.

## Does This Use ANN?

No. The current mempal query uses sqlite-vec `vec0` KNN syntax, but sqlite-vec
0.1.x does not provide a persistent ANN index such as HNSW, IVF, or DiskANN for
this table. The project tracks future ANN support in
<https://github.com/asg017/sqlite-vec/issues/25>, and the sqlite-vec v0.1.0
release notes state that the initial implementation focuses on brute-force
vector search rather than ANN indexes:
<https://alexgarcia.xyz/blog/2024/sqlite-vec-stable-release/index.html>.

The official KNN docs show the same `vec0` shape mempal uses:

```sql
SELECT id, distance
FROM vec_documents
WHERE contents_embedding MATCH :query
  AND k = 10;
```

See <https://alexgarcia.xyz/sqlite-vec/features/knn.html>.

So the current `embedding MATCH vec_f32(?1) AND k = ?2` query should be treated
as an exact/full-scan style KNN over the scoped `vec0` rows, with sqlite-vec
handling storage layout and distance computation inside the virtual table, not
as a sublinear ANN lookup.

## Size Implications

The reported shape is small by row count but large by vector width:

- `10,151 * 4096 = 41,578,496` float components inspected for one broad vector
  query before any reranking.
- At float32, the raw vector payload is about 166 MB (158.6 MiB), before SQLite
  page reads, virtual table chunk metadata, row filtering, joins, and result
  hydration.
- Cosine distance needs high-dimensional dot/norm work per candidate. Even when
  sqlite-vec is efficient, the work scales linearly with vector row count and
  dimension because there is no ANN pruning.
- The query vector is serialized to JSON and parsed through `vec_f32(?1)`. That
  overhead is real but should be minor compared with scanning 10k 4096d vectors.
- BM25, RRF merge, tunnel hints, importance/temporal reranking, and the optional
  reranker operate after or beside the vector scan. They are not the primary
  30s suspect for this issue.

## Recommended Optimization Path

1. Add per-stage search timing before changing ranking behavior.
   Record route resolution, query embedding, vector candidate count, vector scan
   mode, sqlite-vec `knn_k`, vector SQL time, BM25 time, RRF/filter/hydration
   time, and reranker time. The existing `vector_scan` status snapshot already
   exposes scan mode and candidate count; timing would make Issue #652
   reproducible without printing memory contents.

2. Add an explicit interactive fast mode, not a default downgrade.
   A CLI/MCP option such as `search_mode = "bm25_only"` or `--fast` can return
   cited BM25 results immediately for operator lookup. Keep the default hybrid
   path quality-first, and keep reranker disabling out of defaults.

3. Reduce vector candidates before broad KNN when filters are available.
   Project, wing, room, memory-kind, domain, field, anchor, recency, and pinned
   constraints should be pushed as early as possible. For candidate sets below
   4096, the current exact path gives true recall over the filtered set and
   avoids sqlite-vec KNN.

4. Prototype a two-stage hybrid path.
   Use BM25, project scope, recency, importance, or typed metadata to select a
   bounded candidate ID set, then exact-score only that set with the full
   4096-dimensional vector. Fall back to broad vector KNN only when the bounded
   set is too small or the user explicitly requests maximum recall.

5. Improve exact-scan shape before raising its cap.
   The current exact path materializes content and embeddings for every
   candidate before ranking. A safer experiment is to rank `id + embedding`
   first, then hydrate only the final IDs. After that, benchmark whether an
   explicit configurable exact cap around 10k vectors is faster than sqlite-vec
   KNN for high-dimensional local workloads.

6. Offer a smaller-vector profile after quality evaluation.
   If the configured embedder supports smaller dimensions, Matryoshka-style
   truncation, binary/scalar quantization, or a smaller local model, provide an
   opt-in reindex profile. This is a storage and quality trade-off, so it must
   require explicit `mempal reindex` and retrieval-quality checks.

7. Evaluate an ANN-capable backend as a larger architecture change.
   If interactive latency must stay sub-second for broad 4096d searches as the
   database grows, sqlite-vec 0.1.x brute-force KNN is the wrong final primitive.
   Options include adopting future sqlite-vec ANN support when available,
   maintaining a local ANN sidecar/index with rebuild and crash-recovery rules,
   or adding a separate vector backend abstraction. This should be scoped as a
   design change because it affects recall, persistence, migrations, and the
   single-file SQLite promise.

## Immediate Operator Guidance

- Use project, wing, room, or typed filters when possible to keep vector
  candidates below the 4096 exact-scan cap.
- Check REST `GET /api/status` when REST is enabled, `mempal daemon status` when
  it can fetch REST status, or MCP `mempal_status` in the same MCP server process
  for the `vector_scan` snapshot after a search; `mode = "knn"` and
  `candidate_count > 4096` indicates the broad sqlite-vec path.
- Do not treat reranker top-k as the vector bottleneck. Reranking starts after
  hybrid retrieval has already paid the vector scan cost.

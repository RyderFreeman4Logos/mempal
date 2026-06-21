# Stateless Memory Operators

Issue #477 separates memory algorithms from store/runtime concerns without
splitting the single `mempal` crate prematurely.

## Current Boundary Map

| Candidate boundary | Current modules | Responsibility | Should depend on |
| --- | --- | --- | --- |
| `core` | `src/core/types.rs`, `src/core/config.rs`, `src/core/project.rs`, `src/core/protocol.rs` | Shared data contracts, config snapshots, project-scope value types, protocol text | std, serde, small pure helpers |
| `store` | `src/core/db.rs`, `src/core/async_db.rs`, `src/core/queue.rs`, `src/core/reindex.rs` | SQLite schema, migrations, FTS/vector SQL, queue persistence, row hydration | `core`, `algo` outputs when persistence needs ranking/filter decisions |
| `algo` | `src/algo/*`, existing pure candidates in `src/search/route.rs`, `src/search/preview.rs`, `src/importance.rs`, parts of `src/factcheck/*` | Stateless parsing, routing, ranking, scoring, filtering, citation/recall operators | `core` value types when needed; never SQLite, CLI, MCP, daemon, globals, network |
| `embed` | `src/embed/*` | Embedding backend traits, routing, retry policy, transport clients | `core` config/status types; not store/runtime orchestration except explicit adapters |
| `runtime` | `src/daemon*.rs`, `src/hook*.rs`, `src/llm/*`, `src/endpoint_pool.rs`, `src/intelligence.rs` | Long-running workers, retries, daemon lifecycle, hook capture, LLM workers | `core`, `store`, `embed`, `algo`; owns side effects |
| `mcp` | `src/mcp/*` | MCP request/response DTOs and tool orchestration | `core`, `store`, `embed`, `runtime`, `algo`; no algorithm ownership |
| `cli` | `src/main.rs`, `src/cli/*`, command-specific top-level modules | User-facing commands and output formatting | all lower layers; no reusable algorithm ownership |
| `wiki` | `src/wiki.rs`, `src/markdown_export.rs`, `books/*`, `docs/*` | Documentation generation/export surfaces | `core`/`store` read models; no search/runtime policy |

These are module boundaries inside the existing crate. They are not a crate
split plan. The published ergonomics remain one `mempal` binary and one
library crate until there is a concrete compile-time or release reason to split.

## Concrete Operator Boundary

`src/algo/ranking.rs` introduces a stateless ranking boundary:

- `ReciprocalRankFusion` fuses ranked memory IDs and returns `FusedRank` values.
- `RankedMemoryItem` is a small trait for caller-owned memory items.
- `sort_by_similarity_desc_then_id` and `rerank_by_effective_importance` are
  pure ordering operators.

The search store path now adapts DB-backed `SearchResult` values into those
operators. SQLite remains responsible for FTS/vector recall, drawer hydration,
tunnel fanout, and neighbor loading. The operator remains responsible only for
score fusion and deterministic ordering.

## Rules For Future Moves

1. Move logic to `algo` only when it can run from owned inputs without opening a
   database, reading hot config, spawning work, or calling network/embedding
   clients.
2. Keep adapters in `store`, `runtime`, `mcp`, or `cli` when the code performs
   IO, hydration, retries, telemetry, or output formatting.
3. Do not create pass-through modules. A module boundary must own a stable
   concept such as ranking, route matching, leakage filtering, citation
   assembly, or fact contradiction detection.
4. Preserve single-binary behavior. Internal module moves are allowed; CLI/MCP
   contracts should change only when the issue explicitly asks for it.

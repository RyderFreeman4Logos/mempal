# P11 Chunk Neighbors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Add opt-in previous/next chunk context to search results without changing ranking or default response shape.

**Architecture:** Reuse existing `drawers.chunk_index` from schema v7; no schema bump is needed. Add neighbor structs to core search result types, a small DB lookup for adjacent chunks in the same source/wing/room, and opt-in wiring through library search, MCP, and CLI.

**Tech Stack:** Rust 2024, rusqlite, serde, rmcp JsonSchema, existing hybrid search and clap CLI.

---

## Context

- Worktree: `.worktrees/p11-chunk-neighbors`
- Base branch: `p11-transcript-noise-strip`
- Current schema is already v7 and `drawers.chunk_index` already exists from P0. The spec's "add chunk_index / bump schema" note is stale.
- P10 normalize-version already uses schema v7, so this feature must not reuse schema v7 for a new migration.
- Existing `SearchResult` does not expose `chunk_index`; implementation can carry it internally with `#[serde(skip)]`.
- Default behavior must remain backward compatible: omit `with_neighbors` and serialized results must not include `neighbors`.

## File Map

- Modify: `specs/p11-chunk-neighbors.spec.md`
  - Correct stale schema/migration and `src/cli.rs` references.
- Modify: `src/core/types.rs`
  - Add `NeighborChunk`, `ChunkNeighbors`, and `SearchResult.neighbors`.
  - Add internal `SearchResult.chunk_index` if needed for lookup.
- Modify: `src/core/db.rs`
  - Add `neighbor_chunks(source_file, wing, room, chunk_index)`.
- Modify: `src/search/mod.rs`
  - Add `SearchOptions { with_neighbors }`.
  - Keep existing `search_with_filters` and `search_with_vector_and_filters` compatibility wrappers.
  - Add `search_with_options` and `search_with_vector_options`.
  - Hydrate neighbors only when `with_neighbors=true` and `top_k <= 10`.
- Modify: `src/mcp/tools.rs`
  - Add `SearchRequest.with_neighbors`.
  - Add DTO structs for neighbors and include `neighbors` with `skip_serializing_if`.
- Modify: `src/mcp/server.rs`
  - Pass `with_neighbors` into search options and update test helpers.
- Modify: `src/main.rs`
  - Add CLI `search --with-neighbors` and include neighbors in JSON/plain output.
- Create: `tests/search_neighbors.rs`
  - Integration tests for library, MCP serialization, CLI JSON, scope isolation, top_k guard, and ingest chunk indexes.
- Modify: `AGENTS.md`, `CLAUDE.md`
  - Move P11 chunk-neighbors to completed inventory after verification.

## Task 1: Spec Drift And Failing Tests

**Files:**
- Modify: `specs/p11-chunk-neighbors.spec.md`
- Create: `tests/search_neighbors.rs`

- [x] **Step 1: Correct spec drift**

Update the spec decisions:

- Replace schema bump text with "schema v7 already has `drawers.chunk_index`; no migration is added".
- Replace `src/cli.rs` with `src/main.rs`.
- Replace migration acceptance scenario with a compatibility scenario that opens current schema and verifies `chunk_index` exists.

- [x] **Step 2: Add failing integration tests**

Create `tests/search_neighbors.rs` with a stub embedder and helpers to insert drawers/vectors.

Tests:

```rust
#[tokio::test]
async fn test_search_with_neighbors_includes_prev_next() {}

#[tokio::test]
async fn test_with_neighbors_omit_backward_compat() {}

#[tokio::test]
async fn test_first_chunk_has_no_prev() {}

#[tokio::test]
async fn test_last_chunk_has_no_next() {}

#[tokio::test]
async fn test_top_k_over_10_skips_neighbors() {}

#[tokio::test]
async fn test_neighbors_limited_to_same_wing() {}

#[test]
fn test_current_schema_has_chunk_index() {}

#[tokio::test]
async fn test_new_ingest_writes_chunk_index_sequentially() {}
```

Use `search_with_vector_options` in library tests once it exists. First run can fail on unresolved import.

- [x] **Step 3: Run first failing test**

```bash
cargo test --test search_neighbors test_search_with_neighbors_includes_prev_next -- --exact
```

Expected: FAIL because neighbor search API does not exist.

- [x] **Step 4: Commit**

```bash
git add specs/p11-chunk-neighbors.spec.md tests/search_neighbors.rs
git commit -m "test: define chunk neighbor search contract"
```

## Task 2: Core Neighbor Lookup And Library Search

**Files:**
- Modify: `src/core/types.rs`
- Modify: `src/core/db.rs`
- Modify: `src/search/mod.rs`
- Modify: `tests/search_neighbors.rs`

- [x] **Step 1: Add neighbor structs**

In `src/core/types.rs`:

```rust
pub struct NeighborChunk {
    pub drawer_id: String,
    pub content: String,
    pub chunk_index: u32,
}

pub struct ChunkNeighbors {
    pub prev: Option<NeighborChunk>,
    pub next: Option<NeighborChunk>,
}
```

Add to `SearchResult`:

```rust
#[serde(skip)]
pub chunk_index: Option<i64>,
#[serde(skip_serializing_if = "Option::is_none")]
pub neighbors: Option<ChunkNeighbors>,
```

- [x] **Step 2: Add DB lookup**

In `src/core/db.rs`, add:

```rust
pub fn neighbor_chunks(
    &self,
    source_file: &str,
    wing: &str,
    room: Option<&str>,
    chunk_index: i64,
) -> Result<ChunkNeighbors, DbError>
```

Query only `deleted_at IS NULL`, same `source_file`, same `wing`, and same room semantics (`room = ?` or `room IS NULL`).

- [x] **Step 3: Add search options wrappers**

In `src/search/mod.rs`:

```rust
pub struct SearchOptions {
    pub filters: SearchFilters,
    pub with_neighbors: bool,
}
```

Keep existing functions as wrappers. Add:

```rust
pub async fn search_with_options(...)
pub fn search_with_vector_options(...)
```

Hydrate neighbors after tunnel hints only when `with_neighbors && top_k <= 10`.

- [x] **Step 4: Include `chunk_index` in search row mapping**

Add `d.chunk_index` to vector and FTS selects and set `SearchResult.chunk_index`.

- [x] **Step 5: Run core tests**

```bash
cargo test --test search_neighbors test_search_with_neighbors_includes_prev_next -- --exact
cargo test --test search_neighbors test_first_chunk_has_no_prev -- --exact
cargo test --test search_neighbors test_last_chunk_has_no_next -- --exact
cargo test --test search_neighbors test_top_k_over_10_skips_neighbors -- --exact
cargo test --test search_neighbors test_neighbors_limited_to_same_wing -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/core/types.rs src/core/db.rs src/search/mod.rs tests/search_neighbors.rs
git commit -m "feat: hydrate search chunk neighbors"
```

## Task 3: MCP And CLI Opt-In Surface

**Files:**
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`
- Modify: `src/main.rs`
- Modify: `tests/search_neighbors.rs`

- [x] **Step 1: Add MCP request/response fields**

In `SearchRequest`, add:

```rust
pub with_neighbors: Option<bool>,
```

In `SearchResultDto`, add:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub neighbors: Option<ChunkNeighborsDto>,
```

- [x] **Step 2: Pass MCP option into search**

Use `SearchOptions` in `mempal_search`:

```rust
with_neighbors: request.with_neighbors.unwrap_or(false)
```

Update existing `SearchRequest` literals in tests to include `with_neighbors: None`.

- [x] **Step 3: Add CLI flag**

In `Commands::Search`, add:

```rust
#[arg(long)]
with_neighbors: bool,
```

Pass through `SearchCommandArgs` and search options.

- [x] **Step 4: Serialize/print neighbors**

Add `neighbors` to `CliSearchResult` with `skip_serializing_if`. For plain output, print compact labels:

```text
prev[1]: ...
next[3]: ...
```

- [x] **Step 5: Run MCP/CLI focused tests**

```bash
cargo test --test search_neighbors test_with_neighbors_omit_backward_compat -- --exact
cargo test --test search_neighbors test_cli_search_with_neighbors_json -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/mcp/tools.rs src/mcp/server.rs src/main.rs tests/search_neighbors.rs
git commit -m "feat: expose chunk neighbors in search clients"
```

## Task 4: Ingest Index Verification, Inventory, And Gates

**Files:**
- Modify: `tests/search_neighbors.rs`
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/2026-04-24-p11-chunk-neighbors-implementation.md`

- [x] **Step 1: Finish ingest/schema tests**

Run:

```bash
cargo test --test search_neighbors test_current_schema_has_chunk_index -- --exact
cargo test --test search_neighbors test_new_ingest_writes_chunk_index_sequentially -- --exact
```

Expected: PASS.

- [x] **Step 2: Update inventory docs**

Move `specs/p11-chunk-neighbors.spec.md` from current specs to completed specs in `AGENTS.md` and `CLAUDE.md`.

Add:

```markdown
- `docs/plans/2026-04-24-p11-chunk-neighbors-implementation.md` — P11 chunk neighbors（已完成）
```

- [x] **Step 3: Contract checks**

```bash
agent-spec parse specs/p11-chunk-neighbors.spec.md
agent-spec lint specs/p11-chunk-neighbors.spec.md --min-score 0.7
```

Expected: PASS.

- [x] **Step 4: Focused tests**

```bash
cargo test --test search_neighbors
cargo test --test tunnels_explicit
cargo test --test mind_model_bootstrap
```

Expected: PASS.

- [x] **Step 5: Full verification**

```bash
cargo fmt --check
cargo test
cargo check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [x] **Step 6: Mark plan complete and commit**

```bash
git add AGENTS.md CLAUDE.md docs/plans/2026-04-24-p11-chunk-neighbors-implementation.md
git commit -m "docs: close chunk neighbors plan"
```

## Final Checklist

- [x] `with_neighbors` defaults to false.
- [x] Default JSON omits `neighbors`.
- [x] `with_neighbors=true` returns prev/next chunks for top_k <= 10.
- [x] First/last chunk boundaries return one-sided neighbors.
- [x] `top_k > 10` suppresses neighbors.
- [x] Neighbor lookup stays in same source_file, wing, and room.
- [x] CLI and MCP both expose opt-in behavior.
- [x] New ingest still writes sequential chunk_index values.
- [x] No schema bump and no new dependencies.

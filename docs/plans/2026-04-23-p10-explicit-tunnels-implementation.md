# P10 Explicit Tunnels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit cross-wing tunnel links that coexist with existing passive same-room discovery and feed MCP, CLI, and search tunnel hints.

**Architecture:** Keep passive `find_tunnels()` unchanged, add a separate soft-deleted `tunnels` table and typed core APIs for explicit endpoints. Since P12 already uses schema v5 for typed drawers, this plan updates P10 tunnels to schema v6 and shifts normalize-version follow-up expectations to v7. MCP/CLI call the same core functions; search merges passive hints and explicit neighbor hints without changing ranking.

**Tech Stack:** Rust 2024, rusqlite migrations, clap subcommands, rmcp schemas, existing SHA-256 helper style; no new runtime dependencies.

**Source Spec:** [specs/p10-explicit-tunnels.spec.md](/Users/zhangalex/Work/Projects/AI/mempal/specs/p10-explicit-tunnels.spec.md)

---

## File Structure

| File | Role |
|------|------|
| `specs/p10-explicit-tunnels.spec.md` | Update schema drift from v5 to v6 and SHA-256 id decision |
| `specs/p10-normalize-version.spec.md` | Shift follow-up schema references from v6 to v7 |
| `src/core/types.rs` | Add tunnel endpoint / explicit tunnel / follow result structs |
| `src/core/utils.rs` | Add canonical explicit tunnel id builder and endpoint formatter |
| `src/core/db.rs` | Add v6 tunnels migration and core CRUD/follow/hint APIs |
| `src/mcp/tools.rs` | Add `TunnelsRequest`, endpoint DTOs, richer tunnel/follow response fields |
| `src/mcp/server.rs` | Extend `mempal_tunnels` action handling |
| `src/search/mod.rs` | Merge explicit tunnel hints with passive tunnel hints |
| `src/main.rs` | Extend `mempal tunnels` CLI to add/list/delete/follow while preserving default discover |
| `src/core/protocol.rs` | Update Rule 3 with explicit tunnel discovery guidance |
| `tests/tunnels_explicit.rs` | New integration tests for DB/MCP/CLI/search behavior |
| `tests/mind_model_bootstrap.rs` | Update schema-version expectation from 5 to 6 |
| `AGENTS.md`, `CLAUDE.md` | Move P10 explicit tunnels to completed inventory after verification |

## Scope Notes

- Do not remove or rewrite passive room-name discovery.
- Do not change search request/response field names; only extend `tunnel_hints` contents.
- Do not write drawers/triples from tunnel add/delete/follow.
- Keep follow bounded to max 2 hops.
- Keep old `mempal tunnels` with no subcommand as passive discover.

## Pre-Flight Facts

- Current `src/core/db.rs::CURRENT_SCHEMA_VERSION` is already `5` because P12 used v5 for typed drawers.
- Current `find_tunnels()` returns passive `(room, wings)` tuples only.
- Current `mempal_tunnels` MCP handler takes no request parameters and only returns passive discovery.
- Current CLI `Commands::Tunnels` is a unit command and calls passive discovery only.
- `SearchResult.tunnel_hints` is `Vec<String>` and currently contains passive other-wing names.

---

### Task 1: Schema Drift And Core Tunnel Storage

**Files:**
- Modify: `specs/p10-explicit-tunnels.spec.md`
- Modify: `specs/p10-normalize-version.spec.md`
- Modify: `src/core/types.rs`
- Modify: `src/core/utils.rs`
- Modify: `src/core/db.rs`
- Test: `tests/tunnels_explicit.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [x] **Step 1: Add failing core tests**

Create `tests/tunnels_explicit.rs` with:

```rust
#[test]
fn test_schema_v5_to_v6_migration_preserves_data() {}

#[test]
fn test_add_tunnel_dedup_unordered() {}

#[test]
fn test_add_self_tunnel_rejected() {}

#[test]
fn test_delete_explicit_tunnel_soft_delete() {}
```

Expected initial result:

```bash
cargo test --test tunnels_explicit test_add_tunnel_dedup_unordered -- --exact
```

FAIL because tunnel types and DB APIs do not exist.

- [x] **Step 2: Update specs for schema drift**

In `specs/p10-explicit-tunnels.spec.md`:

- Change schema decision from v5 to v6.
- Change migration wording from `v4 → v5` to `v5 → v6`.
- Change id hash wording from `blake3_hex` to existing SHA-256 first 16 hex chars because no new dependency is allowed.
- Keep completion semantics unchanged, except schema version expected `6`.

In `specs/p10-normalize-version.spec.md`:

- Change schema decision from v6 to v7.
- Change migration wording from `v5 → v6` to `v6 → v7`.
- Preserve intent and out-of-scope.

- [x] **Step 3: Add tunnel domain types**

In `src/core/types.rs`, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelEndpoint {
    pub wing: String,
    pub room: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplicitTunnel {
    pub id: String,
    pub left: TunnelEndpoint,
    pub right: TunnelEndpoint,
    pub label: String,
    pub created_at: String,
    pub created_by: Option<String>,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelFollowResult {
    pub endpoint: TunnelEndpoint,
    pub via_tunnel_id: String,
    pub hop: u8,
}
```

Use trimmed non-empty rooms as `Some`, empty rooms as `None`.

- [x] **Step 4: Add canonical id helper**

In `src/core/utils.rs`, add:

```rust
pub fn build_tunnel_id(left: &TunnelEndpoint, right: &TunnelEndpoint) -> String
```

Rules:

- canonicalize endpoint tuple as `(wing, room.unwrap_or(""))`
- sort the two endpoints lexicographically
- SHA-256 hash the sorted four-part tuple with nul separators
- return `tunnel_<first16hex>`

Also add a small formatter:

```rust
pub fn format_tunnel_endpoint(endpoint: &TunnelEndpoint) -> String
```

Return `wing:room` when room exists, otherwise `wing`.

- [x] **Step 5: Add v6 migration and DB core APIs**

In `src/core/db.rs`:

- bump `CURRENT_SCHEMA_VERSION` from `5` to `6`
- add `V6_MIGRATION_SQL` with `CREATE TABLE IF NOT EXISTS tunnels (...)`
- add indexes `idx_tunnels_left` and `idx_tunnels_right`
- append migration version 6
- add `TunnelError` if a typed error is cleaner, or add `DbError::InvalidTunnel(String)`

Core methods:

```rust
pub fn create_tunnel(
    &self,
    left: &TunnelEndpoint,
    right: &TunnelEndpoint,
    label: &str,
    created_by: Option<&str>,
) -> Result<ExplicitTunnel, DbError>

pub fn list_explicit_tunnels(&self, wing: Option<&str>) -> Result<Vec<ExplicitTunnel>, DbError>

pub fn delete_explicit_tunnel(&self, tunnel_id: &str) -> Result<bool, DbError>

pub fn follow_explicit_tunnels(
    &self,
    from: &TunnelEndpoint,
    max_hops: u8,
) -> Result<Vec<TunnelFollowResult>, DbError>

pub fn explicit_tunnel_hints(
    &self,
    wing: &str,
    room: Option<&str>,
) -> Result<Vec<String>, DbError>
```

Behavior:

- `create_tunnel` rejects self-link before writing.
- Duplicate unordered endpoint insert returns existing row.
- `delete_explicit_tunnel` soft-deletes only explicit rows.
- `follow_explicit_tunnels` allows only hops 1 or 2; clamp/reject higher at MCP/CLI layer.
- `explicit_tunnel_hints` returns formatted neighbor endpoints, deduped and sorted.

- [x] **Step 6: Update schema expectation tests**

Update existing P12 migration tests that expect schema `5` to expect `6`.

Run:

```bash
cargo test --test tunnels_explicit test_schema_v5_to_v6_migration_preserves_data -- --exact
cargo test --test tunnels_explicit test_add_tunnel_dedup_unordered -- --exact
cargo test --test tunnels_explicit test_add_self_tunnel_rejected -- --exact
cargo test --test tunnels_explicit test_delete_explicit_tunnel_soft_delete -- --exact
cargo test --test mind_model_bootstrap test_migration_backfills_legacy_drawers_with_bootstrap_defaults -- --exact
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add specs/p10-explicit-tunnels.spec.md specs/p10-normalize-version.spec.md src/core/types.rs src/core/utils.rs src/core/db.rs tests/tunnels_explicit.rs tests/mind_model_bootstrap.rs
git commit -m "feat: add explicit tunnel storage"
```

---

### Task 2: MCP Tunnel Actions

**Files:**
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`
- Test: `tests/tunnels_explicit.rs`

- [x] **Step 1: Add failing MCP tests**

Add:

```rust
#[tokio::test]
async fn test_add_and_list_explicit_tunnel() {}

#[tokio::test]
async fn test_follow_one_hop() {}

#[tokio::test]
async fn test_follow_two_hops() {}

#[tokio::test]
async fn test_delete_passive_tunnel_rejected() {}
```

Expected: FAIL because MCP request/response schema only supports passive discovery.

- [x] **Step 2: Extend MCP tool schemas**

In `src/mcp/tools.rs`, add:

```rust
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TunnelsRequest {
    pub action: Option<String>,
    pub left: Option<TunnelEndpointDto>,
    pub right: Option<TunnelEndpointDto>,
    pub from: Option<TunnelEndpointDto>,
    pub label: Option<String>,
    pub tunnel_id: Option<String>,
    pub wing: Option<String>,
    pub kind: Option<String>,
    pub max_hops: Option<u8>,
}
```

Expand `TunnelDto` to include:

- `tunnel_id`
- `kind`
- `room: Option<String>`
- `wings: Vec<String>`
- `left/right: Option<TunnelEndpointDto>`
- `label/created_at/created_by`
- `via_tunnel_id`
- `hop`

Keep passive rows populated with `kind="passive"`, `room=Some(room)`, and `wings`.

- [x] **Step 3: Add MCP test helper**

In `src/mcp/server.rs`, add test-only helper:

```rust
pub async fn tunnels_json_for_test(&self, value: Value) -> Result<TunnelsResponse, ErrorData>
```

- [x] **Step 4: Implement `mempal_tunnels` action dispatch**

Change handler to:

```rust
async fn mempal_tunnels(
    &self,
    Parameters(request): Parameters<TunnelsRequest>,
) -> Result<Json<TunnelsResponse>, ErrorData>
```

Actions:

- `None` / `"discover"`: passive only
- `"list"`: passive/explicit/all according to `kind`
- `"add"`: requires left/right/label, rejects self-link, returns one explicit row
- `"delete"`: rejects ids starting with `passive_`, soft-deletes explicit id, errors if not found
- `"follow"`: requires `from`, max_hops default 1, rejects >2

Use `ErrorData::invalid_params(...)` for validation failures.

- [x] **Step 5: Run MCP tests**

```bash
cargo test --test tunnels_explicit test_add_and_list_explicit_tunnel -- --exact
cargo test --test tunnels_explicit test_follow_one_hop -- --exact
cargo test --test tunnels_explicit test_follow_two_hops -- --exact
cargo test --test tunnels_explicit test_delete_passive_tunnel_rejected -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/mcp/tools.rs src/mcp/server.rs tests/tunnels_explicit.rs
git commit -m "feat: extend mempal tunnels actions"
```

---

### Task 3: Search And CLI Integration

**Files:**
- Modify: `src/search/mod.rs`
- Modify: `src/main.rs`
- Modify: `src/core/protocol.rs`
- Test: `tests/tunnels_explicit.rs`

- [x] **Step 1: Add failing search and CLI tests**

Add:

```rust
#[tokio::test]
async fn test_search_tunnel_hints_merges_passive_and_explicit() {}

#[test]
fn test_cli_tunnels_add_list_follow_delete() {}
```

Expected: FAIL because search only injects passive hints and CLI has no subcommands.

- [x] **Step 2: Merge explicit hints into search**

In `src/search/mod.rs::inject_tunnel_hints`:

- keep passive logic as-is
- for each result, call `db.explicit_tunnel_hints(&result.wing, result.room.as_deref())`
- append explicit hints to existing passive hints
- sort and dedup

Do not change ranking or search result fields.

- [x] **Step 3: Extend CLI tunnels command**

In `src/main.rs`:

```rust
Tunnels {
    #[command(subcommand)]
    command: Option<TunnelCommands>,
}

enum TunnelCommands {
    Add { #[arg(long)] left: String, #[arg(long)] right: String, #[arg(long)] label: String },
    List { #[arg(long)] wing: Option<String>, #[arg(long, default_value = "all")] kind: String },
    Delete { tunnel_id: String },
    Follow { #[arg(long)] from: String, #[arg(long, default_value_t = 1)] hops: u8 },
}
```

Preserve no-subcommand behavior as passive discover.

Endpoint parser:

- `wing:room` -> room `Some(room)`
- `wing` -> room `None`
- empty wing rejected

- [x] **Step 4: Update protocol Rule 3**

In `src/core/protocol.rs`, append tunnel discovery guidance to Rule 3.

- [x] **Step 5: Run integration tests**

```bash
cargo test --test tunnels_explicit test_search_tunnel_hints_merges_passive_and_explicit -- --exact
cargo test --test tunnels_explicit test_cli_tunnels_add_list_follow_delete -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/search/mod.rs src/main.rs src/core/protocol.rs tests/tunnels_explicit.rs
git commit -m "feat: merge explicit tunnels into search and cli"
```

---

### Task 4: Inventory And Verification

**Files:**
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/2026-04-23-p10-explicit-tunnels-implementation.md`

- [x] **Step 1: Update inventory**

Move `specs/p10-explicit-tunnels.spec.md` from current draft to completed specs in both `AGENTS.md` and `CLAUDE.md`.

Add this plan as completed:

```markdown
- `docs/plans/2026-04-23-p10-explicit-tunnels-implementation.md` — P10 explicit tunnels（已完成）
```

- [x] **Step 2: Run contract and project verification**

```bash
agent-spec parse specs/p10-explicit-tunnels.spec.md
agent-spec lint specs/p10-explicit-tunnels.spec.md --min-score 0.7
cargo test --test tunnels_explicit
cargo test --test mind_model_bootstrap
cargo test --all-features --test tunnels_explicit
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected: PASS.

- [x] **Step 3: Mark plan checkboxes complete**

Mark completed steps and checklist entries in this plan.

- [x] **Step 4: Commit**

```bash
git add AGENTS.md CLAUDE.md docs/plans/2026-04-23-p10-explicit-tunnels-implementation.md
git commit -m "docs: close explicit tunnels plan"
```

## Final Checklist

- [x] Existing passive `find_tunnels()` behavior remains intact
- [x] Explicit tunnels are stored in a separate soft-deleted table
- [x] Unordered duplicate endpoint pairs produce one id
- [x] Self-link add is rejected before write
- [x] MCP add/list/delete/follow actions work through `mempal_tunnels`
- [x] Passive tunnel delete is rejected
- [x] Search tunnel hints merge passive and explicit hints
- [x] CLI supports add/list/delete/follow and preserves old default discover
- [x] Schema migrates from current v5 to v6 without data loss
- [x] P10 normalize-version spec is shifted to v7 follow-up
- [x] No new runtime dependency is introduced

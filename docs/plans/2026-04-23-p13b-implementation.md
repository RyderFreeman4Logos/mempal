# P13B Implementation Plan — Bootstrap Ingest Drawer Identity Parity

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make bootstrap/evidence `drawer_id` generation consistent across MCP, REST, and CLI/file ingest without migrating legacy drawer ids or expanding REST typed-drawer parity.

**Architecture:** Extract the current MCP bootstrap identity component logic into shared core helpers, then make each ingest entry point compute ids from the same metadata-derived component set. MCP remains the canon; REST and file ingest become consumers of that canon while staying evidence-only. Existing `build_drawer_id(...)` remains available for legacy/non-bootstrap callers but no longer drives new bootstrap drawer writes.

**Tech Stack:** Rust 2024, SQLite/rusqlite, axum/tower for REST tests under `--all-features`, existing MCP and integration test harnesses; no new dependencies.

**Source Spec:** [specs/p13-ingest-identity.spec.md](/Users/zhangalex/Work/Projects/AI/mempal/specs/p13-ingest-identity.spec.md)

---

## File Structure

| File | Role |
|------|------|
| `src/core/utils.rs` | Home for shared bootstrap identity component normalization and `build_bootstrap_drawer_id(...)` wrappers |
| `src/core/types.rs` | Add a small borrowed identity-input struct if needed; keep `Drawer` schema unchanged |
| `src/mcp/server.rs` | Replace private MCP-only identity component logic with shared core helper |
| `src/api/handlers.rs` | Make REST evidence ingest compute drawer ids with bootstrap identity defaults |
| `src/ingest/mod.rs` | Make file/CLI evidence ingest compute drawer ids with bootstrap identity metadata |
| `tests/mind_model_bootstrap.rs` | Add P13B integration scenarios, including feature-gated REST tests |
| `AGENTS.md`, `CLAUDE.md` | Mark P13B implementation plan in inventory if implementation lands |

## Scope Notes

- This plan only changes **identity computation** for new bootstrap/evidence ingest writes.
- Do not add REST typed request fields. `POST /api/ingest` remains `content/wing/room/source`.
- Do not add CLI typed drawer flags.
- Do not bump schema and do not rewrite old ids during migration.
- Search, wake-up, and raw `content` semantics must remain unchanged.

## Pre-Flight Facts

- `src/core/utils.rs::build_bootstrap_drawer_id(...)` already exists and hashes `content + identity_components`.
- `src/mcp/server.rs::ValidatedIngestMetadata::identity_components()` is currently the only complete component list.
- `src/api/handlers.rs::ingest_handler` still uses `build_drawer_id(...)`.
- `src/ingest/mod.rs::ingest_file_with_options` still uses `build_drawer_id(...)`.
- `Drawer::new_bootstrap_evidence(...)` derives default bootstrap metadata from `SourceType` via `anchor::bootstrap_defaults(...)`.
- REST is behind the `rest` feature; REST tests should either use `#[cfg(feature = "rest")]` or run only under `cargo test --all-features`.

---

### Task 1: Shared Bootstrap Identity Helper

**Files:**
- Modify: `src/core/utils.rs`
- Modify: `src/core/types.rs`
- Modify: `src/mcp/server.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [x] **Step 1: Add failing MCP canon tests**

Add or adjust these tests in `tests/mind_model_bootstrap.rs`:

```rust
#[tokio::test]
async fn test_mcp_ingest_default_drawer_id_matches_bootstrap_identity() {}

#[tokio::test]
async fn test_knowledge_bootstrap_identity_changes_when_governance_component_changes() {}
```

The first test should:

- call `server.ingest_json_for_test(...)` with only `content/wing/room`
- compute the expected id via the public shared helper using default evidence metadata
- assert the returned id equals bootstrap id
- assert it differs from old `build_drawer_id(...)`

The second test should:

- create a baseline knowledge dry-run request that includes all role ref arrays, all trigger hint arrays, `tier`, `status`, `scope_constraints`, `parent_anchor_id`
- mutate exactly one governance component per subcase
- assert each mutation changes the predicted id

Run:

```bash
cargo test --test mind_model_bootstrap test_mcp_ingest_default_drawer_id_matches_bootstrap_identity -- --exact
cargo test --test mind_model_bootstrap test_knowledge_bootstrap_identity_changes_when_governance_component_changes -- --exact
```

Expected:

- The default MCP test may already partially pass, but it should not be able to call a public shared helper yet.
- The governance coverage test should fail or fail to compile until shared component logic is exposed and complete.

- [x] **Step 2: Introduce shared identity input**

In `src/core/types.rs`, add a borrowed helper struct if this keeps the API clean:

```rust
pub struct BootstrapIdentityParts<'a> {
    pub memory_kind: &'a MemoryKind,
    pub domain: &'a MemoryDomain,
    pub field: &'a str,
    pub anchor_kind: &'a AnchorKind,
    pub anchor_id: &'a str,
    pub parent_anchor_id: Option<&'a str>,
    pub provenance: Option<&'a Provenance>,
    pub statement: Option<&'a str>,
    pub tier: Option<&'a KnowledgeTier>,
    pub status: Option<&'a KnowledgeStatus>,
    pub supporting_refs: &'a [String],
    pub counterexample_refs: &'a [String],
    pub teaching_refs: &'a [String],
    pub verification_refs: &'a [String],
    pub scope_constraints: Option<&'a str>,
    pub trigger_hints: Option<&'a TriggerHints>,
}
```

If a smaller API is clearer, keep the struct private to `utils.rs`, but do not leave the component list private to MCP.

- [x] **Step 3: Move component normalization to core**

In `src/core/utils.rs`, add:

```rust
pub fn bootstrap_identity_components(parts: BootstrapIdentityParts<'_>) -> Vec<String> {
    // Preserve current MCP component names and normalization order.
    // Sort role refs and trigger hint arrays before joining.
}

pub fn build_bootstrap_drawer_id_from_parts(
    wing: &str,
    room: Option<&str>,
    content: &str,
    parts: BootstrapIdentityParts<'_>,
) -> String {
    build_bootstrap_drawer_id(
        wing,
        room,
        content,
        &bootstrap_identity_components(parts),
    )
}
```

Move or duplicate only the enum slug helpers needed by component generation. Prefer keeping one source of truth in `utils.rs`, and make MCP call that. Do not change the component names currently used by MCP (`memory_kind=...`, `domain=...`, etc.).

- [x] **Step 4: Rewire MCP to shared helper**

In `src/mcp/server.rs`:

- replace `ValidatedIngestMetadata::identity_components()` with a method that returns `BootstrapIdentityParts<'_>` or directly calls `build_bootstrap_drawer_id_from_parts(...)`
- keep `validate_ingest_request(...)` behavior unchanged
- keep `build_bootstrap_drawer_id(...)` output unchanged for existing MCP cases

Run:

```bash
cargo test --test mind_model_bootstrap test_mcp_ingest_default_drawer_id_matches_bootstrap_identity -- --exact
cargo test --test mind_model_bootstrap test_bootstrap_identity_ignores_ref_and_hint_order -- --exact
cargo test --test mind_model_bootstrap test_knowledge_bootstrap_identity_changes_when_governance_component_changes -- --exact
```

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/core/utils.rs src/core/types.rs src/mcp/server.rs tests/mind_model_bootstrap.rs
git commit -m "refactor: share bootstrap drawer identity components"
```

---

### Task 2: REST And File Ingest Parity

**Files:**
- Modify: `src/api/handlers.rs`
- Modify: `src/ingest/mod.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [x] **Step 1: Add failing REST identity tests**

Add feature-gated tests to `tests/mind_model_bootstrap.rs`:

```rust
#[cfg(feature = "rest")]
#[tokio::test]
async fn test_rest_ingest_default_evidence_drawer_id_matches_mcp() {}

#[cfg(feature = "rest")]
#[tokio::test]
async fn test_rest_after_mcp_default_ingest_reuses_existing_bootstrap_drawer() {}

#[cfg(feature = "rest")]
#[tokio::test]
async fn test_rest_ingest_does_not_claim_typed_field_parity() {}
```

Use the existing `StubEmbedderFactory` and `ApiState::new(...)`, then call `mempal::api::router(state).oneshot(...)` with `POST /api/ingest`.

Example pattern:

```rust
#[cfg(feature = "rest")]
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header::CONTENT_TYPE},
};
#[cfg(feature = "rest")]
use tower::ServiceExt;

let response = mempal::api::router(state)
    .oneshot(
        Request::builder()
            .method(Method::POST)
            .uri("/api/ingest")
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&payload).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap();
```

Run:

```bash
cargo test --all-features --test mind_model_bootstrap test_rest_ingest_default_evidence_drawer_id_matches_mcp -- --exact
```

Expected: FAIL because REST currently uses `build_drawer_id(...)`.

- [x] **Step 2: Rewire REST ingest id generation**

In `src/api/handlers.rs`:

- derive `SourceType::Manual` defaults with `anchor::bootstrap_defaults(&SourceType::Manual)`
- build `BootstrapIdentityParts` for default evidence metadata:
  - `memory_kind = Evidence`
  - `domain = Project`
  - `field = defaults.field`
  - `anchor_kind = defaults.anchor_kind`
  - `anchor_id = defaults.anchor_id`
  - `parent_anchor_id = defaults.parent_anchor_id`
  - `provenance = defaults.provenance`
  - all knowledge-only fields empty
- call `build_bootstrap_drawer_id_from_parts(...)`
- keep request type evidence-only; ignore unknown JSON fields by default serde behavior
- keep `source_file_or_synthetic(...)`, `drawer_exists(...)`, and vector insertion semantics unchanged

Run:

```bash
cargo test --all-features --test mind_model_bootstrap test_rest_ingest_default_evidence_drawer_id_matches_mcp -- --exact
cargo test --all-features --test mind_model_bootstrap test_rest_after_mcp_default_ingest_reuses_existing_bootstrap_drawer -- --exact
cargo test --all-features --test mind_model_bootstrap test_rest_ingest_does_not_claim_typed_field_parity -- --exact
```

Expected: PASS.

- [x] **Step 3: Add failing file ingest identity test**

Add:

```rust
#[tokio::test]
async fn test_file_ingest_uses_bootstrap_identity_for_evidence_drawer() {}
```

Test shape:

- create a temp directory and a plain text file
- call `ingest_dir_with_options(...)` or `ingest_file_with_options(...)` with stub embedder
- load the resulting drawer
- compute expected id from the actual stored metadata via shared helper
- assert expected id equals stored `drawer.id`
- assert changing only `source_file` / `chunk_index` would not change identity if tested through helper inputs

Run:

```bash
cargo test --test mind_model_bootstrap test_file_ingest_uses_bootstrap_identity_for_evidence_drawer -- --exact
```

Expected: FAIL because file ingest currently uses old `build_drawer_id(...)`.

- [x] **Step 4: Rewire file ingest id generation**

In `src/ingest/mod.rs`:

- compute `source_type = source_type_for(format)` before drawer id generation
- derive bootstrap defaults for that `source_type`
- build `BootstrapIdentityParts` matching `Drawer::new_bootstrap_evidence(...)`
- call `build_bootstrap_drawer_id_from_parts(...)` for each chunk
- keep source lock key behavior as-is unless it relies directly on old id; the lock currently uses source file path, not drawer id, in file ingest
- keep `source_file`, `chunk_index`, `added_at`, and `importance` excluded from identity

Run:

```bash
cargo test --test mind_model_bootstrap test_file_ingest_uses_bootstrap_identity_for_evidence_drawer -- --exact
```

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/api/handlers.rs src/ingest/mod.rs tests/mind_model_bootstrap.rs
git commit -m "feat: align rest and file ingest drawer identity"
```

---

### Task 3: Boundary Closure And Contract Verification

**Files:**
- Modify: `tests/mind_model_bootstrap.rs`
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Verify: `specs/p13-ingest-identity.spec.md`

- [x] **Step 1: Add remaining boundary tests**

Add or tighten:

```rust
#[tokio::test]
async fn test_bootstrap_identity_separates_same_content_with_different_anchors() {}

#[tokio::test]
async fn test_p13b_does_not_rewrite_existing_drawer_ids() {}
```

Notes:

- There is already related coverage for same content / different anchors and migration. It is acceptable to rename or extend existing tests if doing so keeps the suite clearer.
- The migration test should start from the existing `create_v4_db(...)` helper and assert no extra duplicate drawer appears after migration.

Run:

```bash
cargo test --test mind_model_bootstrap test_bootstrap_identity_separates_same_content_with_different_anchors -- --exact
cargo test --test mind_model_bootstrap test_p13b_does_not_rewrite_existing_drawer_ids -- --exact
```

Expected: PASS.

- [x] **Step 2: Update inventory status**

Only after implementation is complete:

- Move `specs/p13-ingest-identity.spec.md` from current draft area to completed area in `AGENTS.md` and `CLAUDE.md`
- Add this plan to implementation plan inventory as completed

Do not mark P13C as done.

- [x] **Step 3: Run contract and focused verification**

Run:

```bash
agent-spec parse specs/p13-ingest-identity.spec.md
agent-spec lint specs/p13-ingest-identity.spec.md --min-score 0.7
cargo test --test mind_model_bootstrap
cargo test --all-features --test mind_model_bootstrap
cargo check
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Expected:

- spec parse/lint passes
- default-feature integration tests pass
- all-feature REST integration tests pass
- check/clippy/fmt pass

- [x] **Step 4: Commit**

```bash
git add tests/mind_model_bootstrap.rs AGENTS.md CLAUDE.md specs/p13-ingest-identity.spec.md docs/plans/2026-04-23-p13b-implementation.md
git commit -m "test: close bootstrap ingest identity contract"
```

---

## Final Verification Checklist

- [x] MCP default evidence ingest id equals canonical bootstrap identity
- [x] REST default evidence ingest id equals MCP default identity
- [x] REST after MCP duplicate does not insert duplicate drawer/vector
- [x] File/CLI ingest uses bootstrap identity for new evidence drawers
- [x] `source_file`, `chunk_index`, `added_at`, and `importance` do not participate in identity
- [x] Different explicit anchors still produce distinct drawer ids
- [x] Knowledge role refs and trigger hints remain order-insensitive
- [x] Governance components participate in knowledge identity
- [x] Invalid explicit anchor rejects before drawer id/write
- [x] Schema v4 legacy drawer ids are not rewritten
- [x] REST typed field parity remains out of scope
- [x] Search and wake-up raw `content` behavior is unchanged

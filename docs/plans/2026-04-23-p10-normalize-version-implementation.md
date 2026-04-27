# P10 Normalize Version Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add drawer-level `normalize_version` tracking so stale content produced by older normalize logic can be detected and rebuilt without clearing the palace.

**Architecture:** Schema v7 adds `drawers.normalize_version`. Runtime ingest stamps new drawers with `CURRENT_NORMALIZE_VERSION`, status exposes stale counts, and `mempal reindex` rebuilds stale/forced source groups by reusing the existing ingest pipeline under the P9 per-source lock. Reindex groups by `(source_file, wing, room)` because normalize changes can change chunk boundaries and drawer IDs, so stale drawers must be replaced at source granularity rather than updated in place.

**Tech Stack:** Rust 2024, SQLite/FTS5/sqlite-vec via `rusqlite`, `tokio`, `clap`, existing mempal ingest/embed traits.

---

## Context

- Current schema is v6 because P10 explicit tunnels already owns schema v6.
- This spec becomes schema v7.
- Existing `mempal reindex` only recreates `drawer_vectors` and embeds active drawer content.
- The spec references `src/cli.rs`, but CLI lives in `src/main.rs`; update the spec boundary while implementing.
- Reindex must not change normalize logic or bump `CURRENT_NORMALIZE_VERSION` above `1`.
- Do not introduce dependencies.

## File Map

- Modify: `specs/p10-normalize-version.spec.md`
  - Correct CLI boundary from `src/cli.rs` to `src/main.rs`.
- Modify: `src/core/types.rs`
  - Add `normalize_version: u32` to `Drawer`.
  - Add a small `ReindexSource` data type if the DB API returns typed source groups.
- Modify: `src/core/db.rs`
  - Bump schema to v7.
  - Add `normalize_version` column migration.
  - Include the column in drawer inserts/selects.
  - Add stale count, version histogram, source selection, and source replacement helpers.
- Modify: `src/ingest/normalize.rs`
  - Add `pub const CURRENT_NORMALIZE_VERSION: u32 = 1`.
- Modify: `src/ingest/mod.rs`
  - Stamp file-ingested drawers with `CURRENT_NORMALIZE_VERSION`.
  - Add `IngestOptions` fields that let reindex preserve stored `source_file` and replace a source under the existing lock.
- Create: `src/ingest/reindex.rs`
  - Shared reindex implementation with injectable `Embedder`, used by CLI and integration tests.
- Modify: `src/main.rs`
  - Add `reindex --stale`, `reindex --force`, `reindex --dry-run`.
  - Use shared reindex module rather than inlining old vector rebuild logic.
- Modify: `src/mcp/tools.rs`
  - Add `normalize_version_current` and `stale_drawer_count` to `StatusResponse`.
- Modify: `src/mcp/server.rs`
  - Fill new status fields.
- Modify: `src/api/handlers.rs`, `src/longmemeval.rs`, tests
  - Set `normalize_version` on direct `Drawer` construction where needed.
- Create: `tests/normalize_version.rs`
  - Integration tests for schema, ingest stamping, status, reindex modes, dry-run, missing source, and lock behavior.
- Modify: `tests/mind_model_bootstrap.rs`, `tests/tunnels_explicit.rs`
  - Update current schema expectations from v6 to v7.

## Task 1: Schema, Types, And DB Queries

**Files:**
- Modify: `specs/p10-normalize-version.spec.md`
- Modify: `src/core/types.rs`
- Modify: `src/core/db.rs`
- Modify: `tests/normalize_version.rs`
- Modify: `tests/mind_model_bootstrap.rs`
- Modify: `tests/tunnels_explicit.rs`

- [x] **Step 1: Add failing migration and DB API tests**

Create `tests/normalize_version.rs` with:

```rust
#[test]
fn test_migration_v6_to_v7_stamps_normalize_version_1() {}

#[test]
fn test_drawer_count_by_normalize_version_and_stale_count() {}
```

Build a v6 fixture by copying the existing P10 v5 fixture shape plus the `tunnels` table and `PRAGMA user_version = 6`.

Assertions:

```rust
assert_eq!(db.schema_version().expect("schema version"), 7);
assert_eq!(db.drawer_count().expect("drawer count"), 20);
assert_eq!(count_normalize_version(&db, 1), 20);
```

Run:

```bash
cargo test --test normalize_version test_migration_v6_to_v7_stamps_normalize_version_1 -- --exact
```

Expected: FAIL because schema is still v6 and the column does not exist.

- [x] **Step 2: Correct spec boundary drift**

In `specs/p10-normalize-version.spec.md`, change allowed file:

```diff
- `src/cli.rs`（reindex --stale / --force / --dry-run 子命令 flag）
+ `src/main.rs`（reindex --stale / --force / --dry-run 子命令 flag）
```

Do not change scope or acceptance criteria.

- [x] **Step 3: Add `normalize_version` to core types**

In `src/core/types.rs`, add to `Drawer`:

```rust
#[serde(default = "default_normalize_version")]
pub normalize_version: u32,
```

Add helper:

```rust
fn default_normalize_version() -> u32 {
    1
}
```

Set `normalize_version: default_normalize_version()` in `Drawer::new_bootstrap_evidence`.

If a typed DB source struct is needed, add:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReindexSource {
    pub source_file: Option<String>,
    pub wing: String,
    pub room: Option<String>,
    pub drawer_count: u64,
}
```

- [x] **Step 4: Add schema v7 migration**

In `src/core/db.rs`:

```rust
const CURRENT_SCHEMA_VERSION: u32 = 7;
```

Add:

```rust
const V7_MIGRATION_SQL: &str = r#"
ALTER TABLE drawers ADD COLUMN normalize_version INTEGER NOT NULL DEFAULT 1;
CREATE INDEX IF NOT EXISTS idx_drawers_normalize_version
    ON drawers(normalize_version)
    WHERE deleted_at IS NULL;
"#;
```

Append migration version 7 to `migrations()`.

Add `normalize_version` to `DRAWER_SELECT_COLUMNS`, `insert_drawer`, and `drawer_from_row`.

Use `i64::from(drawer.normalize_version)` when binding to SQLite and convert DB values back with `u32::try_from`.

- [x] **Step 5: Add DB query helpers**

In `src/core/db.rs`, add:

```rust
pub fn stale_drawer_count(&self, current_normalize_version: u32) -> Result<i64, DbError>
pub fn drawer_count_by_normalize_version(&self) -> Result<Vec<(u32, i64)>, DbError>
pub fn reindex_sources_stale(&self, current_normalize_version: u32) -> Result<Vec<ReindexSource>, DbError>
pub fn reindex_sources_force(&self) -> Result<Vec<ReindexSource>, DbError>
pub fn replace_active_source_drawers(
    &self,
    source_file: &str,
    wing: &str,
    room: Option<&str>,
) -> Result<u64, DbError>
```

Selection semantics:

- Stale mode groups active drawers where `normalize_version < current`.
- Force mode groups all active drawers.
- Group key is `(source_file, wing, room)`.
- Preserve `source_file = NULL` groups so reindex can report them as skipped instead of silently hiding them.

Replacement semantics:

- Only deletes active drawers matching exact `(source_file, wing, room)`.
- Before physical delete, remove rows from `drawers_fts` using the existing FTS external-content delete command.
- Delete matching rows from `drawer_vectors` only if the vector table exists.
- Then delete from `drawers`.

- [x] **Step 6: Update existing schema expectation tests**

Update existing tests that assert current schema `6` to expect `7`, especially:

- `tests/mind_model_bootstrap.rs`
- `tests/tunnels_explicit.rs`

Keep P10 explicit tunnel migration coverage by asserting both `tunnels` and `normalize_version` exist after opening old fixtures.

- [x] **Step 7: Run Task 1 tests**

```bash
cargo test --test normalize_version test_migration_v6_to_v7_stamps_normalize_version_1 -- --exact
cargo test --test normalize_version test_drawer_count_by_normalize_version_and_stale_count -- --exact
cargo test --test mind_model_bootstrap test_migration_backfills_legacy_drawers_with_bootstrap_defaults -- --exact
cargo test --test tunnels_explicit test_schema_v5_to_v6_migration_preserves_data -- --exact
```

Expected: PASS.

- [x] **Step 8: Commit**

```bash
git add specs/p10-normalize-version.spec.md src/core/types.rs src/core/db.rs tests/normalize_version.rs tests/mind_model_bootstrap.rs tests/tunnels_explicit.rs
git commit -m "feat: add drawer normalize version storage"
```

## Task 2: Ingest Stamping And Reindex Replacement Hook

**Files:**
- Modify: `src/ingest/normalize.rs`
- Modify: `src/ingest/mod.rs`
- Modify: `src/mcp/server.rs`
- Modify: `src/api/handlers.rs`
- Modify: `src/longmemeval.rs`
- Modify: `tests/normalize_version.rs`
- Modify: existing tests that construct `Drawer` directly

- [x] **Step 1: Add failing ingest stamp test**

In `tests/normalize_version.rs`, add:

```rust
#[tokio::test]
async fn test_new_ingest_writes_current_normalize_version() {}
```

Use a stub embedder like `tests/tunnels_explicit.rs`.

Write a temp file, ingest it through `ingest_file_with_options`, and assert:

```sql
SELECT DISTINCT normalize_version FROM drawers WHERE source_file = 'doc.md'
```

returns exactly `[CURRENT_NORMALIZE_VERSION]`.

Run:

```bash
cargo test --test normalize_version test_new_ingest_writes_current_normalize_version -- --exact
```

Expected: FAIL until ingest stamps the version.

- [x] **Step 2: Add normalize version constant**

In `src/ingest/normalize.rs`:

```rust
pub const CURRENT_NORMALIZE_VERSION: u32 = 1;
```

Do not change `normalize_content`.

- [x] **Step 3: Extend ingest options for reindex**

In `src/ingest/mod.rs`, extend `IngestOptions`:

```rust
pub struct IngestOptions<'a> {
    pub room: Option<&'a str>,
    pub source_root: Option<&'a Path>,
    pub dry_run: bool,
    pub source_file_override: Option<&'a str>,
    pub replace_existing_source: bool,
}
```

Update all existing literals to set:

```rust
source_file_override: None,
replace_existing_source: false,
```

Compute source file as:

```rust
let source_file = options
    .source_file_override
    .map(ToOwned::to_owned)
    .unwrap_or_else(|| normalize_source_file(path, options.source_root));
```

- [x] **Step 4: Replace source drawers inside the P9 lock**

In `ingest_file_with_options`, after the per-source lock is acquired and before `drawer_exists` checks:

```rust
if options.replace_existing_source {
    db.replace_active_source_drawers(&source_file, wing, Some(resolved_room.as_str()))?;
}
```

Map errors through a new `IngestError::ReplaceSource`.

This must stay inside the existing lock block so concurrent ingest/reindex of the same source serializes.

- [x] **Step 5: Stamp inserted drawers**

When creating file-ingested drawers:

```rust
let mut drawer = Drawer::new_bootstrap_evidence(...);
drawer.normalize_version = CURRENT_NORMALIZE_VERSION;
```

For direct MCP/API/manual `Drawer` literals, set `normalize_version: CURRENT_NORMALIZE_VERSION` or `1` where importing ingest would create an undesirable dependency. Runtime MCP/API paths should use `CURRENT_NORMALIZE_VERSION`.

- [x] **Step 6: Run Task 2 tests**

```bash
cargo test --test normalize_version test_new_ingest_writes_current_normalize_version -- --exact
cargo test --test mind_model_bootstrap test_mcp_ingest_default_drawer_id_matches_bootstrap_identity -- --exact
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add src/ingest/normalize.rs src/ingest/mod.rs src/mcp/server.rs src/api/handlers.rs src/longmemeval.rs tests/normalize_version.rs tests/mind_model_bootstrap.rs tests/tunnels_explicit.rs
git commit -m "feat: stamp drawers with normalize version"
```

## Task 3: Shared Reindex Engine And CLI Flags

**Files:**
- Create: `src/ingest/reindex.rs`
- Modify: `src/ingest/mod.rs`
- Modify: `src/main.rs`
- Modify: `tests/normalize_version.rs`

- [x] **Step 1: Add failing reindex tests**

In `tests/normalize_version.rs`, add:

```rust
#[tokio::test]
async fn test_reindex_stale_only_reprocesses_outdated() {}

#[tokio::test]
async fn test_reindex_dry_run_no_writes() {}

#[tokio::test]
async fn test_reindex_force_reprocesses_all() {}

#[tokio::test]
async fn test_reindex_skips_missing_source_file() {}
```

Use library-level reindex with a stub embedder; do not rely on CLI for write-path tests because the CLI embedder may require local model state.

Test shape:

- Create source files under tempdir.
- Insert current and stale drawer rows with `source_file` matching those files.
- For stale mode, set five rows to `normalize_version = 0`.
- Call shared reindex.
- Assert returned report counts and final active drawer versions.

Run one test first:

```bash
cargo test --test normalize_version test_reindex_dry_run_no_writes -- --exact
```

Expected: FAIL because reindex engine does not exist.

- [x] **Step 2: Implement `src/ingest/reindex.rs`**

Add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexMode {
    Stale,
    Force,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReindexOptions {
    pub mode: ReindexMode,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReindexReport {
    pub candidate_drawers: u64,
    pub candidate_sources: u64,
    pub processed_sources: u64,
    pub reingested_files: usize,
    pub reingested_chunks: usize,
    pub skipped_existing_chunks: usize,
    pub skipped_missing_sources: u64,
    pub skipped_missing_drawers: u64,
}
```

Implement:

```rust
pub async fn reindex_sources<E: Embedder + ?Sized>(
    db: &Database,
    embedder: &E,
    options: ReindexOptions,
) -> Result<ReindexReport, ReindexError>
```

Behavior:

- Select source groups using stale or force DB helpers.
- `candidate_drawers` is the sum of selected group drawer counts.
- `candidate_sources` is selected group count.
- In dry-run, return report without opening files or writing.
- If `source_file` is `None` or path does not exist, increment skipped counts and continue.
- For existing sources, call `ingest_file_with_options` with:

```rust
IngestOptions {
    room: source.room.as_deref(),
    source_root: source_path.parent(),
    dry_run: false,
    source_file_override: source.source_file.as_deref(),
    replace_existing_source: true,
}
```

- Preserve old source path string through `source_file_override`.

- [x] **Step 3: Wire CLI `reindex` flags**

Change command enum:

```rust
Reindex {
    #[arg(long)]
    stale: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    dry_run: bool,
},
```

Dispatch:

```rust
Commands::Reindex { stale, force, dry_run } => {
    reindex_command(&db, &config, stale, force, dry_run).await
}
```

Rules:

- `--stale` and `--force` conflict manually with `bail!`.
- No flags defaults to stale.
- Dry-run should not build embedder; it can call shared reindex with a no-op branch or compute report from DB directly.

Output:

```text
would reprocess 5 drawers from 2 sources
```

for dry-run.

Actual:

```text
reindex complete: processed 2 sources, 5 drawers selected, 5 chunks written, skipped 0 missing-source drawers
```

- [x] **Step 4: Add CLI dry-run smoke test**

In `tests/normalize_version.rs`, add:

```rust
#[test]
fn test_cli_reindex_stale_dry_run_reports_without_writes() {}
```

Use temp `HOME`, write config, insert stale rows directly, run:

```bash
mempal reindex --stale --dry-run
```

Assert stdout contains:

```text
would reprocess 5 drawers
```

Assert versions remain stale.

- [x] **Step 5: Run Task 3 tests**

```bash
cargo test --test normalize_version test_reindex_dry_run_no_writes -- --exact
cargo test --test normalize_version test_reindex_stale_only_reprocesses_outdated -- --exact
cargo test --test normalize_version test_reindex_force_reprocesses_all -- --exact
cargo test --test normalize_version test_reindex_skips_missing_source_file -- --exact
cargo test --test normalize_version test_cli_reindex_stale_dry_run_reports_without_writes -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/ingest/reindex.rs src/ingest/mod.rs src/main.rs tests/normalize_version.rs
git commit -m "feat: reindex stale normalized drawers"
```

## Task 4: MCP Status And Concurrency Coverage

**Files:**
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`
- Modify: `tests/normalize_version.rs`

- [x] **Step 1: Add failing status test**

In `tests/normalize_version.rs`, add:

```rust
#[tokio::test]
async fn test_status_exposes_stale_count() {}
```

Use `MempalMcpServer::new_with_factory` and `status_json_for_test()` helper if present; if not, add a test-only helper matching existing `search_json_for_test` style.

Assert:

```rust
assert_eq!(response.normalize_version_current, CURRENT_NORMALIZE_VERSION);
assert_eq!(response.stale_drawer_count, 5);
```

- [x] **Step 2: Extend status response schema**

In `src/mcp/tools.rs`:

```rust
pub normalize_version_current: u32,
pub stale_drawer_count: u64,
```

In `src/mcp/server.rs`, import `CURRENT_NORMALIZE_VERSION` and fill both fields:

```rust
let stale_drawer_count = db
    .stale_drawer_count(CURRENT_NORMALIZE_VERSION)
    .map_err(db_error)? as u64;
```

- [x] **Step 3: Add reindex lock test**

In `tests/normalize_version.rs`, add:

```rust
#[tokio::test]
async fn test_reindex_respects_per_source_lock() {}
```

Test shape:

- Insert a stale drawer for `doc.md`.
- Acquire the same P9 source lock manually with `source_key(Path::new("doc.md"))`.
- Spawn `reindex_sources(..., Stale)`.
- Assert it does not complete before the manual guard is dropped.
- Drop guard.
- Assert reindex completes and final active drawer count is consistent.

Use bounded `tokio::time::timeout` windows to avoid a hanging test.

- [x] **Step 4: Run Task 4 tests**

```bash
cargo test --test normalize_version test_status_exposes_stale_count -- --exact
cargo test --test normalize_version test_reindex_respects_per_source_lock -- --exact
```

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src/mcp/tools.rs src/mcp/server.rs tests/normalize_version.rs
git commit -m "feat: report normalize version freshness"
```

## Task 5: Inventory, Plan Closure, And Verification

**Files:**
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/2026-04-23-p10-normalize-version-implementation.md`

- [x] **Step 1: Update project inventory**

In both `AGENTS.md` and `CLAUDE.md`:

- Move `specs/p10-normalize-version.spec.md` from current drafts to completed specs.
- Keep description as `schema v7 normalize_version 列 + reindex --stale 机制`.
- Add this plan to implementation plans:

```markdown
- `docs/plans/2026-04-23-p10-normalize-version-implementation.md` — P10 normalize-version（已完成）
```

- [x] **Step 2: Run contract checks**

```bash
agent-spec parse specs/p10-normalize-version.spec.md
agent-spec lint specs/p10-normalize-version.spec.md --min-score 0.7
```

Expected: PASS. Existing advisory warnings are acceptable if quality remains above threshold.

- [x] **Step 3: Run focused tests**

```bash
cargo test --test normalize_version
cargo test --test mind_model_bootstrap
cargo test --test tunnels_explicit
```

Expected: PASS.

- [x] **Step 4: Run full project verification**

```bash
cargo fmt --check
cargo test
cargo check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [x] **Step 5: Mark checkboxes complete**

Mark all completed steps and final checklist items in this plan.

- [x] **Step 6: Commit**

```bash
git add AGENTS.md CLAUDE.md docs/plans/2026-04-23-p10-normalize-version-implementation.md
git commit -m "docs: close normalize version plan"
```

## Final Checklist

- [x] Current schema version is v7.
- [x] v6 databases migrate to v7 with all active drawers stamped `normalize_version = 1`.
- [x] New file ingest stamps drawers with `CURRENT_NORMALIZE_VERSION`.
- [x] `mempal_status` exposes `normalize_version_current` and `stale_drawer_count`.
- [x] `mempal reindex --stale --dry-run` reports candidate drawers without writes.
- [x] `mempal reindex --stale` rebuilds stale source groups only.
- [x] `mempal reindex --force` rebuilds all active source groups.
- [x] Missing source files are skipped and reported without deleting stale drawers.
- [x] Reindex replacement runs under the existing P9 per-source lock.
- [x] Search behavior is unchanged; stale drawers are not filtered.
- [x] No normalize logic changes and no dependency additions.

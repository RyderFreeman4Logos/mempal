# P12 Implementation Plan — Mind-Model Bootstrap

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在现有 `drawers` 存储模型上落地 stage-1 mind-model bootstrap：显式区分 `evidence` / `knowledge` drawers，引入最小 `dao/shu/qi` 治理字段和 `global/repo/worktree` anchor metadata，并通过 ingest/search/MCP 暴露这些能力。

**Architecture:** 本阶段不引入 `knowledge_cards`，而是在现有 `drawers` 表和 `Drawer` 类型上做一次有边界的 schema 扩展。写路径从 `mempal_ingest` 进入，经过 kind-specific validation 和 anchor derivation 后写入；读路径继续走现有 hybrid search，只额外支持最小过滤和 metadata 回传，保持 ranking 与 `content` raw 语义不变。

**Tech Stack:** Rust 2024, `rusqlite`, `serde` / `serde_json`, `clap`, `rmcp`, 现有 `cargo test` / `cargo clippy` 工具链；不引入新依赖。

**Source Spec:** [specs/p12-mind-model-bootstrap.spec.md](/Users/zhangalex/Work/Projects/AI/mempal/specs/p12-mind-model-bootstrap.spec.md)

---

## File Structure

| File | Role |
|------|------|
| `src/core/types.rs` | 新增 mind-model enums / structs，扩展 `Drawer` 的 typed metadata |
| `src/core/db.rs` | schema version bump + migration + row decode/encode + insert/search 读写扩展 |
| `src/core/anchor.rs` (new) | anchor derivation helpers（git worktree/repo/global） |
| `src/core/mod.rs` | 导出 `anchor` 模块 |
| `src/core/utils.rs` | 轻量辅助：knowledge synthetic source URI / JSON helper 可留这里或内聚到 db.rs |
| `src/ingest/mod.rs` | 仅在需要复用 validation / helper 时接线；不改文件 ingest 主流程语义 |
| `src/search/filter.rs` | 扩展过滤子句，支持 `memory_kind/domain/field/tier/status/anchor_kind` |
| `src/search/mod.rs` | 查询结果补充 metadata，但不改 ranking 和 `content` raw 语义 |
| `src/mcp/tools.rs` | 扩展 `IngestRequest` / `SearchRequest` / `SearchResultDto` schema |
| `src/mcp/server.rs` | `mempal_ingest` 写路径验证与 anchor 派生；`mempal_search` 透传过滤和 metadata |
| `src/main.rs` | CLI `search` 增加新过滤参数；必要时为 ingest/bootstrap 增加 helper plumbing |
| `tests/mind_model_bootstrap.rs` (new) | P12 integration scenarios |

## Scope Notes

- 这版只做 **typed drawers + anchor metadata + minimal ingest/search/MCP support**
- **不做** `mempal_context`、skill trigger orchestration、`publish_anchor` API、Phase-2 `knowledge_cards`
- migration 必须把旧 drawer 回填成 bootstrap 默认值
- `SearchResult.content` 必须保持 raw，不得变成 `statement`

## Pre-Flight Facts

- `src/core/db.rs` 当前 `CURRENT_SCHEMA_VERSION = 4`，且已有 migration 逻辑；P12 需要 bump 到 `5`
- `src/core/types.rs` 当前 `Drawer` 只包含旧字段；P12 会显著扩展这个 struct
- `src/mcp/server.rs::mempal_ingest` 当前直接从 `IngestRequest` 构造 `Drawer` 并 `insert_drawer`
- `src/search/filter.rs` 当前只支持 `wing/room` 两个过滤维度
- `src/search/mod.rs` 当前 `SearchResult` 只回传旧 metadata + `content`
- `tests/` 当前以 feature-level integration test 为主，P12 适合单独新建 `tests/mind_model_bootstrap.rs`

---

### Task 1: Schema v5 + Core Typed Drawer Model

**Files:**
- Create: `src/core/anchor.rs`
- Modify: `src/core/mod.rs`
- Modify: `src/core/types.rs`
- Modify: `src/core/db.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [ ] **Step 1: Write the failing migration/backfill tests**

Add these integration tests to `tests/mind_model_bootstrap.rs`:

```rust
#[test]
fn test_migration_backfills_legacy_drawers_with_bootstrap_defaults() {}

#[test]
fn test_global_anchor_rejected_for_non_global_domain() {}
```

Run:

```bash
cargo test --test mind_model_bootstrap test_migration_backfills_legacy_drawers_with_bootstrap_defaults -- --exact
```

Expected: FAIL because schema/types do not yet expose the new fields.

- [ ] **Step 2: Extend `Drawer` and define typed metadata enums**

In `src/core/types.rs`, add explicit enums and structs:

```rust
pub enum MemoryKind { Evidence, Knowledge }
pub enum MemoryDomain { Project, Agent, Skill, Global }
pub enum AnchorKind { Global, Repo, Worktree }
pub enum Provenance { Runtime, Research, Human }
pub enum KnowledgeTier { Qi, Shu, DaoRen, DaoTian }
pub enum KnowledgeStatus { Candidate, Promoted, Canonical, Demoted, Retired }

pub struct TriggerHints {
    pub intent_tags: Vec<String>,
    pub workflow_bias: Vec<String>,
    pub tool_needs: Vec<String>,
}
```

Expand `Drawer` with:

```rust
pub memory_kind: MemoryKind,
pub domain: MemoryDomain,
pub field: String,
pub anchor_kind: AnchorKind,
pub anchor_id: String,
pub parent_anchor_id: Option<String>,
pub provenance: Option<Provenance>,
pub statement: Option<String>,
pub tier: Option<KnowledgeTier>,
pub status: Option<KnowledgeStatus>,
pub supporting_refs: Vec<String>,
pub counterexample_refs: Vec<String>,
pub teaching_refs: Vec<String>,
pub verification_refs: Vec<String>,
pub scope_constraints: Option<String>,
pub trigger_hints: Option<TriggerHints>,
```

- [ ] **Step 3: Add schema v5 migration and row encode/decode**

In `src/core/db.rs`:

- bump `CURRENT_SCHEMA_VERSION` to `5`
- add migration that `ALTER TABLE drawers ADD COLUMN ...` for the new fields
- backfill legacy rows as:
  - `memory_kind='evidence'`
  - `domain='project'`
  - `field='general'`
  - `anchor_kind='repo'`
  - `anchor_id='repo://legacy'`
  - `source_type='project' -> provenance='research'`
  - `source_type='conversation' | 'manual' -> provenance='human'`

Persist list fields as JSON text:

```rust
supporting_refs TEXT,
counterexample_refs TEXT,
teaching_refs TEXT,
verification_refs TEXT,
trigger_hints TEXT,
```

- [ ] **Step 4: Implement insert/load helpers for new metadata**

Update `Database::insert_drawer`, `get_drawer`, `top_drawers`, and any common row-mapping paths to serialize/deserialize the new enums and JSON arrays.

Run:

```bash
cargo test --test mind_model_bootstrap test_migration_backfills_legacy_drawers_with_bootstrap_defaults -- --exact
cargo check
```

Expected: targeted migration test passes; `cargo check` is green.

- [ ] **Step 5: Commit**

```bash
git add src/core/mod.rs src/core/types.rs src/core/db.rs src/core/anchor.rs tests/mind_model_bootstrap.rs
git commit -m "feat: add bootstrap typed drawer schema"
```

---

### Task 2: Anchor Derivation + Kind-Specific Validation

**Files:**
- Modify: `src/core/anchor.rs`
- Modify: `src/core/utils.rs`
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [ ] **Step 1: Write the failing ingest validation tests**

Add these integration tests:

```rust
#[test]
fn test_mcp_ingest_defaults_to_evidence_drawer_bootstrap_metadata() {}

#[test]
fn test_evidence_drawer_rejects_knowledge_only_fields() {}

#[test]
fn test_knowledge_drawer_requires_statement_and_supporting_refs() {}

#[test]
fn test_dao_tian_rejects_noncanonical_status() {}
```

Run:

```bash
cargo test --test mind_model_bootstrap test_evidence_drawer_rejects_knowledge_only_fields -- --exact
```

Expected: FAIL because `IngestRequest` and validation logic do not know these fields yet.

- [ ] **Step 2: Extend `IngestRequest` with bootstrap metadata**

In `src/mcp/tools.rs`, add optional fields:

```rust
pub memory_kind: Option<String>,
pub domain: Option<String>,
pub field: Option<String>,
pub provenance: Option<String>,
pub statement: Option<String>,
pub tier: Option<String>,
pub status: Option<String>,
pub supporting_refs: Option<Vec<String>>,
pub counterexample_refs: Option<Vec<String>>,
pub teaching_refs: Option<Vec<String>>,
pub verification_refs: Option<Vec<String>>,
pub scope_constraints: Option<String>,
pub trigger_hints: Option<TriggerHintsDto>,
pub anchor_kind: Option<String>,
pub anchor_id: Option<String>,
pub parent_anchor_id: Option<String>,
pub cwd: Option<String>,
```

- [ ] **Step 3: Implement anchor derivation helpers**

In `src/core/anchor.rs`, implement:

```rust
pub struct DerivedAnchor {
    pub anchor_kind: AnchorKind,
    pub anchor_id: String,
    pub parent_anchor_id: Option<String>,
}

pub fn derive_anchor_from_cwd(cwd: Option<&Path>) -> Result<DerivedAnchor, AnchorError> { ... }
```

Rules:

- git worktree: `worktree = show-toplevel`, parent repo = `git-common-dir`
- non-git: standalone `worktree` anchor from canonical path
- explicit `global` anchor only valid with `domain=global`

- [ ] **Step 4: Add kind-specific validation and build enriched drawers**

In `src/mcp/server.rs::mempal_ingest`, add a validation helper that:

- defaults omitted fields to evidence bootstrap values
- rejects evidence drawers with `tier/status/statement/refs/trigger_hints`
- requires knowledge drawers to have `statement/tier/status/supporting_refs`
- enforces tier/status compatibility
- assigns `source_file`:
  - evidence: keep current real source behavior
  - knowledge: generate `knowledge://...` synthetic URI

Use the validated metadata to construct the expanded `Drawer`.

- [ ] **Step 5: Run targeted tests**

```bash
cargo test --test mind_model_bootstrap test_mcp_ingest_defaults_to_evidence_drawer_bootstrap_metadata -- --exact
cargo test --test mind_model_bootstrap test_knowledge_drawer_requires_statement_and_supporting_refs -- --exact
cargo test --test mind_model_bootstrap test_dao_tian_rejects_noncanonical_status -- --exact
```

Expected: all three PASS.

- [ ] **Step 6: Commit**

```bash
git add src/core/anchor.rs src/core/utils.rs src/mcp/tools.rs src/mcp/server.rs tests/mind_model_bootstrap.rs
git commit -m "feat: validate bootstrap drawer ingest metadata"
```

---

### Task 3: Search Filters + Metadata Round-Trip

**Files:**
- Modify: `src/search/filter.rs`
- Modify: `src/search/mod.rs`
- Modify: `src/mcp/tools.rs`
- Modify: `src/mcp/server.rs`
- Modify: `src/main.rs`
- Test: `tests/mind_model_bootstrap.rs`

- [ ] **Step 1: Write the failing search tests**

Add these integration tests:

```rust
#[test]
fn test_search_result_exposes_knowledge_metadata_without_rewriting_content() {}

#[test]
fn test_search_filters_by_memory_kind_and_tier_without_rerank_changes() {}
```

Run:

```bash
cargo test --test mind_model_bootstrap test_search_result_exposes_knowledge_metadata_without_rewriting_content -- --exact
```

Expected: FAIL because search results do not yet carry bootstrap metadata.

- [ ] **Step 2: Extend filter builder for bootstrap fields**

In `src/search/filter.rs`, evolve `build_filter_clause(...)` so callers can optionally filter by:

- `memory_kind`
- `domain`
- `field`
- `tier`
- `status`
- `anchor_kind`

Do not change ranking logic; only reduce candidate rows.

- [ ] **Step 3: Expand `SearchResult` and row mapping**

In `src/core/types.rs` and `src/search/mod.rs`, add:

```rust
pub memory_kind: MemoryKind,
pub domain: MemoryDomain,
pub field: String,
pub statement: Option<String>,
pub tier: Option<KnowledgeTier>,
pub status: Option<KnowledgeStatus>,
pub anchor_kind: AnchorKind,
pub anchor_id: String,
pub parent_anchor_id: Option<String>,
```

Update SQL `SELECT` lists and row decoders to load them without touching `content`.

- [ ] **Step 4: Extend MCP and CLI search surfaces**

In `src/mcp/tools.rs`, extend `SearchRequest` and `SearchResultDto`.

In `src/main.rs`, extend CLI `Search` with:

```rust
#[arg(long)] memory_kind: Option<String>,
#[arg(long)] domain: Option<String>,
#[arg(long)] field: Option<String>,
#[arg(long)] tier: Option<String>,
#[arg(long)] status: Option<String>,
#[arg(long)] anchor_kind: Option<String>,
```

Thread these through `search_command` and `mempal_search`.

- [ ] **Step 5: Run targeted tests**

```bash
cargo test --test mind_model_bootstrap test_search_result_exposes_knowledge_metadata_without_rewriting_content -- --exact
cargo test --test mind_model_bootstrap test_search_filters_by_memory_kind_and_tier_without_rerank_changes -- --exact
```

Expected: both PASS; returned `content` remains byte-identical to stored body.

- [ ] **Step 6: Commit**

```bash
git add src/search/filter.rs src/search/mod.rs src/mcp/tools.rs src/mcp/server.rs src/main.rs tests/mind_model_bootstrap.rs
git commit -m "feat: add bootstrap search filters and metadata"
```

---

### Task 4: Full Integration Sweep + Contract Closure

**Files:**
- Modify: `tests/mind_model_bootstrap.rs`
- Modify: `docs/MIND-MODEL-DESIGN.md` (only if implementation clarified a naming mismatch)
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: Add the remaining spec scenarios**

Ensure `tests/mind_model_bootstrap.rs` also covers:

```rust
#[test]
fn test_global_anchor_rejected_for_non_global_domain() {}

#[test]
fn test_git_worktree_derives_worktree_anchor_and_repo_parent() {}

#[test]
fn test_non_git_cwd_falls_back_to_standalone_worktree_anchor() {}
```

If any scenario fits better as a unit test in `src/core/anchor.rs`, keep the integration file focused and add unit coverage there.

- [ ] **Step 2: Run the full targeted suite**

```bash
cargo test --test mind_model_bootstrap
```

Expected: all P12 bootstrap integration tests PASS.

- [ ] **Step 3: Run repo-level verification**

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
agent-spec parse specs/p12-mind-model-bootstrap.spec.md
agent-spec lint specs/p12-mind-model-bootstrap.spec.md --min-score 0.7
```

Expected:

- all Rust tests green
- clippy green with no new `#[allow]`
- fmt check green
- spec parse/lint still green after implementation adjustments

- [ ] **Step 4: Sync any implementation-driven wording drift**

If implementation forced a naming or boundary adjustment, update:

- `docs/MIND-MODEL-DESIGN.md`
- `AGENTS.md`
- `CLAUDE.md`

Do not broaden scope; only correct terminology drift.

- [ ] **Step 5: Commit**

```bash
git add tests/mind_model_bootstrap.rs docs/MIND-MODEL-DESIGN.md AGENTS.md CLAUDE.md specs/p12-mind-model-bootstrap.spec.md
git commit -m "test: close out bootstrap mind-model contract"
```

---

## Final Verification Checklist

- [ ] Old drawers migrate cleanly to schema v5 bootstrap defaults
- [ ] `mempal_ingest` omission remains backward-compatible
- [ ] `knowledge` drawers require `statement/tier/status/supporting_refs`
- [ ] `dao_tian` status guard is enforced
- [ ] `global` anchor cannot be attached to non-global domain
- [ ] git worktree vs non-git anchor derivation behaves deterministically
- [ ] `mempal_search` and CLI search can filter by bootstrap metadata
- [ ] search results expose metadata without rewriting `content`
- [ ] `SearchResult.content` remains raw drawer text

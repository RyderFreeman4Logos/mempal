# P9 Implementation Plan — Fact Checker + Per-Source Ingest Lock

> **For agentic workers:** REQUIRED SUB-SKILL: use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to work task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 P9 的两个独立 spec：
1. **P9-A `mempal_fact_check`** — 新 MCP 工具 + CLI 子命令，基于 KG triples + entity 提取做离线矛盾检测（SimilarNameConflict / RelationContradiction / StaleFact）
2. **P9-B per-source ingest lock** — 给 `ingest_file_with_options` / `ingest_dir_with_options` 加 flock-based 临界区，消除 Claude↔Codex 并发 ingest 同一 source 的 TOCTOU race

**Architecture:**
- P9-A: 新模块 `src/factcheck/`（`mod.rs` / `names.rs` / `relations.rs` / `contradictions.rs`），纯 read，0 网络，0 LLM。复用 `aaak::codec::extract_entities` 做人名抽取；复用 `db.query_triples` 做 KG 查询
- P9-B: 新模块 `src/ingest/lock.rs`（跨平台薄 libc/windows-sys wrapper，**不引** `fs2`/`fd-lock` crate），RAII `IngestLock` guard。锁文件 `~/.mempal/locks/<blake3_16>.lock`，拿锁后 double-check dedup

**Tech Stack:** Rust 2024 + 既有依赖（`serde`/`serde_json`/`rmcp 1.3`/`thiserror`/`tokio`/`regex`）。blake3 已在 `Cargo.toml`（verify: `grep blake3 Cargo.toml`）；若无则改用 `std::hash::DefaultHasher` 的 u64 hex，**不引新 crate**。Unix 用 `libc::flock`（libc 已依赖）；Windows 用 `windows-sys::Win32::Storage::FileSystem::LockFileEx`（若无 dep 则本 spec 暂只支持 Unix，Windows 走 no-op 并在 startup warn，留给未来 spec）。

**Source Specs:**
- `specs/p9-fact-checker.spec.md` (待 lint ≥0.7)
- `specs/p9-ingest-lock.spec.md` (待 lint ≥0.7)

**Baseline Commit:** `e5437fc` (0.3.0 release, P8 closure)

**Plan Review Notes:**
- 两 spec 独立但 P9-B 的 `IngestSummary.lock_wait_ms` 新字段会被 P11-diary-rollup 后续复用。先做 P9-A 再做 P9-B（A 无依赖 B，B 改动更深）。两者可在同一 PR（~700 LoC 总计）或拆两 PR
- 不 bump schema version（保持 v4）— 两 spec 都不改 DB schema
- `CURRENT_SCHEMA_VERSION` 检查仅在 P10 tunnels + normalize_version 两 spec 启动时再动

---

## Scope Sanity Check

P9 是两个独立 feature 合并的中等 spec 集：
- P9-A fact-checker：~400 LoC（module + MCP tool + CLI + tests），1.0d
- P9-B ingest lock：~250 LoC（lock 模块 + ingest 改造 + tests），0.5d

合并总工作量 1.5d。比 P8 的 600 LoC 略多但无新 runtime dep，无 schema migration，无 cross-tool 协议变更。一 PR 搞定。

## File Structure

| 文件 | 所属 | 职责 |
|------|------|------|
| `src/factcheck/mod.rs` (new) | P9-A | pub API `check(text, db, now, scope) -> Vec<FactIssue>`；`FactIssue` enum；`FactCheckError` |
| `src/factcheck/names.rs` (new) | P9-A | Levenshtein 距离（纯 std）+ 从文本/KG 提取 candidate names（复用 `aaak::codec::extract_entities`） |
| `src/factcheck/relations.rs` (new) | P9-A | 文本 → (subject, predicate, object) 启发式抽取（正则 + 句型模板） |
| `src/factcheck/contradictions.rs` (new) | P9-A | `INCOMPATIBLE_PREDICATES` 小表 + `detect_stale` 用 `valid_to < now` |
| `src/ingest/lock.rs` (new) | P9-B | `IngestLock` RAII guard + `acquire_source_lock(home, key, timeout)` + `LockError` |
| `src/ingest/lock_impl_unix.rs` (new) | P9-B | `libc::flock(LOCK_EX | LOCK_NB)` + 重试循环 |
| `src/ingest/lock_impl_windows.rs` (new) | P9-B | `LockFileEx` or no-op fallback |
| `src/ingest/mod.rs` (modify) | P9-B | 在 `ingest_file_with_options` 进 dedup/insert 前 acquire lock；`IngestSummary` 加 `lock_wait_ms: Option<u64>` |
| `src/lib.rs` (modify) | P9-A | `pub mod factcheck;` |
| `src/mcp/tools.rs` (modify) | P9-A | `FactCheckRequest` / `FactCheckResponse` / `FactIssue` DTO |
| `src/mcp/server.rs` (modify) | P9-A | 新 `#[tool]` `mempal_fact_check` handler（tool #10） |
| `src/main.rs` (modify) | P9-A + P9-B | 新 `FactCheck` 子命令；`Ingest` 子命令 stderr lock-wait 提示 |
| `src/core/protocol.rs` (modify) | P9-A | Rule 11 "VERIFY BEFORE INGEST" + TOOLS 列表 9→10 |
| `tests/fact_check.rs` (new) | P9-A | 8 scenarios from spec |
| `tests/ingest_lock.rs` (new) | P9-B | 8 scenarios from spec |

**不动**：`src/core/db.rs` schema 相关（无 migration）、`src/core/schema.sql`、`CURRENT_SCHEMA_VERSION` 常量、`drawers` / `drawer_vectors` / `triples` 表、P6/P7/P8 任何代码、`Cargo.toml`（无新 dep）。

## Pre-Flight Facts (开工前必读)

> 开工前对照这些事实。任一条和当前源码不符就**立即停下** surface 给 author。

**`src/main.rs` CLI**（已验证 baseline `e5437fc`）:
- line 30-33: `#[derive(Parser)] struct Cli { #[command(subcommand)] command: Commands }`
- line 38: `enum Commands { ... }` 含 `Init` / `Ingest` / `Search` / `WakeUp` / `Compress` / `Bench` / `Delete` / `Purge` / `Reindex` / `Kg` / `Tunnels` / `Taxonomy` / `CoworkDrain` / `CoworkStatus` / `CoworkInstallHooks` 等
- line 205: `async fn run() -> Result<()>` 通过 `Cli::parse()` + `match cli.command { ... }` dispatch
- Task P9-A-5 在 `enum Commands` 尾追加 `FactCheck { path: Option<PathBuf>, wing: Option<String>, room: Option<String>, now: Option<String> }`，并在 run() 加 match arm

**`src/ingest/mod.rs`**（已验证）:
- line 34: `pub struct IngestOptions<'a> { room, source_root, dry_run }`
- line 28-32: `pub struct IngestSummary { files, chunks, skipped }` — Task P9-B-3 追加 `lock_wait_ms: Option<u64>`
- line 100: `pub async fn ingest_file(db, embedder, path, wing) -> Result<IngestSummary>`
- line 121: `pub async fn ingest_file_with_options(db, embedder, path, wing, options) -> Result<IngestSummary>` — Task P9-B-4 在这里 acquire lock
- line 246-298: `ingest_dir_with_options` 内循环调 `ingest_file_with_options(..., options)` 每个文件 — 锁在每个文件 scope 拿，**不**在 dir 级别拿

**`src/core/db.rs`**（已验证）:
- line 11: `const CURRENT_SCHEMA_VERSION: u32 = 4;` — P9 不 bump
- line 31: `CREATE TABLE IF NOT EXISTS triples (...)`
- line 481: `add_triple` / line 498: `query_triples` / line 539: `invalidate_triple` — Task P9-A-3 复用 `query_triples`
- **不存在** `extract_entities_from_drawers` — Task P9-A-2 新写一个小 helper `query_known_entities(db, wing, room)`，在 factcheck 模块内

**`src/aaak/signals.rs`**（已验证）:
- line 11: `pub struct AaakSignals { entities, topics, flags, emotions, importance_stars }`
- line 20: `pub fn analyze(text: &str) -> AaakSignals` — 可复用
- line 25: 内部调 `super::codec::extract_entities(&normalized)` —— 仍是 pub crate-level

**`src/aaak/codec.rs`**（已验证）:
- `pub(crate) fn extract_entities(text: &str) -> Vec<String>` — `pub(crate)` 可见性，factcheck 是 crate 内部模块，可直接复用
- 若发现是 `pub(super)` 而非 `pub(crate)`，在 Task P9-A-1 改为 `pub(crate)`（零行为变更，只放宽可见性）

**`src/mcp/server.rs`**（已验证）:
- line 70: `#[tool_router(router = tool_router)]`
- 已有 9 个 `#[tool(...)]` handler（P8 末尾是 `mempal_cowork_push`）
- Task P9-A-6 在最后一个 `#[tool(...)]` 后追加 `mempal_fact_check`（tool #10）

**`src/core/protocol.rs`**（已验证）:
- line 13: `pub const MEMORY_PROTOCOL: &str = r#"..."#`
- 最后一条 rule 是 Rule 10 "COWORK PUSH"
- `TOOLS:` 列表最后一条是 `mempal_cowork_push`
- Task P9-A-7 追加 Rule 11 + 更新 TOOLS 列表 9→10

**依赖检查**（开工前 `cat Cargo.toml` 确认）:
- `blake3` — 若有，P9-B lock key 用之；若无，改用 `std::collections::hash_map::DefaultHasher` + u64 hex
- `libc` — 若有（通常通过 `rusqlite`/`tokio` 间接引入），P9-B Unix lock 可直接用 `libc::flock`；若无，退回 `std::os::unix::fs::OpenOptionsExt` + `fcntl` 的 raw syscall（需 unsafe block）
- `tempfile` — dev-dep 已有（从 P8 `tests/cowork_inbox.rs` 用法可见），用于两个 test 文件

---

## Part A: P9-A Fact Checker (Tasks 1-7)

### Task P9-A-1: Scaffold `src/factcheck/` module skeleton

**Files:**
- Create: `src/factcheck/mod.rs` (stub)
- Create: `src/factcheck/names.rs` (stub)
- Create: `src/factcheck/relations.rs` (stub)
- Create: `src/factcheck/contradictions.rs` (stub)
- Modify: `src/lib.rs` (add `pub mod factcheck;`)
- Modify: `src/aaak/codec.rs` (if needed, widen `extract_entities` to `pub(crate)`)

**TDD:** 本 task 是纯 scaffold，无行为测试；`cargo check` 通过即可。

- [ ] Step 1: Write `src/factcheck/mod.rs` with types only:

```rust
//! Offline fact-checking against KG triples + entity registry.
//!
//! Given a text blob, detect three contradiction classes:
//! 1. SimilarNameConflict — mentioned name ≤2 edit distance from known entity
//! 2. RelationContradiction — KG has incompatible predicate for same (subject, object)
//! 3. StaleFact — text asserts a triple that's valid_to < now
//!
//! Zero LLM, zero network, deterministic.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::db::{Db, DbError};

pub mod contradictions;
pub mod names;
pub mod relations;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FactIssue {
    SimilarNameConflict {
        mentioned: String,
        known_entity: String,
        edit_distance: usize,
    },
    RelationContradiction {
        subject: String,
        text_claim: String,
        kg_fact: String,
        triple_id: String,
        source_drawer: Option<String>,
    },
    StaleFact {
        subject: String,
        predicate: String,
        object: String,
        valid_to: String,
        triple_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FactCheckReport {
    pub issues: Vec<FactIssue>,
    pub checked_entities: Vec<String>,
    pub kg_triples_scanned: usize,
}

#[derive(Debug, Error)]
pub enum FactCheckError {
    #[error("db error: {0}")]
    Db(#[from] DbError),
    #[error("invalid scope: {0}")]
    InvalidScope(String),
    #[error("timeout after checking {partial_count} issues")]
    Timeout { partial_count: usize },
}

pub fn check(
    text: &str,
    db: &Db,
    now: DateTime<Utc>,
    scope: Option<(&str, Option<&str>)>,
) -> Result<FactCheckReport, FactCheckError> {
    // Task P9-A-2/3/4 fill these in
    let _ = (text, db, now, scope);
    Ok(FactCheckReport::default())
}
```

- [ ] Step 2: Write stubs for `names.rs` / `relations.rs` / `contradictions.rs` each with one `pub fn` placeholder returning `Vec::new()`.

- [ ] Step 3: Add `pub mod factcheck;` to `src/lib.rs`.

- [ ] Step 4: Check `extract_entities` visibility in `src/aaak/codec.rs`. If `pub(super)`, widen to `pub(crate)`. Run `cargo check`.

**Verify:**
```
cargo check --lib
cargo clippy --lib --all-targets -- -D warnings
```

---

### Task P9-A-2: Implement `names.rs` — Levenshtein + candidate extraction

**Files:**
- Modify: `src/factcheck/names.rs`

**TDD:** Unit tests inline (`#[cfg(test)] mod tests`) for:
- Levenshtein distance correctness (empty / equal / edit-2 / edit-4 cases)
- Candidate extraction from text returns entity tokens in order seen

- [ ] Step 1: Implement `pub fn edit_distance(a: &str, b: &str) -> usize` — standard DP on bytes (OK for ASCII names; CJK out of scope per spec). Use two-row buffer (O(min(a,b)) space).

- [ ] Step 2: Implement `pub fn candidates_from_text(text: &str) -> Vec<String>` — reuse `crate::aaak::codec::extract_entities` for capitalized tokens, filter ≤2-char noise.

- [ ] Step 3: Implement `pub fn query_known_entities(db: &Db, wing: Option<&str>, room: Option<&str>) -> Result<Vec<String>, DbError>` — SQL `SELECT DISTINCT subject FROM triples` + drawer-level entity aggregation (cap at 50 recent drawers per scope). Dedup via `HashSet`.

- [ ] Step 4: Implement `pub fn detect_similar_name_conflicts(text_names: &[String], known: &[String]) -> Vec<FactIssue>`:
  - For each `mentioned ∈ text_names`:
    - Find closest `known_entity` with `edit_distance ≤ 2` and `≠ mentioned` and length ≥ 3
    - Emit `SimilarNameConflict { mentioned, known_entity, edit_distance }`

- [ ] Step 5: 4+ unit tests in `#[cfg(test)] mod tests`:
  - `test_edit_distance_equal_zero`
  - `test_edit_distance_one_substitution`
  - `test_edit_distance_insertion_plus_deletion`
  - `test_similar_name_detects_bob_vs_bobby`
  - `test_similar_name_ignores_identical`

**Verify:** `cargo test -p mempal --lib factcheck::names`

---

### Task P9-A-3: Implement `contradictions.rs` — predicate table + stale detection

**Files:**
- Modify: `src/factcheck/contradictions.rs`

- [ ] Step 1: Hardcode `INCOMPATIBLE_PREDICATES: &[(&str, &str)]`:

```rust
const INCOMPATIBLE_PREDICATES: &[(&str, &str)] = &[
    ("husband_of", "brother_of"),
    ("husband_of", "father_of"),
    ("wife_of", "sister_of"),
    ("wife_of", "mother_of"),
    ("mother_of", "wife_of"),
    ("father_of", "husband_of"),
    ("employee_of", "founder_of"),
    ("reports_to", "manages"),
    // add more as needed; symmetric check in code
];

pub fn are_incompatible(p1: &str, p2: &str) -> bool {
    INCOMPATIBLE_PREDICATES
        .iter()
        .any(|(a, b)| (a == &p1 && b == &p2) || (a == &p2 && b == &p1))
}
```

- [ ] Step 2: Implement `pub fn detect_relation_contradictions(db: &Db, text_triples: &[(String, String, String)]) -> Result<Vec<FactIssue>, DbError>`:
  - For each text triple `(s, text_pred, o)`:
    - Query KG: `db.query_triples(Some(&s), None, Some(&o), None)` or similar signature — check actual DB method signature via Read tool first
    - For each KG triple returned, if `are_incompatible(text_pred, kg.predicate)`:
      - Emit `RelationContradiction { subject: s, text_claim: text_pred, kg_fact: kg.predicate, triple_id: kg.id, source_drawer: kg.source_drawer }`

- [ ] Step 3: Implement `pub fn detect_stale_facts(db: &Db, text_triples: &[(String, String, String)], now: DateTime<Utc>) -> Result<Vec<FactIssue>, DbError>`:
  - For each text triple `(s, p, o)`:
    - Query KG for exact `(s, p, o)` matches
    - Filter those with `valid_to.is_some() && valid_to < now.to_rfc3339()`
    - Emit `StaleFact { subject, predicate, object, valid_to, triple_id }`

- [ ] Step 4: Unit tests:
  - `test_incompatible_predicates_symmetric` — `are_incompatible("a", "b") == are_incompatible("b", "a")`
  - `test_stale_fact_detection_uses_now_cutoff` — mock triples with valid_to in past / future, assert correct partitioning

**Verify:** `cargo test -p mempal --lib factcheck::contradictions`

---

### Task P9-A-4: Implement `relations.rs` — text → triple heuristics

**Files:**
- Modify: `src/factcheck/relations.rs`

- [ ] Step 1: `pub fn extract_triples(text: &str) -> Vec<(String, String, String)>` using regex for common patterns:

```rust
// Patterns (case-sensitive for proper nouns):
// "X is Y's Z" → (X, Z_of, Y)
// "X is the Z of Y" → (X, Z_of, Y)
// "X married Y" → (X, husband_of|wife_of, Y) — ambiguous; emit both to let contradictions filter
// "X works at Y" → (X, works_at, Y)
// "X is a Z at Y" → (X, Z_at, Y)
```

- [ ] Step 2: Keep it narrow — 4-5 regex patterns, no NLP. Unknown sentence shapes → empty vec (graceful).

- [ ] Step 3: Tie subject/object back to `names::candidates_from_text` to ensure both endpoints look like entities (capitalized ASCII ≥3 chars). Drop triples with non-entity endpoints.

- [ ] Step 4: Unit tests:
  - `test_extracts_possessive_relation` — "Bob is Alice's brother" → `("Bob", "brother_of", "Alice")`
  - `test_extracts_works_at` — "Alice works at Acme" → `("Alice", "works_at", "Acme")`
  - `test_unknown_sentence_returns_empty`

**Verify:** `cargo test -p mempal --lib factcheck::relations`

---

### Task P9-A-5: Wire `check()` + CLI `fact-check` subcommand

**Files:**
- Modify: `src/factcheck/mod.rs` (flesh out `check()`)
- Modify: `src/main.rs` (add `FactCheck` command variant + run branch)

- [ ] Step 1: Fill in `check(text, db, now, scope)`:

```rust
pub fn check(
    text: &str,
    db: &Db,
    now: DateTime<Utc>,
    scope: Option<(&str, Option<&str>)>,
) -> Result<FactCheckReport, FactCheckError> {
    let text_names = names::candidates_from_text(text);
    let known = names::query_known_entities(db, scope.map(|s| s.0), scope.and_then(|s| s.1))?;
    let mut issues = names::detect_similar_name_conflicts(&text_names, &known);

    let text_triples = relations::extract_triples(text);
    let triple_count_before = db.triples_count()?;  // if method exists; else skip metric

    issues.extend(contradictions::detect_relation_contradictions(db, &text_triples)?);
    issues.extend(contradictions::detect_stale_facts(db, &text_triples, now)?);

    Ok(FactCheckReport {
        issues,
        checked_entities: text_names,
        kg_triples_scanned: triple_count_before as usize,
    })
}
```

- [ ] Step 2: Add `FactCheck` variant to `enum Commands` in `src/main.rs`:

```rust
FactCheck {
    /// Path to file, or `-` for stdin
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,
    #[arg(long)]
    wing: Option<String>,
    #[arg(long)]
    room: Option<String>,
    /// RFC3339 timestamp; defaults to now
    #[arg(long)]
    now: Option<String>,
},
```

- [ ] Step 3: Add match branch in `run()`:

```rust
Commands::FactCheck { path, wing, room, now } => {
    let text = match path.as_deref() {
        Some(p) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
        Some(p) => std::fs::read_to_string(p)?,
        None => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
    };
    let now_dt = now
        .as_deref()
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()?
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    let db = /* open palace.db */;
    let scope = wing.as_deref().map(|w| (w, room.as_deref()));
    let report = mempal::factcheck::check(&text, &db, now_dt, scope)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
}
```

- [ ] Step 4: Unit test the CLI entry via `assert_cmd` (if available) or integration test that spawns binary.

**Verify:**
```
cargo build
echo "Bob is Alice's brother" | cargo run -- fact-check --wing mempal
```

---

### Task P9-A-6: Add `mempal_fact_check` MCP tool

**Files:**
- Modify: `src/mcp/tools.rs` (DTOs)
- Modify: `src/mcp/server.rs` (handler)

- [ ] Step 1: In `src/mcp/tools.rs` add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactCheckRequest {
    pub text: String,
    #[serde(default)]
    pub wing: Option<String>,
    #[serde(default)]
    pub room: Option<String>,
    /// RFC3339 timestamp; defaults to now
    #[serde(default)]
    pub now: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactCheckResponse {
    pub issues: Vec<crate::factcheck::FactIssue>,
    pub checked_entities: Vec<String>,
    pub kg_triples_scanned: usize,
}
```

- [ ] Step 2: In `src/mcp/server.rs` add handler after `mempal_cowork_push`:

```rust
#[tool(
    name = "mempal_fact_check",
    description = "Detect contradictions in text against KG triples + known entities. \
                   Returns SimilarNameConflict / RelationContradiction / StaleFact issues. \
                   Zero LLM, zero network, deterministic.",
)]
pub async fn fact_check(
    &self,
    request: Parameters<FactCheckRequest>,
) -> Result<CallToolResult, McpError> {
    let req = request.0;
    let now = req
        .now
        .as_deref()
        .map(chrono::DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|e| McpError::invalid_params(format!("invalid now: {e}"), None))?
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(chrono::Utc::now);
    let db = self.db.lock().await;
    let scope = req.wing.as_deref().map(|w| (w, req.room.as_deref()));
    let report = tokio::task::block_in_place(|| {
        crate::factcheck::check(&req.text, &*db, now, scope)
    })
    .map_err(|e| McpError::internal_error(format!("fact_check: {e}"), None))?;
    let resp = FactCheckResponse {
        issues: report.issues,
        checked_entities: report.checked_entities,
        kg_triples_scanned: report.kg_triples_scanned,
    };
    Ok(CallToolResult::success(vec![Content::json(resp)?]))
}
```

- [ ] Step 3: Confirm `#[tool_router]` macro auto-discovers the new `#[tool]` attribute (per P6/P7/P8 precedent — nothing else to wire).

**Verify:**
```
cargo build
cargo test -p mempal --test mcp_test -- --include-ignored
```

---

### Task P9-A-7: Update Protocol Rule 11 + TOOLS list

**Files:**
- Modify: `src/core/protocol.rs`

- [ ] Step 1: Find last rule (Rule 10 "COWORK PUSH"). After it append:

```
11. VERIFY BEFORE INGEST
   Before ingesting a decision that asserts relationships between named
   entities (who reports to whom, who built what, who decided what), call
   mempal_fact_check with the draft text first. If SimilarNameConflict
   or RelationContradiction issues surface, confirm with the user before
   persisting — those usually indicate a typo or outdated assumption.
   Fact checking is pure read against KG triples + known entities, no
   LLM, no network. Skip for brainstorming / scratch text.
```

- [ ] Step 2: In `TOOLS:` list replace the count and append entry:

```
  mempal_fact_check     — offline contradiction check against KG triples + entities (P9)
```

Update the `Key invariant:` section if it counts tools.

- [ ] Step 3: `cargo test -p mempal --test protocol_test` to verify protocol snapshot (if any) accepts the new line.

**Verify:** `cargo test -p mempal`

---

### Task P9-A-8: Integration tests — `tests/fact_check.rs`

**Files:**
- Create: `tests/fact_check.rs`

- [ ] Step 1: Write 8 scenarios from `specs/p9-fact-checker.spec.md`:
  - `test_similar_name_conflict_detected` — seed KG + call check, assert SimilarNameConflict
  - `test_relation_contradiction_detected` — seed `(Bob, husband_of, Alice)` + text "Bob is Alice's brother"
  - `test_stale_fact_detected` — seed triple with `valid_to < now`, assert StaleFact
  - `test_consistent_text_no_issues`
  - `test_mcp_fact_check_round_trip` — spawn MCP server in-process, call tool, assert JSON
  - `test_fact_check_has_no_db_side_effects` — snapshot drawer_count + triple_count + schema_version before/after
  - `test_cli_fact_check_from_stdin` — use `assert_cmd` or direct `main()` invocation
  - `test_unknown_entity_no_false_positive`

- [ ] Step 2: Use `tempfile::tempdir()` + test fixture db builder (check `tests/cowork_peek.rs` for pattern; factor out `fixture::new_palace()` if needed).

**Verify:** `cargo test --test fact_check`

---

## Part B: P9-B Ingest Lock (Tasks 9-13)

### Task P9-B-9: Scaffold `src/ingest/lock.rs` + platform impls

**Files:**
- Create: `src/ingest/lock.rs` (pub API + LockError)
- Create: `src/ingest/lock_impl_unix.rs` (libc::flock wrapper)
- Create: `src/ingest/lock_impl_windows.rs` (stub or LockFileEx)
- Modify: `src/ingest/mod.rs` (add `mod lock;` + pub exports)

- [ ] Step 1: First verify `Cargo.toml` has `libc` (direct or transitive). If not, add `libc = "0.2"` to `[dependencies]` (this is minimal and standard — **document in plan review note if added**). If "no new dep" is strict, use `std::os::unix::io::AsRawFd` + `std::io::Error::last_os_error` + raw `fcntl` syscall via `libc::c_int` — but that still needs libc. Decision: add `libc` if missing.

- [ ] Step 2: Write `src/ingest/lock.rs`:

```rust
//! Per-source filesystem advisory lock for ingest critical sections.
//!
//! Acquire before dedup/delete/insert; release on drop (RAII).
//! Winner-takes-all; losers block up to `timeout`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use thiserror::Error;

#[cfg(unix)]
#[path = "lock_impl_unix.rs"]
mod imp;

#[cfg(windows)]
#[path = "lock_impl_windows.rs"]
mod imp;

#[derive(Debug, Error)]
pub enum LockError {
    #[error("timed out acquiring lock after {}ms", .0.as_millis())]
    Timeout(Duration),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid source key: {0}")]
    InvalidSourceKey(String),
}

pub struct IngestLock {
    _file: File,
    path: PathBuf,
    _wait: Duration,
}

impl IngestLock {
    pub fn wait_duration(&self) -> Duration {
        self._wait
    }
}

impl Drop for IngestLock {
    fn drop(&mut self) {
        // File close releases flock automatically on Unix
        // On Windows, LockFileEx unlocks at handle close
    }
}

pub fn acquire_source_lock(
    mempal_home: &Path,
    source_key: &str,
    timeout: Duration,
) -> Result<IngestLock, LockError> {
    if source_key.is_empty() || source_key.contains('/') || source_key.contains('\\') {
        return Err(LockError::InvalidSourceKey(source_key.to_string()));
    }
    let locks_dir = mempal_home.join("locks");
    std::fs::create_dir_all(&locks_dir)?;
    let lock_path = locks_dir.join(format!("{source_key}.lock"));

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;

    let start = Instant::now();
    loop {
        match imp::try_lock_exclusive(&file) {
            Ok(()) => {
                return Ok(IngestLock {
                    _file: file,
                    path: lock_path,
                    _wait: start.elapsed(),
                });
            }
            Err(imp::LockWouldBlock) => {
                if start.elapsed() >= timeout {
                    return Err(LockError::Timeout(timeout));
                }
                // 50ms + jitter
                let jitter = fastrand_jitter_ms();
                std::thread::sleep(Duration::from_millis(50 + jitter));
            }
        }
    }
}

fn fastrand_jitter_ms() -> u64 {
    // Simple mixing of nanos — avoids fastrand dep
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % 30
}

pub fn source_key(source_file: &Path) -> String {
    let normalized = source_file.to_string_lossy();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hasher::write(&mut hasher, normalized.as_bytes());
    let v = std::hash::Hasher::finish(&hasher);
    format!("{:016x}", v)
}
```

- [ ] Step 3: `src/ingest/lock_impl_unix.rs`:

```rust
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

pub struct LockWouldBlock;

pub fn try_lock_exclusive(file: &File) -> Result<(), LockWouldBlock> {
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        Ok(())
    } else {
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EWOULDBLOCK) => Err(LockWouldBlock),
            _ => panic!("flock failed unexpectedly: {err}"),
        }
    }
}
```

- [ ] Step 4: `src/ingest/lock_impl_windows.rs` — for v1, no-op (always Ok) + compile warning:

```rust
use std::fs::File;

pub struct LockWouldBlock;

/// Windows: no-op for v1. Concurrent ingest on Windows is
/// not race-protected yet; follow-up spec to use LockFileEx.
pub fn try_lock_exclusive(_file: &File) -> Result<(), LockWouldBlock> {
    Ok(())
}
```

- [ ] Step 5: Add `mod lock;` at top of `src/ingest/mod.rs`.

**Verify:** `cargo check && cargo clippy -- -D warnings`

---

### Task P9-B-10: Wire lock into `ingest_file_with_options`

**Files:**
- Modify: `src/ingest/mod.rs`

- [ ] Step 1: Augment `IngestSummary`:

```rust
pub struct IngestSummary {
    pub files: usize,
    pub chunks: usize,
    pub skipped: usize,
    pub lock_wait_ms: Option<u64>,  // new
}
```

- [ ] Step 2: In `ingest_file_with_options`, before dedup check:

```rust
// ~line 121, after reading + normalizing but before db.check_dedup
let mempal_home = /* resolve via $HOME/.mempal */;
let skey = lock::source_key(&normalized_source);
let lock_timeout = std::time::Duration::from_secs(5);

let (lock_wait, _guard) = if options.dry_run {
    (None, None)
} else {
    let guard = lock::acquire_source_lock(&mempal_home, &skey, lock_timeout)
        .map_err(IngestError::Lock)?;
    (Some(guard.wait_duration().as_millis() as u64), Some(guard))
};

// Re-check dedup AFTER acquiring lock (double-checked locking)
// ... existing dedup logic ...

// ... existing delete + insert ...

// _guard dropped here on function return
```

- [ ] Step 3: Add `IngestError::Lock` variant:

```rust
#[error("failed to acquire ingest lock: {0}")]
Lock(#[from] lock::LockError),
```

- [ ] Step 4: CLI `Ingest` command in `src/main.rs` — stderr lock-wait warning at > 500ms:

```rust
if let Some(ms) = summary.lock_wait_ms {
    if ms > 500 {
        eprintln!("mempal: waited {ms}ms for ingest lock on '{source}'");
    }
}
```

- [ ] Step 5: `ingest_dir_with_options` — no change needed; inner `ingest_file_with_options` handles per-file locking.

**Verify:** `cargo build && cargo test -p mempal --lib ingest`

---

### Task P9-B-11: MCP `mempal_ingest` response exposes `lock_wait_ms`

**Files:**
- Modify: `src/mcp/tools.rs` (`IngestResponse` field)
- Modify: `src/mcp/server.rs` (propagate)

- [ ] Step 1: Add `pub lock_wait_ms: Option<u64>` to `IngestResponse`.
- [ ] Step 2: In handler, copy from `IngestSummary` to DTO.
- [ ] Step 3: `#[serde(skip_serializing_if = "Option::is_none")]` to preserve backward-compat JSON shape.

**Verify:** `cargo build && cargo test -p mempal --test mcp_test`

---

### Task P9-B-12: Integration tests — `tests/ingest_lock.rs`

**Files:**
- Create: `tests/ingest_lock.rs`

- [ ] Step 1: Write 8 scenarios from `specs/p9-ingest-lock.spec.md`:
  - `test_concurrent_ingest_same_source_single_drawer` — spawn two `tokio::task` (`tokio::spawn` + `block_in_place` or use `tokio::task::spawn_blocking`), both call `ingest_file_with_options` same path; assert drawer_count == 1 and exactly one has `lock_wait_ms > 0`
  - `test_concurrent_ingest_different_source_no_blocking` — two different files, both `lock_wait_ms < 50`
  - `test_lock_timeout_returns_error` — task A holds guard with `sleep`; task B acquires with 1s timeout, expects `LockError::Timeout`
  - `test_lock_released_on_guard_drop` — sequential acquire-drop-acquire, second succeeds within 100ms
  - `test_double_check_after_lock_skips_duplicate` — task A completes ingest; task B was waiting, acquires lock, `check_dedup` returns `exists`, outcome is `Skipped`
  - `test_panic_in_critical_section_releases_lock` — `std::panic::catch_unwind` wraps ingest; task B acquires within 500ms
  - `test_dry_run_does_not_acquire_lock` — A runs dry_run with artificial sleep injected in normalize; B real ingest doesn't wait
  - `test_mcp_ingest_response_exposes_lock_wait` — via in-process MCP server, assert JSON serialization

- [ ] Step 2: Fixture helper: `fn new_tempdir_mempal_home() -> TempDir` returning `TempDir` with `.mempal/` structure.

- [ ] Step 3: For concurrency test, use `std::thread::scope` or `tokio::task::spawn_blocking` + `tokio::join!`. Assert invariants with `db.drawer_count()` after both complete.

**Verify:** `cargo test --test ingest_lock -- --test-threads=4`

---

### Task P9-B-13: Regression gate — P6/P7/P8 tests still pass

**Files:** None (test run only)

- [ ] Step 1: Full regression:

```
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
```

- [ ] Step 2: Manual E2E smoke test:
  - Launch Claude Code + Codex in same project
  - Issue `mempal_ingest` concurrently from both (same file)
  - Verify `mempal_status` shows drawer_count increments by exactly N (not 2N)

---

## Completion Checklist

### P9-A Fact Checker
- [ ] `src/factcheck/` 4 files implemented
- [ ] CLI `mempal fact-check` works with file / stdin input
- [ ] MCP `mempal_fact_check` tool accessible (tool count 9→10)
- [ ] Protocol Rule 11 added; TOOLS list updated
- [ ] `tests/fact_check.rs` 8 scenarios green
- [ ] spec lint ≥ 0.7: `agent-spec lint specs/p9-fact-checker.spec.md --min-score 0.7`

### P9-B Ingest Lock
- [ ] `src/ingest/lock.rs` + platform impls implemented
- [ ] `IngestSummary.lock_wait_ms` field threaded through CLI + MCP
- [ ] `tests/ingest_lock.rs` 8 scenarios green
- [ ] Concurrent ingest same source produces single drawer (manual verify)
- [ ] `IngestError::Lock` variant added
- [ ] spec lint ≥ 0.7: `agent-spec lint specs/p9-ingest-lock.spec.md --min-score 0.7`

### Shared
- [ ] `cargo test --workspace --all-features` clean
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo build --workspace --release --all-features` clean
- [ ] `CLAUDE.md` spec 表新增 P9 行（p9-fact-checker + p9-ingest-lock 两行）
- [ ] Final commit message follows `feat(factcheck):` + `fix(ingest):` convention per project CLAUDE.md

---

## Rollback Plan

若并发测试揭露死锁或数据一致性 bug：

1. Lock 相关改动全部 revert（`git revert <commit>` 的 P9-B 范围）
2. 保留 P9-A（无 ingest 依赖）
3. 重新设计 lock 策略（可能改走 SQLite `BEGIN IMMEDIATE` 事务路径替代 advisory flock）

若 fact-check 误报率高（用户反馈）：

1. `INCOMPATIBLE_PREDICATES` 表降为空（临时 disable relation contradiction）
2. 只保留 SimilarNameConflict + StaleFact（更可靠）
3. Iterate predicate 表后再启用

## Next Up

P9 完成后：
- P10-C explicit tunnels（依赖 schema v5 migration）
- P10-D normalize_version（依赖 schema v6，本 plan 落后于 P10-C）
- P11 可选三项按需挑选

每一批新 spec 前跑 `agent-spec lint specs/<spec>.spec.md --min-score 0.7` 验收 spec 质量再进实现。

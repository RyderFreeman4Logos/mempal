# P7 Search Structured Signals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `src/aaak/` 下新增公共 analysis API `signals::analyze`，把 AAAK codec 已有的 5 个提取器（entities / topics / flags / emotions / weight）暴露给 MCP 层；在 `mempal_search` 响应的每条 `SearchResultDto` 上无条件附加 5 个新字段，让 agent 直接过滤/排序结构化信号而不需要解析 AAAK 语法。`content` 保持 raw 不变。

**Architecture:** 新 module `src/aaak/signals.rs` 暴露 `pub struct AaakSignals` 和 `pub fn analyze(text: &str) -> AaakSignals`；`src/aaak/codec.rs` 中 7 个私有 fn（`normalize_whitespace` / `extract_entities` / `extract_topics` / `detect_flags` / `detect_emotions` / `infer_weight` / `default_entity_code`）从 `fn` 升为 `pub(crate) fn`，逻辑 0 改动；`SearchResultDto` 扩展 5 个新字段，始终 on-wire；`mempal_search` handler 用新的命名构造函数 `SearchResultDto::with_signals_from_result` 组装响应，删除既有的 `From<SearchResult>` 实现。palace.db schema 不变，无迁移，无新 runtime 依赖。

**Tech Stack:** Rust 2024 + 既有依赖（`serde` / `serde_json` / `rmcp` 1.3 / `jieba-rs`）。测试层继续沿用 P6 的 `tempfile` + 真 `Database` hermetic 模式。

**Source Spec:** `specs/p7-search-structured-signals.spec.md`
**Source Design Doc:** `docs/specs/2026-04-13-p7-search-structured-signals.md`

---

## Scope Sanity Check

P7 是单一内聚的 feature：一个新 analysis module + 一个 DTO 扩展 + 一个 handler rewiring + 12 个测试场景。不需要拆子项目。

## File Structure

| 文件 | 职责 |
|------|------|
| `src/aaak/codec.rs` | **仅 visibility 调整**：7 个 `fn` 升为 `pub(crate) fn`。**不改任何算法**。 |
| `src/aaak/signals.rs` (new) | 定义 `AaakSignals` struct + `pub fn analyze(text: &str) -> AaakSignals`；内部调用 codec.rs 的 7 个 pub(crate) 辅助函数。含单元测试 `#[cfg(test)] mod tests`（覆盖 S3/S5/S6/S12）。 |
| `src/aaak/mod.rs` | 新增 `pub mod signals;` + `pub use signals::{AaakSignals, analyze};` |
| `src/mcp/tools.rs` | `SearchResultDto` 结构体加 5 字段 + rustdoc；新增 `impl SearchResultDto { pub fn with_signals_from_result(result: SearchResult) -> Self }`；删除 `impl From<SearchResult> for SearchResultDto`；清理 `use` 语句（`SearchResult` 仍需保留，因为构造函数签名用它） |
| `src/mcp/server.rs` | `mempal_search` handler 的 collect 调用改成 `SearchResultDto::with_signals_from_result`（3 行替换）；`#[tool(description = ...)]` 追加一段说明新字段用途 |
| `tests/search_structured_signals.rs` (new) | 8 个集成测试场景（S1/S2/S4/S7/S8/S9/S10/S11），hermetic palace.db |
| `specs/p7-search-structured-signals.spec.md` | Spec 不动（lint 已 100%） |
| `CLAUDE.md` | 已完成 spec 列表追加 P7 行 |

**不动**：`Cargo.toml`（无新依赖）、`src/core/` 下任何文件、`drawers` / `drawer_vectors` / `triples` 表 schema、任何其他 MCP 工具。

## Pre-Flight Facts（开工前必读）

> 实施时请先打开源文件对照这些事实，任何一条与当前源码不符就**立即停下来** surface 给 author，**不要基于 stale spec/plan 动工**。

**codec.rs 当前行号 / 签名**（2026-04-13 验证过）：
- `fn normalize_whitespace(text: &str) -> String` @ **line 337**
- `fn extract_entities(text: &str) -> Vec<String>` @ **line 346**
- `fn extract_topics(text: &str) -> Vec<String>` @ **line 370**
- `fn detect_flags(text: &str) -> Vec<String>` @ **line 403** — 无匹配时 push `"CORE"` 到 line 412-414
- `fn detect_emotions(text: &str) -> Vec<String>` @ **line 419** — 无匹配时 push `DEFAULT_EMOTION` ("determ") 到 line 429-431
- `fn infer_weight(flags: &[String]) -> u8` @ **line 436** — DECISION/PIVOT→4, TECHNICAL→3, else→**2** (line 445)
- `fn default_entity_code(entity: &str) -> String` @ **line 480**
- `const DEFAULT_EMOTION: &str = "determ";` @ line 18
- `const DEFAULT_ENTITY_CODE: &str = "UNK";` @ line 19
- `encode()` 内部的 entity code mapping + UNK fallback 在 **lines 213-231**（`analyze` 要复刻这段 stateless 版）

**tools.rs 当前状态**：
- `use crate::core::types::{RouteDecision, SearchResult, TaxonomyEntry};` @ line 1 — 要**保留** `SearchResult`（新的 `with_signals_from_result` 构造函数签名会继续用它）
- `pub struct SearchResultDto` @ line 33 — 当前 8 字段（drawer_id, content, wing, room, source_file, similarity, route, tunnel_hints）
- `tunnel_hints` 字段用了 `#[serde(skip_serializing_if = "Vec::is_empty")]` — **保留**，新 signals 字段**不**加此属性
- `impl From<SearchResult> for SearchResultDto` @ line 244-257 — **删除**

**server.rs 当前状态**：
- `use super::tools::{... SearchResponse, SearchResultDto, ...};` @ line 23 — `SearchResultDto` 已导入，handler 可用
- `mempal_search` handler 的 collect 行在 **line 138-140**：
  ```rust
  Ok(Json(SearchResponse {
      results: results.into_iter().map(SearchResultDto::from).collect(),
  }))
  ```

**aaak/mod.rs 当前状态**（14 行）：
```rust
#![warn(clippy::all)]

mod codec;
mod model;
mod parse;
mod spec;

pub use codec::AaakCodec;
pub use model::{AaakDocument, AaakHeader, AaakLine, AaakMeta, ArcLine, EncodeOutput, EncodeReport, ParseError, RoundtripReport, Tunnel, Zettel};
pub use spec::generate_spec;
```

**测试目录布局**：目前 `tests/` 只有 `cowork_peek.rs` + `fixtures/`。`tests/search_structured_signals.rs` 是新加的同级 integration test 文件，沿用 P6 的 `tempfile` + 真 `Database` hermetic 模式。

**Sentinel semantics（测试依赖的不变量）**：
- `detect_flags("")` → `["CORE"]`（永远 >= 1）
- `detect_emotions("")` → `["determ"]`（永远 >= 1）
- `extract_entities("")` → `[]`（空）— `analyze` 再加 `["UNK"]` fallback
- `extract_topics("")` → `[]`（**可以空**，不加 default）
- `infer_weight(&["CORE".to_string()])` → **2**（else 分支）

---

## Task 1: Codec visibility bump（0 逻辑变更）

**Files:**
- Modify: `src/aaak/codec.rs` (7 spots: 337, 346, 370, 403, 419, 436, 480)

**Why first**: `signals.rs` 的实现依赖这 7 个 pub(crate) helper。先做 visibility，后续 TDD 才能编译通过。这一步是纯机械替换，可以一次 commit 无风险。

- [ ] **Step 1: 编辑 codec.rs 的 7 个签名**

Apply these exact Edits (each `fn` keyword → `pub(crate) fn`):

| Line | Before | After |
|------|--------|-------|
| 337 | `fn normalize_whitespace(text: &str) -> String {` | `pub(crate) fn normalize_whitespace(text: &str) -> String {` |
| 346 | `fn extract_entities(text: &str) -> Vec<String> {` | `pub(crate) fn extract_entities(text: &str) -> Vec<String> {` |
| 370 | `fn extract_topics(text: &str) -> Vec<String> {` | `pub(crate) fn extract_topics(text: &str) -> Vec<String> {` |
| 403 | `fn detect_flags(text: &str) -> Vec<String> {` | `pub(crate) fn detect_flags(text: &str) -> Vec<String> {` |
| 419 | `fn detect_emotions(text: &str) -> Vec<String> {` | `pub(crate) fn detect_emotions(text: &str) -> Vec<String> {` |
| 436 | `fn infer_weight(flags: &[String]) -> u8 {` | `pub(crate) fn infer_weight(flags: &[String]) -> u8 {` |
| 480 | `fn default_entity_code(entity: &str) -> String {` | `pub(crate) fn default_entity_code(entity: &str) -> String {` |

Do not touch function bodies, callers, or any other file.

- [ ] **Step 2: 编译检查**

Run: `cargo check --no-default-features --features model2vec`
Expected: clean build, no new warnings. Any `unused: function is never used` 警告表示某个 fn 之前只被内部调用、现在被 pub(crate) 后没被其他 module 用——在 Task 2 引入 `signals.rs` 调用它们之后会消失，这一步不用管。

- [ ] **Step 3: 跑既有测试确认零回归**

Run: `cargo test --no-default-features --features model2vec --lib aaak`
Expected: 既有 aaak unit tests 全部 PASS（visibility 变更不影响行为）。

- [ ] **Step 4: Commit**

```bash
git add src/aaak/codec.rs
git commit -m "$(cat <<'EOF'
refactor(aaak): expose 7 codec helpers as pub(crate) for signals module (P7 task 1)

Zero logic change. Pure visibility bump on normalize_whitespace,
extract_entities, extract_topics, detect_flags, detect_emotions,
infer_weight, default_entity_code so the new src/aaak/signals.rs
module can reach them.

Spec: specs/p7-search-structured-signals.spec.md Decisions line
  "src/aaak/codec.rs 里 7 处 fn 升为 pub(crate) fn (0 逻辑变更)"

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: New `src/aaak/signals.rs` module + unit tests (TDD)

**Files:**
- Create: `src/aaak/signals.rs`
- Modify: `src/aaak/mod.rs`
- Test: `src/aaak/signals.rs` 内嵌 `#[cfg(test)] mod tests`

**TDD order**: 先写 struct 和 stub analyze（返回 placeholder），再写失败的 unit test，再写真实现让测试通过。

- [ ] **Step 1: 创建 `src/aaak/signals.rs` stub**

Write this exact content (stub `analyze` intentionally returns wrong values so tests fail):

```rust
//! Public analysis layer extracted from AaakCodec internals.
//!
//! Provides structured signal extraction (entities, topics, flags, emotions,
//! importance) from arbitrary text, without going through the full AAAK
//! encoding pipeline. Used by `mempal_search` to attach structured metadata
//! to `SearchResultDto` instances.
//!
//! Design: docs/specs/2026-04-13-p7-search-structured-signals.md
//! Spec:   specs/p7-search-structured-signals.spec.md

use serde::{Deserialize, Serialize};

/// Structured signals extracted from a piece of text by the AAAK analysis
/// primitives. Mirrors the fields produced by `AaakCodec` internally, but
/// without the AAAK document format wrapping.
///
/// Sentinel semantics (matching existing extractor behavior — P7 does NOT
/// change extractor algorithms):
/// - `entities`: always has >= 1 entry. When no entities are detected,
///   contains `["UNK"]` (the `DEFAULT_ENTITY_CODE` sentinel).
/// - `flags`: always has >= 1 entry. When no flag keywords matched,
///   contains `["CORE"]` (the "uncategorized" sentinel). Agents filter
///   real decisions via `flags.contains("DECISION")` and detect the
///   uncategorized case via `flags == ["CORE"]`.
/// - `emotions`: always has >= 1 entry. Defaults to `["determ"]`
///   (the `DEFAULT_EMOTION` sentinel).
/// - `topics`: can be empty. `extract_topics` does not add a default.
/// - `importance_stars`: always 2, 3, or 4. Direct output of
///   `infer_weight(&flags)`; the else branch returns 2. No post-processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AaakSignals {
    pub entities: Vec<String>,
    pub topics: Vec<String>,
    pub flags: Vec<String>,
    pub emotions: Vec<String>,
    pub importance_stars: u8,
}

/// Analyze a piece of text and extract structured signals.
///
/// Infallible: the underlying extractors already fall back to sentinel
/// values for degenerate inputs, and Rust's `&str` guarantees valid UTF-8.
pub fn analyze(_text: &str) -> AaakSignals {
    // STUB — replaced in Task 2 Step 3. Returns obviously-wrong sentinel
    // so unit tests in Step 2 fail loudly.
    AaakSignals {
        entities: Vec::new(),
        topics: Vec::new(),
        flags: Vec::new(),
        emotions: Vec::new(),
        importance_stars: 0,
    }
}
```

- [ ] **Step 2: 写 4 个失败的 unit test**

Append to `src/aaak/signals.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Spec scenario S3 (importance_stars defaults to 2 for uncategorized).
    #[test]
    fn test_importance_stars_defaults_to_2_for_uncategorized_text() {
        let signals = analyze("weather update today");
        assert_eq!(signals.importance_stars, 2, "else branch of infer_weight");
        assert!(
            signals.flags.contains(&"CORE".to_string()),
            "CORE sentinel fallback from detect_flags"
        );
        assert!(
            !signals.flags.contains(&"DECISION".to_string()),
            "no DECISION keyword in input"
        );
    }

    /// Spec scenario S5 (empty content → sentinel defaults).
    #[test]
    fn test_empty_content_yields_sentinel_defaults() {
        let signals = analyze("");
        assert_eq!(signals.entities, vec!["UNK".to_string()]);
        assert_eq!(signals.flags, vec!["CORE".to_string()]);
        assert_eq!(signals.emotions, vec!["determ".to_string()]);
        assert!(signals.topics.is_empty(), "extract_topics adds no default");
        assert_eq!(signals.importance_stars, 2);
    }

    /// Spec scenario S6 (whitespace content matches empty sentinel behavior).
    #[test]
    fn test_whitespace_content_matches_empty_sentinel_behavior() {
        let signals = analyze("   \t\n  ");
        assert_eq!(signals.entities, vec!["UNK".to_string()]);
        assert_eq!(signals.flags, vec!["CORE".to_string()]);
        assert_eq!(signals.emotions, vec!["determ".to_string()]);
        assert!(signals.topics.is_empty());
        assert_eq!(signals.importance_stars, 2);
    }

    /// Spec scenario S12 (entities never empty after code mapping,
    /// matches 3-4 uppercase letters).
    #[test]
    fn test_analyze_entities_never_empty_after_code_mapping() {
        let cases = [
            "",
            "   \t\n  ",
            "12345",
            "just a boring sentence",
            "Decision: use Arc<Mutex<>>",
        ];
        for text in cases {
            let signals = analyze(text);
            assert!(
                !signals.entities.is_empty(),
                "entities empty for input: {text:?}"
            );
            let first = &signals.entities[0];
            let is_3_4_upper = first.chars().count() >= 3
                && first.chars().count() <= 4
                && first.chars().all(|c| c.is_ascii_uppercase());
            assert!(
                is_3_4_upper,
                "first entity {first:?} (from input {text:?}) should be 3-4 uppercase letters"
            );
        }
    }
}
```

- [ ] **Step 3: 把 signals 挂到 `src/aaak/mod.rs` 上**

Edit `src/aaak/mod.rs`:

```rust
#![warn(clippy::all)]

mod codec;
mod model;
mod parse;
pub mod signals;
mod spec;

pub use codec::AaakCodec;
pub use model::{
    AaakDocument, AaakHeader, AaakLine, AaakMeta, ArcLine, EncodeOutput, EncodeReport, ParseError,
    RoundtripReport, Tunnel, Zettel,
};
pub use signals::{AaakSignals, analyze};
pub use spec::generate_spec;
```

- [ ] **Step 4: 跑 unit test 确认它们 FAIL**

Run:
```
cargo test --no-default-features --features model2vec --lib aaak::signals::tests
```
Expected: 4 tests, 4 failures. Each failure should point at the stub returning wrong values (empty vecs, importance_stars=0).

**If any test passes with the stub**: the test's assertions are wrong or too loose — fix before proceeding.

- [ ] **Step 5: 替换 stub 为真实 `analyze` 实现**

Replace the stub `pub fn analyze` body in `src/aaak/signals.rs` with:

```rust
pub fn analyze(text: &str) -> AaakSignals {
    use std::collections::BTreeSet;

    let normalized = super::codec::normalize_whitespace(text);

    // Entities: raw extraction → 3-letter code mapping → dedup → UNK fallback.
    // Mirrors encode()'s behavior at src/aaak/codec.rs:213-231 but stateless
    // (no custom entity_map, uses only default_entity_code).
    let mut entity_codes: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for entity in super::codec::extract_entities(&normalized) {
        let code = super::codec::default_entity_code(&entity);
        if seen.insert(code.clone()) {
            entity_codes.push(code);
        }
    }
    if entity_codes.is_empty() {
        entity_codes.push("UNK".to_string());
    }

    // Flags already contain "CORE" fallback when no flag keyword matched
    // (see detect_flags src/aaak/codec.rs:412-414). Pass through to
    // infer_weight directly — no .max(1) post-processing.
    let flags = super::codec::detect_flags(&normalized);
    let importance_stars = super::codec::infer_weight(&flags);

    AaakSignals {
        entities: entity_codes,
        topics: super::codec::extract_topics(&normalized),
        emotions: super::codec::detect_emotions(&normalized),
        importance_stars,
        flags,
    }
}
```

- [ ] **Step 6: 跑 unit test 确认 4 个都 PASS**

Run:
```
cargo test --no-default-features --features model2vec --lib aaak::signals::tests
```
Expected: `test result: ok. 4 passed; 0 failed`.

- [ ] **Step 7: 跑完整 lib tests 确认 visibility 变更 + 新 module 没破坏任何现有测试**

Run:
```
cargo test --no-default-features --features model2vec --lib
```
Expected: all pre-existing lib tests still pass.

- [ ] **Step 8: Commit**

```bash
git add src/aaak/signals.rs src/aaak/mod.rs
git commit -m "$(cat <<'EOF'
feat(aaak): add signals::analyze public API for structured signal extraction (P7 task 2)

New module src/aaak/signals.rs exposes AaakSignals struct and
analyze(text) -> AaakSignals. analyze() wraps the existing codec.rs
extractors (normalize_whitespace, extract_entities + default_entity_code
mapping, detect_flags, detect_emotions, infer_weight, extract_topics),
mirroring encode()'s stateless signal extraction path without producing
the AAAK document wrapper.

Sentinel defaults preserved: entities ≥1 (UNK fallback after code
mapping), flags ≥1 (CORE from detect_flags), emotions ≥1 (determ from
detect_emotions), topics can be empty, importance_stars ∈ {2,3,4}.

Unit tests cover spec scenarios S3 / S5 / S6 / S12:
- test_importance_stars_defaults_to_2_for_uncategorized_text
- test_empty_content_yields_sentinel_defaults
- test_whitespace_content_matches_empty_sentinel_behavior
- test_analyze_entities_never_empty_after_code_mapping

Spec: specs/p7-search-structured-signals.spec.md
Design: docs/specs/2026-04-13-p7-search-structured-signals.md

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Extend `SearchResultDto` + named constructor + delete `From` impl

**Files:**
- Modify: `src/mcp/tools.rs` (struct @ line 33-45; `From` impl @ line 244-257)

**TDD approach for this task**: no new tests yet — we're touching the DTO shape. We rely on `cargo check` to catch break points. The integration tests in Task 5 will exercise the shape end-to-end. After this task, `server.rs` will fail to compile (it still calls `SearchResultDto::from`), which is expected and will be fixed in Task 4.

- [ ] **Step 1: 扩展 `SearchResultDto` 字段**

Replace the `pub struct SearchResultDto { ... }` block (currently 8 fields) with the 13-field version:

```rust
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResultDto {
    pub drawer_id: String,
    /// RAW drawer text, byte-identical to what was ingested. Never compressed,
    /// never transformed. This is the authoritative quote source. Structured
    /// metadata lives in the `entities` / `topics` / `flags` / `emotions` /
    /// `importance_stars` fields below.
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub similarity: f32,
    pub route: RouteDecisionDto,
    /// Other wings sharing this room (tunnel cross-references).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tunnel_hints: Vec<String>,

    // ---- P7: structured signals, always populated on the wire ----
    /// 3-letter entity codes (e.g. "DEC", "ARC") extracted from `content` via
    /// `aaak::signals::analyze`. Always has >= 1 entry — when no entities
    /// are detected, contains `["UNK"]`. Enables agents to filter/group
    /// results without parsing AAAK grammar.
    #[serde(default)]
    pub entities: Vec<String>,

    /// Topic keywords (snake_case) extracted from `content`. May be empty
    /// when no topics were detected (`extract_topics` does not add a default).
    #[serde(default)]
    pub topics: Vec<String>,

    /// Category flags: DECISION, CORE, PIVOT, ORIGIN, TECHNICAL, SENSITIVE.
    /// Always has >= 1 entry — when no flag keywords matched, contains
    /// `["CORE"]` (the uncategorized sentinel). Agents filter real decisions
    /// via `flags.contains("DECISION")` and detect the uncategorized case
    /// via `flags == ["CORE"]`.
    #[serde(default)]
    pub flags: Vec<String>,

    /// Emotion codes (mindset tags) extracted from `content`. Always has
    /// >= 1 entry — when no emotions detected, contains `["determ"]`
    /// (the `DEFAULT_EMOTION` sentinel).
    #[serde(default)]
    pub emotions: Vec<String>,

    /// Importance rating, always **2, 3, or 4**. Direct output of
    /// `infer_weight(&flags)`: 4 when flags contain DECISION or PIVOT,
    /// 3 when TECHNICAL, 2 otherwise (including the CORE fallback case).
    #[serde(default = "default_importance_stars")]
    pub importance_stars: u8,
}

fn default_importance_stars() -> u8 {
    2
}
```

**Serde contract**: new signal fields use `#[serde(default)]` (deserialize-only default for backward compat with older clients). They intentionally do NOT use `#[serde(skip_serializing_if = ...)]` — server responses always emit these 5 fields, so clients can rely on their presence. Only `tunnel_hints` keeps its existing `skip_serializing_if` (out of P7 scope).

- [ ] **Step 2: 删除 `impl From<SearchResult> for SearchResultDto`**

Remove the entire `impl` block from tools.rs (currently lines 244-257). Do NOT touch the neighboring `impl From<RouteDecision>` or `impl From<TaxonomyEntry>` impls — they're unrelated.

- [ ] **Step 3: 添加 `impl SearchResultDto { pub fn with_signals_from_result }` 命名构造函数**

Insert this block where the deleted `From<SearchResult>` used to live:

```rust
impl SearchResultDto {
    /// Build a `SearchResultDto` from a raw `SearchResult`, attaching
    /// P7 structured signals via `aaak::signals::analyze(&result.content)`.
    ///
    /// Replaces the deleted `impl From<SearchResult> for SearchResultDto` —
    /// named explicitly so the "signals are attached here" step is visible
    /// at every call site.
    pub fn with_signals_from_result(result: SearchResult) -> Self {
        let signals = crate::aaak::analyze(&result.content);
        Self {
            drawer_id: result.drawer_id,
            content: result.content,
            wing: result.wing,
            room: result.room,
            source_file: result.source_file,
            similarity: result.similarity,
            route: result.route.into(),
            tunnel_hints: result.tunnel_hints,
            entities: signals.entities,
            topics: signals.topics,
            flags: signals.flags,
            emotions: signals.emotions,
            importance_stars: signals.importance_stars,
        }
    }
}
```

**Visibility rationale**: `pub` (not `pub(crate)`) so `tests/search_structured_signals.rs` (a separate crate) can call it. This is the single test-visible seam into the handler's DTO-construction logic.

**Design compliance note**: the design doc's Open Question 1 and spec Decisions list both say "删除 From impl，handler 里显式构造". The spirit is "no implicit trait conversion at the conversion site". A named inherent method with a single call site (the handler) satisfies that spirit while remaining DRY with integration tests. If review disagrees, fallback is inlining the construction into the handler and duplicating it in the tests module.

- [ ] **Step 4: 编译检查（预期 server.rs 报错）**

Run: `cargo check --no-default-features --features model2vec`
Expected: **one specific error** in `src/mcp/server.rs` around line 139:
```
error[E0599]: no function or associated item named `from` found for struct `SearchResultDto` ...
```
This is expected — `SearchResultDto::from` no longer exists. Task 4 wires up the new constructor. Any **other** error (missing field, type mismatch in the new struct) is a real problem — stop and fix before Task 4.

- [ ] **Step 5: Commit**

```bash
git add src/mcp/tools.rs
git commit -m "$(cat <<'EOF'
feat(mcp): extend SearchResultDto with 5 structured signal fields + named constructor (P7 task 3)

Adds entities/topics/flags/emotions/importance_stars to SearchResultDto,
always emitted on the wire. Replaces the implicit impl From<SearchResult>
with the named constructor SearchResultDto::with_signals_from_result,
which calls crate::aaak::analyze internally so the "attach signals here"
step is explicit at every call site.

Serde contract:
- #[serde(default)] on each new field for backward compat with older
  clients sending requests without these fields (server responses still
  always emit them).
- Intentionally NOT using skip_serializing_if — wire shape is stable
  per P7 design risk table.

Build is intentionally broken at src/mcp/server.rs (still calls the
deleted SearchResultDto::from); wired up in P7 task 4.

Spec: specs/p7-search-structured-signals.spec.md
Design: docs/specs/2026-04-13-p7-search-structured-signals.md

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

> **Note on pre-commit hook**: if the hook runs `cargo check` and fails because of the known server.rs break, re-do Task 3 + Task 4 as a single commit instead. In that case, skip this Step 5 commit and commit once at the end of Task 4 Step 4.

---

## Task 4: Wire `mempal_search` handler to the new constructor

**Files:**
- Modify: `src/mcp/server.rs` (handler tail @ line 138-140; tool description)

- [ ] **Step 1: 替换 handler 的 collect 调用**

In `src/mcp/server.rs`, replace the `mempal_search` handler's trailing `Ok(Json(...))` block (currently lines 138-140) with:

```rust
        Ok(Json(SearchResponse {
            results: results
                .into_iter()
                .map(SearchResultDto::with_signals_from_result)
                .collect(),
        }))
```

- [ ] **Step 2: 在 `#[tool(description = ...)]` 里追加新字段说明**

The current description string (around line 103-105) ends with `...for citation."`. Append this sentence so clients see the new fields in tool schema:

> `Each result also carries AAAK-derived structured signals — entities (3-letter codes), topics, flags (DECISION/CORE/PIVOT/TECHNICAL/…), emotions, and importance_stars (2-4) — extracted from content. Use flags.contains(\"DECISION\") to filter real decisions and sort by importance_stars for relevance.`

Keep the full string as a single Rust string literal (rmcp attribute macro doesn't accept raw strings, so use escaped double quotes). If the existing description uses a different quoting style, match it.

- [ ] **Step 3: `cargo check` + `cargo clippy`**

Run:
```
cargo check --no-default-features --features model2vec
cargo clippy --no-default-features --features model2vec -- -D warnings
```
Expected: both clean. No new warnings.

- [ ] **Step 4: 跑完整 lib + 现有 integration tests 确认无回归**

Run:
```
cargo test --no-default-features --features model2vec --lib
cargo test --no-default-features --features model2vec --test cowork_peek
```
Expected: all PASS. P6 integration test `cowork_peek` must not be affected (MCP server state is the same; the only changed field shape is SearchResultDto which P6 doesn't touch).

- [ ] **Step 5: Commit**

```bash
git add src/mcp/server.rs
git commit -m "$(cat <<'EOF'
feat(mcp): wire mempal_search handler to SearchResultDto::with_signals_from_result (P7 task 4)

mempal_search responses now always carry 5 structured signal fields
(entities, topics, flags, emotions, importance_stars) extracted via
aaak::signals::analyze. The content field remains byte-identical raw
drawer text. Tool description updated so MCP clients see the new
fields in the schema.

No new request parameters, no change to content semantics, no schema
migration — pure additive extension.

Spec: specs/p7-search-structured-signals.spec.md
Design: docs/specs/2026-04-13-p7-search-structured-signals.md

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Integration tests `tests/search_structured_signals.rs` (8 scenarios)

**Files:**
- Create: `tests/search_structured_signals.rs`

**Scenarios in this file** (8; the 4 unit scenarios S3/S5/S6/S12 already live in `src/aaak/signals.rs`):

| ID | Filter name | Focus |
|----|---|---|
| S1 | `test_search_response_includes_structured_signals` | 5 fields present + non-empty where spec requires |
| S2 | `test_decision_keyword_yields_decision_flag` | DECISION content → flags includes "DECISION", importance_stars >= 2 |
| S4 | `test_pure_cjk_content_yields_jieba_derived_entities` | 纯中文 content → entities != `["UNK"]` |
| S7 | `test_content_field_byte_identical_to_raw` | content byte-level unchanged, no `V1\|` prefix, no ★ |
| S8 | `test_drawer_id_and_source_file_unchanged_by_signals` | drawer_id + source_file byte-identical |
| S9 | `test_result_count_unchanged_after_signals_wiring` | top_k=5 returns exactly 5 |
| S10 | `test_search_with_signals_has_no_db_side_effects` | drawer_count / triple_count / schema_version 不变（3 次 search） |
| S11 | `test_zero_result_query_returns_empty_response` | `results == []` 且非 null |

- [ ] **Step 1: 决定测试入口形式**

Integration tests call `SearchResultDto::with_signals_from_result(result)` directly after seeding a `Database` + running `search_with_vector`. This exercises the exact DTO-construction function the MCP handler uses (same code path, zero duplication), without instantiating the rmcp server trait machinery.

Required public surface for tests:
- `mempal::core::db::Database` (already public, P6 test uses it)
- `mempal::embed::EmbedderFactory` + `Model2VecFactory` (already public)
- `mempal::search::{resolve_route, search_with_vector}` (verify they're `pub` before writing tests — if not, add a `pub fn` wrapper in `src/search/mod.rs` as a **separate prep commit** ahead of this task)
- `mempal::mcp::SearchResultDto::with_signals_from_result` (exposed in Task 3)
- `mempal::aaak::{AaakSignals, analyze}` (exposed in Task 2)

**Prep check before writing the test file**:

```
cargo doc --no-default-features --features model2vec --lib --no-deps --open 2>/dev/null
```

or just grep:

```
grep -n 'pub fn resolve_route\|pub fn search_with_vector' src/search/*.rs
```

If either is private, commit the visibility bump as Task 5 Step 0a before writing tests. Explicit sub-step below.

- [ ] **Step 1a: (conditional) Expose search pipeline fns if currently private**

If `resolve_route` or `search_with_vector` are not `pub`, edit `src/search/mod.rs` (or the respective module) to mark them `pub`. Commit as a separate single-step commit:

```bash
git add src/search/mod.rs  # or the actual file
git commit -m "refactor(search): expose resolve_route/search_with_vector as pub for integration tests (P7 task 5 prep)"
```

If they're already `pub`, skip this sub-step.

- [ ] **Step 2: 创建 `tests/search_structured_signals.rs` 空壳 + 共享 helper**

```rust
//! Integration tests for P7 mempal_search structured signals.
//!
//! Run with:
//!   cargo test --test search_structured_signals --no-default-features --features model2vec
//!
//! These tests build a hermetic tempfile palace.db, seed drawers, invoke
//! the real search pipeline (resolve_route + search_with_vector), and
//! construct SearchResultDto via the production code path
//! (SearchResultDto::with_signals_from_result). They do NOT touch
//! ~/.mempal/palace.db.

use mempal::aaak::analyze;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::core::utils::{build_drawer_id, current_timestamp};
use mempal::embed::{EmbedderFactory, Model2VecFactory};
use mempal::mcp::tools::SearchResultDto;
use mempal::search::{resolve_route, search_with_vector};
use std::path::PathBuf;
use tempfile::TempDir;

/// Bring up a tempfile palace.db, a real model2vec embedder, and optionally
/// seed the DB with the given drawers. Returns (TempDir guard, Database,
/// embedder factory).
///
/// Seeded drawers are inserted via the full ingest-style path so
/// `drawer_vectors` get populated — a search-only test needs real vectors.
async fn harness(seeds: &[(&str, &str, Option<&str>, &str)]) -> (TempDir, Database, Box<dyn EmbedderFactory>) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let factory = Box::new(Model2VecFactory::default());
    let embedder = factory.build().await.expect("build embedder");

    // Seed drawers. Arguments per seed tuple: (wing, room_or_empty, source_file, content).
    for (wing, room, source_file, content) in seeds {
        let drawer_id = build_drawer_id(wing, *room, content);
        let vectors = embedder.embed(&[*content]).await.expect("embed");
        let vector = vectors.into_iter().next().expect("vector");
        let drawer = Drawer {
            drawer_id: drawer_id.clone(),
            wing: (*wing).to_string(),
            room: room.map(|r| r.to_string()),
            content: (*content).to_string(),
            source_file: source_file.map(|s| s.to_string()).unwrap_or_default(),
            source_type: SourceType::Note,
            created_at: current_timestamp(),
            importance: 0,
            deleted_at: None,
        };
        db.insert_drawer(&drawer, &vector).expect("insert drawer");
    }

    (tmp, db, factory)
}

/// Run the full search pipeline → DTO construction for a given query.
async fn run_search(
    db: &Database,
    factory: &dyn EmbedderFactory,
    query: &str,
    top_k: usize,
) -> Vec<SearchResultDto> {
    let embedder = factory.build().await.expect("build embedder");
    let query_vector = embedder
        .embed(&[query])
        .await
        .expect("embed query")
        .into_iter()
        .next()
        .expect("query vector");
    let route = resolve_route(db, query, None, None).expect("resolve route");
    let raw = search_with_vector(db, query, &query_vector, route, top_k).expect("search");
    raw.into_iter()
        .map(SearchResultDto::with_signals_from_result)
        .collect()
}
```

**Important**: the field names on `Drawer` and the signature of `db.insert_drawer` must match the current source. Verify by grepping `src/core/db.rs` for `fn insert_drawer` and adjusting argument count/types. If the real insertion helper is different (e.g., an `Ingester` in `src/ingest/`), use that instead — the goal is just "get a drawer + vector into the DB", the mechanism doesn't matter.

Also: `Model2VecFactory` may be called `DefaultEmbedderFactory` or similar — check with `grep -r "EmbedderFactory" src/embed/`. Same for `build_drawer_id` and `current_timestamp`.

- [ ] **Step 3: 写 S1 (happy path, all fields present)**

```rust
#[tokio::test]
async fn test_search_response_includes_structured_signals() {
    let (_tmp, db, factory) = harness(&[
        ("mempal", Some("design"), Some("notes.md"),
         "Decision: use Arc<Mutex<>> for shared state across handlers"),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "state", 10).await;
    assert!(!dtos.is_empty(), "expected at least 1 result");

    let hit = &dtos[0];
    assert!(!hit.entities.is_empty(), "entities must have >= 1 entry");
    assert!(!hit.flags.is_empty(), "flags must have >= 1 entry (CORE sentinel fallback)");
    assert!(!hit.emotions.is_empty(), "emotions must have >= 1 entry (determ sentinel fallback)");
    assert!(hit.importance_stars >= 2, "importance_stars min is 2 (else branch of infer_weight)");
    // topics can be empty (extract_topics adds no default); just check the field exists and is a Vec
    let _ = hit.topics.len();
}
```

Run: `cargo test --test search_structured_signals test_search_response_includes_structured_signals --no-default-features --features model2vec`
Expected: PASS (because Task 4 already wired production).

If it fails on seeding (harness errors), fix the helper first — don't touch production code.

- [ ] **Step 4: 写 S2 (DECISION flag)**

```rust
#[tokio::test]
async fn test_decision_keyword_yields_decision_flag() {
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("decision.md"),
         "Decision: chose X over Y because Z outperforms under load"),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "decision X Y", 10).await;
    assert!(!dtos.is_empty());
    let hit = &dtos[0];
    assert!(
        hit.flags.contains(&"DECISION".to_string()),
        "expected DECISION in flags, got {:?}",
        hit.flags
    );
    assert!(hit.importance_stars >= 2);
}
```

Run + verify PASS.

- [ ] **Step 5: 写 S4 (pure CJK content)**

```rust
#[tokio::test]
async fn test_pure_cjk_content_yields_jieba_derived_entities() {
    // Pure CJK, no ASCII — so the assertion "entities != [\"UNK\"]" can only
    // pass if the jieba CJK branch in extract_entities produced a real code.
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("cjk.md"),
         "系统决策：采用共享内存同步机制解决状态漂移问题"),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "共享内存", 10).await;
    assert!(!dtos.is_empty(), "expected hit for CJK query");
    let hit = &dtos[0];
    assert!(!hit.entities.is_empty());
    assert_ne!(
        hit.entities,
        vec!["UNK".to_string()],
        "pure CJK should produce a non-UNK entity code via jieba, got {:?}",
        hit.entities
    );
}
```

Run + verify PASS. If it fails with `["UNK"]`, the issue is in `default_entity_code` for non-ASCII inputs (it uses stable_hash → 3 uppercase letters, should never return "UNK" for non-empty CJK); re-read codec.rs:480-499 and the test seed content.

- [ ] **Step 6: 写 S7/S8 (content & id byte-identical)**

```rust
#[tokio::test]
async fn test_content_field_byte_identical_to_raw() {
    let raw = "Decision: use Arc<Mutex<>>";
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("fixture.md"), raw),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "Mutex", 10).await;
    assert!(!dtos.is_empty());
    let hit = &dtos[0];
    assert_eq!(hit.content, raw, "content must be byte-identical to raw");
    assert!(!hit.content.starts_with("V1|"), "content must not be AAAK formatted");
    assert!(!hit.content.contains('★'), "content must not contain star glyph");
}

#[tokio::test]
async fn test_drawer_id_and_source_file_unchanged_by_signals() {
    let seeds: &[(&str, Option<&str>, Option<&str>, &str)] = &[
        ("mempal", None, Some("a.md"), "first drawer about routing"),
        ("mempal", None, Some("b.md"), "second drawer about indexing"),
    ];
    // Compute expected ids the same way ingest does.
    let expected: Vec<(String, String)> = seeds
        .iter()
        .map(|(wing, room, src, content)| {
            (
                build_drawer_id(wing, *room, content),
                src.unwrap_or("").to_string(),
            )
        })
        .collect();

    // Rebuild harness expecting the tuple shape used by the helper
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("a.md"), "first drawer about routing"),
        ("mempal", None, Some("b.md"), "second drawer about indexing"),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "drawer", 10).await;
    assert_eq!(dtos.len(), 2);

    for dto in &dtos {
        let matched = expected.iter().any(|(id, src)| {
            dto.drawer_id == *id && dto.source_file == *src
        });
        assert!(
            matched,
            "dto {{drawer_id: {}, source_file: {}}} did not match any seed",
            dto.drawer_id, dto.source_file
        );
    }
}
```

Run + verify PASS.

- [ ] **Step 7: 写 S9 + S11 (count unchanged + zero-result query)**

```rust
#[tokio::test]
async fn test_result_count_unchanged_after_signals_wiring() {
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("s1.md"), "first note about architecture"),
        ("mempal", None, Some("s2.md"), "second note about architecture"),
        ("mempal", None, Some("s3.md"), "third note about architecture"),
        ("mempal", None, Some("s4.md"), "fourth note about architecture"),
        ("mempal", None, Some("s5.md"), "fifth note about architecture"),
    ]).await;

    let dtos = run_search(&db, factory.as_ref(), "architecture", 5).await;
    assert_eq!(dtos.len(), 5, "top_k=5 must return exactly 5 results");
}

#[tokio::test]
async fn test_zero_result_query_returns_empty_response() {
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("s1.md"), "some regular content"),
    ]).await;

    let dtos = run_search(
        &db,
        factory.as_ref(),
        "nonexistent_xyzqqq_impossible_match",
        10,
    ).await;
    assert_eq!(dtos.len(), 0, "zero-match query must return empty vec");

    // Also assert the JSON serialization of the empty response is [] not null.
    let response_json = serde_json::json!({ "results": dtos });
    let s = serde_json::to_string(&response_json).unwrap();
    assert!(s.contains("\"results\":[]"), "empty results must serialize to [], got {s}");
}
```

**Note on S11**: BM25 + vector + RRF may still return results for an "impossible" query because vector similarity is never exactly 0. If the test flakes, either (a) lower top_k + check that all returned results have similarity below a threshold, or (b) switch to counting raw drawers matching the query's routing filters. The spec says "返回合法空 response" so the strict check is `results.len() == 0`; if BM25/vector happens to score something, the test needs to reflect that reality. **If spec and reality disagree, fix the test, not production** (P7 is not a search-quality change).

Run + verify PASS.

- [ ] **Step 8: 写 S10 (no DB side effects — real snapshot, P6 pattern)**

```rust
#[tokio::test]
async fn test_search_with_signals_has_no_db_side_effects() {
    let (_tmp, db, factory) = harness(&[
        ("mempal", None, Some("s.md"), "any content"),
    ]).await;

    let drawers_before = db.drawer_count().expect("drawer count");
    let triples_before = db.triple_count().expect("triple count");
    let schema_before = db.schema_version().expect("schema version");
    assert_eq!(schema_before, 4, "baseline palace.db should be schema v4");

    for _ in 0..3 {
        let _ = run_search(&db, factory.as_ref(), "content", 10).await;
    }

    let drawers_after = db.drawer_count().expect("drawer count");
    let triples_after = db.triple_count().expect("triple count");
    let schema_after = db.schema_version().expect("schema version");

    assert_eq!(drawers_before, drawers_after, "drawer_count changed after search");
    assert_eq!(triples_before, triples_after, "triple_count changed after search");
    assert_eq!(schema_before, schema_after, "schema_version changed after search");
}
```

**Method names** (`drawer_count` / `triple_count` / `schema_version`) must match the real public API on `Database`. Verify by looking at `src/core/db.rs` (P6's `tests/cowork_peek.rs` already uses these — copy the exact call style).

Run + verify PASS.

- [ ] **Step 9: 跑整个 integration 文件确认 8 个都 PASS**

```
cargo test --test search_structured_signals --no-default-features --features model2vec
```
Expected: `test result: ok. 8 passed; 0 failed`.

- [ ] **Step 10: Commit**

```bash
git add tests/search_structured_signals.rs
git commit -m "$(cat <<'EOF'
test(mcp): integration tests for mempal_search structured signals (P7 task 5)

Adds tests/search_structured_signals.rs covering the 8 integration
scenarios from specs/p7-search-structured-signals.spec.md:
- S1:  test_search_response_includes_structured_signals
- S2:  test_decision_keyword_yields_decision_flag
- S4:  test_pure_cjk_content_yields_jieba_derived_entities
- S7:  test_content_field_byte_identical_to_raw
- S8:  test_drawer_id_and_source_file_unchanged_by_signals
- S9:  test_result_count_unchanged_after_signals_wiring
- S10: test_search_with_signals_has_no_db_side_effects
- S11: test_zero_result_query_returns_empty_response

Unit scenarios S3/S5/S6/S12 live in src/aaak/signals.rs (task 2).

Hermetic pattern: tempfile palace.db + real Model2VecFactory embedder +
direct call to SearchResultDto::with_signals_from_result (the same
function the mempal_search MCP handler calls), bypassing rmcp trait
machinery. Follows P6's tempfile-DB-snapshot approach for S10.

Spec: specs/p7-search-structured-signals.spec.md

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Full verification sweep

**Files:** (read-only checks)

- [ ] **Step 1: Full test suite**

```
cargo test --no-default-features --features model2vec
```
Expected: everything PASS. No flakes.

- [ ] **Step 2: Clippy (deny warnings)**

```
cargo clippy --no-default-features --features model2vec --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 3: Rustfmt check**

```
cargo fmt --check
```
Expected: clean. If not, run `cargo fmt` and add the diff to a separate fixup commit:
```bash
git add -u
git commit -m "style: cargo fmt (P7 task 6)"
```

- [ ] **Step 4: agent-spec lint**

```
agent-spec lint specs/p7-search-structured-signals.spec.md --min-score 0.7
```
Expected: spec lint still >= 0.7 (should be unchanged from the committed 100% baseline, but rerun to catch any drift).

- [ ] **Step 5: Tool count sanity check**

The description in `src/core/protocol.rs` (MEMORY_PROTOCOL constant, Rule 0) mentions "7 tools" or "8 tools" depending on whether `mempal_peek_partner` counts. P6 added `mempal_peek_partner` as tool 8. P7 **does not add any new tool** — count stays 8. Verify:

```
grep -c '#\[tool(' src/mcp/server.rs
```
Expected: `8`. If not 8, investigate.

- [ ] **Step 6: MCP tool description sanity check**

```
grep -A2 'name = "mempal_search"' src/mcp/server.rs
```
Expected: description string contains both the original "Search persistent project memory..." prefix and the new "Each result also carries AAAK-derived structured signals..." suffix added in Task 4.

---

## Task 7: Spec status closure + CLAUDE.md update

**Files:**
- Modify: `CLAUDE.md` (Spec table, current Spec section)

- [ ] **Step 1: `CLAUDE.md` 追加 P7 行**

Edit the "已完成的 Spec（P0-P6）" section heading to "已完成的 Spec（P0-P7）" and append this row to the table (after the P6 row):

```markdown
| `specs/p7-search-structured-signals.spec.md` | 完成 | `mempal_search` 响应每条结果附加 5 个 AAAK-derived 结构化字段（entities/topics/flags/emotions/importance_stars），content 保持 raw |
```

Also under "当前 Spec":

```markdown
### 当前 Spec

（无，P7 已完成）
```

And update the "实现计划" list by appending:

```markdown
- `docs/plans/2026-04-13-p7-implementation.md` — P7（已完成）
```

And update "MCP 工具（8 个）"'s description row for `mempal_search` to mention signals:

```markdown
| `mempal_search` | 混合检索（BM25 + 向量 + RRF + tunnel hints）+ AAAK 结构化 signals |
```

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "$(cat <<'EOF'
docs(p7): mark P7 complete in project spec index (P7 task 7)

- Spec table: P0-P6 → P0-P7
- Current Spec: P7 done, none in flight
- Implementation plans: add P7 plan reference
- mempal_search tool description: mention structured signals

Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 3: Save a decision drawer via mempal_ingest (MEMORY_PROTOCOL Rule 4)**

After the final commit lands, call `mempal_ingest` with:
- `wing`: `"mempal"`
- `room`: `"design"`
- `source`: `"docs/plans/2026-04-13-p7-implementation.md"`
- `importance`: `4`
- `content`: a 3-5 sentence decision drawer covering:
  1. What P7 did (expose analyze API, 5 DTO fields, no new params)
  2. Why not Proposal C (empirical +42% size blew up compression premise)
  3. Where to look next (observe agent usage pattern; if nobody uses signals, consider deleting AAAK codec entirely)

Follow CHECK-BEFORE-WRITE: first call `mempal_search("P7 structured signals")` to make sure no equivalent drawer already exists; if Codex or anyone else already ingested one, elaborate/supersede via `mempal_kg` triples instead of writing a duplicate drawer.

---

## Post-Plan Review Gate (DO NOT SKIP)

**Stop here.** Per writing-plans skill, this plan must pass a `plan-document-reviewer` subagent review before execution begins. The user explicitly asked me to stop at this gate — do not start Task 1 until:

1. The user has read the plan
2. The user approves execution OR a reviewer has audited the plan for missing steps / line-number drift / test-coverage gaps

**When execution begins** (in a later session), the executor has two options:
1. **Subagent-Driven (recommended)** — dispatch a fresh subagent per task; review between tasks
2. **Inline Execution** — follow `superpowers:executing-plans` in a long-running session with checkpoints

Both paths honor the bite-sized `- [ ]` steps above. Do not batch-execute multiple tasks in one commit unless a step explicitly instructs it (Task 3 + Task 4 can be merged if the Task 3 commit fails the pre-commit hook due to the known server.rs break).

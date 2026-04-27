# P11 Transcript Noise Strip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Strip scoped Claude Code / Codex transcript UI noise during normalize while preserving user-authored bytes and enabling P10 normalize-version reindex.

**Architecture:** Add a focused `ingest::noise` module with no new dependencies; it uses line/tag whitelist scanning instead of regex because `regex` is not a direct dependency and the spec forbids new deps. `normalize_content_with_options` returns normalized content plus optional stripped-byte metrics; file ingest stamps `CURRENT_NORMALIZE_VERSION = 2`, reports `noise_bytes_stripped`, and CLI exposes `--no-strip-noise`.

**Tech Stack:** Rust 2024, existing `serde_json`, `tokio`, `clap`, SQLite ingest/reindex stack from P10 normalize-version.

---

## Context

- This worktree is stacked on `p10-normalize-version`.
- P10 provides `drawers.normalize_version`, `CURRENT_NORMALIZE_VERSION = 1`, and `mempal reindex --stale`.
- P11 must bump `CURRENT_NORMALIZE_VERSION` to `2`.
- Current code uses `Format::CodexJsonl` for Codex rollout JSONL; there is no `Format::CodexRollout`.
- Current code uses `IngestStats`, not `IngestOutcome`.
- CLI lives in `src/main.rs`, not `src/cli.rs`.
- `regex` is transitive but not a direct dependency; do not use it and do not add it.

## File Map

- Modify: `specs/p11-transcript-noise-strip.spec.md`
  - Correct implementation naming drift: `CodexRollout` -> `CodexJsonl`, `src/cli.rs` -> `src/main.rs`, `IngestOutcome` -> `IngestStats`, regex wording -> dependency-free whitelist scanning.
- Create: `src/ingest/noise.rs`
  - Public strip functions and unit-friendly helpers.
- Modify: `src/ingest/mod.rs`
  - Export `noise`.
  - Add `IngestStats.noise_bytes_stripped`.
  - Add `IngestOptions.no_strip_noise`.
  - Aggregate strip metrics through directory ingest.
- Modify: `src/ingest/normalize.rs`
  - Bump `CURRENT_NORMALIZE_VERSION` to `2`.
  - Add `NormalizeOptions` and `NormalizeOutput`.
  - Apply noise stripping only for `ClaudeJsonl` and `CodexJsonl`.
- Modify: `src/main.rs`
  - Add CLI `ingest --no-strip-noise`.
  - Use `ingest_dir_with_options` for normal and dry-run paths.
  - Print `noise_bytes_stripped` when available.
- Modify: tests that construct `IngestOptions` literals.
- Create: `tests/noise_strip.rs`
  - Unit and integration tests for strip behavior, scope, metrics, no-strip, and reindex.
- Modify: `tests/normalize_version.rs`
  - Update expectations from version `1` to `CURRENT_NORMALIZE_VERSION` where appropriate.
- Modify: `AGENTS.md`, `CLAUDE.md`
  - Move spec to completed in final task.

## Task 1: Spec Drift And Noise Strip Module

**Files:**
- Modify: `specs/p11-transcript-noise-strip.spec.md`
- Create: `src/ingest/noise.rs`
- Modify: `src/ingest/mod.rs`
- Create: `tests/noise_strip.rs`

- [x] **Step 1: Add failing unit tests for strip functions**

Create `tests/noise_strip.rs` with tests:

```rust
#[test]
fn test_claude_jsonl_strips_system_reminder() {}

#[test]
fn test_code_block_preserved_verbatim() {}

#[test]
fn test_user_message_angle_brackets_preserved() {}

#[test]
fn test_codex_rollout_session_markers_stripped() {}

#[test]
fn test_strip_no_match_returns_identity() {}

#[test]
fn test_strip_preserves_unicode_bytes() {}
```

Use direct calls:

```rust
use mempal::ingest::noise::{strip_claude_jsonl_noise, strip_codex_rollout_noise};
```

Expected first run:

```bash
cargo test --test noise_strip test_claude_jsonl_strips_system_reminder -- --exact
```

FAIL because `ingest::noise` does not exist.

- [x] **Step 2: Correct spec drift**

In `specs/p11-transcript-noise-strip.spec.md`:

- Replace `Format::CodexRollout` with current `Format::CodexJsonl`.
- Replace `src/cli.rs` with `src/main.rs`.
- Replace `IngestOutcome` with `IngestStats`.
- Replace "正则白名单" / "regex" wording with "dependency-free whitelist scanning".

Do not change acceptance intent.

- [x] **Step 3: Add `ingest::noise` module**

In `src/ingest/mod.rs`:

```rust
pub mod noise;
```

Create `src/ingest/noise.rs` with:

```rust
pub fn strip_claude_jsonl_noise(content: &str) -> String
pub fn strip_codex_rollout_noise(content: &str) -> String
```

Implementation rules:

- Preserve lines inside fenced code blocks starting with ```.
- Remove `<system-reminder>...</system-reminder>` outside fenced code blocks, including same-line and multi-line blocks.
- Remove standalone `<command-name>...</command-name>` lines outside code blocks.
- Remove standalone JSON array lines whose objects are all `{ "type": "tool_use_id", ... }`.
- Remove DORA/RUST skill loaded banner blocks until the next empty line.
- Remove Codex `[session ... started]` and `[session ... ended]` marker lines.
- Return input unchanged when no rule matches.

Do not add dependencies.

- [x] **Step 4: Run strip unit tests**

```bash
cargo test --test noise_strip test_claude_jsonl_strips_system_reminder -- --exact
cargo test --test noise_strip test_code_block_preserved_verbatim -- --exact
cargo test --test noise_strip test_user_message_angle_brackets_preserved -- --exact
cargo test --test noise_strip test_codex_rollout_session_markers_stripped -- --exact
cargo test --test noise_strip test_strip_no_match_returns_identity -- --exact
cargo test --test noise_strip test_strip_preserves_unicode_bytes -- --exact
```

Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add specs/p11-transcript-noise-strip.spec.md src/ingest/mod.rs src/ingest/noise.rs tests/noise_strip.rs
git commit -m "feat: add scoped transcript noise stripping"
```

## Task 2: Normalize Integration, Metrics, And Version Bump

**Files:**
- Modify: `src/ingest/normalize.rs`
- Modify: `src/ingest/mod.rs`
- Modify: tests using `IngestOptions`
- Modify: `tests/noise_strip.rs`
- Modify: `tests/normalize_version.rs`

- [x] **Step 1: Add failing normalize/ingest tests**

In `tests/noise_strip.rs`, add:

```rust
#[tokio::test]
async fn test_plain_markdown_not_stripped() {}

#[tokio::test]
async fn test_ingest_outcome_reports_stripped_bytes() {}

#[tokio::test]
async fn test_normalize_version_bump_triggers_reindex_opportunity() {}
```

Test shape:

- Use a local stub embedder like `tests/normalize_version.rs`.
- `test_plain_markdown_not_stripped` ingests a `.md` file containing fake `<system-reminder>` and asserts drawer content still contains it.
- `test_ingest_outcome_reports_stripped_bytes` ingests Claude JSONL whose message contains a known-size system reminder and asserts `stats.noise_bytes_stripped` is within the expected range.
- `test_normalize_version_bump_triggers_reindex_opportunity` inserts a stale drawer at version 1 from a Claude JSONL source, runs library `reindex_sources`, and asserts active content has no reminder and version is `2`.

- [x] **Step 2: Add normalize output API**

In `src/ingest/normalize.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeOptions {
    pub strip_noise: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeOutput {
    pub content: String,
    pub noise_bytes_stripped: Option<u64>,
}
```

Add:

```rust
pub fn normalize_content_with_options(
    content: &str,
    format: Format,
    options: NormalizeOptions,
) -> Result<NormalizeOutput>
```

Keep existing:

```rust
pub fn normalize_content(content: &str, format: Format) -> Result<String>
```

as a compatibility wrapper using `NormalizeOptions { strip_noise: true }`.

- [x] **Step 3: Apply noise strip only to transcript formats**

In `normalize_content_with_options`:

- `Format::ClaudeJsonl`: strip extracted message text through `strip_claude_jsonl_noise` only when `options.strip_noise`.
- `Format::CodexJsonl`: strip extracted event message text through `strip_codex_rollout_noise` only when `options.strip_noise`.
- `Format::PlainText`, `Format::ChatGptJson`, `Format::SlackJson`: no noise strip and `noise_bytes_stripped = None`.

Compute stripped bytes as `original_message.len() - stripped_message.len()` summed over messages. Return `Some(total)` only for Claude/Codex when strip is enabled.

- [x] **Step 4: Bump normalize version**

In `src/ingest/normalize.rs`:

```rust
pub const CURRENT_NORMALIZE_VERSION: u32 = 2;
```

Update `tests/normalize_version.rs` assertions that hard-code `1` for current-version behavior to use `CURRENT_NORMALIZE_VERSION`.

- [x] **Step 5: Extend ingest stats and options**

In `src/ingest/mod.rs`:

```rust
pub struct IngestStats {
    ...
    pub noise_bytes_stripped: Option<u64>,
}

pub struct IngestOptions<'a> {
    ...
    pub no_strip_noise: bool,
}
```

Use `normalize_content_with_options`:

```rust
let normalize_output = normalize_content_with_options(
    &content,
    format,
    NormalizeOptions {
        strip_noise: !options.no_strip_noise,
    },
)?;
let normalized = normalize_output.content;
stats.noise_bytes_stripped = normalize_output.noise_bytes_stripped;
```

In `ingest_dir_with_options`, sum `noise_bytes_stripped` across files.

Update all struct literals for `IngestOptions` to include `no_strip_noise: false`, except no-strip tests.

- [x] **Step 6: Run Task 2 tests**

```bash
cargo test --test noise_strip test_plain_markdown_not_stripped -- --exact
cargo test --test noise_strip test_ingest_outcome_reports_stripped_bytes -- --exact
cargo test --test noise_strip test_normalize_version_bump_triggers_reindex_opportunity -- --exact
cargo test --test normalize_version
```

Expected: PASS.

- [x] **Step 7: Commit**

```bash
git add src/ingest/normalize.rs src/ingest/mod.rs tests/noise_strip.rs tests/normalize_version.rs tests/ingest_lock.rs tests/mind_model_bootstrap.rs src/main.rs
git commit -m "feat: apply transcript noise strip during normalize"
```

## Task 3: CLI No-Strip Flag

**Files:**
- Modify: `src/main.rs`
- Modify: `tests/noise_strip.rs`

- [x] **Step 1: Add failing CLI flag test**

In `tests/noise_strip.rs`, add:

```rust
#[test]
fn test_cli_no_strip_noise_flag() {}
```

Use a temp `HOME`, write config, and run:

```bash
mempal ingest <dir> --wing mempal --dry-run --no-strip-noise
```

Assert:

- process succeeds
- stdout contains `noise_bytes_stripped=0` or omits stripped bytes

Because the CLI uses the real embedder for non-dry-run writes, do not make this test require actual embedding downloads. Library integration tests cover written drawer content.

- [x] **Step 2: Add CLI flag**

In `Commands::Ingest` in `src/main.rs`, add:

```rust
#[arg(long)]
no_strip_noise: bool,
```

Pass it into `ingest_command`.

- [x] **Step 3: Wire `IngestOptions.no_strip_noise`**

In `ingest_command`, use `ingest_dir_with_options` for both dry-run and normal paths:

```rust
IngestOptions {
    room: None,
    source_root: Some(dir),
    dry_run,
    source_file_override: None,
    replace_existing_source: false,
    no_strip_noise,
}
```

For file paths, either keep existing directory-only behavior or route file paths through `ingest_file_with_options`. Prefer file support if the change remains small.

- [x] **Step 4: Print strip metric**

Extend CLI output:

```text
dry_run=false files=1 chunks=1 skipped=0 noise_bytes_stripped=2048
```

If `None`, print `noise_bytes_stripped=0` to keep test output deterministic.

- [x] **Step 5: Run CLI test**

```bash
cargo test --test noise_strip test_cli_no_strip_noise_flag -- --exact
```

Expected: PASS.

- [x] **Step 6: Commit**

```bash
git add src/main.rs tests/noise_strip.rs
git commit -m "feat: add no-strip-noise ingest flag"
```

## Task 4: Inventory, Full Verification, And Plan Closure

**Files:**
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/plans/2026-04-23-p11-transcript-noise-strip-implementation.md`

- [x] **Step 1: Update inventory**

In `AGENTS.md` and `CLAUDE.md`:

- Move `specs/p11-transcript-noise-strip.spec.md` from current drafts to completed specs.
- Keep scope description concise: `Claude/Codex transcript noise strip + normalize_version bump to 2`.
- Add this plan as completed:

```markdown
- `docs/plans/2026-04-23-p11-transcript-noise-strip-implementation.md` — P11 transcript noise strip（已完成）
```

- [x] **Step 2: Contract checks**

```bash
agent-spec parse specs/p11-transcript-noise-strip.spec.md
agent-spec lint specs/p11-transcript-noise-strip.spec.md --min-score 0.7
```

Expected: PASS.

- [x] **Step 3: Focused tests**

```bash
cargo test --test noise_strip
cargo test --test normalize_version
cargo test --test ingest_lock
```

Expected: PASS.

- [x] **Step 4: Full verification**

```bash
cargo fmt --check
cargo test
cargo check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: PASS.

- [x] **Step 5: Mark plan complete**

Mark all plan checkboxes and final checklist entries complete.

- [x] **Step 6: Commit**

```bash
git add AGENTS.md CLAUDE.md docs/plans/2026-04-23-p11-transcript-noise-strip-implementation.md
git commit -m "docs: close transcript noise strip plan"
```

## Final Checklist

- [x] Claude JSONL system-reminder blocks are stripped outside code blocks.
- [x] Code blocks, user angle brackets, Chinese, emoji, and quoted text are preserved.
- [x] Plain/Markdown content is not stripped.
- [x] Codex session marker lines are stripped.
- [x] `CURRENT_NORMALIZE_VERSION == 2`.
- [x] Ingest reports `noise_bytes_stripped`.
- [x] `--no-strip-noise` disables transcript stripping.
- [x] `reindex --stale` can refresh version-1 drawers to version 2.
- [x] No schema bump and no new dependencies.

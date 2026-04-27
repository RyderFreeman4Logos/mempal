# P13A Implementation Plan — Wake-Up Consumes Knowledge Statement

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `mempal wake-up` 在面对 knowledge drawers 时优先消费 `statement`，而不是继续把长 `content` rationale 当作默认唤醒文本。

**Architecture:** 本任务不改 schema，不改 search，不引入 runtime assembler。只在现有 CLI wake-up surface 上定义并接线一个 `effective wake-up text`：knowledge drawer 若有非空 `statement` 就用它，否则回退到 `content`；evidence drawer 始终继续用 `content`。plain wake-up、AAAK wake-up、token estimate 同步对齐这一规则。

**Tech Stack:** Rust 2024, 现有 CLI / AAAK codec / integration test harness；不引入新依赖。

**Source Spec:** [specs/p13-wake-up-statement.spec.md](/Users/zhangalex/Work/Projects/AI/mempal/specs/p13-wake-up-statement.spec.md)

---

## File Structure

| File | Role |
|------|------|
| `src/main.rs` | `wake_up_command` / `wake_up_aaak_command` 行为接线；提炼 effective wake-up text helper；保持排序与 `content` raw 语义不变 |
| `tests/mind_model_bootstrap.rs` | P13A integration scenarios；优先复用现有 bootstrap drawer / CLI helpers，而不是新建独立 test target |
| `docs/MIND-MODEL-DESIGN.md` | 仅当实现迫使术语收敛时才修；默认不动 |

## Scope Notes

- 这版只做 **CLI wake-up surface 的 statement/content 切换**
- **不做** `mempal_context`、skill trigger orchestration、REST parity、ingest identity 统一
- `SearchResult.content` / MCP search / CLI search 的 raw 语义不得改变
- wake-up 的排序逻辑不得改变；只能改变“每条 drawer 以什么文本参与 wake-up”

## Pre-Flight Facts

- `src/main.rs::wake_up_command` 当前在 L1 summary 中直接 `truncate_for_summary(&drawer.content, 120)`
- `src/main.rs::wake_up_aaak_command` 当前把 `top_drawers` 的 `content` 直接拼接后送进 `AaakCodec`
- `src/main.rs::estimate_tokens` 当前总是按 `drawer.content` 词数累加
- `Drawer` 已经在 P12 中具备：
  - `memory_kind`
  - `statement`
  - `content`
- `tests/mind_model_bootstrap.rs` 已经有成熟的 `bootstrap_drawer(...)` / CLI test helpers，可直接扩展

---

### Task 1: Effective Wake-Up Text + Plain Wake-Up

**Files:**
- Modify: `tests/mind_model_bootstrap.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add failing plain wake-up tests**

Add these integration tests to `tests/mind_model_bootstrap.rs`:

```rust
#[test]
fn test_wake_up_prefers_knowledge_statement_in_plain_output() {}

#[test]
fn test_wake_up_evidence_drawer_still_uses_content() {}

#[test]
fn test_wake_up_knowledge_without_statement_falls_back_to_content() {}

#[test]
fn test_wake_up_estimated_tokens_use_effective_text() {}

#[test]
fn test_wake_up_protocol_output_unchanged() {}
```

Use existing temp-db helpers plus `Command::new(mempal_bin())` style CLI invocation where appropriate.

Run:

```bash
cargo test --test mind_model_bootstrap test_wake_up_prefers_knowledge_statement_in_plain_output -- --exact
```

Expected: FAIL because wake-up still reads `drawer.content`.

- [ ] **Step 2: Introduce a minimal effective wake-up text helper**

In `src/main.rs`, add a focused helper near the other wake-up utilities:

```rust
fn effective_wake_up_text<'a>(drawer: &'a mempal::core::types::Drawer) -> &'a str {
    match drawer.memory_kind {
        mempal::core::types::MemoryKind::Knowledge => {
            drawer
                .statement
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(drawer.content.as_str())
        }
        mempal::core::types::MemoryKind::Evidence => drawer.content.as_str(),
    }
}
```

Do not allocate unless necessary. This helper is the only behavior switch in P13A.

- [ ] **Step 3: Rewire plain wake-up to use the effective text**

In `wake_up_command`:

- keep `top_drawers(5)` exactly as-is
- keep `source_file`, `drawer_id`, `wing/room`, Memory Protocol output unchanged
- change only the summary line:

```rust
println!("  {}", truncate_for_summary(effective_wake_up_text(drawer), 120));
```

Also change token estimation to a wake-up-specific variant:

```rust
fn estimate_wake_up_tokens(drawers: &[mempal::core::types::Drawer]) -> usize {
    drawers
        .iter()
        .map(|drawer| effective_wake_up_text(drawer).split_whitespace().count())
        .sum()
}
```

Then update `wake_up_command` to call that instead of `estimate_tokens(...)`.

- [ ] **Step 4: Run targeted plain wake-up tests**

Run:

```bash
cargo test --test mind_model_bootstrap test_wake_up_prefers_knowledge_statement_in_plain_output -- --exact
cargo test --test mind_model_bootstrap test_wake_up_evidence_drawer_still_uses_content -- --exact
cargo test --test mind_model_bootstrap test_wake_up_knowledge_without_statement_falls_back_to_content -- --exact
cargo test --test mind_model_bootstrap test_wake_up_estimated_tokens_use_effective_text -- --exact
cargo test --test mind_model_bootstrap test_wake_up_protocol_output_unchanged -- --exact
```

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/mind_model_bootstrap.rs
git commit -m "feat: use statements in plain wake-up"
```

---

### Task 2: AAAK Wake-Up + Ordering Closure

**Files:**
- Modify: `tests/mind_model_bootstrap.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add failing AAAK / ordering tests**

Add these integration tests:

```rust
#[test]
fn test_wake_up_aaak_prefers_knowledge_statement() {}

#[test]
fn test_wake_up_preserves_existing_top_drawer_order() {}
```

The ordering test should verify that changing wake-up payload text does not reorder drawers. Use existing `importance` / `added_at` semantics, not hand-rolled sorting.

Run:

```bash
cargo test --test mind_model_bootstrap test_wake_up_aaak_prefers_knowledge_statement -- --exact
```

Expected: FAIL because `wake_up_aaak_command` still concatenates `drawer.content`.

- [ ] **Step 2: Rewire AAAK wake-up to use the effective text**

In `wake_up_aaak_command`, replace:

```rust
top_drawers
    .iter()
    .map(|drawer| drawer.content.as_str())
```

with:

```rust
top_drawers
    .iter()
    .map(effective_wake_up_text)
```

Keep:
- `AaakCodec::default().encode(...)`
- `wing` / `room` selection from the first drawer
- `"mempal wake-up: no recent drawers"` fallback text

unchanged.

- [ ] **Step 3: Run targeted AAAK / ordering tests**

Run:

```bash
cargo test --test mind_model_bootstrap test_wake_up_aaak_prefers_knowledge_statement -- --exact
cargo test --test mind_model_bootstrap test_wake_up_preserves_existing_top_drawer_order -- --exact
```

Expected: PASS. The first proves `statement` is used; the second proves wake-up ordering still matches `top_drawers(5)`.

- [ ] **Step 4: Run focused verification**

Run:

```bash
cargo test --test mind_model_bootstrap
cargo check
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Expected:
- all P12 + P13A integration tests green
- `cargo check` green
- clippy green
- fmt green

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/mind_model_bootstrap.rs
git commit -m "feat: use statements in aaak wake-up"
```

---

## Final Verification Checklist

- [ ] knowledge drawer 在 plain wake-up 中优先显示 `statement`
- [ ] knowledge drawer 在 `--format aaak` 中优先参与 `statement`
- [ ] evidence drawer 继续按 `content` 唤醒
- [ ] knowledge drawer `statement` 缺失或空白时回退到 `content`
- [ ] `estimated_tokens` 基于 effective wake-up text
- [ ] `mempal wake-up --format protocol` 输出不变
- [ ] wake-up 的 drawer 顺序不变
- [ ] `SearchResult.content` / MCP search / CLI search raw 语义不变

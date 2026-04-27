# P11 Diary Daily Rollup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in `agent-diary` daily rollup ingest so chatty agent diary entries append into one drawer per UTC day and room.

**Architecture:** Put rollup semantics in `mempal::ingest` as shared library code, then route CLI and MCP through it. Add one DB helper for safe drawer content replacement plus vector refresh, preserving existing schema and FTS behavior.

**Tech Stack:** Rust 2024, tokio, rusqlite/sqlite-vec, clap, rmcp, agent-spec.

---

## File Structure

- `src/ingest/diary.rs`: new focused module for rollup constants, day/id/key helpers, validation, lock acquisition, append/upsert, and embedding.
- `src/ingest/mod.rs`: expose diary module, extend `IngestOptions`, and route `ingest_file_with_options` through diary rollup when enabled.
- `src/core/db.rs`: add `upsert_drawer_and_replace_vector` and `diary_rollup_days`; no schema migration.
- `src/mcp/tools.rs`: add `IngestRequest.diary_rollup` and `StatusResponse.diary_rollup_days`.
- `src/mcp/server.rs`: route MCP ingest rollup requests through the shared ingest helper and expose status count.
- `src/main.rs`: add `mempal ingest <path> --diary-rollup --wing agent-diary --room <room>` and print `lock_wait_ms`.
- `src/core/protocol.rs`: update Rule 5a with daily rollup guidance.
- `tests/diary_rollup.rs`: integration tests for day/room identity, append semantics, validation, size limit, vector refresh, and same-day concurrency.

## Task 1: Contract Tests

- [x] Create `tests/diary_rollup.rs` with a deterministic `LengthEmbedder`.
- [x] Add tests named by the spec:
  - `test_first_rollup_creates_day_drawer`
  - `test_second_rollup_same_day_appends`
  - `test_different_day_creates_new_rollup`
  - `test_different_room_separate_rollup`
  - `test_rollup_wrong_wing_rejected`
  - `test_rollup_over_limit_rejected`
  - `test_rollup_vector_refreshed_on_upsert`
  - `test_concurrent_rollup_same_day_serialized`
- [x] Run `cargo test --test diary_rollup -- --nocapture` and confirm failures are missing API/behavior, not test setup.
- [ ] Commit as `test: define diary daily rollup contract`.

## Task 2: Shared Ingest Rollup

- [x] Create `src/ingest/diary.rs` with:
  - `DIARY_ROLLUP_WING = "agent-diary"`
  - `DAILY_ROLLUP_LIMIT_BYTES = 32 * 1024`
  - `current_rollup_day_utc()`
  - `diary_rollup_drawer_id(room, day)`
  - `ingest_diary_rollup(...)`
- [x] Extend `IngestOptions` with `diary_rollup: bool` and `diary_rollup_day: Option<&str>` for deterministic tests.
- [x] Add `IngestError::DiaryRollupWrongWing`, `IngestError::DiaryRollupMissingRoom`, and `IngestError::DailyRollupFull`.
- [x] In `ingest_file_with_options`, if `diary_rollup` is true, read/normalize content and call shared rollup logic instead of chunking.
- [x] Add DB helper that updates existing drawer content while manually keeping FTS and `drawer_vectors` in sync.
- [x] Run `cargo test --test diary_rollup`.
- [ ] Commit as `feat: add diary daily rollup ingest`.

## Task 3: CLI, MCP, Status, Protocol

- [x] Add CLI args `--room` and `--diary-rollup` to `mempal ingest`.
- [x] Add MCP `diary_rollup` request field and route through shared rollup code; dry-run returns the deterministic day drawer id.
- [x] Add `diary_rollup_days` to `mempal_status` and CLI `status`.
- [x] Update protocol Rule 5a to mention daily rollup.
- [x] Run targeted MCP/status tests, then `cargo test --test diary_rollup`.
- [ ] Commit as `feat: expose diary rollup in clients`.

## Task 4: Verification And Closeout

- [x] Run `cargo fmt -- --check`.
- [x] Run `cargo check`.
- [x] Run `cargo clippy -- -D warnings`.
- [x] Run `cargo test`.
- [x] Run `agent-spec parse specs/p11-diary-daily-rollup.spec.md`.
- [x] Run `agent-spec lint specs/p11-diary-daily-rollup.spec.md --min-score 0.7`.
- [x] Update this plan checkboxes and commit docs closeout if needed.
- [ ] Ingest a post-commit project memory summarizing implementation decisions.

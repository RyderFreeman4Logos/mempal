# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: MINOR bumps introduce new features, PATCH bumps are bug-fix only).

## [0.4.0] — 2026-04-20

First release with **write-safety** + **content-sanity** guarantees for the
Claude Code ↔ Codex cowork pair. Closes P9 (`specs/p9-fact-checker.spec.md` and
`specs/p9-ingest-lock.spec.md`).

### Added

- **`mempal_fact_check` MCP tool** (10th tool) and `mempal fact-check` CLI
  subcommand. Offline contradiction detection against the KG `triples` table
  and the AAAK entity registry. Flags three issue kinds:
  - `SimilarNameConflict` — mentioned name is ≤2 edit-distance from a known
    entity and not identical (typo / confusable).
  - `RelationContradiction` — text asserts a predicate that's in the
    incompatibility dictionary versus an existing KG triple with the same
    `(subject, object)` endpoints.
  - `StaleFact` — text asserts a triple whose KG row has `valid_to <
    now_unix_secs`.
  Pure read, zero LLM, zero network, deterministic.
- **Protocol Rule 11 "VERIFY BEFORE INGEST"** embedded in
  `mempal_status.memory_protocol`. Guides agents to call `mempal_fact_check`
  before ingesting decisions that assert entity relationships.
- **Per-source ingest lock** (advisory `flock` on Unix). Eliminates the
  TOCTOU race between concurrent `mempal_ingest` calls targeting the same
  source (Claude Code + Codex writing the same drawer simultaneously). Lock
  file lives at `~/.mempal/locks/<16-hex>.lock`; guard releases on drop.
- **`IngestStats.lock_wait_ms` / `IngestResponse.lock_wait_ms`** — optional
  field reporting how long the ingest call waited for the per-source lock.
  Non-zero values indicate observed concurrency with a peer agent. Omitted
  in dry-run and when the write path was bypassed.
- `IngestError::Lock` variant wrapping `ingest::lock::LockError`
  (`Timeout { path, timeout }` / `Io { path, source }` / `InvalidSourceKey`).

### Changed

- `MEMORY_PROTOCOL` tool list grew 9 → 10 entries; rule count 10 → 11.
- `src/aaak/mod.rs`: widened `codec` from `mod codec` to
  `pub(crate) mod codec` so the `factcheck` module can reuse
  `extract_entities` without duplicating logic. No external API change.

### Fixed

- Concurrent same-source ingest no longer produces duplicate drawers or
  mismatched `drawer_vectors` rows. Verified by the cross-thread
  `test_concurrent_ingest_same_source_single_drawer` integration test.

### Platform notes

- Linux and macOS have full lock enforcement via `flock(LOCK_EX | LOCK_NB)`
  implemented with inline `extern "C"` (no `libc` crate dependency).
- Windows currently runs a no-op fallback for the lock path — concurrent
  ingest on Windows is **not** race-protected in 0.4.0. Follow-up work will
  adopt `LockFileEx`.

### Compatibility

- Schema version unchanged (still `4`). Existing `~/.mempal/palace.db` files
  open without migration.
- No new runtime or dev-dependency in `Cargo.toml`.
- `mempal_ingest` response adds `lock_wait_ms` with
  `#[serde(skip_serializing_if = "Option::is_none")]`, so existing JSON
  consumers that ignore unknown fields see no change. Consumers that
  destructure the struct need to accept the new field.

### Internal

- New modules: `src/factcheck/{mod,names,relations,contradictions}.rs`,
  `src/ingest/lock.rs`.
- Tests added: 24 unit tests (18 factcheck + 6 ingest lock) and 18
  integration tests (10 `tests/fact_check.rs` + 8 `tests/ingest_lock.rs`),
  including a cross-thread concurrent-ingest race gate.
- Project spec index (`CLAUDE.md`) promoted `p9-fact-checker.spec.md` and
  `p9-ingest-lock.spec.md` to "completed" and registered five new draft
  specs (P10 explicit tunnels, P10 normalize_version, P11 diary daily
  rollup, P11 chunk neighbors, P11 transcript noise strip).

---

## [0.3.1] — 2026-04-16

### Fixed

- `mempal_cowork_push` now recognizes `codex-mcp-client` as a valid Codex
  MCP client identity (the actual string Codex sends per
  `codex-rs/codex-mcp/src/mcp_connection_manager.rs`). Previously, pushes
  from Codex were rejected with "cannot infer caller tool" even when
  Codex was correctly connected.

---

## [0.3.0] — 2026-04-14

First release shipping the full **Claude ↔ Codex cowork** stack (P6 + P7 +
P8) on top of hybrid search and the knowledge graph.

### Added

- **P6 — `mempal_peek_partner` MCP tool**: read the partner agent's live
  session log (Claude `.jsonl` transcripts, Codex rollout files) in place,
  without ingesting or mutating anything. Use for "what is the other agent
  doing right now" across Claude Code and Codex.
- **P6 — Memory Protocol Rules 8 & 9**: "PARTNER AWARENESS" and
  "DECISION CAPTURE" guidance embedded in `mempal_status`.
- **P7 — Structured AAAK-derived signals in search results**: every
  `mempal_search` hit now carries `entities`, `topics`, `flags`,
  `emotions`, `importance_stars` alongside raw `content`. Agents can
  filter by `DECISION` / `TECHNICAL` flags and rank by stars without
  parsing AAAK text.
- **P8 — `mempal_cowork_push` MCP tool**: send a short ephemeral handoff
  (≤ 8 KB, up to 16 pending / 32 KB per inbox) to the partner agent's
  inbox. Delivery is at-next-UserPromptSubmit, not real-time.
- **P8 — CLI commands**:
  - `mempal cowork-drain --target <claude|codex>` — drain inbox from a
    hook; exits 0 on any failure (graceful degrade).
  - `mempal cowork-status --cwd <PATH>` — read-only inbox inspection.
  - `mempal cowork-install-hooks [--global-codex]` — one-shot installer
    for the symmetric UserPromptSubmit hook on both Claude Code and
    Codex, idempotent and self-healing.
- **P8 — Memory Protocol Rule 10 "COWORK PUSH"**.
- Crate exclude list for `cargo package` — `.claude/**`, `.mcp.json`,
  `AGENTS.md`, `CLAUDE.md`, `hooks/**`, `specs/**`, `docs/plans/**` now
  stay out of the published tarball.

### Known limitations (see README)

- Codex `codex_hooks` feature flag must be enabled (`codex features
  enable codex_hooks`); `install-hooks` detects and warns.
- Codex TUI caches config at startup; restart after enabling the flag or
  re-running `install-hooks`.
- Claude Code spawns the mempal MCP server at client startup — restart
  Claude Code after upgrading the mempal binary so newly added tools
  (e.g. `mempal_cowork_push`, `mempal_fact_check` in 0.4.0) are visible.
- `mempal_cowork_push` requires the MCP client to identify as Claude or
  Codex via `ClientInfo.name` (by design for the Claude ↔ Codex pair).

---

## Earlier versions

Earlier releases (0.1.x, 0.2.x) are tracked only in Git history. Run
`git log --oneline` on the repository to inspect them.

[0.4.0]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.4.0
[0.3.1]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.3.1
[0.3.0]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.3.0

# All Open Issues — Ordered Execution Plan

Date: 2026-05-13
Scope: 10 open issues (#188, #187, #191, #192, #193, #197, #198, #199, #200, #201)

## Dependency Graph

```
#188 (scope isolation)     ← IN PROGRESS (fix session running)
  │
  ├── #198 (temporal decay)   ← no deps, schema change
  ├── #199 (source confidence) ← no deps, schema change
  │     └── #197 (compaction)  ← benefits from #198/#199
  │           ├── #200 (sleep cycle) ← needs #197+#198+#199
  │           └── #201 (auto-crystallize) ← needs #197+#198+#199
  │
  ├── #191 (LLM modes)       ← independent, Hermes plugin
  ├── #187 (typed ingest)    ← independent, core+plugin
  ├── #192 (REST reliability) ← independent, Hermes plugin
  └── #193 (tests & docs)    ← last, covers everything
```

## TODO

### Task 1: Finish #188 — Hermes scope isolation
- **Status**: CSA codex fix session `01KRGD9PNWW6YDYDRKR5EDQKA1` running (fixing R2-001: raw cwd as project_id)
- **Remaining**: re-review → push → PR → merge → install binary
- **Branch**: `feat/hermes-scope-isolation`
- **DONE WHEN**: PR merged to main, `mempal --version` updated

### Task 2: #198 — Temporal decay
- **Scope**: Add `valid_from`/`valid_until` columns, configurable decay function in search scoring
- **Branch**: `feat/temporal-decay`
- **DONE WHEN**: `cargo test` passes, new columns in schema, search respects decay config

### Task 3: #199 — Source confidence
- **Scope**: Add `source_type` enum + `confidence` field, enhance fact_check conflict resolution
- **Branch**: `feat/source-confidence`
- **DONE WHEN**: `cargo test` passes, ingest accepts source_type/confidence params, fact_check uses confidence

### Task 4: #197 — Memory compaction
- **Scope**: `mempal consolidate` CLI, vector-cluster merge, soft-delete with `compacted_into`, optional LLM summaries
- **Branch**: `feat/memory-compaction`
- **DONE WHEN**: `mempal consolidate --dry-run` works, compaction log exists, tests pass

### Task 5: #191 — LLM intelligence modes
- **Scope**: Hermes plugin tiered LLM modes (none/local/paid), quality-aware routing
- **Branch**: `feat/hermes-llm-modes`
- **DONE WHEN**: Plugin respects `[llm]` config, three tiers functional, tests pass

### Task 6: #187 — Typed ingest and canonical pinned facts
- **Scope**: Hermes plugin typed ingest categories, always-on pinned facts recall
- **Branch**: `feat/hermes-typed-ingest`
- **DONE WHEN**: Plugin categorizes ingest by type, pinned facts surface in every search, tests pass

### Task 7: #200 — Sleep cycle
- **Scope**: Three-phase offline consolidation (NREM prune, REM conflict test, salience scoring)
- **Branch**: `feat/sleep-cycle`
- **Depends**: #197, #198, #199
- **DONE WHEN**: `mempal sleep` runs all three phases, daemon schedule works, tests pass

### Task 8: #201 — Auto-crystallize
- **Scope**: Automatic knowledge card generation from drawer clusters, review gate
- **Branch**: `feat/auto-crystallize`
- **Depends**: #197, #198, #199
- **DONE WHEN**: `mempal cards --pending` shows auto-generated candidates, lifecycle works, tests pass

### Task 9: #192 — REST reliability
- **Scope**: Hermes plugin BM25 fallback, shutdown drain, connection resilience
- **Branch**: `feat/hermes-rest-reliability`
- **DONE WHEN**: Plugin handles REST failures gracefully, drain on shutdown, tests pass

### Task 10: #193 — Integration tests and docs
- **Scope**: End-to-end integration tests covering all new features, updated docs
- **Branch**: `feat/hermes-integration-tests`
- **DONE WHEN**: Full test suite passes, README updated, all new MCP params documented

## Execution Pattern

Each task follows: branch → csa-codex implement → tier-4 review → fix findings → re-review until PASS → push → PR → merge → install binary → next task

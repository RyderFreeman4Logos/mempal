spec: task
name: "P106: context distill signal"
inherits: project
tags: [mind-model, context, distill, read-only]
estimate: 0.5d
---

## Intent

P106 adds a read-only `distill_suggestions` signal to mind-model context. When
assembling `mempal context` / `mempal_context`, mempal deterministically detects
fields where evidence is dense but no promoted knowledge exists yet, and surfaces
a suggestion that the agent MAY act on by running the explicit distill -> gate
lifecycle. This makes "this is worth distilling" a client-agnostic, pull-based
signal that appears where agents already look, without changing what knowledge is
injected and without any auto-distill or auto-promotion.

## Decisions

- Add a `distill_suggestions` field to the context pack returned by
  `assemble_context` in `src/context.rs`, surfaced through both `mempal context`
  CLI output and the `mempal_context` MCP `ContextResponse`.
- The detector is deterministic and read-only: it counts active drawers and
  performs no LLM call and no database write.
- Grouping dimension is `field`. For each `field`, count active evidence drawers
  (`memory_kind=evidence`) and active promoted-or-canonical knowledge drawers.
- A suggestion is emitted for a `field` only when evidence count is at least the
  threshold `5` AND the field has zero promoted-or-canonical knowledge drawers.
- Each suggestion contains `field`, `evidence_count`, up to `3`
  `sample_evidence_drawer_ids`, and a suggested tier of `dao_ren`.
- At most `3` suggestions are returned, ordered by descending `evidence_count`
  then ascending `field` for deterministic output.
- The signal is on by default via request flag `include_distill_suggestions`
  defaulting to true; callers may disable it. It never changes `include_cards`,
  `include_evidence`, `dao_tian_limit`, or section assembly ordering.
- Writing remains explicit and gate-enforced: the signal never distills,
  creates, or promotes knowledge (governance per P77/P80 unchanged).

## Boundaries

### Allowed Changes
- src/context.rs
- src/core/db.rs
- src/main.rs
- src/brief.rs
- src/mcp/tools.rs
- src/mcp/server.rs
- tests/context_assembler.rs
- tests/knowledge_lifecycle.rs
- docs/MIND-MODEL-DESIGN.md
- specs/p106-context-distill-signal.spec.md
- docs/plans/2026-06-04-p106-context-distill-signal.md
- AGENTS.md
- CLAUDE.md

### Forbidden
- Do not write to the database during context assembly.
- Do not auto-distill, auto-create, or auto-promote any knowledge.
- Do not change `include_cards`, `include_evidence`, or `dao_tian_limit`
  defaults, nor the dao_tian -> dao_ren -> shu -> qi -> evidence ordering.
- Do not add card-level embeddings or new schema tables.

## Acceptance Criteria

Scenario: dense evidence field with no promoted knowledge yields a suggestion
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_suggests_dense_field
  Given a field with five active evidence drawers and zero promoted knowledge
  When context is assembled with `include_distill_suggestions=true`
  Then `distill_suggestions` contains that field
  And the suggestion reports `evidence_count` of "5"
  And the suggested tier is `dao_ren`

Scenario: field with promoted knowledge yields no suggestion
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_skips_promoted_field
  Given a field with five active evidence drawers and one promoted knowledge drawer
  When context is assembled with `include_distill_suggestions=true`
  Then `distill_suggestions` does not contain that field

Scenario: field below threshold yields no suggestion
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_below_threshold
  Given a field with four active evidence drawers and zero promoted knowledge
  When context is assembled with `include_distill_suggestions=true`
  Then `distill_suggestions` is empty

Scenario: context assembly performs no database write
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_is_read_only
  Given a database with dense evidence in one field
  When context is assembled with `include_distill_suggestions=true`
  Then the schema version is unchanged
  And the active drawer count is unchanged

Scenario: suggestions are capped and deterministically ordered
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_caps_and_orders
  Given five distinct fields that each qualify for a suggestion
  When context is assembled with `include_distill_suggestions=true`
  Then `distill_suggestions` contains "3" entries
  And the entries are ordered by descending `evidence_count`

Scenario: signal disabled returns no suggestions and unchanged sections
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_disabled
  Given a field that qualifies for a suggestion
  When context is assembled with `include_distill_suggestions=false`
  Then `distill_suggestions` is empty
  And the assembled tier sections are unchanged from the default run

Scenario: CLI context JSON output exposes the distill signal
  Test:
    Filter: cargo test --test context_assembler test_cli_context_json_includes_distill_suggestions
  Given a field that qualifies for a suggestion
  When `mempal context` is run through `src/main.rs` with `--format json`
  Then the JSON output contains a `distill_suggestions` array

Scenario: MCP context response exposes the distill signal
  Test:
    Filter: cargo test --test context_assembler test_mcp_context_response_includes_distill_suggestions
  Given a field that qualifies for a suggestion
  When `mempal_context` is invoked through the MCP server in `src/mcp/server.rs`
  Then the `ContextResponse` contains a `distill_suggestions` field

Scenario: empty memory returns no suggestions without error
  Test:
    Filter: cargo test --test context_assembler test_context_distill_signal_empty_db_no_error
  Given a database with no evidence drawers
  When context is assembled with `include_distill_suggestions=true`
  Then the call returns successfully
  And `distill_suggestions` is empty

## Out of Scope

- Actually distilling, creating, or promoting knowledge (stays the agent's
  explicit `mempal_knowledge_distill` plus gate).
- LLM-based or embedding-based clustering — the detector is count-based only.
- Config-tunable thresholds — the threshold and caps are fixed constants in v1.
- Client-side `/dream` skills or session-end hooks — those are separate surfaces.
- Card-level or anchor-level grouping — v1 groups by `field` only.
- No file-output mode: P106 adds no `-o`/file-writing flag; the signal appears
  only in the existing CLI stdout (`--format json`/`plain`) and MCP response.

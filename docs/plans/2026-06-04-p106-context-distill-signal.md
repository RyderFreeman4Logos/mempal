# P106 Context Distill Signal

Spec: `specs/p106-context-distill-signal.spec.md`

> Read-only, deterministic distill signal surfaced through `mempal context` /
> `mempal_context`. No auto-distill, no auto-promotion, no DB writes — writing
> stays explicit and gate-enforced (governance per P77/P80).

## Design summary

When assembling mind-model context, group active **evidence** drawers by
`field`. For each field, emit a read-only suggestion iff:

- `evidence_count >= 5`, AND
- the field has **zero** active promoted-or-canonical knowledge drawers.

Return at most 3 suggestions, ordered by descending `evidence_count` then
ascending `field`. Each suggestion = `{ field, evidence_count,
sample_evidence_drawer_ids (<=3), suggested_tier: "dao_ren" }`.

## Tasks

- [x] `src/context.rs`: add `DistillSuggestion` struct + `distill_suggestions:
      Vec<DistillSuggestion>` field on `ContextPack`.
- [x] `src/context.rs`: add `include_distill_suggestions: bool` (default true) to
      `ContextRequest`; implement the deterministic detector as a read-only DB
      query (no writes, no LLM).
- [x] `src/context.rs`: enforce threshold=5, cap=3, sample cap=3, and the
      descending-count / ascending-field ordering as named constants.
- [x] `src/main.rs`: surface `distill_suggestions` in `mempal context` output
      (`--format json` and `plain`); add `--no-distill-suggestions` to disable.
- [x] `src/mcp/tools.rs`: add `include_distill_suggestions: Option<bool>` to the
      MCP `ContextRequest` and a `distill_suggestions` field to `ContextResponse`.
- [x] `src/mcp/server.rs`: wire the request flag through to `assemble_context`.
- [x] `tests/context_assembler.rs`: add the nine P106 scenarios (dense-field
      suggest, promoted-field skip, below-threshold skip, read-only invariant,
      cap+order, disabled, CLI JSON surface, MCP response surface, empty-db
      no-error).
- [x] `docs/MIND-MODEL-DESIGN.md`: document the read-only distill signal and its
      governance boundary.
- [x] Update `AGENTS.md` / `CLAUDE.md` spec + plan inventory and MCP tool notes.

## Boundaries reminder

- No DB writes during context assembly.
- No auto-distill / auto-create / auto-promote.
- Do not change `include_cards` / `include_evidence` / `dao_tian_limit` defaults
  or the `dao_tian -> dao_ren -> shu -> qi -> evidence` ordering.
- No new schema tables, no card embeddings.

## Verification

```bash
agent-spec parse specs/p106-context-distill-signal.spec.md
agent-spec lint specs/p106-context-distill-signal.spec.md --min-score 0.7
cargo test --test context_assembler
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
# read-only invariant: schema version + active drawer count unchanged after assemble
```

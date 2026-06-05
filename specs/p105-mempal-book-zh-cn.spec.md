spec: task
name: "P105: mempal book zh-CN"
inherits: project
tags: [docs, book, mdbook, writing, zh-cn]
---

## Intent

P105 creates the first Chinese edition of "mempal 之书" as an mdBook. The book
must explain mempal from decisions to architecture to practical usage, while
remaining concise and grounded in the current implementation on `main`.

## Decisions

- The book lives under `books/zh-CN`.
- P105 writes Chinese content only; English and Japanese editions are planned but
  not authored in this task.
- The mdBook outline is the authoritative chapter order for the Chinese edition.
- Each chapter gets a writing spec under `books/zh-CN/specs/`.
- Each chapter gets a writing plan under `books/zh-CN/plans/`.
- Chapter writing specs are real agent-spec task contracts, not informal notes.
- Chapter writing plans must point back to the matching chapter spec and list
  concrete writing steps.
- Mermaid diagrams are rendered through committed local Mermaid JS assets. P105
  deliberately avoids the installed `mdbook-mermaid 0.17.0` preprocessor
  because that preprocessor is incompatible with the local `mdbook 0.4.52`
  JSON input.
- The writing style is concise, decision-first, source-backed, and operator
  oriented.
- "Concise" means compressed technical-book prose, not outline bullets. The
  Chinese manuscript must explain motivations, implementation boundaries, and
  usage flows with enough depth for a new agent/user to operate mempal.
- mdBook generated output under `books/*/book/` is ignored and not committed.

## Boundaries

### Allowed Changes
- specs/p105-mempal-book-zh-cn.spec.md
- docs/plans/2026-05-28-p105-mempal-book-zh-cn.md
- books/**
- books/zh-CN/mermaid.min.js
- books/zh-CN/mermaid-init.js
- .gitignore
- AGENTS.md
- CLAUDE.md

### Forbidden
- Do not modify Rust source code.
- Do not write English or Japanese full translations in P105.
- Do not add new runtime dependencies.
- Do not change existing CLI, MCP, schema, or tests.

## Acceptance Criteria

Scenario: mdBook Chinese outline exists
  Test:
    Filter: test -f books/zh-CN/book.toml && test -f books/zh-CN/src/SUMMARY.md
    Targets: mdBook structure.
  When the P105 book is inspected
  Then `books/zh-CN/book.toml` exists
  And `books/zh-CN/src/SUMMARY.md` links the preface, ten chapters, and appendix

Scenario: every chapter has writing spec and plan
  Test:
    Filter: test "$(find books/zh-CN/specs -name '*.spec.md' | wc -l)" -ge 12 && test "$(find books/zh-CN/plans -name '*.plan.md' | wc -l)" -ge 12 && rg "spec: task" books/zh-CN/specs
    Targets: chapter writing governance.
  When chapter governance files are counted
  Then every preface/chapter/appendix unit has a writing spec
  And every preface/chapter/appendix unit has a writing plan
  And each chapter writing spec is an agent-spec task contract

Scenario: chapter specs are parseable contracts
  Test:
    Filter: command -v agent-spec && agent-spec parse books/zh-CN/specs/00-preface.spec.md && agent-spec parse books/zh-CN/specs/03-architecture.spec.md && agent-spec parse books/zh-CN/specs/09-self-evolution.spec.md
    Targets: representative chapter specs.
  When chapter specs are validated
  Then the preface, architecture, and self-evolution contracts parse
  And their acceptance scenarios are machine-checkable

Scenario: Mermaid diagrams render through mdBook HTML assets
  Test:
    Filter: rg "additional-js" books/zh-CN/book.toml && rg "language-mermaid|mermaid.run|mermaid.init" books/zh-CN/mermaid-init.js && test -f books/zh-CN/mermaid.min.js && test -f books/zh-CN/mermaid-init.js && mdbook build books/zh-CN
    Targets: mdBook Mermaid configuration.
  When the Chinese book is built
  Then Mermaid diagrams are transformed from mdBook code blocks by local JS
  And no external CDN or incompatible preprocessor is required

Scenario: Chinese book content is complete
  Test:
    Filter: test -f books/zh-CN/src/ch10-ops-and-usage.md && test -f books/zh-CN/src/appendix-commands.md && test "$(wc -m books/zh-CN/src/*.md | tail -1 | awk '{print $1}')" -ge 24000
    Targets: Chinese manuscript.
  When the Chinese mdBook source is inspected
  Then all linked chapter files exist
  And the content covers decisions, architecture, storage/search, mind model,
      knowledge governance, runtime context, cowork, self-evolution, and usage
  And the manuscript is expanded beyond outline-level notes

Scenario: CLI examples avoid known stale flags
  Test:
    Filter: ! rg "wake-up --wing|knowledge demote .*--counterexample-ref|knowledge-card link .*--evidence-ref|knowledge-card promote .*--target-status|phase3 evaluator advise --format" books/zh-CN/src
    Targets: command examples in the Chinese manuscript.
  When the book source is searched for stale CLI examples
  Then removed or renamed flags from older implementations are absent
  And examples follow the current `target/debug/mempal --help` surface

Scenario: project inventory includes P105
  Test:
    Filter: rg "p105-mempal-book-zh-cn" AGENTS.md CLAUDE.md docs/plans specs
    Targets: governance inventory.
  When project governance files are searched
  Then AGENTS.md references the P105 spec and plan
  And CLAUDE.md references the P105 spec and plan

## Out of Scope

- Publishing the book.
- Rendering or hosting multilingual editions.
- Exhaustive API reference for every command and MCP action.
- Automated translation workflow.

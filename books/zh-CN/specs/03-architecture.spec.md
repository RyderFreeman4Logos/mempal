spec: task
name: "mempal book zh-CN chapter 3 architecture"
inherits: project
tags: [book, writing, zh-cn, architecture, mermaid]
---

## Intent

Write chapter 3 to explain the implemented architecture. The chapter must show
how CLI, MCP, SQLite, search, context, knowledge governance, Phase 3 adoption,
and cowork fit together.

## Decisions

- The chapter must contain a Mermaid architecture diagram.
- MCP must be presented as the primary agent runtime surface.
- CLI must be presented as the operator surface.
- REST must be treated as optional and non-primary.
- The data path must flow from evidence to knowledge/card to context/adoption.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch03-architecture.md
- books/zh-CN/book.toml
- books/zh-CN/mermaid.min.js
- books/zh-CN/mermaid-init.js

### Forbidden
- Do not describe unimplemented services.
- Do not claim there is a background daemon.

## Acceptance Criteria

Scenario: Mermaid architecture renders
  Test: mdbook build books/zh-CN
  When the book is built
  Then Mermaid diagrams are processed by `mdbook-mermaid`
  And the build succeeds

Scenario: Runtime surfaces are explained
  Test: rg "CLI|MCP|SQLite|Search|Context|Phase-3|Cowork" books/zh-CN/src/ch03-architecture.md
  When chapter 3 is read
  Then all major implemented surfaces are covered

Scenario: Data path is explicit
  Test: rg "evidence|knowledge|card|context|adoption" books/zh-CN/src/ch03-architecture.md
  When chapter 3 is read
  Then it explains how information moves through the system

## Out of Scope

- Exhaustive module-level source walkthrough.
- REST endpoint documentation.

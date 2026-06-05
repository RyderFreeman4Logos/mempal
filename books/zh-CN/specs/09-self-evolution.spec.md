spec: task
name: "mempal book zh-CN chapter 9 self evolution"
inherits: project
tags: [book, writing, zh-cn, self-evolution]
---

## Intent

Write chapter 9 to explain mempal's self-evolution loop. The chapter must show
that self-evolution is evidence-driven and governed, not autonomous promotion.

## Decisions

- The chapter must contain a Mermaid loop diagram.
- The loop must cover research, evidence, distill, gate, card/context usage,
  adoption, analytics, readiness, default proposal, and rollback.
- The chapter must distinguish automatic checks from explicit writes.
- The chapter must state autonomous promotion is out of bounds.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch09-self-evolution.md

### Forbidden
- Do not promise autonomous dao creation.
- Do not say evaluator can write lifecycle state directly.

## Acceptance Criteria

Scenario: Loop diagram exists
  Test: rg "flowchart|research|evidence|adoption|rollback" books/zh-CN/src/ch09-self-evolution.md
  When chapter 9 is read
  Then the self-evolution loop is diagrammed

Scenario: Governance boundary is explicit
  Test: rg "autonomous|promotion|human|gate|边界" books/zh-CN/src/ch09-self-evolution.md
  When chapter 9 is read
  Then autonomous promotion is rejected

Scenario: Automatic versus explicit steps are covered
  Test: rg "自动|显式|--execute|readiness|proposal" books/zh-CN/src/ch09-self-evolution.md
  When chapter 9 is read
  Then the chapter explains which steps require explicit execution

## Out of Scope

- Background scheduler design.
- Research tool implementation.

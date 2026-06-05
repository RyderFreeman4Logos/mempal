spec: task
name: "mempal book zh-CN chapter 6 knowledge governance"
inherits: project
tags: [book, writing, zh-cn, knowledge, governance]
---

## Intent

Write chapter 6 to explain how mempal turns evidence into governed knowledge and
knowledge cards. The chapter must make clear that candidate knowledge is not
promoted or canonical knowledge.

## Decisions

- The chapter must cover distill, gate, promote, and demote.
- The chapter must cover cards, evidence links, and lifecycle events.
- The chapter must distinguish Stage-1 drawers from Phase-2 cards.
- The chapter must state evaluator advice cannot bypass deterministic gates.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch06-knowledge-governance.md

### Forbidden
- Do not imply automatic promotion.
- Do not treat card retrieval as default context behavior.

## Acceptance Criteria

Scenario: Lifecycle path is covered
  Test: rg "distill|gate|promote|demote|candidate|promoted" books/zh-CN/src/ch06-knowledge-governance.md
  When chapter 6 is read
  Then knowledge lifecycle states and transitions are explained

Scenario: Card model is covered
  Test: rg "knowledge_cards|knowledge_evidence_links|knowledge_events|card" books/zh-CN/src/ch06-knowledge-governance.md
  When chapter 6 is read
  Then the Phase-2 card model is explained

Scenario: Governance boundary is explicit
  Test: rg "evaluator|human review|deterministic|gate|不能绕过" books/zh-CN/src/ch06-knowledge-governance.md
  When chapter 6 is read
  Then evaluator and human-gated boundaries are stated

## Out of Scope

- Full policy threshold table.
- UI design for reviewing knowledge.

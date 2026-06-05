spec: task
name: "mempal book zh-CN chapter 2 principles"
inherits: project
tags: [book, writing, zh-cn, principles]
---

## Intent

Write chapter 2 to explain the design decisions behind mempal. The chapter must
show why mempal prioritizes traceability, governance, and rollback over
unbounded automation.

## Decisions

- The chapter must include a decision table.
- The table must include raw storage, citation-first, SQLite, AAAK output-only,
  human-gated lifecycle, research evidence-only, and runtime adoption.
- Each decision must include reason and trade-off.
- The chapter must explicitly state why wrong memory is worse than no memory.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch02-principles.md

### Forbidden
- Do not introduce new architecture modules.
- Do not present governance as optional style.

## Acceptance Criteria

Scenario: Decision matrix exists
  Test: rg "raw storage|citation-first|SQLite|AAAK|human-gated|runtime adoption" books/zh-CN/src/ch02-principles.md
  When chapter 2 is read
  Then all core design decisions are covered

Scenario: Trade-offs are included
  Test: rg "代价|取舍|风险|边界" books/zh-CN/src/ch02-principles.md
  When chapter 2 is read
  Then it explains why each design choice has cost

Scenario: Automation boundary is explicit
  Test: rg "自动|授权|promotion|human review|gate" books/zh-CN/src/ch02-principles.md
  When chapter 2 is read
  Then it distinguishes automatic suggestion from lifecycle authority

## Out of Scope

- Command-by-command usage.
- Full self-evolution loop.

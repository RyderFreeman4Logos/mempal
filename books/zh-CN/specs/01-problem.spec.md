spec: task
name: "mempal book zh-CN chapter 1 problem"
inherits: project
tags: [book, writing, zh-cn, problem]
---

## Intent

Write chapter 1 to define the problem mempal solves. The chapter must
distinguish persistent project memory from long context windows, ordinary RAG,
and ad hoc chat logs.

## Decisions

- The chapter must lead with coding agent failure modes.
- The chapter must state mempal's one-sentence goal.
- The chapter must distinguish retrieval, memory, and governed cognition.
- The chapter must use concrete project scenarios rather than abstract AI claims.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch01-problem.md

### Forbidden
- Do not compare current commercial memory products.
- Do not discuss model benchmark rankings.

## Acceptance Criteria

Scenario: Problem statement is concrete
  Test: rg "session|失忆|历史决策|10 秒|出处" books/zh-CN/src/ch01-problem.md
  When chapter 1 is read
  Then it explains agent session loss
  And it states the 10-second cited recall goal

Scenario: RAG boundary is explained
  Test: rg "RAG|检索|搜索|理解|治理" books/zh-CN/src/ch01-problem.md
  When chapter 1 is read
  Then it distinguishes search from memory
  And it explains why retrieval alone is insufficient

Scenario: Coding agent use case is present
  Test: rg "coding agent|项目|commit|PR|协作" books/zh-CN/src/ch01-problem.md
  When chapter 1 is read
  Then it ties the problem to real coding agent work

## Out of Scope

- Storage schema details.
- Knowledge lifecycle commands.

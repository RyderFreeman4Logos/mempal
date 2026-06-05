spec: task
name: "mempal book zh-CN chapter 5 mind model"
inherits: project
tags: [book, writing, zh-cn, mind-model]
---

## Intent

Write chapter 5 to explain mempal's mind model: evidence, dao_tian, dao_ren,
shu, qi, and anchors. The chapter must preserve the user's dao/qi/shu insight
while keeping it operational rather than philosophical.

## Decisions

- The chapter must define `dao_tian`, `dao_ren`, `shu`, `qi`, and `evidence`.
- The chapter must explain Tian Dao vs Ren Dao as scope difference.
- The chapter must explain global/repo/worktree anchors.
- The chapter must state that research tools do not define dao.
- The chapter must explain context order.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch05-mind-model.md

### Forbidden
- Do not write a philosophical essay detached from implementation.
- Do not expand dao_tian beyond conservative limits.

## Acceptance Criteria

Scenario: Type taxonomy is explicit
  Test: rg "dao_tian|dao_ren|shu|qi|evidence" books/zh-CN/src/ch05-mind-model.md
  When chapter 5 is read
  Then every memory kind is defined

Scenario: Anchor semantics are explicit
  Test: rg "worktree|repo|global|anchor" books/zh-CN/src/ch05-mind-model.md
  When chapter 5 is read
  Then it explains scope and project anchoring

Scenario: Research boundary is explicit
  Test: rg "research|外部|不能直接定义|dao" books/zh-CN/src/ch05-mind-model.md
  When chapter 5 is read
  Then it states research output enters evidence first

## Out of Scope

- Full lifecycle gate policy.
- Translation of philosophical terminology.

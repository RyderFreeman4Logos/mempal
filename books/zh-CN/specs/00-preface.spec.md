spec: task
name: "mempal book zh-CN preface"
inherits: project
tags: [book, writing, zh-cn, preface]
---

## Intent

Write the Chinese preface for "mempal 之书". The preface sets reader
expectations, states that the Chinese edition is the source edition, and frames
mempal as governed memory infrastructure for coding agents rather than a
marketing concept.

## Decisions

- The preface must be concise but not skeletal.
- The preface must name the target readers.
- The preface must explain the reading paths.
- The preface must avoid promising fully autonomous evolution.

## Boundaries

### Allowed Changes
- books/zh-CN/src/preface.md

### Forbidden
- Do not introduce new technical claims that are not covered later in the book.
- Do not write English or Japanese content.

## Acceptance Criteria

Scenario: Reader positioning is explicit
  Test: rg "适合|读者|阅读路径" books/zh-CN/src/preface.md
  When the preface is read
  Then it identifies who should read the book
  And it gives at least one reading path

Scenario: Scope boundary is explicit
  Test: rg "不是|不会|边界|自治|governed" books/zh-CN/src/preface.md
  When the preface is read
  Then it states mempal is not a fully autonomous daemon
  And it explains the governance boundary

Scenario: Language strategy is explicit
  Test: rg "中文|英文|日文|翻译" books/zh-CN/src/preface.md
  When the preface is read
  Then it states Chinese is written first
  And English/Japanese are deferred until Chinese is accepted

## Out of Scope

- Full architecture explanation.
- Command reference.

spec: task
name: "mempal book zh-CN chapter 7 runtime context"
inherits: project
tags: [book, writing, zh-cn, runtime, context]
---

## Intent

Write chapter 7 to explain how an agent should use mempal during runtime. The
chapter must distinguish context, brief, search, trigger hints, and adoption
recording.

## Decisions

- The chapter must give a recommended task-start call sequence.
- The chapter must explain `mempal_context`, `mempal_brief`, and `mempal_search`.
- The chapter must state `trigger_hints` are bias only.
- The chapter must explain adoption capture/review/analytics as feedback.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch07-runtime-context.md

### Forbidden
- Do not say context automatically executes skills.
- Do not describe brief as an LLM summarizer.

## Acceptance Criteria

Scenario: Runtime read paths are distinct
  Test: rg "mempal_context|mempal_brief|mempal_search|context|brief|search" books/zh-CN/src/ch07-runtime-context.md
  When chapter 7 is read
  Then the three read paths are distinguished

Scenario: Trigger hint boundary is present
  Test: rg "trigger_hints|bias|不能自动执行|覆盖" books/zh-CN/src/ch07-runtime-context.md
  When chapter 7 is read
  Then hints are described as non-authoritative

Scenario: Adoption feedback is covered
  Test: rg "adoption|capture|review|analytics|feedback" books/zh-CN/src/ch07-runtime-context.md
  When chapter 7 is read
  Then runtime feedback recording is explained

## Out of Scope

- Full MCP JSON schema for every tool.
- Prompt engineering cookbook.

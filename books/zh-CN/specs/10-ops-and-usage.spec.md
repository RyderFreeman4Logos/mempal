spec: task
name: "mempal book zh-CN chapter 10 ops usage"
inherits: project
tags: [book, writing, zh-cn, ops, usage]
---

## Intent

Write chapter 10 to give practical installation, diagnostic, MCP restart, and
daily usage guidance. The chapter must address the real old-binary/new-schema
failure mode.

## Decisions

- The chapter must start with install and doctor.
- The chapter must explain PATH binary mismatch and schema version risk.
- The chapter must explain MCP server respawn/restart.
- The chapter must include daily workflow, maintenance workflow, and cowork workflow.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch10-ops-and-usage.md

### Forbidden
- Do not document publishing as completed.
- Do not skip the `doctor` step.

## Acceptance Criteria

Scenario: Install diagnostic workflow is covered
  Test: rg "cargo install|which mempal|mempal doctor|schema|PATH" books/zh-CN/src/ch10-ops-and-usage.md
  When chapter 10 is read
  Then install and binary mismatch diagnosis are explained

Scenario: MCP restart is covered
  Test: rg "mempal serve --mcp|重启|respawn|MCP" books/zh-CN/src/ch10-ops-and-usage.md
  When chapter 10 is read
  Then the MCP restart requirement is stated

Scenario: Operational workflows are covered
  Test: rg "release-readiness|maintenance guided-run|cowork|adoption" books/zh-CN/src/ch10-ops-and-usage.md
  When chapter 10 is read
  Then daily, maintenance, and cowork workflows are present

## Out of Scope

- CI/CD publishing automation.
- Full troubleshooting matrix for all commands.

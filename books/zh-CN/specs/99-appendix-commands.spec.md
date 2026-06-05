spec: task
name: "mempal book zh-CN appendix commands"
inherits: project
tags: [book, writing, zh-cn, appendix, commands]
---

## Intent

Write the command appendix as a concise operator cheat sheet. The appendix must
group commands by usage surface without replacing built-in `--help`.

## Decisions

- Commands must be grouped by install, MCP, memory, context, knowledge,
  phase3, research, cowork, and maintenance.
- The appendix must keep examples short.
- The appendix must not enumerate every flag.
- Command examples must be checked against the current `target/debug/mempal
  <command> --help` surface before the appendix is accepted.

## Boundaries

### Allowed Changes
- books/zh-CN/src/appendix-commands.md

### Forbidden
- Do not invent commands that are not in the current CLI.
- Do not turn the appendix into full reference documentation.

## Acceptance Criteria

Scenario: Command groups exist
  Test: rg "安装|MCP|记忆|Context|Knowledge|Phase 3|Cowork|Maintenance" books/zh-CN/src/appendix-commands.md
  When the appendix is read
  Then common command groups are present

Scenario: Current CLI commands are used
  Test: rg "mempal doctor|mempal context|mempal phase3|mempal cowork|mempal maintenance" books/zh-CN/src/appendix-commands.md
  When the appendix is read
  Then examples use current command names

Scenario: Known stale CLI examples are absent
  Test: ! rg "wake-up --wing|knowledge demote .*--counterexample-ref|knowledge-card link .*--evidence-ref|knowledge-card promote .*--target-status|phase3 evaluator advise --format" books/zh-CN/src/appendix-commands.md
  When the appendix is checked against current CLI help
  Then examples do not use old wake-up filters
  And examples do not use stale knowledge lifecycle flags
  And evaluator examples include required subject/action arguments

Scenario: Appendix remains concise
  Test: test "$(wc -w < books/zh-CN/src/appendix-commands.md)" -lt 1200
  When the appendix is measured
  Then it remains a cheat sheet rather than full reference

## Out of Scope

- Exhaustive command reference.
- MCP JSON schema reference.

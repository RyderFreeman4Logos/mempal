spec: task
name: "mempal book zh-CN chapter 8 cowork"
inherits: project
tags: [book, writing, zh-cn, cowork, multi-agent]
---

## Intent

Write chapter 8 to explain multi-agent collaboration through mempal. The chapter
must cover both pairwise Claude/Codex primitives and the concrete multi-agent
cowork bus.

## Decisions

- The chapter must explain peek, push, inbox, bus, channel, delivery, ack, tmux,
  session, handoff, and capture.
- The chapter must include a three-agent workflow.
- Runtime cowork artifacts must be described as ephemeral unless explicitly
  captured.
- tmux peek must be described as read-only.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch08-cowork.md

### Forbidden
- Do not claim push is real-time UI injection.
- Do not treat cowork events as durable memory.

## Acceptance Criteria

Scenario: Multi-agent bus is covered
  Test: rg "cowork-register|cowork-send|cowork-channel|cowork-ack|cowork-events" books/zh-CN/src/ch08-cowork.md
  When chapter 8 is read
  Then concrete bus operations are explained

Scenario: tmux boundary is covered
  Test: rg "tmux|peek|read-only|只读" books/zh-CN/src/ch08-cowork.md
  When chapter 8 is read
  Then tmux transport and peek boundaries are explained

Scenario: Durable capture boundary is covered
  Test: rg "session|handoff|capture|durable|ephemeral" books/zh-CN/src/ch08-cowork.md
  When chapter 8 is read
  Then runtime vs durable memory is distinguished

## Out of Scope

- Real-time UI integration.
- Distributed team server.

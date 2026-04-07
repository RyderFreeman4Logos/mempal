spec: task
name: "P2: MCP server"
inherits: project
tags: [p2, mcp, agent]
depends: [p1-routing-citation]
estimate: 1d
---

## Intent

实现 mempal-mcp crate，通过 rmcp 提供 MCP 服务器。支持 `mempal serve --mcp` 启动，
Claude Code 通过 `claude mcp add mempal -- mempal serve --mcp` 接入。
精简到 4 个核心工具：status、search、ingest、taxonomy。

## Decisions

- MCP 框架：`rmcp` crate
- 传输：stdio（默认）、SSE（可选）
- 工具数量：4 个（不是 MemPalace 的 19 个）
- `mempal_status`：返回 taxonomy 概览 + drawer 统计
- `mempal_search`：接受 query / wing / room / top_k，返回带引用的结果
- `mempal_ingest`：接受 content / wing / room / source，写入单条 drawer
- `mempal_taxonomy`：接受 action（list / edit），返回或修改分类
- `mempal serve --mcp` 启动 MCP 模式
- `mempal serve` 同时启动 MCP + REST（如果 rest feature 启用）

## Boundaries

### Allowed Changes
- crates/mempal-mcp/**
- crates/mempal-cli/**（添加 serve 命令）

### Forbidden
- 不实现 AAAK 输出（P3）
- 不实现 REST API（P4）
- 不修改 search 或 ingest 核心逻辑

## Completion Criteria

Scenario: MCP 服务器启动
  Test: test_mcp_server_start
  Given mempal serve --mcp
  When MCP 客户端连接
  Then 服务器返回 tool listing 包含 4 个工具

Scenario: MCP search 工具
  Test: test_mcp_search
  Given palace.db 包含数据
  When MCP 客户端调用 mempal_search(query="auth decision")
  Then 返回 JSON 结果包含 drawer_id, content, source_file, similarity

Scenario: MCP ingest 工具
  Test: test_mcp_ingest
  Given 空 palace.db
  When MCP 客户端调用 mempal_ingest(content="decided to use Clerk", wing="myapp", room="auth")
  Then drawers 表包含新 drawer
  And 返回 drawer_id

Scenario: MCP status 工具
  Test: test_mcp_status
  Given palace.db 包含数据
  When MCP 客户端调用 mempal_status
  Then 返回 wing/room 统计和 drawer 总数

Scenario: Claude Code 集成
  Test: test_claude_code_integration
  Review: human
  Given mempal serve --mcp 已启动
  When 用户执行 `claude mcp add mempal -- mempal serve --mcp`
  Then Claude Code 可以使用 mempal 的 4 个工具

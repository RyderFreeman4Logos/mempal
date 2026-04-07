spec: task
name: "P1: Search routing + citation assembly"
inherits: project
tags: [p1, search, routing]
depends: [p0-search-cli]
estimate: 1d
---

## Intent

为 mempal-search 添加查询路由层和引用组装。路由基于 taxonomy 关键词匹配，自动缩小搜索范围。
路由结果透明返回，包含置信度和理由。同时完善 wake-up 和 status 命令。

## Decisions

- `route_query()` 使用 taxonomy 表的 keywords 字段做确定性匹配
- 置信度 < 0.5 时退回全局搜索
- RouteDecision 包含 wing, room, confidence, reason
- `wake-up` 命令：输出 L0（identity.txt）+ L1（top drawers 摘要）
- `status` 命令：输出 wing/room 统计、drawer 总数、数据库大小
- `taxonomy list` / `taxonomy edit` 命令

## Boundaries

### Allowed Changes
- crates/mempal-search/**
- crates/mempal-cli/**
- crates/mempal-core/**（如需扩展类型）

### Forbidden
- 不实现 LLM 重排序
- 不修改 SQLite schema

## Completion Criteria

Scenario: 路由命中
  Test: test_route_hit
  Given taxonomy 表包含 wing="myapp", room="auth", keywords=["auth", "login", "clerk"]
  When 调用 `route_query("why did we switch to Clerk")`
  Then RouteDecision.wing 等于 "myapp"
  And RouteDecision.room 等于 "auth"
  And RouteDecision.confidence >= 0.5

Scenario: 路由未命中退回全局
  Test: test_route_fallback
  Given taxonomy 表只包含 wing="myapp", room="auth"
  When 调用 `route_query("what is the weather")`
  Then RouteDecision.confidence < 0.5
  And RouteDecision.wing 为 None

Scenario: 路由结果可解释
  Test: test_route_explainable
  Given 任意路由调用
  When 检查 RouteDecision
  Then reason 字段非空，描述匹配原因

Scenario: wake-up 输出
  Test: test_wakeup
  Given palace.db 包含 drawer 数据
  When CLI 执行 `mempal wake-up`
  Then stdout 输出 L0 identity 和 L1 top drawers
  And 输出 token 估算

Scenario: status 输出
  Test: test_status
  Given palace.db 包含数据
  When CLI 执行 `mempal status`
  Then 输出 wing/room 统计表
  And 输出 drawer 总数和数据库文件大小

Scenario: taxonomy 编辑
  Test: test_taxonomy_edit
  Given taxonomy 表包含 wing="myapp", room="auth"
  When 修改 room 的 keywords
  Then 后续路由使用新 keywords

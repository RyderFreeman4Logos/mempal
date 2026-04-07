spec: task
name: "P4: REST API (feature-gated)"
inherits: project
tags: [p4, api, rest]
depends: [p1-routing-citation]
estimate: 1d
---

## Intent

实现 mempal-api crate，通过 axum 提供 REST API。Feature-gated（`--features rest`），
不启用时不增加编译时间和二进制体积。`mempal serve` 同时启动 MCP + REST。

## Decisions

- Web 框架：`axum` 0.8+
- Feature gate：`rest` feature 在 mempal-cli 的 Cargo.toml
- 默认端口：3080
- 响应格式：JSON
- 无认证（本地工具，不暴露到网络）
- CORS：默认允许 localhost

## Boundaries

### Allowed Changes
- crates/mempal-api/**
- crates/mempal-cli/Cargo.toml（添加 rest feature）
- crates/mempal-cli/src/（添加 REST 启动逻辑）

### Forbidden
- 不实现认证/授权
- 不实现 WebSocket
- 不修改 search 或 ingest 核心逻辑

## Completion Criteria

Scenario: GET /api/search
  Test: test_api_search
  Given palace.db 包含数据
  When GET /api/search?q=auth+decision
  Then 返回 200 + JSON 数组
  And 每个元素包含 drawer_id, content, wing, similarity, source_file

Scenario: POST /api/ingest
  Test: test_api_ingest
  Given 空 palace.db
  When POST /api/ingest `{"content": "decided to use Clerk", "wing": "myapp"}`
  Then 返回 201 + drawer_id
  And drawers 表包含新记录

Scenario: GET /api/taxonomy
  Test: test_api_taxonomy
  Given taxonomy 表包含数据
  When GET /api/taxonomy
  Then 返回 200 + wing/room 列表

Scenario: GET /api/status
  Test: test_api_status
  Given palace.db 包含数据
  When GET /api/status
  Then 返回 200 + 统计信息（drawer_count, db_size_bytes, wings）

Scenario: 不启用 rest feature 时无额外依赖
  Test: test_no_rest_feature
  Given Cargo.toml 未启用 rest feature
  When `cargo build`
  Then axum 不被编译
  And 二进制不包含 REST 相关代码

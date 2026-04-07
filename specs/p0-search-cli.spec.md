spec: task
name: "P0: Search engine + CLI commands (init/ingest/search)"
inherits: project
tags: [p0, search, cli]
depends: [p0-core-scaffold, p0-embed-trait, p0-ingest]
estimate: 2d
---

## Intent

实现 mempal-search crate（向量检索 + 元数据过滤）和 mempal-cli crate（init/ingest/search 三个
核心命令）。完成 P0 后用户可以跑通完整链路：`mempal init → mempal ingest → mempal search`。

## Decisions

- 搜索：sqlite-vec 的向量距离查询 + drawers 表 WHERE 过滤
- 相似度：`1.0 - distance`（cosine distance to similarity）
- 结果数：默认 top-10，可通过 `--top-k` 覆盖
- 输出格式：默认 human-readable，`--json` 输出 JSON
- `init` 命令：扫描目录结构，自动检测 room 候选，生成 taxonomy 到 SQLite
- `ingest` 命令：调用 mempal-ingest，支持 `--format convos` 和 `--wing`
- `search` 命令：调用 mempal-search，支持 `--wing` `--room` `--top-k` `--json`

## Boundaries

### Allowed Changes
- crates/mempal-search/**
- crates/mempal-cli/**

### Forbidden
- 不实现搜索路由（P1）
- 不实现 MCP 服务器（P2）
- 不实现 AAAK 输出（P3）

## Out of Scope

- 查询路由（route_query）
- wake-up 命令
- taxonomy edit 命令
- serve 命令

## Completion Criteria

Scenario: 向量搜索返回结果
  Test: test_search_basic
  Given palace.db 包含 10 条 drawer
  When 调用 `search("auth decision", top_k=5)`
  Then 返回最多 5 条 SearchResult
  And 每条结果包含 drawer_id, content, wing, similarity
  And 结果按 similarity 降序排列

Scenario: Wing 过滤
  Test: test_search_wing_filter
  Given palace.db 包含 wing_a 和 wing_b 各 5 条 drawer
  When 调用 `search("query", wing="wing_a")`
  Then 只返回 wing_a 的结果

Scenario: Wing + Room 过滤
  Test: test_search_wing_room_filter
  Given palace.db 包含 wing_a/room_auth 和 wing_a/room_deploy 各 3 条
  When 调用 `search("query", wing="wing_a", room="room_auth")`
  Then 只返回 wing_a/room_auth 的结果

Scenario: 搜索结果带引用
  Test: test_search_citation
  Given 一条 drawer 的 source_file 为 "/path/to/file.py"
  When 该 drawer 出现在搜索结果中
  Then 结果的 source_file 等于 "/path/to/file.py"
  And 结果的 drawer_id 非空

Scenario: JSON 输出
  Test: test_search_json_output
  Given palace.db 包含数据
  When CLI 执行 `mempal search "query" --json`
  Then stdout 输出合法 JSON 数组
  And 每个元素包含 drawer_id, content, wing, similarity, source_file

Scenario: 空数据库搜索
  Test: test_search_empty_db
  Given palace.db 为空（无 drawer）
  When 调用 `search("anything")`
  Then 返回空结果集
  And 不返回错误

Scenario: init 命令扫描目录
  Test: test_cli_init
  Given 一个包含 src/auth/ 和 src/deploy/ 的项目目录
  When CLI 执行 `mempal init <dir>`
  Then taxonomy 表包含自动检测的 room（如 "auth", "deploy"）
  And 输出检测到的 wing/room 列表

Scenario: ingest 命令导入
  Test: test_cli_ingest
  Given 一个已 init 的项目
  When CLI 执行 `mempal ingest <dir> --wing myproject`
  Then drawers 表包含导入的数据
  And stdout 输出导入统计（文件数、chunk 数）

Scenario: 端到端链路
  Test: test_e2e_init_ingest_search
  Given 一个包含 README.md（内含 "decided to use PostgreSQL"）的目录
  When 依次执行 init → ingest → search "database decision"
  Then 搜索结果包含 README.md 的相关内容
  And 结果的 source_file 指向 README.md

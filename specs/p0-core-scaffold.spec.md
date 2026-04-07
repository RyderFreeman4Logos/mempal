spec: task
name: "P0: Workspace scaffold + core data model + SQLite schema"
inherits: project
tags: [p0, core, scaffold]
estimate: 1d
---

## Intent

搭建 Rust workspace 骨架，实现 mempal-core crate 的数据模型和 SQLite schema。
这是所有后续 crate 的基础——没有 core 的类型定义和数据库初始化，其他 crate 无法编译。

## Decisions

- Workspace root `Cargo.toml` 定义 8 个 member crate
- `mempal-core` 包含：数据类型（Drawer, Wing, Room, Triple, Taxonomy）、SQLite 初始化、配置加载
- SQLite 使用 `rusqlite` with `bundled` feature
- `sqlite-vec` 通过 `rusqlite` 的 extension loading 接入
- 配置文件路径：`~/.mempal/config.toml`
- 数据库路径：`~/.mempal/palace.db`（可配置覆盖）

## Boundaries

### Allowed Changes
- Cargo.toml（workspace root）
- crates/mempal-core/**
- crates/mempal-cli/（最小 main.rs 占位）
- crates/mempal-embed/（trait 定义占位）
- crates/mempal-ingest/（lib.rs 占位）
- crates/mempal-search/（lib.rs 占位）
- crates/mempal-aaak/（lib.rs 占位）
- crates/mempal-mcp/（lib.rs 占位）
- crates/mempal-api/（lib.rs 占位）

### Forbidden
- 不实现嵌入逻辑（P0 只定义 trait）
- 不实现搜索逻辑
- 不实现 CLI 命令（只有 clap 骨架）

## Completion Criteria

Scenario: workspace 编译通过
  Test: test_workspace_builds
  Given workspace root Cargo.toml 定义了 8 个 member crate
  When 执行 `cargo build --workspace`
  Then 编译成功，无 error

Scenario: 数据库初始化
  Test: test_db_init
  Given 不存在 palace.db
  When 调用 `Database::open(path)`
  Then 创建 palace.db 文件
  And 包含 drawers、drawer_vectors、triples、taxonomy 四张表
  And 包含 idx_drawers_wing 和 idx_drawers_wing_room 索引

Scenario: 数据库已存在时不破坏
  Test: test_db_idempotent
  Given 已存在包含数据的 palace.db
  When 再次调用 `Database::open(path)`
  Then 已有数据不被删除或修改

Scenario: 配置加载
  Test: test_config_load
  Given 存在 `~/.mempal/config.toml` 包含 `[embed] backend = "onnx"`
  When 调用 `Config::load()`
  Then `config.embed.backend` 等于 "onnx"

Scenario: 配置不存在时使用默认值
  Test: test_config_defaults
  Given 不存在 config.toml
  When 调用 `Config::load()`
  Then 使用默认配置（embed.backend = "onnx"，db_path = "~/.mempal/palace.db"）

Scenario: Drawer 数据类型完整
  Test: test_drawer_type
  Given Drawer struct
  When 检查字段
  Then 包含 id, content, wing, room, source_file, source_type, added_at, chunk_index

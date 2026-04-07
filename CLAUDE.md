# mempal

Rust 实现的 coding agent 项目记忆工具。单二进制，`cargo install mempal`，10 秒内带出处找回历史决策。

## 设计文档

`docs/specs/2026-04-08-mempal-design.md` — 完整架构设计，所有实现必须以此为准。

## 关键架构约束

- **存储**：SQLite + sqlite-vec，单文件 `~/.mempal/palace.db`
- **嵌入**：ONNX 默认（MiniLM），可选外部 API，通过 `Embedder` trait 抽象
- **AAAK 是输出格式化器**：不被 ingest 或 search 依赖，只在 CLI/MCP 输出侧可选调用
- **数据永远 raw 存储**：drawers 表存原文，向量索引在 drawer_vectors 表
- **搜索结果强制带引用**：每个 `SearchResult` 必须包含 `source_file` 和 `drawer_id`

## Workspace 结构

```
crates/
├── mempal-core/      # 数据模型 + SQLite schema + taxonomy
├── mempal-ingest/    # 导入管道
├── mempal-search/    # 搜索 + 路由
├── mempal-embed/     # 嵌入层（trait 抽象）
├── mempal-aaak/      # AAAK 编解码（输出侧，不被 ingest/search 依赖）
├── mempal-mcp/       # MCP 服务器
├── mempal-api/       # REST API（feature-gated）
└── mempal-cli/       # CLI 入口
```

## 实现优先级

P0: core + embed + cli (init/ingest/search) → P1: search 路由+引用 → P2: mcp → P3: aaak → P4: api

## 代码规范

- Edition 2024
- `#![warn(clippy::all)]`
- 错误处理：`anyhow`（应用层）+ `thiserror`（库层）
- 异步：`tokio`，features=["full"]
- 不用 `.unwrap()`，用 `?` 或 `.expect("reason")`

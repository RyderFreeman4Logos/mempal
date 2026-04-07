spec: project
name: "mempal"
tags: [rust, cli, mcp, memory, coding-agent]
---

## Intent

mempal 是一个 Rust 实现的 coding agent 项目记忆工具。单二进制分发，让任何 coding agent
在 10 秒内带出处找回历史决策。借鉴 MemPalace 的 Wing/Room 结构和 verbatim 存储理念，
但收缩定位为 coding agent 专用，并修复 AAAK 的形式完整性缺陷。

## Constraints

- Edition 2024，`#![warn(clippy::all)]`
- 不使用 `.unwrap()`，用 `?` 或 `.expect("reason")`
- 错误处理：`anyhow`（应用层 bin crate）+ `thiserror`（库层 lib crate）
- 异步运行时：`tokio`，features=["full"]
- 所有搜索结果必须包含 `source_file` 和 `drawer_id` 引用
- AAAK crate 不被 `mempal-ingest` 或 `mempal-search` 依赖
- 数据永远以 raw verbatim 存入 `drawers` 表，AAAK 只在输出侧格式化

## Decisions

- 存储：SQLite + `sqlite-vec`，单文件 `~/.mempal/palace.db`
- 嵌入：`ort` crate（ONNX Runtime）默认加载 MiniLM，可选外部 API
- 嵌入 trait：`Embedder` trait 抽象，`OnnxEmbedder` + `ApiEmbedder`
- CLI：`clap` 4.x
- MCP 服务器：`rmcp` crate
- REST API：`axum`（feature-gated `--features rest`）
- 配置：`~/.mempal/config.toml`（TOML 格式）
- AAAK 编码器必须有对应解码器和往返测试
- Workspace 结构：8 个 crate（core / ingest / search / embed / aaak / mcp / api / cli）

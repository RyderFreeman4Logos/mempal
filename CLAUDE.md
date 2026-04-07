# mempal

Rust 实现的 coding agent 项目记忆工具。单二进制，`cargo install mempal`，10 秒内带出处找回历史决策。

## Skills

**必须使用项目内的 Rust 技能**：`skills/rust-skills/SKILL.md`

编写、审查、调试、重构 Rust 代码时，遵循该 skill 的四步工作流（理解 → 服从 → 释放 → 约束）和概念锚点框架。

## 参考实现

mempal 借鉴 MemPalace 的设计理念（verbatim 存储、Wing/Room 结构、AAAK 压缩），用 Rust 从零实现并修复其缺陷。以下两个本地项目是关键参考：

- **MemPalace 源码**：`/Users/zhangalex/Work/Projects/AI/mempalace` — Python 原版实现，查看 `mempalace/` 目录下的 searcher.py、palace_graph.py、dialect.py、knowledge_graph.py 等模块了解原始设计
- **MemPalace 书稿**：`/Users/zhangalex/Work/Projects/AI/mempalace-book` — 基于源码的设计分析书，`book/src/` 下 25 章 + 4 个附录，包含架构评估、AAAK 完整度分析、benchmark 诚实性审查等深度内容

实现时遇到设计疑问，优先查阅书稿中的分析（特别是附录 C 的 AAAK 评估和附录 A/B 的 E2E Trace），而非直接复制 Python 代码。

## 设计文档

`docs/specs/2026-04-08-mempal-design.md` — 完整架构设计，所有实现必须以此为准。

## Spec 体系

项目使用 agent-spec 管理任务合约。所有实现必须对照 spec 验收。

### 项目级 Spec
- `specs/project.spec.md` — 项目约束（edition、依赖、编码规范、架构不变量）

### 任务 Spec（按优先级）

| Spec | 范围 | 依赖 | 估时 |
|------|------|------|------|
| `specs/p0-core-scaffold.spec.md` | workspace 骨架 + SQLite schema | 无 | 1d |
| `specs/p0-embed-trait.spec.md` | Embedder trait + ONNX 实现 | core | 1d |
| `specs/p0-ingest.spec.md` | 导入管道（格式检测/归一化/分块/存储） | core + embed | 2d |
| `specs/p0-search-cli.spec.md` | 搜索引擎 + CLI（init/ingest/search） | core + embed + ingest | 2d |
| `specs/p1-routing-citation.spec.md` | 查询路由 + 引用组装 + wake-up | search-cli | 1d |
| `specs/p2-mcp.spec.md` | MCP 服务器（4 工具） | routing | 1d |
| `specs/p3-aaak.spec.md` | AAAK 编解码（BNF + 往返验证） | core（独立） | 2d |
| `specs/p4-rest-api.spec.md` | REST API（feature-gated） | routing | 1d |

### 关键路径

```
core(1d) → embed(1d) → ingest(2d) → search-cli(2d) → routing(1d) → mcp(1d)
                                                                    → rest(1d)
core(1d) → aaak(2d)  ← 可并行
```

### Spec 使用方式

```bash
# 查看任务合约
agent-spec parse specs/p0-core-scaffold.spec.md

# 验证质量
agent-spec lint specs/p0-core-scaffold.spec.md --min-score 0.7

# 开始实现前：读 spec → 理解 intent/decisions/boundaries → 按 scenarios 验收
```

## 实现计划

`docs/plans/2026-04-08-p0-implementation.md` — P0 关键路径的完整实现计划（10 个 Task，TDD 流程）。

每个 Task 包含：写失败测试 → 验证失败 → 实现 → 验证通过 → 提交。按顺序执行：

1. Workspace scaffold + stub crates
2. Core data types
3. SQLite Database + schema
4. Config loading
5. Embedder trait + ONNX
6. Ingest — format detection + chunking
7. Ingest — full pipeline with storage
8. Search engine
9. CLI — init, ingest, search, status
10. Integration + cleanup

开始实现时：**先读对应的 spec（`specs/p0-*.spec.md`），再按 plan 的 Task 步骤执行。**

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

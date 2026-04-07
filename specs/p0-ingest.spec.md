spec: task
name: "P0: Ingest pipeline — format detection, normalization, chunking, storage"
inherits: project
tags: [p0, ingest, pipeline]
depends: [p0-core-scaffold, p0-embed-trait]
estimate: 2d
---

## Intent

实现 mempal-ingest crate：完整的导入管道，从原始文件到 SQLite 存储。支持项目文件和对话文件两种模式。
格式检测覆盖 Claude Code JSONL、ChatGPT JSON、纯文本和代码文件。分块后调用 Embedder 生成向量，
写入 drawers 和 drawer_vectors 表。

## Decisions

- 格式检测：基于文件内容自动识别，不依赖文件扩展名
- 项目文件分块：800 字符 + 100 字符重叠，段落感知
- 对话文件分块：按问答对（user message + assistant response 为一个 chunk）
- 归一化输出格式：统一为 `> user\nassistant response` transcript
- Wing/Room 路由：基于 taxonomy 关键词匹配，fallback 到 `wing_default`
- Drawer ID：`drawer_{wing}_{room}_{content_hash_first8}`
- 重复检测：同一 drawer_id 不重复写入

## Boundaries

### Allowed Changes
- crates/mempal-ingest/**

### Forbidden
- 不修改 mempal-core schema
- 不实现 Slack 格式（v2）
- 不实现背景索引/文件监控（v2）

## Completion Criteria

Scenario: 导入纯文本项目文件
  Test: test_ingest_text_file
  Given 一个 500 字符的 .md 文件
  When 调用 ingest_file(path, wing="myproject")
  Then 文件被分块并存入 drawers 表
  And drawer_vectors 表包含对应向量
  And 每条 drawer 的 wing 等于 "myproject"

Scenario: 导入代码文件
  Test: test_ingest_code_file
  Given 一个 2000 字符的 .py 文件
  When 调用 ingest_file(path, wing="myproject")
  Then 文件被分为至少 2 个 chunk（800+100 重叠策略）
  And 每个 chunk 的 source_file 指向原文件路径

Scenario: 导入 Claude Code JSONL 对话
  Test: test_ingest_claude_jsonl
  Given 一个 Claude Code JSONL 文件（每行一个 JSON 对象，包含 type 和 message 字段）
  When 调用 ingest_file(path, wing="myproject", format="convos")
  Then 对话被按问答对分块
  And source_type 等于 "conversation"

Scenario: 导入 ChatGPT JSON 对话
  Test: test_ingest_chatgpt_json
  Given 一个 ChatGPT conversations.json 文件
  When 调用 ingest_file(path, wing="myproject", format="convos")
  Then 对话被正确解析和分块

Scenario: 重复导入不创建重复 drawer
  Test: test_ingest_dedup
  Given 一个已导入过的文件
  When 再次调用 ingest_file
  Then drawers 表中不出现重复 ID 的记录

Scenario: 空文件跳过
  Test: test_ingest_empty_file
  Given 一个 0 字节的文件
  When 调用 ingest_file
  Then 不写入任何 drawer
  And 不返回错误

Scenario: 目录递归导入
  Test: test_ingest_directory
  Given 一个包含 3 个 .rs 文件和 1 个 .md 文件的目录
  When 调用 ingest_dir(dir_path, wing="myproject")
  Then 所有 4 个文件被导入
  And 忽略 .git / target / node_modules 等目录

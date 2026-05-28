# 前言：为什么 agent 需要记忆

> **本书定位**：用一套精简但完整的中文说明，解释 mempal 从设计决策、系统架构到实际使用的全貌。

mempal 是一个 Rust 实现的 coding agent 项目记忆工具。它的目标不是把聊天记录永久保存下来，而是让 agent 在长期项目中带出处找回历史决策、证据、工具用法、协作状态和 runtime 反馈。换句话说，mempal 关心的不是“存得多”，而是“能不能在下一次任务开始时少犯同样的错，并知道为什么”。

这本书写给三类读者。

第一类是正在使用 Claude Code、Codex、Gemini CLI 等 coding agent 的开发者。你需要知道什么时候该调用 search，什么时候该用 context，什么时候该把一次 handoff 显式 capture 成 durable memory。

第二类是想给 agent 增加长期项目记忆的工具作者。你需要理解 mempal 为什么选择 SQLite、raw drawer、hybrid search、MCP、knowledge card、runtime adoption evidence，而不是直接做一个“向量库加总结器”。

第三类是维护多 agent 协作和知识治理流程的项目负责人。你需要关心的不是某个 agent 是否“看起来更聪明”，而是它的记忆是否有证据链、是否能被审计、是否能被回滚、是否能跨 agent 协作。

## 阅读路径

如果你只想开始用 mempal，先读第 1、4、7、8、10 章，再看附录命令速查。它们覆盖问题定位、检索与引用、runtime context、多 agent 通信和日常操作。

如果你想理解架构，按第 2、3、4、5、6 章阅读。它们解释核心设计原则、SQLite/检索路径、道术器心智模型，以及 evidence 如何被治理成 knowledge。

如果你关心自进化，重点读第 2、5、6、7、9 章。其中第 2 章的“自动化不能先于治理”是后面闭环的设计原点；mempal 的自进化不是“agent 静默改写自己的信念”，而是 research、evidence、distill、gate、context、adoption、rollback 组成的可治理闭环。

## 本书基线

mempal 是活跃演进的项目，因此本书必须声明分析基线，避免读者把某个时点的实现当成永久契约。

- **存储 schema**：v9（`drawers`、向量表、`triples`、Phase-2 knowledge card 三表、`runtime_adoption_events` 等）。
- **spec 范围**：P0–P105。本书的概念、命令和边界都以这一段 spec 与对应 `docs/plans/` 计划为准。
- **关键运行约束**：单二进制、单 `~/.mempal/palace.db`、edition 2024、model2vec 默认嵌入（potion-multilingual-128M）、BM25 + 向量 + RRF 混合检索。

具体 CLI 参数和 MCP action 仍以 `mempal --help`、`mempal_status` 和各 spec 为准。当你读到与当前 binary 行为不一致的地方，先用 `mempal doctor` 对照 binary 与 schema 版本（见第 10 章）。

## 边界

本书不会把 mempal 包装成一个全自动大脑。当前实现提供的是 governed memory substrate：它可以记录 evidence、蒸馏候选知识、执行 deterministic gate、暴露 runtime context、记录 adoption evidence，并通过显式控制启用或回滚默认行为。它不会静默 promotion，不会让 evaluator 越权，也不会把 research report 直接变成 `dao`。

本书也不是完整 API 手册。它只解释当前 mempal 新版本里最重要的概念、架构和操作路径。完整 CLI 参数仍以 `mempal --help` 和各个 spec 为准。

中文是本书源版本。英文版和日文版会在中文内容稳定、术语边界确认后再翻译。

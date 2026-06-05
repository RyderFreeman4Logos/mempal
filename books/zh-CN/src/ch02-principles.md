# 第 2 章：核心决策与设计原则

> **本章定位**：把 mempal 的关键工程选择讲清楚，并说明这些选择的代价。

mempal 的核心设计不是从“我要做一个 AI 记忆产品”开始，而是从 coding agent 的真实约束开始：本地项目、频繁切换分支、多 agent 并发、历史决策需要出处、记忆不能随 session 消失、错误经验必须可以降级。所有架构选择都服务于这个场景。

## 决策表

| 决策 | 选择 | 原因 | 代价 |
|---|---|---|---|
| 分发 | Rust 单二进制 | `cargo install mempal`，本地运行，少依赖 | 功能要尽量收敛，不能依赖复杂服务编排 |
| 存储 | SQLite + sqlite-vec | 一个 `palace.db` 就是整个 memory palace，易备份、易移动 | 高并发写入和分布式同步不是当前主线 |
| 存储内容 | raw drawers | 原文永远保留，所有引用有根 | 摘要和压缩不能替代原文，需要额外治理层 |
| 检索 | BM25 + vector + RRF | 同时照顾关键词、语义和排序稳定性 | 排序解释比单一检索更复杂 |
| 接口 | CLI + MCP + feature-gated REST | 人类、agent、外部系统分别有入口 | 必须保证 CLI/MCP 行为语义一致 |
| 知识治理 | evidence first | 先存证据，再 distill knowledge，再 gate promotion | 学习速度慢于“自动总结后直接相信” |
| 自进化 | measured + rollback | 先记录 runtime adoption，再考虑默认行为变化 | 默认行为变更需要更多证据 |
| 协作 | runtime bus + explicit capture | 多 agent 可以通信，但 durable memory 必须显式写入 | 通信日志不会自动变成项目事实 |

这些选择共同服务一个约束：mempal 必须能在本地项目里长期运行，而不是依赖一个不断变化的云服务。它要能被 agent 调用，也要能被人类用命令行诊断；要能写入 durable memory，也要能保持协作消息的 ephemeral 边界。

## 这些选择什么时候不成立

工程决策没有“永远正确”，只有“在这个场景里更划算”。把每条决策的反面写清楚，比单方面陈述收益更诚实，也能帮读者判断 mempal 是否适合自己的场景。

**单二进制不是免费的。** `cargo install` 的代价是功能必须收敛在一个进程里。当需求变成“一个团队共享同一份记忆”“多台机器实时同步 drawer”“记忆库要进 CI 流水线供多个 runner 并发读写”时，单二进制 + 本地 SQLite 文件就不再是优势，而是瓶颈。那种场景需要的是一个带网络层、鉴权和并发控制的服务，而 mempal 的 feature-gated REST 只是接入点，不是为高并发多写设计的。如果你的记忆从一开始就是团队级共享写入，mempal 的本地优先模型并不是最佳起点。

**SQLite + sqlite-vec 有规模上限。** 在单机、十万量级 drawer、个人或小团队顺序写入的场景，sqlite-vec 的向量检索足够快且零运维。但当向量规模上到百万级、需要 ANN 索引调参、需要分片或高 QPS 并发查询时，专用向量库（Qdrant、LanceDB 等）在召回精度和吞吐上会明显胜出。mempal 选 sqlite-vec 是因为目标是“一个 coding 项目的记忆”，不是“一个组织的知识中台”——规模假设不同，结论就不同。

**raw-first 要为存储和重算买单。** 永久保留原文意味着同一份内容会同时存在于 `drawers.content`、FTS 索引和向量表里，存储量是 summary-only 方案的数倍；`reindex` 重算 embedding 也要重新跑全量原文。换来的是可追溯和可重算的能力。如果你的场景对存储极度敏感、且能接受“丢失原文、只信任摘要”，summary-only 会更省——但那样就失去了 mempal 最核心的引用根（详见第 4 章）。

换句话说，mempal 的保守是有前提的：本地项目、单机或小团队、记忆需要长期可审计。脱离这些前提，下面几条原则的代价会比收益更突出。

## 原则一：raw data 不可丢

无论后续有多少 compression、summary、card、context pack，原始 drawer 都是引用根。`drawer_id` 和 `source_file` 是 mempal 回答历史问题时的最低可信单位。没有引用根，agent 给出的“项目记忆”就会退化成模型自己的复述。

这也是 AAAK 在架构中的位置：AAAK 是输出格式化器，不是存储编码器。它可以帮助压缩输出，可以作为结构化 signals 的来源，但不能进入 ingest/search 的必经路径。AAAK 出错不应该破坏 raw storage 和 search。

raw-first 还意味着 embedding 不是事实本身。向量可以帮助找回语义相近内容，但原文仍然保存在 drawers 中，citation 也指向 drawers。

## 原则二：自动化不能先于治理

agent 可以建议，research 可以提供材料，evaluator 可以 advisory，但 promotion、default-on、rollback 都需要确定性 gate 和显式操作。

这是 mempal 与很多“自动学习”系统的分界线。一个自动总结器可以很快产生知识，但很难解释这个知识来自哪里、是否有反例、是否应该作用于所有 worktree、是否可以回滚。mempal 选择慢一点：research output 先进入 evidence；candidate knowledge 要从 evidence refs distill；promotion 要过 gate；demotion 要有 counterexample 或 reason；default-on 要有 adoption evidence 和 rollback criteria。

这种约束不是为了降低智能，而是为了避免长期污染。记忆系统一旦错误地积累信念，后续 agent 会在错误基础上继续推理，损失会比单次 hallucination 更大。

## 原则三：runtime hints 不是权限系统

`trigger_hints` 可以影响 agent 选择 workflow、skill 或工具，但不能越过 system、user、repo、client-native skill 规则。mempal context 给的是 guidance，不是 authority。

这个边界很重要。如果 memory hint 能直接触发工具或覆盖用户指令，记忆层就变成了隐式 policy engine。它会让系统难以审计，也会让旧知识具备不该有的执行权。

因此，mempal 的 runtime surface 采用保守设计：context 可以提示，brief 可以组织，phase3 可以记录 adoption，evaluator 可以建议，但最终执行仍然受当前 agent runtime 和用户指令约束。

## 原则四：anchor 与 semantic partition 分离

早期容易把 `wing` 当成项目身份，但这是错误的。`wing` 是语义分区，回答“这条 memory 属于哪个主题”；anchor 才回答“这条 memory 适用于哪个持久化范围”。

mempal 使用 `global / repo / worktree` anchor。worktree 保护分支实验，repo 共享稳定项目知识，global 承载跨项目原则。这个设计让 branch-local workaround 不会污染整个 repo，也让稳定知识不必困在某个 checkout。anchor 与 tier、field、provenance 等坐标如何组合，详见第 5 章 §正交坐标。

## 保守性的收益

mempal 的设计看起来保守：不静默 promotion，不默认启用 cards，不自动执行 research ingestion，不让 evaluator 写 lifecycle，不把 cowork runtime log 自动变成 durable memory。但这种保守让记忆可以长期积累。短期少一点自动化，换来长期可解释、可审计、可回滚。

这也是本书后续章节的主线：先建立可靠的存储和引用，再建立 typed knowledge，再建立 context，再建立 adoption measurement，最后才谈自进化。

## 本章来源

本章依据 `docs/specs/2026-04-08-mempal-design.md`、`docs/MIND-MODEL-DESIGN.md`、`docs/MIND-MODEL-IMPLEMENTATION-ARTICLE.zh-CN.md` 和 P46-P50、P77-P81 的 policy specs 整理。

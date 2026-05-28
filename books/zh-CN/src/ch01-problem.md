# 第 1 章：mempal 要解决的问题

> **本章定位**：解释 mempal 为什么不是普通搜索工具，而是 coding agent 的持久记忆层。

mempal 解决的问题不是“让 agent 搜到更多东西”，而是“让 agent 能把历史经验变成可复用的项目记忆”。这个差别很关键。搜索可以找回文本，但项目记忆要回答更难的问题：当时为什么这么设计？哪个结论已经被验证？哪个工具命令适用于当前 worktree？哪个 agent 正在处理同一个任务？某条建议被实际采用后效果如何？

coding agent 的默认状态是短命的。一次 session 里，它可以理解当前 diff、当前对话、当前错误；session 结束后，大部分判断依据都会消失。下次再打开项目，agent 只能重新 grep、重新推断、重新问人。项目越复杂，这种重启成本越高。

这种短命性会带来四类具体损失。

第一是决策丢失。一个架构选择可能在 PR、聊天、review comment 和本地调试日志里分散出现。agent 重新进入项目时只能看到当前文件，不知道“为什么不是另一种方案”。

第二是重复推理。每个新 session 都要重新建立问题背景，重新排除已经排除过的路线。模型越强，这种浪费越隐蔽，因为它总能重新生成一段看似合理的分析。

第三是协作断层。当一个 Claude Code 实例、两个 Codex 实例和一个 tmux pane 同时在同一项目工作时，“谁知道什么、谁做了什么、谁需要接手什么”不能只靠人类口头同步。

第四是错误知识无法降级。agent 可能总结出一个临时 workaround；如果系统没有 counterexample、demotion、rollback，这个 workaround 很容易被误当成长期规律。

长上下文只能缓解一部分问题。上下文窗口再大，也不是持久记忆。它没有生命周期，没有出处治理，没有跨 agent 共享，也不会自动告诉你哪些经验已经被验证、哪些只是候选判断。长上下文更像是一个很大的临时工作台，而不是一个可审计的项目记忆系统。

普通 RAG 也不够。RAG 的典型行为是：找到若干 chunk，塞给模型，然后生成回答。它解决的是“相关文本在哪里”，不是“这个项目长期形成了哪些决策、规律、工具使用方式和协作状态”。检索结果本身不是知识，搜索命中也不是理解。

这也是 mempal 与“文档检索”最重要的区别。mempal 的 search 仍然重要，但 search 只是底层能力。它必须服务于更高层目标：把 raw evidence、typed knowledge、knowledge cards、runtime context、cowork handoff 和 adoption evidence 放进同一个治理模型。

mempal 的目标可以压缩成一句话：

> 在 10 秒内，带出处找回 coding agent 需要的项目历史决策、证据和可执行上下文。

这个目标有几个含义。

第一，记忆必须有出处。没有 `source_file`、`drawer_id`、evidence ref 的回答，只是模型的临时判断。mempal 要让 agent 回答“为什么”时能指向原始记录。

第二，记忆必须分层。项目事实、领域规律、工具用法、方法论、agent 行为反馈不应该混成一个向量库。mempal 用 evidence、dao、shu、qi、knowledge card 和 runtime adoption 把它们分开。

第三，记忆必须能被治理。不是每条总结都能变成知识。research 只能进入 evidence 或 evidence-backed candidate；promotion 必须通过 gate；demotion 必须有 evidence-backed reason（counterexample 只是其中一种）。

第四，记忆必须支持协作。一个 Claude Code、两个 Codex、一个 tmux pane，本质上都是项目里的 actor。它们需要通信、handoff、ack、session close，也需要把重要 handoff 显式提升为 durable memory。

## 不解决什么

mempal 不替代模型推理。它给 agent 提供 source-backed memory，但不会保证 agent 一定正确使用这些 memory。

mempal 不把所有历史内容自动提升为知识。raw conversation、research finding、runtime observation 都先是 evidence。它们需要 distill、gate 和 explicit lifecycle action 才能变成可信 knowledge。

mempal 也不做静默后台自治。多 agent runtime 通信、maintenance run、adoption capture 都可以被工具化，但 durable memory 的写入和 lifecycle authority 必须有显式边界。

## 一个典型场景

假设你回到一个两周前中断的分支。你想知道为什么 card context 仍然默认关闭。普通 grep 可能找到多个 PR、spec 和聊天片段；普通 RAG 可能返回几段相似文本。mempal 应该返回更具体的东西：相关 drawer、source file、当前的政策边界（默认仍 opt-in，必须先有 readiness 与 rollback 机制才能默认开启，详见第 6 章与第 9 章）、是否有 runtime adoption evidence、是否存在 rollback signals，以及下一步应该使用哪个 context 或 readiness 命令。

这才是“项目记忆”的价值：不是给 agent 更多文字，而是给它有出处、有层次、有状态、可操作的历史。

因此，mempal 不是一个笔记工具，也不是单纯的 MCP search server。它是 coding agent 的记忆层、协作层和自进化证据层。

## 本章来源

本章依据 `docs/specs/2026-04-08-mempal-design.md`、`docs/MIND-MODEL-DESIGN.md`、`docs/MIND-MODEL-IMPLEMENTATION-ARTICLE.zh-CN.md` 和 `AGENTS.md` 的项目定位整理。

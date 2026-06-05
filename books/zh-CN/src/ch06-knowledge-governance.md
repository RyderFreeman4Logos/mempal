# 第 6 章：知识治理：从 evidence 到 card

> **本章定位**：解释 mempal 如何把原始 evidence 逐步治理成可使用、可审计、可降级的 knowledge。

mempal 不把“总结”直接当知识。知识必须经过生命周期治理。

Stage-1 的路径是 typed drawer：

```mermaid
flowchart LR
    E[Evidence Drawer] --> D[knowledge distill]
    D --> C[Candidate Knowledge]
    C --> G[knowledge gate]
    G --> P[Promoted Knowledge]
    P --> X[Demote / Retire with Counterexample]
```

`knowledge distill` 从已有 evidence refs 创建 candidate knowledge。它不会凭空写 dao，也不会自动 promote。promotion 前必须经过 gate。gate 会检查 tier、status、supporting refs、verification refs、counterexamples、reviewer 等条件。

demotion 同样需要证据。一个知识被反例推翻时，不能只改状态；需要记录 counterexample refs 和 audit。

## Stage-1：typed drawers

Stage-1 的目标是先用最小改动让 drawer 具备 mind-model 语义。evidence drawer 保存“看到了什么”；knowledge drawer 保存“系统当前认为值得治理的 belief”。两者可以共享 storage，但 metadata 和 runtime 语义不同。

knowledge drawer 里最关键的字段是 `statement`、`tier`、`status` 和 evidence refs。`statement` 是短命题，适合 wake-up 和 context 组装；长内容放在 `content`，用于解释 rationale、examples 和 boundaries。

refs 必须区分角色：

| Ref role | 含义 |
|---|---|
| supporting | 支持该 claim 的证据 |
| verification | 独立验证或复现证据 |
| counterexample | 削弱或推翻该 claim 的证据 |
| teaching | 人类教学或明确指令来源 |

这四种 role 不是约定俗成的字符串，而是代码里的封闭枚举 `KnowledgeEvidenceRole`（`src/core/types.rs`，取值 `Supporting` / `Verification` / `Counterexample` / `Teaching`），并在 `knowledge_evidence_links` 表上以 `CHECK(role IN (...))` 约束落库。也就是说，写错 role 名在数据库层就会被拒，不会出现“拼错的第五种角色”。

这种 role separation 让 gate 可以做确定性判断。只说“有三个 evidence refs”不够，因为三个 supporting refs 和一个 counterexample ref 的含义完全不同——前者推动提升，后者直接阻断。

Phase-2 引入 knowledge cards。card 是更明确的知识治理单元。它包含：

- `knowledge_cards`
- `knowledge_evidence_links`
- `knowledge_events`

cards 让 evidence role 更清楚：supporting、counterexample、teaching、verification。lifecycle event 也变成 append-only 记录。这样 agent 不只知道“现在状态是什么”，还知道“这个状态如何形成”。

## Phase-2：cards 是 governed beliefs

Typed drawers 证明了模型，但 drawer 仍然更适合保存 raw evidence。Phase-2 把 distilled belief 抽成独立 card，并在同一个 SQLite `palace.db` 里新增三类表。

`knowledge_cards` 保存 statement、content、tier、status、domain、field、anchor、trigger hints 和 timestamps。它是 belief 本体。

`knowledge_evidence_links` 把 card 连接到 evidence drawer，并标注 role。它保留了 card 与 raw source 的关系。

`knowledge_events` 记录 created、linked、promoted、demoted、retired、published_anchor 等 lifecycle 事件。它是 append-only audit log。

这个设计让 mempal 能回答两类问题：当前系统相信什么，以及这个 belief 是如何形成和变化的。

card 的状态不等于 runtime 默认行为。P41 明确过：cards 已治理，但不是默认 search/context source。P44 以后，context 可以显式 `include_cards`。P46/P78 以后，是否默认开启 card context 必须有 readiness、proposal 和 rollback 条件——这条默认开关的完整演进（readiness、proposal、default control、rollback）详见第 9 章 §5。

## Gate 与 authority

gate 是 read-only 或 gate-enforced 的确定性检查。它可以告诉你 candidate 是否满足提升门槛，但它不应该被 evaluator 或 research 绕过。

“确定性”意味着 gate 不靠模型判断，而是按 tier 查一张固定的策略表，逐项比对 evidence 计数。`src/knowledge_gate.rs` 里的 `gate_requirements_for_policy` 把每个 tier 的提升门槛写成代码常量：

| tier → 目标状态 | 最少 supporting | 最少 verification | 最少 teaching | 需要 reviewer | 有 counterexample 即阻断 |
|---|---|---|---|---|---|
| `dao_tian → canonical` | 3 | 2 | 1 | 是 | 是 |
| `dao_ren → promoted` | 2 | 1 | 0 | 否 | 是 |
| `shu / qi → promoted` | 1 | 1 | 0 | 否 | 是 |

可以看到门槛随 tier 抬升：越通用、越跨领域的知识（`dao_tian`），要求越多独立 verification、强制 teaching 来源、强制人类 reviewer。这正好对应第 5 章说的误判不对称——`dao_tian` 一旦错升会污染所有领域，所以它的门最高。

gate 还有一个容易被忽略的硬约束：所有 evidence ref 必须指向真实存在的 **evidence** drawer。`validate_gate_refs` 会逐个加载 ref，确认它以 `drawer_` 开头、能查到、且 `memory_kind` 是 `Evidence`；否则直接报错。这意味着你无法用一条 knowledge drawer 去“支撑”另一条 knowledge——支撑链必须落到原始证据上，避免 belief 互相循环背书。

Evaluator 可以 advisory。它可以建议 supporting refs、risk notes、promotion readiness，但不能直接 mutate lifecycle state。Research 也一样：它可以提供 evidence 和 candidate insights，但不能直接写 promoted knowledge 或 canonical dao。

这条边界保护的是 lifecycle authority。mempal 允许自动化帮助判断，但不允许自动化静默改变系统信念。

最小治理流程如下：

```bash
mempal knowledge distill \
  --tier dao_ren \
  --statement "..." \
  --content "Evidence-backed rationale, scope, and examples." \
  --supporting-ref drawer_...

mempal knowledge gate drawer_... --format json

mempal knowledge-card gate card_... --format json
mempal knowledge-card promote card_... \
  --status promoted \
  --verification-ref drawer_... \
  --reason "verified by linked evidence" \
  --enforce-gate \
  --format json
```

对 agent 来说，对应 MCP surface 包括 `mempal_knowledge_distill`、`mempal_knowledge_gate`、`mempal_knowledge_promote`、`mempal_knowledge_demote`、`mempal_knowledge_publish_anchor` 和 `mempal_knowledge_cards`。CLI 和 MCP 的区别只是入口不同，治理语义不应该不同。

## Demotion 同等重要

很多 memory 系统重视“学会新知识”，但忽略“忘掉错误知识”。mempal 把 demote/retire 当成 lifecycle 的一等公民。一个 card 被反例削弱时，应该链接 counterexample evidence，并写入 event，而不是只从 context 里消失。

这点对长期 agent 很关键。没有 demotion，memory 只会单向积累；单向积累最终会把临时 workaround、过时工具行为和错误推断固化成长期偏见。

核心规则只有一个：知识不能因为“看起来合理”就进入 promoted/canonical 状态。它必须有 evidence，必须通过 deterministic gate，必要时必须有人类 review。

这让 mempal 的知识层比普通笔记更慢，但也更可靠。长期记忆的风险不是忘记，而是错误地记住。

## 本章来源

本章依据 P17-P25 Stage-1 lifecycle specs、P31-P40 knowledge card specs、P41-P48 card runtime/policy specs、P49-P50 research/evaluator policy，以及 `docs/MIND-MODEL-IMPLEMENTATION-ARTICLE.zh-CN.md` 整理。

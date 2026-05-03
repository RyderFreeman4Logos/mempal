# 从记忆工具到可治理的心智模型

> 本文解释 mempal 心智模型从最初的“道 / 术 / 器”讨论，到 P54-P59
> Phase 3 runtime baseline 已实现部分之间的设计决策、实现边界和后续方向。

## 1. 问题：记忆不只是召回

多数 agent memory 系统一开始都会采用一个简单前提：agent 见过某段信息，
就把它存起来；以后需要时再检索出来，塞回上下文。

这个前提有用，但对一个长期运行的 coding agent 来说远远不够。

coding agent 需要的不只是旧文本。它还需要知道：哪些旧文本是证据，哪些
是稳定结论，哪些是工作流，哪些是工具命令，哪些已经被反例削弱或推翻。
如果所有记忆都被当成同一种对象保存，系统迟早会失去“观察”和“规律”之间
的边界。

mempal 的心智模型工作就是从这个失败模式出发的。

核心论点是：

> 记忆应该是一个可治理的知识演化层。先记录原始证据，再蒸馏候选知识，
> 最后只有通过显式 gate 的知识，才可以成为可信的 runtime 指导。

所以这套系统不是“RAG 加 skills”。RAG 负责检索旧材料，skills 负责编码
过程，而心智模型要治理的是：证据如何变成知识，知识如何影响 skill 和工具
选择，runtime 行为又如何反过来推动系统继续演化。

当前实现已经达到一个具体 baseline：

- Stage 1 typed drawers 已经区分 evidence 和 knowledge。
- runtime context 已经按 `dao_tian -> dao_ren -> shu -> qi` 组装。
- knowledge lifecycle 已经支持 distill、gate、promote、demote 和向外发布
  anchor。
- Phase 2 knowledge cards 已经把“被治理的 belief”和 raw evidence drawer
  分离。
- card retrieval 和 card-aware context 已经存在，但仍然是 opt-in。
- Phase 3 runtime adoption evidence 已经通过 schema v9 和 CLI gates 落地。

因此，下一阶段不应该再回头补 baseline，而应该验证这个 baseline 是否真的
改善 agent 的实际行为。

## 2. 道、术、器：知识轴，而不是存储目录

最初的概念拆分是 `dao / shu / qi`。

在 mempal 里，它们的含义是：

- `dao`：治理原则和高层知识。
- `shu`：可复用的方法、工作流和过程性知识。
- `qi`：具体工具、接口、命令和工具相关用法。

`dao` 又进一步拆成两层：

- `dao_tian`：跨领域高层原则，数量最少，稳定性最高。
- `dao_ren`：领域规律，仍然稳定，但有明确 field 边界。

这个拆分重要，是因为 coding agent 对不同知识的使用方式不同。

`dao_tian` 应该塑造判断。例如“证据先于断言”或者“只有 promotion 没有
demotion 会造成知识污染”。这类知识不应该多，也不应该频繁变化。

`dao_ren` 应该塑造领域推理。对一个 Rust 项目来说，它可能是关于 schema
migration、citation 语义、agent runtime 行为边界的稳定规则。

`shu` 应该塑造行动。它可以影响 workflow 和 skill 选择。例如，在实现一个
功能前，先查看现有 spec，写最小 contract，再用确定性工具验证。

`qi` 应该绑定执行。它说明具体有哪些 CLI、哪些 MCP tool、哪些 feature flag，
以及某个工具接口具体怎么用。

关键设计决策是：`dao / shu / qi` 不是 project memory、agent memory 或
skill memory 的替代品。它们是不同坐标轴。

完整坐标应该包括：

- `domain`：这条记忆服务于谁，例如 project、agent、skill、global。
- `tier`：它是哪类知识，例如 `dao_ren`、`shu`、`qi`。
- `field`：它属于哪个主题领域。
- `provenance`：它来自哪里，例如 runtime、research、human。
- `anchor`：它属于哪个持久化范围，例如 global、repo、worktree。

这个正交关系可以避免几类常见错误：

- 项目局部 workaround 不应该变成普遍规律。
- 临时工具行为不应该变成稳定领域原则。
- workflow tip 不应该被误认为治理原则。
- research summary 不应该直接创建 `dao`。

因此，设计上把 `dao` 放在 memory layer。外部工具可以产生 evidence，但
promotion 的所有权属于 memory。

## 3. Evidence 和 Knowledge 必须是不同对象

当前实现最核心的拆分，是 evidence memory 和 knowledge memory。

Evidence memory 保存“看到了什么”：

- 原始 research 输出
- runtime 观察
- 人类显式 teaching
- 具体失败
- 矛盾
- 反例

Evidence 可以不一致。这不是 bug，而是预期行为。真实世界本来就会给出冲突
观察，agent 应该能先记录这些观察，而不是急着把它们压平成一个结论。

Knowledge memory 保存“系统当前相信什么”：

- 工具使用说明
- workflow 方法
- 领域模式
- 治理原则

Knowledge 的数量应该更少，必须可审计，并且有状态。它可以是 candidate、
promoted、canonical、demoted 或 retired。

这个拆分回答了一个基础问题：什么才算真正学习？

学习不是“存了更多文本”。学习发生在观察持续积累、模式被蒸馏、有用候选被
提升、错误或过时 belief 可以被降级的时候。

因此 lifecycle 被拆成四个基本动作：

1. `record`：存原始证据。
2. `distill`：从证据创建候选知识。
3. `promote`：让知识进入可信 runtime 使用。
4. `demote`：当知识被反例削弱或过时时，降低或退休它。

这是刻意不对称的设计。Evidence 应该增长快，law 应该增长慢。

项目已经用两种形态实现了这个拆分：

- Stage 1 用 typed drawers 作为 bootstrap 模型。
- Phase 2 引入专门的 knowledge card tables。

这个演进顺序很重要。它让 mempal 不需要一开始就重写存储系统，而是先用低成
本方式验证模型；等治理形状稳定之后，再抽取出更干净的 knowledge object。

## 4. Stage 1：Typed Drawers 作为 Bootstrap 架构

在心智模型工作之前，drawer 基本上就是 durable memory unit。Stage 1 没有
替换 drawer 系统，而是让 drawer 具备类型语义。

核心区分变成：

- evidence drawer
- knowledge drawer

两者共享基础 drawer 字段，但元数据和 runtime 语义不同。

evidence drawer 记录 source-backed material。它包含
`memory_kind=evidence`、`domain`、`field`、`provenance`、`anchor_kind`、
`anchor_id` 等字段。它不携带 `tier`、`status`、`statement` 或 lifecycle refs。

knowledge drawer 记录被蒸馏出的 belief。它包含 `memory_kind=knowledge`、
`statement`、`tier`、`status`、role-separated evidence refs、scope
constraints、trigger hints 和 anchor metadata。

`statement` 字段很关键。它是短的 wake-up proposition。较长的 `content`
用于解释 rationale、examples 和 boundaries。runtime context 应该能先唤醒
一个短结论，而不是每次都把长篇解释塞进上下文。

Stage 1 还引入了 role-separated refs：

- `supporting_refs`
- `verification_refs`
- `counterexample_refs`
- `teaching_refs`

这不只是元数据更整洁。它让 promotion gate 能理解一个 claim 为什么可信。
supporting ref 和 counterexample ref 不能被混在同一个匿名 evidence list 里。

随后实现了最小 lifecycle surface：

- `mempal knowledge distill`
- `mempal knowledge gate`
- `mempal knowledge promote`
- `mempal knowledge demote`
- `mempal knowledge publish-anchor`

runtime agent 需要直接访问的部分，也补了 MCP surface：

- `mempal_knowledge_distill`
- `mempal_knowledge_gate`
- `mempal_knowledge_promote`
- `mempal_knowledge_demote`
- `mempal_knowledge_publish_anchor`

关键约束是：这些操作不允许调用者直接声明 confidence。Promotion 依赖确定性
policy。例如 Stage 1 policy 要求证据数量、verification refs，并且对
`dao_tian` 要求人类 reviewer。

这就是系统避免危险捷径的方式：

> “agent 发现了一个有趣模式，所以系统学会了一条 law。”

这条捷径被明确禁止。

## 5. Anchors：Worktree、Repo、Global

心智模型还必须解决项目身份问题。

最早容易想到的是用 `wing` 表示项目身份。但这不对，因为 `wing` 是语义分区。
它回答“这条 memory 属于哪个语义区域”，而不是“它来自哪个 checkout”。

最终实现的 anchor model 区分：

- `global`
- `repo`
- `worktree`

这个区分对 coding 工作很必要。

只有 repo anchor 太宽。不同 branch experiment 会互相污染，一个 worktree 中的
失败实验可能影响另一个 worktree 的推理。

只有 worktree anchor 又太窄。稳定项目知识会在不同 checkout 之间碎片化，每个
新 worktree 都会过于空白。

因此设计采用双层 scope：

- 本地观察先写到 `worktree`
- 被验证的项目知识再向外发布到 `repo`
- 只有跨项目原则才发布到 `global`

这也产生了两种不同的上升运动：

- tier promotion，例如 `qi -> shu` 或 `shu -> dao_ren`
- anchor publication，例如 `worktree -> repo -> global`

两者不是同一个操作。一个 workflow 可能在当前 worktree 内可信，但还不应该在
整个 repo 中共享。一个 repo-level pattern 可能对 mempal 稳定，但还没资格成为
global `dao_tian`。

runtime context assembly 的 anchor 优先级是：

1. 当前 worktree
2. 当前 repo
3. global

这样 agent 会先拿到 branch-local context，再拿到稳定项目 memory，最后拿到
通用 law。tier order 仍然在 anchor 逻辑内部生效。

## 6. Runtime Context：Wake-Up 不是 Context Assembly

实现里一个重要边界，是 wake-up 和 mind-model context 的分离。

Wake-up 仍然是 L0/L1 memory refresh surface。它可以把旧材料重新带回注意力，
但它不负责组装 typed operating guidance。

typed operating guidance 属于：

- `mempal context`
- `mempal_context`

这些 surface 按以下顺序组装 context：

1. `dao_tian`
2. `dao_ren`
3. `shu`
4. `qi`
5. optional evidence

这个顺序是刻意设计的。

`dao_tian` 校准世界观。`dao_ren` 校准当前领域。`shu` 影响 workflow 和 skill
selection。`qi` 绑定具体工具。Evidence 用于 grounding、异常处理和证明。

实现上还让 `dao_tian` 默认保持稀疏。`mempal context` 和 `mempal_context`
默认最多注入一条 `dao_tian`，除非调用者显式提高 budget。这避免了实践执行被
抽象原则淹没。

runtime skill guidance 也被设计成非权威层。`trigger_hints` 可以 bias workflow
和 tool choice，但不能执行 skill，也不能覆盖 system、user、repo 或 client-native
skill rules。

这个边界非常关键。如果 memory hints 可以直接触发工具或覆盖显式指令，系统就会
变成一个不受治理的 policy engine。当前设计让 memory 只提供指导，agent 仍然
服从正常的 instruction hierarchy。

## 7. Phase 2：Knowledge Cards 作为被治理的 Beliefs

Typed drawers 证明了模型，但它仍然是 bootstrap 架构。Drawer 适合保存 raw
evidence，但不是 governed belief 的最终形态。

Phase 2 在同一个 SQLite `palace.db` 中引入了专门的 knowledge card storage。

核心表是：

- `knowledge_cards`
- `knowledge_evidence_links`
- `knowledge_events`

这是一个明确的存储决策。Cards 默认不应该放到外部数据库。mempal 的产品不变量
是 local、single-binary、single-file memory palace。把 cards 放在同一个 SQLite
数据库里，可以保持 lifecycle update 的事务性，也避免在模型还没证明需要独立
扩展之前引入额外运维复杂度。

分层关系很清楚：

- drawers 仍然是 raw evidence 和 citation root
- cards 保存 distilled beliefs
- evidence links 把 cards 连接到 source-backed drawers
- events 记录 belief 如何演化

一张 card 包含 statement、content、tier、status、domain、field、anchor
metadata、trigger hints 和 timestamps。

一条 evidence link 用 role 把 card 连接到 evidence drawer：

- supporting
- verification
- counterexample
- teaching

一条 event 记录 lifecycle history：

- created
- promoted
- demoted
- retired
- linked
- unlinked
- updated
- published_anchor

这让 Phase 2 比 Stage 1 更显式。Drawer 继续保存 evidence，card 保存 belief，
event stream 解释这个 belief 为什么发生变化。

已实现的 surface 包括：

- `mempal knowledge-card create`
- `mempal knowledge-card get`
- `mempal knowledge-card list`
- `mempal knowledge-card link`
- `mempal knowledge-card event`
- `mempal knowledge-card events`
- `mempal knowledge-card backfill-plan`
- `mempal knowledge-card backfill-apply`
- `mempal knowledge-card gate`
- `mempal knowledge-card promote`
- `mempal knowledge-card demote`
- `mempal knowledge-card retrieve`

MCP tool `mempal_knowledge_cards` 也暴露了 read、lifecycle 和 retrieve actions。

关键边界是：Phase 2 cards 已经是 governed objects，但还不是默认 runtime
sources。它们可以被 retrieve，也可以通过 `--include-cards` 或 `include_cards=true`
注入 context。但普通 `mempal_search` 仍然 drawer-based，默认 context 仍然
drawer-only。

这不是实现缺口，而是 trust boundary。

默认 runtime context 是高信任路径。要从 drawer-backed knowledge 切换到
card-backed guidance，必须有 runtime evidence 证明这种切换能改善行为，并且不
破坏 citations、lifecycle semantics 或 context budget。

## 8. Research 和 Evaluator：证据生产者，不是权威

设计也明确了 external research 和 evaluator 的角色。

`research-rs` 是外部工具。它是 `qi`，不是 `dao`。

它的职责是收集和组织外部材料：

- raw sources
- wiki pages
- schema
- index
- logs
- lint reports

它的输出可以成为 evidence。它可以产生 structured summaries、candidate
insights 和 contradiction signals。但它不能直接创建 `dao_tian`、promoted
knowledge 或 canonical knowledge。

这个边界已经进入实现 policy：

- research output 进入 evidence
- candidate knowledge 必须从 evidence refs distill
- promotion 仍由 lifecycle gates 控制
- contradictions 变成 counterexamples 或 demotion evidence

Evaluator 也有类似边界。

Evaluator 可以建议 promotion 或 demotion。它可以提出 supporting refs、
verification refs、counterexample refs 和 risk notes。它可以解释为什么某个
knowledge item 看起来 ready 或 unsafe。

但 evaluator 不能直接 mutate lifecycle state。

Evaluator output 是 advisory-only。它不能满足 human reviewer requirement，
不能绕过 deterministic gates，也不能 canonicalize `dao_tian`。

这就是 assistance 和 authority 的区别。系统可以使用 evaluator advice，但不让
evaluator 成为 lifecycle actor。

## 9. Phase 3 到目前为止：先收集 Runtime Evidence，再增加 Runtime Power

P51 之后，baseline 已经关闭。P52 定义 Phase 3 是新阶段，而不是继续补
P12-P50 没做完的部分。P53 审计 candidate tracks 后确认：没有任何候选方向应该
直接进入默认行为变更。

被推荐的第一个 Phase 3 track 是 runtime adoption evidence。

这个推荐已经通过 P54-P59 实现。

P54 增加 schema v9 表 `runtime_adoption_events`。这张表记录具体 runtime
signals：

- signal 属于哪个 track
- 发生了哪种 signal
- 涉及哪个 feature
- optional query、context hash、card id、evaluator id、research report id
- note、metadata、timestamp

支持的 tracks 包括 runtime adoption、card context、card embedding、evaluator
和 research adapter。支持的 signals 包括 used、accepted、rejected、miss、
rollback、contradiction、neutral。

P55 通过 CLI 暴露这张表：

- `mempal phase3 adoption record`
- `mempal phase3 adoption list`
- `mempal phase3 adoption stats`

P56 增加 card context default 的 read-only gate。当前 policy 仍然是 opt-in。
这个 gate 只报告是否有足够 accepted `card_context` evidence，以及是否没有
rollback signals。

P57 增加 card embedding 的 read-only gate。它明确不增加 card vector schema。
Card embeddings 需要重复、可测量的 miss evidence，证明 linked-evidence
retrieval 因为只能匹配 evidence wording 而漏掉了应该通过 card statement 找到
的 active card。

P58 增加 evaluator API 的 read-only gate。它保持 advisory-only 边界。
Evaluator signals 不会 mutate lifecycle state，也不会绕过 gates。

P59 增加 `mempal phase3 research-validate-plan`。它验证 external research
report contract，包括 `report_id`、`title`、`sources`、`findings` 和 optional
`candidate_insights`。它只做 validate 和 plan，不 ingest promoted 或 canonical
knowledge。

模式是一致的：

> 在增加 runtime power 之前，先收集 runtime evidence。

这就是 Phase 3 的目标。

## 10. 当前到底已经构建了什么

现在可以把已实现系统概括成一个 governed cognition stack。

存储与检索：

- SQLite `palace.db` 是唯一 local storage root。
- Drawers 仍然是 raw memory 和 evidence citation roots。
- BM25 加 vector retrieval 仍然是基础 search path。
- AAAK 仍然是 output formatter，不是 storage dependency。

Stage 1 mind model：

- evidence 和 knowledge drawers 已经 typed
- `statement` 用于 knowledge wake-up
- `dao_tian`、`dao_ren`、`shu`、`qi` 有明确 status rules
- anchors 区分 worktree、repo、global scope
- lifecycle operations 是显式且 evidence-backed 的

Runtime context：

- `mempal context` 和 `mempal_context` 组装 typed guidance
- context order 是 `dao_tian -> dao_ren -> shu -> qi`
- evidence 是 opt-in
- card context 是 opt-in
- memory hints bias skill/tool choice，但不执行

Phase 2 cards：

- `knowledge_cards` 保存 governed beliefs
- `knowledge_evidence_links` 保留 role-separated citations
- `knowledge_events` 提供 append-only lifecycle audit
- cards 可以从 Stage 1 knowledge backfill
- cards 可以 gate、promote、demote、retrieve
- 默认 search 不返回 cards

Policy boundaries：

- research output 不能直接创建 `dao`
- evaluator output 是 advisory-only
- `dao_tian` canonicalization 需要 human review
- card embeddings 延后到有证据支持后再做
- default card context 延后到 runtime evidence 证明后再做

Phase 3 baseline：

- runtime adoption events 已存入 schema v9
- adoption evidence 可通过 CLI inspect
- card context default、card embeddings、evaluator APIs、research adapter
  ingestion 都有 readiness gates
- research report validation 已存在，但不会自动 promotion

这已经足够进入新阶段，因为系统不再只是设计。它已经有 storage、lifecycle、
runtime context、MCP surfaces、card governance 和 measurement gates。

## 11. 哪些事情仍然故意没做

有些事情仍然故意不做。

Card-aware context 还不是默认行为。它仍然 opt-in，因为默认 runtime context 是
trust boundary。系统需要重复 runtime traces 证明它有收益，才能默认启用 cards。

Card embeddings 还不存在。当前 card retrieval 是 linked-evidence-first。这保留
citation semantics，并避免 stale card-vector maintenance。Card embeddings 需要
statement-match misses 和 rollback plan。

Research adapters 不会直接 ingest promoted knowledge。它们可以 validate input
plans，未来也可以写 evidence 或 candidate insights，但 promotion 必须留在 memory
lifecycle governance 下。

Evaluator APIs 不会 mutate knowledge state。它们可以 advise，但 gates 和
human-review requirements 仍然是权威边界。

系统现在也不宣称已经实现完全自主自进化。它有的是一个可治理的 self-improvement
substrate：

- evidence capture
- candidate distillation
- gate evaluation
- reversible lifecycle
- runtime adoption measurement

Autonomy 只能在有证据证明有效，并且 rollback 明确时逐步加入。

## 12. 后续设计原则

下一阶段应该用一个问题来判断每个改动：

> 这个改动是否能产生可测量的 runtime improvement，同时不削弱 evidence、
> citation、audit 或 rollback 边界？

如果答案是否定的，这个改动就应该保持 advisory、opt-in 或 deferred。

这也是为什么下一步更适合先把 Phase 3 runtime surfaces 暴露给 MCP，并定义
adoption-recording protocol。Agent 需要直接记录某个 context item、card、
evaluator suggestion 或 research result 是否被 used、accepted、rejected、missed
或 rolled back。

只有这些 evidence 积累起来之后，系统才应该考虑更强默认行为：

- card context default-on
- card embeddings
- evaluator APIs
- research adapter apply paths

所以心智模型的推进顺序应该是：

1. 定义 knowledge hierarchy
2. 分离 evidence 和 knowledge
3. 实现 lifecycle governance
4. 暴露 runtime context
5. 抽取 governed cards
6. 保持 citations 和 audit
7. 测量 runtime adoption
8. 最后才增加 authority

这个顺序就是当前实现最重要的成果。

它把 memory 从被动 recall 变成了 governed learning loop。

它让 `dao` 留在 memory，`shu` 留在可复用方法，`qi` 留在工具层，而 evidence
成为所有 durable belief 的 justification substrate。

它也给下一阶段留下了明确任务：

> 从已经实现的 cognition architecture，推进到可测量、可回滚、证据驱动的
> agent evolution。

## Source Notes

本文基于仓库中的设计和实现清单：

- `docs/MIND-MODEL-DESIGN.md`
- `AGENTS.md`
- `CLAUDE.md`
- `specs/p12-mind-model-bootstrap.spec.md`
- `specs/p14-context-assembler.spec.md`
- `specs/p15-mcp-context.spec.md`
- `specs/p17-knowledge-lifecycle.spec.md`
- `specs/p18-knowledge-distill.spec.md`
- `specs/p20-promotion-gate-policy.spec.md`
- `specs/p31-knowledge-card-schema.spec.md`
- `specs/p32-knowledge-card-schema-v8.spec.md`
- `specs/p38-knowledge-card-gate.spec.md`
- `specs/p44-card-context-assembler.spec.md`
- `specs/p45-card-linked-evidence-retrieval.spec.md`
- `specs/p49-research-ingestion-policy.spec.md`
- `specs/p50-evaluator-promotion-policy.spec.md`
- `specs/p51-mind-model-closure-audit.spec.md`
- `specs/p52-phase-3-intake-roadmap.spec.md`
- `specs/p53-phase-3-candidate-evidence-audit.spec.md`
- `specs/p54-runtime-adoption-evidence.spec.md`
- `specs/p55-runtime-adoption-cli.spec.md`
- `specs/p56-card-context-default-gate.spec.md`
- `specs/p57-card-embedding-evidence-gate.spec.md`
- `specs/p58-evaluator-api-evidence-gate.spec.md`
- `specs/p59-research-adapter-ingestion-contract.spec.md`

同时交叉核对了已存入 mempal 的 evidence：

- `drawer_mempal_p31_cc86ab94`, `source_file=mempal-p31-knowledge-card-schema-spec.md`
- `drawer_mempal_p36_3770af58`, `source_file=mempal-p36-knowledge-card-backfill-report.md`
- `drawer_mempal_p18_b6dac6fe`, `source_file=mempal-p18-knowledge-distill-memory.md`
- `drawer_mempal_p30_7451b74c`, `source_file=mempal-p30-knowledge-card-storage-boundary.md`
- `drawer_mempal_p53_82a98e1f`, `source_file=mempal-p53-phase-3-candidate-evidence-audit.md`
- `drawer_mempal_p54_p59_866027ab`, `source_file=mempal-p54-p59-phase3-runtime.md`

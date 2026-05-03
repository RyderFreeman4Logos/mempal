# From Memory Tool to Governed Mind Model

> This article explains the design decisions and implemented surfaces behind
> mempal's mind-model work from the initial dao/shu/qi discussion through the
> completed P54-P59 Phase-3 runtime baseline.

## 1. The Problem: Memory Is Not Just Recall

Most agent memory systems start from a simple premise: if the agent has seen
something before, store it, retrieve it later, and put it back into context.
That premise is useful, but it is not enough for a long-running coding agent.

A coding agent does not merely need old text. It needs to know which old text is
evidence, which part is a stable conclusion, which part is a workflow, which
part is a tool-specific command, and which part has already been contradicted.
If all memories are stored as the same kind of item, the system eventually loses
the difference between observation and law.

The mind-model work in mempal starts from that failure mode.

The thesis is:

> Memory should be a governed knowledge evolution layer. Raw evidence is
> recorded first, candidate knowledge is distilled second, and trusted runtime
> guidance is promoted only through explicit gates.

That is why this work is not "RAG plus skills." RAG retrieves old material.
Skills encode procedures. The mind model tries to govern how evidence becomes
knowledge, how knowledge affects skills, and how runtime behavior feeds back
into future evolution.

The current implementation has reached a concrete baseline:

- Stage 1 typed drawers separate evidence from knowledge.
- Runtime context assembles `dao_tian -> dao_ren -> shu -> qi`.
- Knowledge lifecycle supports distill, gate, promote, demote, and outward
  anchor publication.
- Phase 2 knowledge cards separate governed beliefs from raw evidence drawers.
- Card retrieval and card-aware context exist, but remain opt-in.
- Phase 3 runtime adoption evidence now exists as schema v9 and CLI gates.

The next stage should therefore not reopen the baseline. It should test whether
the baseline improves real agent behavior.

## 2. Dao, Shu, Qi: A Knowledge Axis, Not a Storage Folder

The original conceptual split is `dao / shu / qi`.

In mempal, the split means:

- `dao`: governing principles and high-level knowledge.
- `shu`: reusable methods, workflows, and procedures.
- `qi`: concrete tools, interfaces, commands, and tool-specific usage.

`dao` is further split:

- `dao_tian`: cross-domain high-level principles, minimal and extremely stable.
- `dao_ren`: domain-specific laws, still stable but with clear field boundaries.

This matters because a coding agent uses different kinds of knowledge
differently.

A `dao_tian` item should shape judgment. It is the type of knowledge that says
"evidence precedes assertion" or "promotion without demotion causes knowledge
pollution." It should not be numerous, and it should not change often.

A `dao_ren` item should shape domain reasoning. For a Rust project, this may be
a stable rule about schema migration boundaries, citation semantics, or agent
runtime behavior.

A `shu` item should shape action. It can bias workflow and skill choice. For
example, before implementing a feature, inspect the existing spec, write a
minimal contract, and verify with deterministic tools.

A `qi` item should bind execution. It may say which CLI command exists, which
MCP tool accepts which fields, or which feature flag controls a runtime path.

The key design decision is that `dao / shu / qi` is not the same thing as
project memory, agent memory, or skill memory. Those are different axes.

The orthogonal coordinates are:

- `domain`: who the memory is for, such as project, agent, skill, or global.
- `tier`: what kind of knowledge it is, such as `dao_ren`, `shu`, or `qi`.
- `field`: which subject area it belongs to.
- `provenance`: where it came from, such as runtime, research, or human.
- `anchor`: which persistence scope it belongs to, such as global, repo, or
  worktree.

This prevents several common errors:

- A local project workaround should not become universal law.
- A temporary tool behavior should not become a stable domain principle.
- A workflow tip should not be confused with a governing rule.
- A research summary should not directly create `dao`.

The design therefore treats `dao` as memory-level high-order knowledge. External
tools may produce evidence, but memory owns promotion.

## 3. Evidence and Knowledge Must Be Different Objects

The central implementation split is between evidence memory and knowledge
memory.

Evidence memory stores what was observed:

- raw research output
- runtime observations
- human teachings
- concrete failures
- contradictions
- counterexamples

Evidence can be inconsistent. That is expected. The world is messy, and the
agent should be allowed to record competing observations without prematurely
resolving them.

Knowledge memory stores what the system currently believes:

- a tool note
- a workflow method
- a domain pattern
- a governing principle

Knowledge is lower-volume, auditable, and stateful. It has a status. It can be
candidate, promoted, canonical, demoted, or retired.

This split answers a basic question: what counts as learning?

Learning is not "we stored more text." Learning happens when observations
accumulate, patterns are distilled, useful candidates are promoted, and false or
obsolete beliefs can be demoted.

The resulting lifecycle is:

1. `record`: store raw evidence.
2. `distill`: create candidate knowledge from evidence.
3. `promote`: move knowledge into trusted runtime use.
4. `demote`: weaken or retire knowledge when contradicted.

This is intentionally asymmetric. Evidence should grow quickly. Law should grow
slowly.

The project has already implemented this in two forms:

- Stage 1 uses typed drawers as a bootstrap model.
- Phase 2 introduces dedicated knowledge card tables.

That progression matters. It allowed mempal to validate the model without
rewriting the entire storage system first, then move toward a cleaner knowledge
object once the governance shape became stable.

## 4. Stage 1: Typed Drawers as the Bootstrap Architecture

Before the mind-model work, a drawer was essentially the durable memory unit.
Stage 1 did not replace that system. It made the drawer model typed.

The core distinction became:

- evidence drawer
- knowledge drawer

Both share base drawer fields, but they carry different metadata and different
runtime meaning.

An evidence drawer records source-backed material. It includes fields such as
`memory_kind=evidence`, `domain`, `field`, `provenance`, `anchor_kind`, and
`anchor_id`. It does not carry `tier`, `status`, `statement`, or lifecycle refs.

A knowledge drawer records a distilled belief. It includes
`memory_kind=knowledge`, `statement`, `tier`, `status`, role-separated evidence
refs, scope constraints, trigger hints, and anchor metadata.

The `statement` field is important. It is the short wake-up proposition. The
longer `content` field explains rationale, examples, and boundaries. Runtime
context should be able to wake a concise claim without dumping the entire
explanation every time.

Stage 1 also introduced role-separated refs:

- `supporting_refs`
- `verification_refs`
- `counterexample_refs`
- `teaching_refs`

This is not just cleaner metadata. It is what lets promotion gates understand
why a claim is believed. A supporting reference and a counterexample reference
should not be collapsed into the same anonymous evidence list.

The minimum lifecycle surfaces were then added:

- `mempal knowledge distill`
- `mempal knowledge gate`
- `mempal knowledge promote`
- `mempal knowledge demote`
- `mempal knowledge publish-anchor`

And equivalent MCP surfaces were added where runtime agents needed direct
access:

- `mempal_knowledge_distill`
- `mempal_knowledge_gate`
- `mempal_knowledge_promote`
- `mempal_knowledge_demote`
- `mempal_knowledge_publish_anchor`

The important constraint is that these operations do not let the caller simply
declare confidence. Promotion depends on deterministic policy. For example,
Stage 1 policy requires evidence counts, verification refs, and, for
`dao_tian`, human review.

This is how the system avoids the dangerous shortcut:

> "The agent found an interesting pattern, therefore the system learned a law."

That shortcut is explicitly forbidden.

## 5. Anchors: Worktree, Repo, Global

The mind model also needed to solve project identity.

The early temptation is to use `wing` as the project identity. That fails
because `wing` is semantic. It answers which area the memory belongs to, not
which checkout produced it.

The implemented anchor model separates:

- `global`
- `repo`
- `worktree`

This distinction is necessary for coding work.

A repo-only anchor is too broad. Branch experiments contaminate each other. A
failed experiment in one worktree can pollute reasoning in another.

A worktree-only anchor is too narrow. Stable project knowledge fragments across
checkouts, and every new worktree starts too empty.

The design therefore uses a dual-scope model:

- write local observations at `worktree`
- publish verified project knowledge outward to `repo`
- publish only cross-project principles to `global`

This creates two separate promotion movements:

- tier promotion, such as `qi -> shu` or `shu -> dao_ren`
- anchor publication, such as `worktree -> repo -> global`

Those operations are intentionally not the same. A workflow may be trusted
inside one worktree but not yet shareable across the repo. A repo-level pattern
may be stable for mempal but not universal enough to become global `dao_tian`.

At runtime, context assembly prefers:

1. current worktree
2. current repo
3. global

This ordering gives the agent branch-local context first, stable project memory
second, and universal law last. The tier order still applies inside that anchor
logic.

## 6. Runtime Context: Wake-Up Is Not Context Assembly

A major boundary in the implementation is the separation between wake-up and
mind-model context.

Wake-up remains an L0/L1 memory refresh surface. It can bring old material back
into attention, but it does not assemble typed operating guidance.

The typed operating guidance belongs to:

- `mempal context`
- `mempal_context`

Those surfaces assemble context in this order:

1. `dao_tian`
2. `dao_ren`
3. `shu`
4. `qi`
5. optional evidence

The order is deliberate.

`dao_tian` calibrates worldview. `dao_ren` calibrates the current field. `shu`
biases workflow and skill selection. `qi` binds concrete tools. Evidence grounds
exceptions and proofs.

The implementation keeps `dao_tian` sparse by default. `mempal context` and
`mempal_context` inject at most one `dao_tian` item unless the caller explicitly
raises the budget. This avoids drowning practical execution under abstract
principles.

The runtime skill guidance is also intentionally non-authoritative.
`trigger_hints` may bias workflow and tool choice, but they do not execute
skills and do not override system, user, repository, or client-native skill
rules.

This boundary is one of the most important practical decisions. If memory hints
could directly trigger tools or override explicit instructions, the system would
become an ungoverned policy engine. Instead, memory provides guidance, and the
agent still operates under the normal instruction hierarchy.

## 7. Phase 2: Knowledge Cards as Governed Beliefs

Typed drawers proved the model, but they are still a bootstrap architecture.
Drawers are the right shape for raw evidence. They are not the final shape for
governed beliefs.

Phase 2 introduced dedicated knowledge card storage in the same SQLite
`palace.db`.

The key tables are:

- `knowledge_cards`
- `knowledge_evidence_links`
- `knowledge_events`

This was an explicit storage decision. Cards should not live in an external
database by default. mempal's product invariant is a local, single-binary,
single-file memory palace. Keeping cards in the same SQLite database preserves
transactional lifecycle updates and avoids adding operational complexity before
the model proves it needs a separate store.

The separation is clean:

- drawers remain raw evidence and citation roots
- cards store distilled beliefs
- evidence links connect cards to source-backed drawers
- events record how beliefs evolved

A card contains statement, content, tier, status, domain, field, anchor
metadata, trigger hints, and timestamps.

An evidence link connects a card to an evidence drawer by role:

- supporting
- verification
- counterexample
- teaching

An event records lifecycle history:

- created
- promoted
- demoted
- retired
- linked
- unlinked
- updated
- published_anchor

This makes the Phase 2 model more explicit than Stage 1. A drawer can still hold
evidence. A card can now hold belief. The event stream can explain why that
belief changed.

The implemented surfaces include:

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

The MCP tool `mempal_knowledge_cards` exposes read and lifecycle actions,
including retrieval.

The important boundary is that Phase 2 cards are governed objects, but they are
not default runtime sources. They can be retrieved. They can be injected into
context with `--include-cards` or `include_cards=true`. But ordinary
`mempal_search` remains drawer-based, and default context remains drawer-only.

This is not an implementation gap. It is a trust boundary.

Default runtime context is a high-trust path. Switching from drawer-backed
knowledge to card-backed guidance should require runtime evidence that the
change improves behavior without damaging citations, lifecycle semantics, or
context budget.

## 8. Research and Evaluators: Evidence Producers, Not Authority

The design also fixes the role of external research and evaluator systems.

`research-rs` is an external tool. It is `qi`, not `dao`.

Its job is to gather and organize external material:

- raw sources
- wiki pages
- schema
- index
- logs
- lint reports

Its output can become evidence. It can produce structured summaries, candidate
insights, and contradiction signals. But it must not directly create
`dao_tian`, promoted knowledge, or canonical knowledge.

That boundary is now part of the implemented policy:

- research output enters as evidence
- candidate knowledge must be distilled from evidence refs
- promotion remains controlled by lifecycle gates
- contradictions become counterexamples or demotion evidence

Evaluators have a similar boundary.

An evaluator may recommend promotion or demotion. It may propose supporting
refs, verification refs, counterexample refs, and risk notes. It may explain why
a knowledge item appears ready or unsafe.

But it must not mutate lifecycle state directly.

Evaluator output is advisory-only. It cannot satisfy human reviewer
requirements. It cannot bypass deterministic gates. It cannot canonicalize
`dao_tian`.

This is the difference between assistance and authority. The system can use
evaluator advice, but it does not let the evaluator become the lifecycle actor.

## 9. Phase 3 So Far: Runtime Evidence Before Runtime Power

After P51, the baseline was closed. P52 defined Phase 3 as a new stage, not a
continuation of unfinished P12-P50 work. P53 audited candidate tracks and found
that no candidate should jump directly into default behavior changes.

The recommended first Phase 3 track was runtime adoption evidence.

That recommendation has now been implemented through P54-P59.

P54 adds schema v9 table `runtime_adoption_events`. This table records concrete
runtime signals:

- which track the signal belongs to
- which signal occurred
- which feature was involved
- optional query, context hash, card id, evaluator id, research report id
- note, metadata, timestamp

The supported tracks include runtime adoption, card context, card embedding,
evaluator, and research adapter work. The supported signals include used,
accepted, rejected, miss, rollback, contradiction, and neutral.

P55 exposes the table through CLI:

- `mempal phase3 adoption record`
- `mempal phase3 adoption list`
- `mempal phase3 adoption stats`

P56 adds a read-only gate for making card context default. The current policy is
still opt-in. The gate merely reports whether enough accepted `card_context`
evidence exists and whether rollback signals are absent.

P57 adds a read-only card embedding gate. It intentionally adds no card vector
schema. Card embeddings require repeated measured misses showing that
linked-evidence retrieval failed because the card statement itself needed to be
retrieved.

P58 adds a read-only evaluator API gate. It preserves the advisory-only
boundary. Evaluator signals do not mutate lifecycle state or bypass gates.

P59 adds `mempal phase3 research-validate-plan`. This validates an external
research report contract with `report_id`, `title`, `sources`, `findings`, and
optional `candidate_insights`. It validates and plans; it does not ingest
promoted or canonical knowledge.

The pattern is consistent:

> Before adding runtime power, collect runtime evidence.

That is the Phase 3 goal.

## 10. What Has Actually Been Built

The implemented system can now be summarized as a governed cognition stack.

Storage and retrieval:

- SQLite `palace.db` is the single local storage root.
- Drawers remain raw memory and evidence citation roots.
- BM25 plus vector retrieval remains the base search path.
- AAAK remains an output formatter, not a storage dependency.

Stage 1 mind model:

- evidence and knowledge drawers are typed
- `statement` is used for knowledge wake-up
- `dao_tian`, `dao_ren`, `shu`, and `qi` have explicit status rules
- anchors separate worktree, repo, and global scope
- lifecycle operations are explicit and evidence-backed

Runtime context:

- `mempal context` and `mempal_context` assemble typed guidance
- context order is `dao_tian -> dao_ren -> shu -> qi`
- evidence is opt-in
- card context is opt-in
- memory hints bias skill/tool choice but do not execute

Phase 2 cards:

- `knowledge_cards` store governed beliefs
- `knowledge_evidence_links` preserve role-separated citations
- `knowledge_events` provide append-only lifecycle audit
- cards can be backfilled from Stage 1 knowledge
- cards can be gated, promoted, demoted, and retrieved
- default search does not return cards

Policy boundaries:

- research output cannot directly create `dao`
- evaluator output is advisory-only
- `dao_tian` canonicalization requires human review
- card embeddings are deferred until evidence justifies them
- default card context is deferred until runtime evidence justifies it

Phase 3 baseline:

- runtime adoption events are stored in schema v9
- adoption evidence is inspectable through CLI
- readiness gates exist for card context default, card embeddings, evaluator
  APIs, and research adapter ingestion
- research report validation exists without automatic promotion

This is enough to enter a new phase because the system is no longer just a
design. It has implemented storage, lifecycle, runtime context, MCP surfaces,
card governance, and measurement gates.

## 11. What Is Still Deliberately Not Done

Several things remain intentionally unfinished.

Card-aware context is not default. It remains opt-in because default runtime
context is a trust boundary. The system needs repeated runtime traces showing
benefit before enabling cards by default.

Card embeddings do not exist. Current card retrieval is linked-evidence-first.
This preserves citation semantics and avoids stale card-vector maintenance. Card
embeddings require measured statement-match misses and a rollback plan.

Research adapters do not directly ingest promoted knowledge. They can validate
input plans and eventually write evidence or candidate insights, but promotion
must stay under memory lifecycle governance.

Evaluator APIs do not mutate knowledge state. They can advise, but gates and
human-review requirements remain authoritative.

The system also does not claim fully autonomous self-improvement yet. What it
has is a governed substrate for self-improvement:

- evidence capture
- candidate distillation
- gate evaluation
- reversible lifecycle
- runtime adoption measurement

Autonomy should be added only where evidence shows it improves behavior and
where rollback is explicit.

## 12. The Design Principle Going Forward

The next phase should be judged by one question:

> Does this change produce measurable runtime improvement without weakening
> evidence, citation, audit, or rollback boundaries?

If the answer is no, the change should remain advisory, opt-in, or deferred.

That is why the next likely work should expose Phase 3 runtime surfaces to MCP
and define an adoption-recording protocol. Agents need a direct way to record
whether a context item, card, evaluator suggestion, or research result was used,
accepted, rejected, missed, or rolled back.

Only after that evidence accumulates should the system consider stronger
defaults:

- card context default-on
- card embeddings
- evaluator APIs
- research adapter apply paths

The mind model therefore advances in a disciplined order:

1. define the knowledge hierarchy
2. separate evidence from knowledge
3. implement lifecycle governance
4. expose runtime context
5. extract governed cards
6. preserve citations and audit
7. measure runtime adoption
8. only then increase authority

That ordering is the main achievement of the current implementation.

It turns memory from passive recall into a governed learning loop.

It keeps `dao` in memory, `shu` in reusable methods, `qi` in tools, and evidence
as the substrate from which all durable belief must be justified.

And it gives the next stage a clear mission:

> Move from implemented cognition architecture to measured, reversible,
> evidence-driven agent evolution.

## Source Notes

This article is based on the repository design and implementation inventory:

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

It also cross-checks stored mempal evidence, including:

- `drawer_mempal_p31_cc86ab94`, `source_file=mempal-p31-knowledge-card-schema-spec.md`
- `drawer_mempal_p36_3770af58`, `source_file=mempal-p36-knowledge-card-backfill-report.md`
- `drawer_mempal_p18_b6dac6fe`, `source_file=mempal-p18-knowledge-distill-memory.md`
- `drawer_mempal_p30_7451b74c`, `source_file=mempal-p30-knowledge-card-storage-boundary.md`
- `drawer_mempal_p53_82a98e1f`, `source_file=mempal-p53-phase-3-candidate-evidence-audit.md`
- `drawer_mempal_p54_p59_866027ab`, `source_file=mempal-p54-p59-phase3-runtime.md`

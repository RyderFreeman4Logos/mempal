# Architecture Overview

## System Overview

mempal is a Rust, single-binary, SQLite-first project memory system for coding agents. The installed `mempal` binary owns the CLI, stdio MCP server, optional daemon REST API, and background workers; the database remains a local SQLite file with WAL, FTS5, vector tables, raw drawer storage, queue state, knowledge graph records, and audit metadata. Current behavior is source-of-truth here; older specs and plans describe design history unless this document, README, or current code confirms the behavior.

## Module Map

### Entry Points

| Path | Role |
| --- | --- |
| `src/main.rs` | CLI entry point, clap command definitions, command dispatch, daemon management, and one-shot command wiring. |
| `src/lib.rs` | Library facade that exposes the reusable modules used by CLI, MCP, daemon, tests, and optional REST builds. |

### Primary Directories

| Path | Role |
| --- | --- |
| `src/core/` | Core data model and storage layer: SQLite schema/migrations, WAL-backed database access, config, project scope, queue, taxonomy, triples, timeline, reindex, decay, skills, patterns, and fork-extension metadata. |
| `src/embed/` | Embedding abstraction and backends: OpenAI-compatible endpoint routing, model2vec, ONNX, retry policy, endpoint health, factory selection, and embedder status snapshots. |
| `src/ingest/` | Ingestion pipeline: format detection, parsing, normalization, noise stripping, chunking, privacy/gating hooks, novelty evaluation, diary rollup, conversation ingestion, reindex helpers, and source locks. |
| `src/search/` | Retrieval engine: BM25/FTS5, vector search, reciprocal-rank fusion, query routing, project/tunnel filtering, tiered retrieval, previews, and optional reranking. |
| `src/aaak/` | AAAK output formatter and parser: compact memory dialect models, codec, BNF/spec text, signals, and round-trip validation support. |
| `src/mcp/` | MCP server surface: tool schemas, server implementation, protocol instructions, timeline helpers, runtime diagnostics, and the smoke-tested 19-tool baseline documented in README/usage. |
| `src/api/` | Optional REST API behind the `rest` feature, including daemon state, handlers, and Hermes-compatible routes. |
| `src/cli/` | CLI command helpers that are split out of `main.rs` when command logic needs a smaller module. |
| `src/llm/` | Local/OpenAI-compatible LLM client pool, routing, retry/status tracking, and worker tasks used by optional gating and intelligence features. |
| `src/cowork/` | Multi-agent cowork primitives: Claude/Codex live-session peek, inbox push, concrete-agent bus, channels, sessions, handoff, delivery status, and tmux transport helpers. |
| `src/factcheck/` | Deterministic pre-ingest contradiction guard over names, relations, KG triples, stale facts, and repeated-failure repair signals. |
| `src/algo/` | Pure algorithms, currently including ranking utilities kept independent from storage and IO. |
| `src/observability/` | Runtime telemetry and status snapshots for operations, vector scan mode, ingest worker backoff, resource usage, and endpoint health. |
| `src/hotpatch/` | CLAUDE.md hotpatch suggestion generation and management; suggestions are gated and operator-applied. |
| `src/xurl/` | Conversation transcript indexing/search/timeline pipeline for agent session logs, separate from the standalone `xurl` project. |
| `src/integrations/` | External tool integration adapters for Claude Code, Codex, CSA, and related transcript or hook sources. |

### Top-Level Modules

| Path | Role |
| --- | --- |
| `src/daemon.rs` | Long-lived daemon loop: singleton startup, REST listener, hook IPC, queue claiming, embedding, gating, novelty, and drawer writes. |
| `src/daemon_bootstrap.rs` | Daemon bootstrap context: config, runtime, logging, database handles, singleton lock, and startup validation. |
| `src/daemon_singleton.rs` | Single-daemon ownership and diagnostics for PID/lock coordination. |
| `src/daemon_status.rs` | Daemon status files and embedder runtime status reporting. |
| `src/context.rs` | Tiered context assembly for dao_tian/dao_ren/shu/qi guidance, evidence, cards, patterns, skills, and distill suggestions. |
| `src/brief.rs` | Cognitive brief generation built on context assembly, producing concise facts, evidence, uncertainty, and next actions. |
| `src/hook.rs`, `src/hook_*.rs` | Passive capture hook parsing, installation, diagnostics, IPC, and envelope handling. |
| `src/knowledge_*.rs` | Knowledge lifecycle surfaces: anchors, gates, distillation, cards, backfill, retrieval, promotion/demotion, and publication metadata. |
| `src/importance.rs` | Importance scoring and decay logic used by wake-up, timeline, pruning, and context prioritization. |
| `src/patterns.rs` | CLI-facing pattern listing, promotion, and retirement over the core pattern store. |
| `src/skills.rs` | CLI-facing skill listing, proposal, promotion, adoption, rejection, and retirement over the core skill store. |
| `src/repair.rs` | Deterministic anti-pattern detection and repair warning packages for repeated failures. |
| `src/sleep.rs` | Offline maintenance cycle: NREM pruning/compaction, REM contradiction checks, and salience updates. |
| `src/reflect.rs` | Deterministic reflection findings derived from stored evidence without remote LLM dependence. |
| `src/crystallize.rs` | Knowledge card candidate generation and deterministic crystallization support. |
| `src/wiki.rs` | Source-backed knowledge wiki generation and verification. |
| `src/markdown_export.rs` | Markdown export of memory surfaces for review or external reading. |
| `src/doctor.rs` | Health diagnostics for install, schema, daemon, MCP, REST, and runtime environment checks. |
| `src/bench_matrix.rs`, `src/longmemeval.rs` | Benchmark harnesses and LongMemEval support for local evaluation. |

## Data Flow

1. **Ingest**: CLI, hooks, MCP, REST/Hermes, or transcript indexers submit raw content. The ingest layer detects format, normalizes text, strips known transcript noise, chunks content, applies privacy/gating/novelty rules where configured, and preserves the original drawer content as raw evidence.
2. **Embed**: The selected embedder turns chunks or queries into vectors. The default provider family is OpenAI-compatible local/LAN endpoints, with explicit model2vec and ONNX paths available by configuration/features. Retry, circuit status, and endpoint health are observable.
3. **Store**: `core` writes drawers, chunks, vectors, FTS rows, queue receipts, KG triples, lifecycle events, and audit metadata to SQLite. Drawer text remains verbatim; derived indices and cards live in separate tables.
4. **Search**: Queries combine BM25 and vector candidates through RRF, apply project/wing/room/tunnel filters, and return cited results with drawer IDs, source files, structured signals, preview metadata, and warnings when fallback modes are active.
5. **Context Assembly**: `context` and `brief` turn retrieval results into agent-facing guidance: wake-up/status surfaces, typed dao/shu/qi context, linked evidence, knowledge cards, repair warnings, skill/pattern hints, and compact cognitive briefs.

## Key Surfaces

- **CLI**: `src/main.rs` exposes the broad operator surface: init, ingest, search, context, brief, timeline, status, daemon, doctor, reindex, KG, tunnels, taxonomy, cards, lifecycle, phase3, cowork, xurl, hook, sleep, repair, patterns, skills, benchmarks, and export/wiki workflows. The current CLI has 80+ subcommand paths across these families.
- **MCP**: `mempal serve --mcp` exposes the smoke-tested baseline of 19 documented MCP tools grouped below as conceptual profiles for agent discovery. Some builds or newer code paths may register additional governed or diagnostic tools; treat README/usage as the baseline compatibility contract.
- **REST**: With the `rest` feature and daemon configuration enabled, the daemon serves loopback REST/Hermes-compatible endpoints from `src/api/`. REST search and writes run inside the daemon process and share daemon-owned database/embedder state.

Compatibility expectations for these entry points are defined in the next section.

### Cowork Semantics

Cowork has three separate verbs. They must not be treated as interchangeable:

| Verb | Surface | Storage contract | Use when |
| --- | --- | --- | --- |
| READ | `mempal_peek_partner`, `mempal cowork-tmux-peek` | Reads live partner session output. Nothing is written to the mempal drawer database. | An agent needs current partner context. |
| SEND | `mempal_cowork_push`; `mempal_cowork_bus action=send\|broadcast\|channel_send`; CLI `mempal cowork-send`, `mempal cowork-broadcast`, `mempal cowork-channel-send` | Best-effort ephemeral delivery through a bounded inbox file or tmux transport. Transport metadata and bus events are coordination state, not durable memory. The target agent must drain or otherwise read the message. | A partner should notice transient status, a blocker, or a handoff on its next turn. |
| PERSIST | `mempal ingest`, `mempal_ingest`, and explicit cowork handoff capture | Writes a normal drawer to SQLite, with the same fact-check, importance, decay, search, and citation rules as other memories. | The information must survive sessions or be searchable later. |

READ means "read live partner output", not "read the memory database"; database reads use `mempal search`, `mempal context`, `mempal brief`, or drawer-read tools. SEND messages are not queued for database ingestion or durable replay, and SEND is not a substitute for PERSIST. If a partner must remember something permanently, ingest it or run explicit handoff capture instead of only sending it.

### MCP Tool Profiles

MCP profiles are documentation-only groupings. They do not change tool
registration, permissions, compatibility, or runtime behavior.

#### Default Agent

Use these tools for the normal agent recall, write, and context loop.

- `mempal_status` -- system health, warnings, protocol discovery, and runtime status.
- `mempal_search` -- hybrid BM25/vector search with citations and preview metadata.
- `mempal_ingest` -- store raw memories with optional importance and dry-run support.
- `mempal_operation_status` -- poll receipt-backed asynchronous ingest operations.
- `mempal_delete` -- soft-delete drawers with audit metadata.
- `mempal_context` -- assemble tiered dao/shu/qi guidance and optional evidence.
- `mempal_brief` -- produce a citation-first cognitive brief for a query.
- `mempal_pinned_facts` -- read canonical pinned facts without vector lookup.
- `mempal_read_drawer` -- fetch one full raw drawer after a truncated search preview.
- `mempal_read_drawers` -- fetch multiple full raw drawers by drawer ID.

#### Knowledge Management

Use these tools when curating typed memory, graph facts, and cross-scope links.

- `mempal_kg` -- add, query, invalidate, timeline, or summarize knowledge graph triples.
- `mempal_taxonomy` -- inspect or edit wing/room routing keywords.
- `mempal_field_taxonomy` -- read recommended typed-memory `field` values.
- `mempal_tunnels` -- discover and manage cross-scope memory links.
- `mempal_timeline` -- inspect recent or important memory by project scope.
- `mempal_knowledge_distill` -- create candidate dao_ren/qi knowledge from evidence refs.
- `mempal_fact_check` -- detect offline name, relation, and stale-fact contradictions.

#### Workspace And Skills

Use these tools for local environment and agent-skill support.

- `mempal_skill` -- inspect skill and runtime guidance helpers for agents.
- `mempal_doctor` -- run install, schema, daemon, MCP, and runtime diagnostics.

## Product Surface Classification

This classification describes compatibility expectations for current product surfaces. CLI and MCP names are listed together when they expose the same capability. REST endpoints inherit the classification of the underlying store/search/ingest/status capability; an experimental module does not become stable solely because it is reachable through HTTP.

### Stable

Stable surfaces are the core product contract. Command names, MCP tool names, raw-storage semantics, citations, and operation receipts should remain compatible across normal 0.x updates unless a migration note calls out a change.

| Surface | Compatibility expectation |
| --- | --- |
| `mempal serve --mcp` | The stdio MCP server remains the stable protocol entry point; individual tools follow the classification in this table. |
| `mempal status` / `mempal_status` | System status, warnings, schema/config/runtime diagnostics, and protocol discovery remain a stable entry point. |
| `mempal search` / `mempal_search` | Hybrid retrieval with citations, drawer IDs, source files, project scoping, and fallback warnings remains stable. |
| `mempal_read_drawer` / `mempal_read_drawers` | Full raw drawer reads remain stable companion tools for search preview expansion. |
| `mempal ingest` / `mempal_ingest` | Memory storage preserves raw drawer content and returns operation receipts or drawer IDs. |
| `mempal operation status` / `mempal_operation_status` | Receipt-backed async ingest polling remains the stable way to observe queued writes. |
| `mempal delete` / `mempal_delete` | Soft-delete with audit trail remains the stable deletion contract. |
| `mempal pinned` / `mempal_pinned_facts` | Canonical pinned fact reads remain available without embedding lookup. |
| `mempal wake-up` | Importance-ordered context refresh remains a stable CLI convenience surface. |
| `mempal context` / `mempal_context` | Tiered dao/shu/qi context assembly remains the stable agent guidance surface. |
| `mempal brief` / `mempal_brief` | Citation-first cognitive briefs remain the stable compact human/agent summary surface. |
| `mempal doctor` / `mempal_doctor` | Health diagnostics for install, schema, daemon, MCP, REST, and runtime environment remain stable. |
| Local SQLite storage and embedding routing | Raw drawers stay in SQLite; embedding backend routing is configurable and observable. |
| REST core operations | REST handlers that delegate to stable status/search/ingest/delete-style operations follow the same compatibility expectations when the `rest` feature is enabled. |

### Advanced

Advanced surfaces are supported and useful, but they expose governance, operator policy, or specialized data models. Expect additive fields, policy tightening, and more setup-specific behavior than the stable core.

| Surface | Notes |
| --- | --- |
| `mempal kg` / `mempal_kg` | Knowledge graph triples and temporal facts. |
| `mempal fact-check` / `mempal_fact_check` | Deterministic contradiction detection over KG triples and known entities. |
| `mempal taxonomy` / `mempal_taxonomy` | Routing keyword management. |
| `mempal field-taxonomy` / `mempal_field_taxonomy` | Recommended typed memory fields. |
| `mempal tunnels` / `mempal_tunnels` | Cross-scope links and tunnel hints. |
| `mempal timeline` / `mempal_timeline` | Recent/important memory timelines. |
| `mempal skill` / `mempal_skill` | Skill and runtime guidance helpers. |
| `mempal knowledge distill` / `mempal_knowledge_distill` | Governed knowledge lifecycle candidate creation; promotion remains gate-controlled. |
| LLM gating | Tier 1, Tier 2, and optional local/OpenAI-compatible LLM classifier paths are configuration-sensitive. |
| `mempal bench longmemeval` | Benchmark harness for local retrieval evaluation. |
| `mempal xurl` | Conversation transcript indexing, search, timeline, stats, reindex, and backfill. |

#### Fact-Check Contract

`mempal fact-check` and `mempal_fact_check` are deterministic pre-ingest guards for draft memories that assert named-entity relationships or dated KG facts. Agents should run fact-check before committing those drafts with `mempal ingest` or `mempal_ingest`.

What it does: extract bounded patterns from the candidate text, compare them with known entities and KG triples, and report `SimilarNameConflict`, `RelationContradiction`, or `StaleFact` issues. A reported issue is advisory: pause, inspect the evidence, and use human or agent judgment before deciding whether to rewrite, reject, or ingest the draft.

What it does not do: prove truth, call an LLM, use the network, resolve ambiguous claims, or detect semantic contradictions outside the supported patterns. A clean report means only that no supported deterministic pattern matched a known conflict.

### Experimental

Experimental surfaces are implemented but not compatibility commitments. They may change command names, MCP/REST payloads, defaults, storage layout, or required operator workflow before 1.0. Automation should pin the mempal version and review the changelog before depending on them.

| Surface | Experimental scope |
| --- | --- |
| Hermes plugin intelligence layer (`src/intelligence.rs`) | Plugin intelligence and Hermes-adjacent behavior. |
| Cowork bus (`src/cowork/`, all `mempal cowork-*` commands) | Multi-agent bus, channel, session, handoff, delivery, and tmux transport workflows. |
| Hotpatch (`src/hotpatch/`, `mempal hotpatch`) | CLAUDE.md suggestion generation and operator-applied patches. |
| Sleep/Reflect/Repair/Crystallization | `mempal sleep`, `mempal reflect`, `mempal repair`, and `mempal crystallize` maintenance workflows. |
| Maintenance rejudge artifact mode | `mempal maintenance rejudge --candidates-file` artifact-driven cleanup. |
| Session self-review | `mempal patterns` and `SessionEnd` hook self-review extraction. |
| `mempal phase3` | Runtime adoption analytics and default-readiness evidence. |
| `mempal insight` | Design insight recording and drain workflow. |
| `mempal foresight` | Future-bound memory signals. |
| `mempal case` | Case/procedural memory surface. |

## Historical Context

The `specs/` and `docs/plans/` trees contain upstream and fork-extension design contracts, implementation plans, and milestone records. They are valuable for intent, rationale, and acceptance scenarios, but they are historical references rather than a current module map. When a spec conflicts with current `src/`, README, usage guide, or this document, prefer the current implementation for operational architecture and use the spec as background.

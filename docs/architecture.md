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
| `src/factcheck/` | Deterministic contradiction detection over names, relations, KG triples, stale facts, and repeated-failure repair signals. |
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
- **MCP**: `mempal serve --mcp` exposes the smoke-tested baseline of 19 documented MCP tools for status, search, timeline, drawer reads, context, ingest, delete, taxonomy, field taxonomy, KG, tunnels, cowork, fact-check, doctor, brief, and governed knowledge operations. Some builds or newer code paths may register additional governed or diagnostic tools; treat README/usage as the baseline compatibility contract.
- **REST**: With the `rest` feature and daemon configuration enabled, the daemon serves loopback REST/Hermes-compatible endpoints from `src/api/`. REST search and writes run inside the daemon process and share daemon-owned database/embedder state.

## Stable vs Experimental

Stable current behavior includes the single-package install path, raw SQLite drawer storage, cited hybrid search, CLI-first operation, MCP baseline tools, daemon queue processing, deterministic fact checking, knowledge lifecycle gates, and local-first embedding/LLM configuration. More experimental or operator-gated areas include hotpatch suggestions, phase3 runtime adoption evidence, auto-generated knowledge cards, LLM-assisted gating, advanced cowork bus transports, xurl transcript recall, and sleep-cycle maintenance. Those areas are implemented in code, but their policies and defaults may evolve faster than the core ingest/search/store contract.

## Historical Context

The `specs/` and `docs/plans/` trees contain upstream and fork-extension design contracts, implementation plans, and milestone records. They are valuable for intent, rationale, and acceptance scenarios, but they are historical references rather than a current module map. When a spec conflicts with current `src/`, README, usage guide, or this document, prefer the current implementation for operational architecture and use the spec as background.

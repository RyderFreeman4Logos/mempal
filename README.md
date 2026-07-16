# mempal

Project memory for coding agents. Single binary, `cargo install mempal`, find past decisions with citations in seconds. Local-first storage by default: SQLite with no hidden model2vec download and no cloud LLM, embedding, or rerank calls unless you configure them.

## What It Does

```
Agent writes code → commits → mempal saves the decision context
Next session (any agent) → mempal search → finds the decision with source citation
```

- **Hybrid search**: BM25 keyword matching + vector semantic search, merged via Reciprocal Rank Fusion
- **Knowledge graph**: subject-predicate-object triples with temporal validity (valid_from/valid_to)
- **Cross-project tunnels**: automatic discovery when the same room appears in multiple wings
- **Self-describing protocol**: MEMORY_PROTOCOL embedded in MCP ServerInfo teaches any agent how to use mempal — no system prompt configuration required
- **Configurable embeddings**: prefer an explicit OpenAI-compatible local/LAN provider for production; model2vec is an opt-in feature/backend
- **Single file**: everything lives in `~/.mempal/palace.db` (SQLite + sqlite-vec)
- **No cloud by default**: remote embeddings, LLM gating, and rerankers are disabled until configured explicitly

## Quick Start

> **Caution: `cargo install --git` from a fork is unreliable across schema migrations.**
> `cargo install --git <fork-url> --branch main --force mempal` may report success while actually
> skipping the rebuild (cargo's source cache returns a stale ref despite `--force`, which only
> forces *installation* not *re-fetch*). After a `CURRENT_SCHEMA_VERSION` bump in `src/core/db.rs`,
> the resulting binary will fail with a schema mismatch error that tells you to update the mempal
> binary and, for MCP servers, verify the MCP client command/path configuration.
> See [#76](https://github.com/RyderFreeman4Logos/mempal/issues/76).
>
> For fork builds, prefer the root `--path` route below (clones a fresh checkout, builds locally).
> A one-liner is provided at [`scripts/install-from-source.sh`](scripts/install-from-source.sh).

Released crate:

```bash
cargo install mempal
```

Current repository checkout:

```bash
cargo install --path . --locked

mempal init ~/code/myapp
mempal ingest ~/code/myapp --wing myapp
mempal search "auth decision clerk"
mempal wake-up
```

With REST support:

```bash
cargo install --path . --locked --features rest
```

## Configuration

Config at `~/.mempal/config.toml`:

With no config file, mempal uses the local SQLite database at `~/.mempal/palace.db` and does not silently download or load model2vec. Configure an embedding endpoint for ingest/search, or explicitly enable `backend = "model2vec"` with the `model2vec` Cargo feature for local static models.

```toml
db_path = "~/.mempal/palace.db"

[embed]
backend = "openai_compat"                      # default provider family

# Optional: keep long-lived daemon RSS lower than in-process local models.
# Requires [embed.openai_compat] or [[embed.endpoints]] below.
[daemon]
embedder_mode = "remote"                       # configured | remote | small_local

[embed.openai_compat]
base_url = "http://127.0.0.1:18002/v1"         # local/LAN embedding endpoint
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096

[search.reranker]
enabled = false                                # default: no reranker call
# endpoint = "http://gb10:18003/v1/rerank"     # local/LAN reranker endpoint
# model = "qwen3-reranker"
# timeout_secs = 2
# top_k = 20

[privacy.remote_calls]
fail_closed = false                            # set true to block external endpoints unless allowed below
allow_embedding = false
allow_llm = false
allow_rerank = false
```

Run `mempal cost status` to see whether embedding, LLM, or rerank paths are disabled, local/private, or configured for a redacted external endpoint.

Optional LLM gating can be configured with multiple OpenAI-compatible endpoints. Lower
`priority` values are tried first; equal values share capacity. Set Spark's one
`priority` line to `0` for equal priority with Qwen, or keep it higher to use
Spark only when Qwen is unavailable/saturated:

```toml
[llm]
enabled = true
backend = "openai_compat"
enabled_for = ["gating"]
request_timeout_secs = 3000

[[llm.endpoints]]
id = "qwen"
base_url = "http://gb10:18009/v1"
model = "qwen3.6-27b-decensor-by-aeon"
priority = 0

[[llm.endpoints]]
id = "spark"
base_url = "http://localhost:8317/v1"
model = "spark"
priority = 10 # change only this line to 0 for equal priority
api_key_env = "SPARK_API_KEY"
```

When LLM gating is configured, endpoint outages are surfaced by `mempal status`;
historical cleanup leaves LLM-gated work pending for retry instead of silently
downgrading quality.

For high-volume historical cleanup, Qwen can propose candidates and Spark can
confirm each proposed candidate immediately before a reversible soft-delete:

```bash
mempal maintenance rejudge --all --execute \
  --backup-dir /absolute/path/to/rejudge-backups \
  --progress-file /absolute/path/to/rejudge-progress.jsonl \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

If an endpoint is unavailable, rerun with `--resume`; `mempal status` shows the
`waiting_llm` checkpoint and pending aggregate counts.

When Spark confirmation quota is exhausted for a long window, keep Qwen
proposal capacity busy without holding the old paired runner in memory:

```bash
mempal maintenance rejudge --all --execute \
  --proposal-only \
  --progress-file /absolute/path/to/rejudge-progress.jsonl \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

Later, drain only the persisted Spark confirmation backlog; Qwen is not called
again for rows already marked `confirm_pending`:

```bash
mempal maintenance rejudge --all --resume --execute \
  --confirm-pending-only \
  --backup-dir /absolute/path/to/rejudge-backups \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

Other backends:

```toml
# Local ONNX (requires --features onnx)
[embed]
backend = "onnx"

# Opt-in external API
[embed]
backend = "api"
api_endpoint = "http://localhost:11434/api/embeddings"
api_model = "nomic-embed-text"
```

The `onnx` feature dynamically loads ONNX Runtime 1.24.2. Install the matching
shared library and set `ORT_DYLIB_PATH` to its full path: `libonnxruntime.so`
on Linux, `libonnxruntime.dylib` on macOS, or `onnxruntime.dll` on Windows.
Model assets may download on first use; the ONNX Runtime library does not.

Daemon low-memory mode:

- `[daemon].embedder_mode = "configured"` uses the normal `[embed]` backend.
- `"remote"` forces daemon workers and daemon REST embedding to use the configured OpenAI-compatible/API endpoint and disables daemon fallback to local model2vec.
- `"small_local"` forces the daemon to use `minishlab/potion-base-8M` instead of a larger explicitly configured in-process model; it requires installing with `--features model2vec`.
- After changing backend/model/dimensions, run `mempal reindex`, then `mempal daemon restart`.
- On Linux, after starting or restarting a service, use `mempal daemon wait --timeout-secs 10` before sending writes; it verifies the singleton, current executable, and write IPC transport.
- `mempal daemon status` and `mempal doctor` report daemon RSS/PSS, whether the daemon executable is deleted/replaced, and whether the daemon embedder cache is loaded.
- For 24/7 operation, `[sleep] auto_interval_secs = 86400` enables daemon-owned consolidation; `0` keeps it disabled, and `phases` selects `nrem`, `rem`, and/or `salience`.

## Commands

This is the main user-facing command surface, not an exhaustive list of every
maintenance subcommand. Use `mempal --help` and nested `--help` output for the
full tree.

| Command | Purpose |
|---------|---------|
| `mempal init <DIR> [--dry-run]` | Infer wing/rooms from project tree |
| `mempal ingest <DIR> --wing <W> [--dry-run]` | Chunk, embed, and store |
| `mempal search <QUERY> [--wing W] [--room R] [--json]` | Hybrid search (BM25 + vector + RRF) |
| `mempal brief <QUERY>` | Citation-first cognitive brief with facts, evidence, uncertainty, and next actions |
| `mempal context <QUERY> [--format json]` | Typed runtime guidance (`dao_tian` -> `dao_ren` -> `shu` -> `qi`) |
| `mempal timeline [--wing W] [--since S] [--format F] [--raw]` | Project-scoped recent/important memory digest |
| `mempal pinned [--project P] [--reorder ...] [--json]` | Read canonical pinned facts without embedding lookup |
| `mempal field-taxonomy [--format json]` | Inspect recommended `field` values for typed memory |
| `mempal wake-up [--format aaak]` | Context refresh, sorted by importance |
| `mempal compress <TEXT>` | AAAK format output |
| `mempal delete <DRAWER_ID>` | Soft-delete a drawer |
| `mempal purge [--before TIMESTAMP]` | Permanently remove soft-deleted drawers |
| `mempal kg add <S> <P> <O>` | Add a knowledge graph triple |
| `mempal kg query [--subject S] [--predicate P]` | Query triples |
| `mempal kg timeline <ENTITY>` | Chronological view of an entity |
| `mempal kg stats` | Knowledge graph statistics |
| `mempal tunnels` | Cross-wing room links |
| `mempal taxonomy list / edit` | Manage routing keywords |
| `mempal knowledge distill/gate/policy` | Governed knowledge lifecycle checks and candidate creation |
| `mempal reindex` | Re-embed all drawers after model change |
| `mempal status` | DB stats, schema version, scopes |
| `mempal doctor` | Install, schema, runtime, and MCP diagnostics |
| `mempal operation status <OPERATION_ID>` | Poll receipt-backed async ingest work |
| `mempal skill ...` | Inspect skill/runtime guidance helpers |
| `mempal serve [--mcp]` | MCP server (+ REST with feature) |
| `mempal cowork-install-hooks [--global-codex]` | Install UserPromptSubmit hooks for Claude Code (+ optional Codex merge) |
| `mempal cowork-drain --target <claude\|codex>` | Drain inbox messages (for hook use; exits 0 on any failure) |
| `mempal cowork-status --cwd <PATH>` | Read-only view of both inboxes at `<PATH>` |
| `mempal fact-check [PATH\|-] [--wing W] [--room R] [--now <UNIX_SECS>]` | Offline contradiction check against KG triples + known entities |
| `mempal hermes ingest/search/recall` | Cwd-scoped semantic recall over Hermes Agent sessions |
| `mempal bench longmemeval <FILE>` | LongMemEval retrieval benchmark |

> **Heads-up — two different `xurl`s.** `mempal xurl` is a mempal subcommand: an `ingest` / `search` / `timeline` / `stats` / `reindex` / `backfill` pipeline over agent session transcripts. It is **not** the standalone `xurl` CLI (Xuanwo's "Resolve and read code-agent threads", invoked as `xurl …` with `agents://` URIs). The two are independent projects that happen to share a name — neither is an alias for the other.

### Hermes Session Recall

Hermes transcripts are indexed through the xurl conversation store but exposed as first-class commands:

```bash
mempal hermes ingest --profile default --cwd /path/to/repo
mempal hermes ingest --profile work --export-jsonl hermes-session.jsonl
mempal hermes search --cwd . --query "mktd Step 7 failure recovery"
mempal recall hermes --session-id <hermes-session-id> --query "review verdict PASS"
mempal recall hermes --cwd . --latest --query "what is the current issue queue?"
```

Default profile reads `~/.hermes/state.db`; named profiles read `~/.hermes/profiles/<profile>/state.db` unless `--db` or `--export-jsonl` is supplied. Searches default to the process cwd, preserve Hermes profile boundaries, and return `profile/session/message` citations so exact pages can be fetched later through Hermes or xurl.

## MCP Server (19 verified baseline tools)

`mempal serve --mcp` exposes at least this smoke-tested MCP baseline via Model
Context Protocol. Use `mempal doctor` or protocol-level `tools/list` against
`mempal serve --mcp` for the runtime-advertised surface in a specific build:

| Tool | Purpose |
|------|---------|
| `mempal_status` | State + protocol + AAAK spec (teaches agent on first call) |
| `mempal_search` | Hybrid search with tunnel hints, citations, and AAAK-derived structured signals |
| `mempal_brief` | Citation-first cognitive brief with facts, evidence, uncertainty, and next actions |
| `mempal_context` | Typed runtime context assembler (`dao_tian` -> `dao_ren` -> `shu` -> `qi`) |
| `mempal_timeline` | Project-scoped memory overview ordered by importance and recency |
| `mempal_pinned_facts` | Read canonical pinned facts without vector lookup |
| `mempal_field_taxonomy` | Recommended `field` values for typed memory |
| `mempal_ingest` | Store memories with optional importance (0-5), dry_run, and receipt-based `wait` / `wait_timeout_secs`; reports `lock_wait_ms` when concurrent ingest was observed and can be polled via `mempal_operation_status` |
| `mempal_operation_status` | Poll a receipt-backed ingest op; returns drawer_id / rejection / failure and timing breakdowns when available |
| `mempal_read_drawer` | Fetch one full raw drawer after a truncated search preview |
| `mempal_read_drawers` | Fetch multiple full raw drawers after truncated search previews |
| `mempal_delete` | Soft-delete with audit trail |
| `mempal_taxonomy` | List or edit routing keywords |
| `mempal_kg` | Knowledge graph: add/query/invalidate/timeline/stats |
| `mempal_tunnels` | Cross-wing room discovery |
| `mempal_doctor` | Release, install, schema, and MCP runtime diagnostics |
| `mempal_skill` | Skill/runtime guidance helper for agents |
| `mempal_knowledge_distill` | Create candidate `dao_ren` / `qi` knowledge from existing evidence refs |
| `mempal_fact_check` | Offline contradiction detection vs KG triples + known entities (similar-name, relation mismatch, stale facts) |

The server embeds MEMORY_PROTOCOL (11 behavioral rules) in the MCP `initialize.instructions` field. Any MCP client learns the workflow automatically.

## Write Guidance

- Passive or low-risk diary writes: use `mempal_ingest` with the default fire-and-forget receipt path, or `mempal ingest --stdin` without `--wait`.
- Load-bearing decisions: use `mempal_ingest` with `wait=true` or CLI `mempal ingest --stdin --wait [--wait-timeout-secs N]` so you block until the write is terminal.
- User-facing capture: always surface rejected or failed reasons instead of only returning a queued receipt.
- Receipt polling: use `mempal_operation_status` or CLI `mempal operation status <operation_id>`; both include `timings` when the ingest has completed.

## Memory Protocol

mempal teaches agents these rules through self-description:

0. **FIRST-TIME SETUP** — call `mempal_status` to discover wings before filtering
1. **WAKE UP** — different clients have different pre-load mechanisms
2. **VERIFY BEFORE ASSERTING** — search before stating project facts
3. **QUERY WHEN UNCERTAIN** — search on "why did we...", "last time we..."
3a. **TRANSLATE TO ENGLISH** — translate non-English queries before searching
4. **SAVE AFTER DECISIONS** — persist rationale, not just outcomes
5. **CITE EVERYTHING** — reference drawer_id and source_file
5a. **KEEP A DIARY** — record behavioral observations in wing="agent-diary"
8. **PARTNER AWARENESS** — use `mempal_peek_partner` for live partner-agent session, not crystallized drawers
9. **DECISION CAPTURE** — `mempal_ingest` is for firm decisions only; include partner input when peek informed the call
10. **COWORK PUSH** — use `mempal_cowork_push` as the SEND primitive in the SEND/READ/PERSIST triad; at-next-submit delivery, not real-time
11. **VERIFY BEFORE INGEST** — call `mempal_fact_check` before persisting a decision that asserts entity relationships; it catches similar-name typos, relation mismatches against the KG, and stale facts with expired `valid_to`

## Agent Cowork (P6 peek + P8 push)

Two coding agents (Claude Code and Codex) can collaborate on the same repo through a per-project inbox + hook-driven injection channel, on top of `mempal_peek_partner` (read live partner session) and `mempal_cowork_push` (send ephemeral handoff).

Install hooks once per repo (run at the repo root):

```bash
mempal cowork-install-hooks --global-codex
```

This writes:

- `.claude/hooks/user-prompt-submit.sh` + merges a registration entry into `.claude/settings.json` so Claude Code fires the hook on every user prompt.
- `~/.codex/hooks.json` UserPromptSubmit entry so Codex fires the same drain on every user prompt.

The `--global-codex` part is optional. The re-run is idempotent and self-heals stale/wrong drain entries — re-installing after a mempal upgrade is always safe.

Delivery is **at-next-UserPromptSubmit**, not real-time: a push from Claude to Codex becomes visible only when the Codex user submits their next prompt, at which point the hook drains the inbox and prepends the message as `additionalContext` on that turn.

Check inbox state at any time without draining:

```bash
mempal cowork-status --cwd "$PWD"
```

### Known limitations

- **Codex feature flag dependency**: Codex's hooks runtime is gated behind the `codex_hooks` feature flag (currently "under development" in shipped `codex-cli`). If the flag is off, Codex silently ignores `~/.codex/hooks.json`. `install-hooks` detects this and prints a warning with the activation command: `codex features enable codex_hooks`.
- **Two Claude-side artifacts**: Claude Code does not auto-discover hook scripts by filename. Both `.claude/hooks/user-prompt-submit.sh` and the matching entry in `.claude/settings.json` are required. `install-hooks` writes both; do not remove either by hand.
- **TUI restart needed after config changes on the Codex side**: Codex reads `config.toml` + `hooks.json` at process startup only. After enabling the feature flag or running `install-hooks`, fully quit and relaunch the Codex TUI before expecting hooks to fire.
- **MCP server re-spawn**: Claude Code spawns the mempal MCP server at client startup. After upgrading the mempal binary (`cargo install ...`), restart Claude Code so the MCP server respawns and exposes newly added tools like `mempal_cowork_push` or `mempal_fact_check`.
- **Hermes production path**: prefer the Hermes MemoryProvider/hooks integration against the daemon REST API. Avoid long-lived stdio MCP registrations for routine Hermes use because they create additional SQLite holders; direct HTTP MCP at `/mcp` is not the supported Hermes path until that transport has production coverage.
- **Bidirectional scope**: `mempal_cowork_push` currently requires an MCP client identifying itself as `claude-code` or `codex` (or their aliases). Generic MCP clients cannot push because caller identity is required to fill the `from` field and enforce self-push rejection. This is by design for the Claude ↔ Codex pair.

## Concurrent Ingest Safety (P9-B)

Two agents writing to the same source simultaneously used to be a TOCTOU race: both would pass the dedup check, both would insert, producing duplicate drawers or mismatched vectors. Since 0.4.0, `mempal_ingest` and `ingest_file_with_options` acquire a per-source advisory lock before entering the dedup + insert critical section.

- Lock files live at `~/.mempal/locks/<16-hex>.lock`, created lazily, released on guard drop.
- 5 s timeout, 50 ms retry + jitter; `LockError::Timeout` surfaces as an `ingest` error.
- Every non-dry-run response carries `lock_wait_ms: Option<u64>` so agents can detect contention.
- Dry-run does not acquire the lock (no writes, no race).
- Unix only in 0.4.0. On Windows the lock path is a no-op fallback; `LockFileEx` support is tracked for a follow-up.

## Offline Fact Checking (P9-A)

`mempal_fact_check` — and its CLI counterpart `mempal fact-check` — compare a text blob against the existing KG `triples` + the entity registry derived from recent drawers. It flags three issue classes, all deterministic and zero-LLM:

| Issue | Trigger |
|-------|---------|
| `SimilarNameConflict` | Text mentions a name within Levenshtein distance ≤ 2 of a known entity, and the names are not equal. |
| `RelationContradiction` | Text asserts a predicate (e.g. `brother_of`) that's in the incompatibility dictionary against an existing KG triple with the same `(subject, object)` endpoints. |
| `StaleFact` | Text asserts a triple whose KG row has `valid_to < now` (Unix seconds). |

Extracted triples today cover three narrow patterns: "X is Y's ROLE", "X works at / for Y", and "X is [the|a|an] ROLE of Y". Unknown sentence shapes are silently ignored, so the tool errs toward under-reporting rather than false positives.

Protocol Rule 11 guides agents to run this before ingesting a decision that asserts entity relationships. See `specs/p9-fact-checker.spec.md` for the full contract.

## Search Architecture

```
query → BM25 (FTS5)     → ranked by keyword match
      → Vector (sqlite-vec) → ranked by semantic similarity
      → RRF Fusion (k=60)   → merged ranking
      → Wing/Room filter     → scoped results
      → Tunnel hints         → cross-project references
```

## Knowledge Graph

```bash
mempal kg add "Kai" "recommends" "Clerk"
mempal kg add "Clerk" "replaced" "Auth0" --source-drawer drawer_xxx
mempal kg timeline "Kai"
mempal kg stats
```

Triples support temporal validity — relationships can be invalidated when they expire.

## Agent Diary

Cross-session behavioral learning — agents record observations, lessons, and patterns:

```bash
# Search diary entries
mempal search "lesson" --wing agent-diary
mempal search "pattern" --wing agent-diary --room claude
```

Diary entries use the existing `mempal_ingest` tool with `wing="agent-diary"` and `room=agent-name`. MEMORY_PROTOCOL Rule 5a teaches agents to write diary entries. Integrates with Claude Code's auto-dream for automatic memory consolidation.

## Ingest Formats (5)

| Format | Auto-detected by |
|--------|-----------------|
| Claude Code JSONL | `type` + `message` fields |
| ChatGPT JSON | Array or `mapping` tree |
| Codex CLI JSONL | `session_meta` + `event_msg` entries |
| Slack DM JSON | `type: "message"` + `user` + `text` |
| Plain text | Fallback |

## AAAK Compression

Output-only format readable by any LLM without decoding:

```bash
mempal compress "Kai recommended Clerk over Auth0 based on pricing and DX"
# V1|manual|compress|1744156800|cli
# 0:KAI+CLK+AUT|kai_clerk_auth0|"Kai recommended Clerk over Auth0..."|★★★★|determ|DECISION
```

Chinese text uses jieba-rs POS tagging for proper word segmentation.

## Current Package Layout

mempal currently builds as one Cargo package named `mempal`, with the binary at `src/main.rs`. Older specs and plans may mention a future or historical multi-crate workspace; those references are not the current install path.

| Path | Responsibility |
|------|---------------|
| `src/core/` | Types, SQLite schema, taxonomy, triples, config, queue |
| `src/embed/` | Embedder implementations (OpenAI-compatible routing, explicit model2vec and ONNX paths) |
| `src/ingest/` | Format detection, normalization, chunking, gating, novelty |
| `src/search/` | Hybrid search (BM25 + vector + RRF), routing, tunnels |
| `src/aaak/` | AAAK encode/decode with BNF grammar + roundtrip tests |
| `src/mcp/` | MCP server tools and protocol surface |
| `src/api/` | Feature-gated REST API |
| `src/main.rs`, `src/cli/` | CLI entrypoint and command helpers |

Key design choices:
- **OpenAI-compatible embeddings** as the default provider family; model2vec stays explicit opt-in
- **ort (ONNX)** available behind `onnx` feature flag for max quality
- **FTS5** for BM25 keyword search — synced via SQLite triggers
- **Soft-delete** with audit trail — `mempal delete` + `mempal purge`
- **Importance ranking** — drawers have 0-5 importance, wake-up sorts by importance
- **Semantic dedup** — ingest warns (doesn't block) when similar content exists

## Development

This repository is Linux-first for project and agent gates. PR/push GitHub
Actions CI is intentionally absent; GitHub Pages and release workflows are
publishing/release automation, not the default completion gate. Before
push/merge, run the local aggregate gate and keep exact-head CSA/Codex review
as the review gate:

```bash
just local-gates
csa review --range main...HEAD --sa-mode true
```

`just local-gates` runs `fmt-check`, strict clippy, default tests, REST feature
tests, and release/package checks. `lefthook` enforces the same local gates on
pre-push and keeps branch protection plus the exact-head review marker check.
Do not spend agent time polling `gh pr checks` unless branch protection or the
user explicitly asks for it.

After changing the embedding model, re-embed existing drawers:

```bash
mempal reindex
```

## Docs

- Current architecture overview: [`docs/architecture.md`](docs/architecture.md)
- Product surface classification: [`docs/architecture.md#product-surface-classification`](docs/architecture.md#product-surface-classification)
- Search latency investigation: [`docs/search-latency-investigation.md`](docs/search-latency-investigation.md)
- Historical design baseline: [`docs/specs/2026-04-08-mempal-design.md`](docs/specs/2026-04-08-mempal-design.md) — useful for intent, but some package-layout details are implementation history; use the current package layout above for install/runtime facts.
- Usage guide: [`docs/usage.md`](docs/usage.md)
- AAAK dialect: [`docs/aaak-dialect.md`](docs/aaak-dialect.md)
- Specs (internal agent-spec contracts, on GitHub): <https://github.com/RyderFreeman4Logos/mempal/tree/main/specs>
- Plans (internal implementation plans, on GitHub): <https://github.com/RyderFreeman4Logos/mempal/tree/main/docs/plans>
- Benchmark: [`benchmarks/longmemeval_s_summary.md`](benchmarks/longmemeval_s_summary.md) — includes the older 384d baseline and the newer model2vec 256d run

## Book: MemPalace — Reforging Memory in Rust

The mempal design analysis and full technical narrative are covered in Part 10
of *MemPalace: First Principles of AI Memory* (chapters 26-30):

- [English](https://zhanghandong.github.io/mempalace-book/en/ch26-why-rewrite-in-rust.html)

| Chapter | Coverage |
|---------|----------|
| 26 | Why reforge in Rust: trigger points, rewrite criteria, and language choice |
| 27 | What stayed and what changed: five-dimension comparison plus architecture diagrams |
| 28 | Self-describing protocol: MEMORY_PROTOCOL, operating rules, and agent lifecycle |
| 29 | Multi-agent cowork: Claude/Codex handoffs, antipattern discovery, and agent diary |
| 30 | Honest gaps: benchmark data and remaining gaps |

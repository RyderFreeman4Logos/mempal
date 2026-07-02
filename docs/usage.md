# mempal Usage Guide

This guide is for the repository as it exists today: local CLI workflows, MCP usage, AAAK output, the optional REST server, and the native LongMemEval harness.

`mempal` is a local memory system for coding agents. It stores raw text in SQLite, builds embeddings for retrieval, and always returns citations such as `drawer_id` and `source_file`.

## Mental Model

Before using the CLI, keep four nouns straight:

- `wing`: the top-level scope, usually one project or knowledge domain
- `room`: a sub-scope inside a wing, usually inferred from directory names or edited by taxonomy
- `drawer`: one stored memory item or chunk
- `source_file`: where the drawer came from; for directory ingest, stored relative to the ingest root

`mempal` is raw-first:

- original text lives in the `drawers` table
- vectors live in `drawer_vectors`
- AAAK is output-only and does not replace stored raw text
- default storage is local-first: SQLite only, with no hidden model2vec load and no cloud LLM, embedding, or rerank calls unless configured explicitly

## Install

> **Caution: `cargo install --git` from a fork is unreliable across schema migrations.**
> `cargo install --git <fork-url> --branch main --force mempal` may report success while actually
> skipping the rebuild (cargo's source cache returns a stale ref despite `--force`, which only
> forces *installation* not *re-fetch*). After a `CURRENT_SCHEMA_VERSION` bump in `src/core/db.rs`,
> the resulting binary will fail with a schema mismatch error that tells you to update the mempal
> binary and, for MCP servers, verify the MCP client command/path configuration.
> See [#76](https://github.com/RyderFreeman4Logos/mempal/issues/76).
>
> For fork builds, prefer the root `--path` route below (clones a fresh checkout, builds locally).
> A one-liner is provided at [`scripts/install-from-source.sh`](../scripts/install-from-source.sh).

Install the released crate:

```bash
cargo install mempal
```

Install from the current repository checkout:

```bash
cargo install --path . --locked
```

Install with REST support:

```bash
cargo install --path . --locked --features rest
```

For development without installation:

```bash
cargo run -- --help
cargo run --features rest -- serve --help
```

The repository currently has one Cargo package named `mempal`, with the binary at `src/main.rs`. Historical specs and plans may mention a multi-crate workspace; treat those as implementation history or roadmap notes, not current install instructions.

## Configuration

Config file path:

```text
~/.mempal/config.toml
```

Recommended explicit embedder config:

```toml
db_path = "~/.mempal/palace.db"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "http://127.0.0.1:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096

[daemon]
# configured: use [embed] as-is
# remote: daemon uses [embed.openai_compat] / [[embed.endpoints]]
# small_local: daemon uses minishlab/potion-base-8M
embedder_mode = "configured"

[search.reranker]
enabled = false
# endpoint = "http://gb10:18003/v1/rerank"
# model = "qwen3-reranker"
# timeout_secs = 2
# top_k = 20

[privacy.remote_calls]
fail_closed = false
allow_embedding = false
allow_llm = false
allow_rerank = false
```

With no config file, `mempal` uses the local SQLite database at `~/.mempal/palace.db` and does not silently download or load model2vec. Configure an embedding endpoint for ingest/search, or explicitly enable `backend = "model2vec"` with the `model2vec` Cargo feature for local static models.

Use `mempal cost status` to print redacted remote-call status for embedding, LLM, and rerank paths. Set `[privacy.remote_calls] fail_closed = true` to block external endpoints unless the matching `allow_*` toggle is also true.

Use local ONNX instead of the default OpenAI-compatible provider family:

```toml
db_path = "~/.mempal/palace.db"

[embed]
backend = "onnx"
```

Optionally use an external embedding API instead of local embeddings:

```toml
db_path = "~/.mempal/palace.db"

[embed]
backend = "api"
api_endpoint = "http://localhost:11434/api/embeddings"
api_model = "nomic-embed-text"
```

For a long-lived daemon on a memory-constrained machine, prefer a local/LAN
OpenAI-compatible embedding service:

```toml
[daemon]
embedder_mode = "remote"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "http://127.0.0.1:18002/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 4096
```

`remote` mode is daemon-only: normal one-shot CLI commands still use `[embed]`
as configured, while daemon workers and daemon REST embedding avoid loading the
in-process local embedder cache. If you need an all-local low-memory daemon, install
with `--features model2vec` and set `embedder_mode = "small_local"` to use `minishlab/potion-base-8M`. After
changing backend/model/dimensions, run `mempal reindex` and restart the daemon.

Notes:

- `openai_compat` is the default backend family, but real embedding work needs a configured endpoint.
- `model2vec` is explicit opt-in through both Cargo feature and `backend = "model2vec"`.
- First use of `model2vec` or `onnx` may download model assets.
- If `config.toml` is missing, `mempal` still works with defaults.
- The benchmark and search commands use whatever embedder backend is configured here.
- `mempal daemon status` and `mempal doctor` report daemon RSS/PSS,
  `exe_deleted`, and sanitized embedder cache status so upgrades that leave a
  resident `/usr/local/bin/mempal (deleted)` daemon are visible.
- Reranking is disabled by default. To use a local/LAN reranker, set
  `[search.reranker] enabled = true`, `endpoint`, `model`, `timeout_secs`, and
  `top_k`. A bare endpoint like `gb10:18003` is normalized to
  `http://gb10:18003/v1/rerank`; do not put secrets in the endpoint URL.
- If the reranker is absent, disabled, times out, or returns an error, search
  keeps the existing BM25/vector ranking and reports a warning instead of
  failing the request.

Optional LLM gating can use a pool of OpenAI-compatible chat-completion endpoints. When
LLM gating is configured, endpoint outage is visible in `mempal status`; long
historical cleanup work leaves the current item pending so a wrapper can retry
with `--resume` instead of silently downgrading quality.

```toml
[llm]
enabled = true
backend = "openai_compat"
enabled_for = ["gating"]
request_timeout_secs = 3000
retry_interval_secs = 60

[[llm.endpoints]]
id = "qwen"
base_url = "http://gb10:18009/v1"
model = "qwen3.6-27b-decensor-by-aeon"
priority = 0
max_concurrent = 1

[[llm.endpoints]]
id = "spark"
base_url = "http://localhost:8317/v1"
model = "spark"
# Set this to 0 for equal priority with Qwen; keep it higher to save Spark quota
# and use Spark only after Qwen is unavailable or saturated.
priority = 10
max_concurrent = 1
# Prefer api_key_env for secrets in committed examples.
api_key_env = "SPARK_API_KEY"
```

Do not mix `[[llm.endpoints]]` with legacy scalar `llm.base_url` / `llm.model`.

### Two-stage historical cleanup with Qwen proposals and Spark confirmation

For large reversible cleanup runs, use Qwen as the proposal gate and Spark as the
immediate confirmation gate:

```bash
mempal maintenance rejudge \
  --all --execute \
  --backup-dir /absolute/path/to/rejudge-backups \
  --progress-file /absolute/path/to/rejudge-progress.jsonl \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

Behavior:

- Qwen scores each active drawer first.
- If Qwen keeps the drawer, Spark is not called.
- If Qwen proposes forgetting the drawer, Spark is called immediately for that
  drawer before any soft-delete is written.
- Only Spark-confirmed candidates are soft-deleted; use restore commands/backups
  to reverse them. Do not use `--hard-delete` for quality-gated cleanup.
- The proposal stage is persisted before Spark confirmation. If Spark is
  unavailable after Qwen proposes a candidate, rerun with `--resume`; Qwen is not
  called again for that candidate.
- If a configured LLM endpoint is unavailable, the current work item remains
  pending, the checkpoint status becomes `waiting_llm`, and `mempal status`
  shows a warning instead of silently keeping low-quality records.

When Spark quota is exhausted for days, split the two stages explicitly. First,
run only Qwen proposals; delete candidates are persisted as SQLite
`confirm_pending` work items and no drawer is soft-deleted or hard-deleted:

```bash
mempal maintenance rejudge \
  --all --execute \
  --proposal-only \
  --progress-file /absolute/path/to/rejudge-progress.jsonl \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

When Spark quota returns, drain only the persisted confirmation backlog. This
mode reuses the stored Qwen proposal score and does not call Qwen again for
`confirm_pending` rows:

```bash
mempal maintenance rejudge \
  --all --resume --execute \
  --confirm-pending-only \
  --backup-dir /absolute/path/to/rejudge-backups \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark
```

Progress files and `mempal status` expose aggregate `no_stage_pending_count` and
`confirm_pending_count` values only. They do not include drawer content, prompts,
model responses, endpoint credentials, URLs with secrets, or other raw payloads.

For a write-free historical artifact run, use `--candidates-file` instead of
`--execute`. The main process keeps the source SQLite database read-only and
writes `proposals.jsonl`, `confirmations.jsonl`, and `cursor.json` under the
artifact directory. To avoid waiting for the full Qwen scan before spending Spark
quota, run an independent Spark confirmation drainer against the same artifact
directory while the Qwen proposal scanner is still appending proposals:

```bash
ART_DIR="$HOME/.mempal/runs/rejudge-artifacts-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$ART_DIR"

mempal maintenance rejudge \
  --all \
  --candidates-file "$ART_DIR" \
  --progress-file "$ART_DIR/progress.jsonl" \
  --proposal-llm-endpoint qwen \
  --confirm-llm-endpoint spark &

while true; do
  mempal maintenance rejudge \
    --all \
    --confirm-pending-only \
    --candidates-file "$ART_DIR" \
    --proposal-llm-endpoint qwen \
    --confirm-llm-endpoint spark \
    --format json || true
  sleep 300
done
```

The drainer does not call Qwen, does not need `--execute` or `--resume`, and does
not mutate the SQLite database. It reads completed `proposal_decision=forget`
JSONL records, tolerates the proposal scanner actively appending the final line,
and appends Spark results to `confirmations.jsonl`. Later, after reviewing the
artifacts and taking an explicit backup, use the artifact apply command to mutate
or delete rows.

Maintenance rejudge is designed as an IO-first service path when proposal and
confirmation judges are external endpoints. Full `--all` runs snapshot work into
SQLite `historical_rejudge_work_items`, page through that table with
`--page-size`, and persist split-stage proposal backlog as `confirm_pending`
rows. New SQLite backup files use a streaming payload hash so each page append
does not reload or parse the complete backup history into memory.

Memory budget discipline:

- Idle/lightweight daemon paths should stay in the tens of MiB when configured
  for remote embedding.
- Large historical maintenance runs should use bounded page buffers and stay in
  the low hundreds of MiB, excluding memory used by external model services.
- Avoid increasing `--page-size` beyond what the host can comfortably keep in
  memory with the current drawer size distribution.
- Progress JSONL and final reports include a `memory` object with aggregate
  fields only: `rss_bytes`, `pss_bytes`, `vm_hwm_bytes`,
  `private_dirty_bytes`, `anonymous_bytes`, and `swap_bytes` when the platform
  exposes them. Unsupported platforms report `available=false`.
- Memory reports never include drawer content, prompts, model responses,
  endpoint URLs, API keys, or other raw payloads.

For unattended runs, wrap the command with `nohup` and retry with `--resume`.
Keep logs/progress content-free: inspect counts, status, cursor, and model names,
not raw drawer text or model responses.

```bash
RUN_DIR="$HOME/.mempal/runs/rejudge-$(date -u +%Y%m%dT%H%M%SZ)"
export RUN_DIR
mkdir -p "$RUN_DIR/backups"
nohup bash -lc '
set -euo pipefail
while true; do
  if mempal maintenance rejudge \
    --all --execute --resume \
    --backup-dir "$RUN_DIR/backups" \
    --progress-file "$RUN_DIR/progress.jsonl" \
    --proposal-llm-endpoint qwen \
    --confirm-llm-endpoint spark; then
    exit 0
  fi
  mempal status
  sleep 300
done
' >"$RUN_DIR/runner.log" 2>&1 &
```

Watch progress with aggregate-only outputs:

```bash
mempal status
tail -n 20 "$RUN_DIR/progress.jsonl"
tail -n 100 "$RUN_DIR/runner.log"
```

## Command Cheat Sheet

Use this when you already know the concepts and just need the right command
quickly. This table covers the main user-facing commands; use `mempal --help`
and nested `--help` output for the full maintenance command tree.

| Command | Purpose |
|---------|---------|
| `mempal init <DIR> [--dry-run]` | infer a `wing` and seed initial taxonomy rooms from a project tree |
| `mempal ingest --wing <WING> <DIR> [--parser auto --no-llm] [--dry-run]` | chunk, embed, and store a project tree or deterministic document set |
| `mempal search <QUERY> [--wing W] [--room R] [--json]` | hybrid search (BM25 + vector + RRF) with tunnel hints |
| `mempal brief <QUERY>` | generate a citation-first brief with facts, evidence, uncertainty, and next actions |
| `mempal context <QUERY> [--format json] [--include-evidence] [--dao-tian-limit N]` | assemble mind-model runtime context (`dao_tian -> dao_ren -> shu -> qi`); default `dao_tian` budget is 1 |
| `mempal timeline [--wing W] [--since S] [--format F] [--raw]` | show a project-scoped digest ordered by importance and recency |
| `mempal pinned [--project P] [--reorder ...] [--json]` | read canonical pinned facts without embedding lookup |
| `mempal field-taxonomy [--format json]` | inspect read-only recommended `field` values for typed memory |
| `mempal knowledge distill --statement ... --content ... --tier dao_ren --supporting-ref <ID>` | create candidate knowledge from evidence refs |
| `mempal knowledge policy [--format json]` | inspect read-only Stage-1 promotion policy thresholds |
| `mempal knowledge gate <ID> [--format json]` | evaluate whether knowledge satisfies promotion gate policy without mutating it |
| `mempal knowledge promote <ID> --status promoted --verification-ref <ID> --reason ...` | promote bootstrap knowledge into active runtime use |
| `mempal knowledge demote <ID> --status demoted --evidence-ref <ID> --reason ... --reason-type contradicted` | demote or retire contradicted / obsolete bootstrap knowledge |
| `mempal wake-up [--format aaak]` | L0/L1 refresh sorted by importance; not a typed mind-model context pack |
| `mempal compress <TEXT>` | format arbitrary text as AAAK |
| `mempal kg add <S> <P> <O> [--source-drawer ID]` | add a knowledge graph triple |
| `mempal kg query [--subject S] [--predicate P] [--object O]` | query triples |
| `mempal kg timeline <ENTITY>` | chronological view of an entity's relationships |
| `mempal kg stats` | knowledge graph statistics |
| `mempal tunnels` | discover rooms shared across multiple wings |
| `mempal taxonomy list` | inspect current routing keywords |
| `mempal taxonomy edit <WING> <ROOM> --keywords ...` | tune routing behavior |
| `mempal reindex` | re-embed all drawers after model/backend change |
| `mempal status` | schema version, drawer counts, triples, deleted drawers, scopes |
| `mempal doctor` | inspect install, schema, runtime, and MCP diagnostics |
| `mempal operation status <OPERATION_ID>` | poll receipt-backed async ingest work |
| `mempal skill ...` | inspect skill/runtime guidance helpers |
| `mempal delete <DRAWER_ID>` | soft-delete one drawer |
| `mempal purge [--before ...]` | permanently remove soft-deleted drawers |
| `mempal serve --mcp` | run the MCP server over stdio |
| `mempal bench longmemeval <DATA_FILE>` | run the native LongMemEval retrieval benchmark |

## First 5 Minutes

This is the shortest realistic flow for a new project.

### 1. Inspect the inferred taxonomy

Preview which `wing` and `room` names `mempal` will infer:

```bash
mempal init ~/code/myapp --dry-run
```

Typical output:

```text
dry_run=true
wing: myapp
rooms:
- auth
- deploy
- docs
```

Write those taxonomy entries:

```bash
mempal init ~/code/myapp
```

### 2. Preview ingest before writing

```bash
mempal ingest ~/code/myapp --wing myapp --dry-run
```

Typical output:

```text
dry_run=true files=12 chunks=34 skipped=2
```

This reads, normalizes, chunks, and counts, but does not write drawers or vectors.

For document folders, keep parser selection deterministic:

```bash
mempal ingest docs/ --wing docs --parser auto --no-llm --dry-run
```

`--parser auto` dispatches through built-in Rust parsers for text, Markdown,
code, JSONL, and OOXML Office files (`.docx`, `.pptx`, `.xlsx`). Deterministic
PDF text parsing is disabled unless a bounded extractor is added; use an
explicit OCR/LLM parser only when that provider is configured and acceptable.
Image, audio, video, OCR, VLM, and MM-LLM parsing require explicit
`--allow-llm`; without that opt-in, `--no-llm`/default policy rejects those
inputs instead of making remote calls.

### 3. Ingest the project

```bash
mempal ingest ~/code/myapp --wing myapp
```

Optional explicit format selector:

```bash
mempal ingest ~/code/myapp --wing myapp --format convos
```

Optional explicit parser selector:

```bash
mempal ingest docs/ --wing docs --parser office --no-llm
```

Every ingest appends a JSONL audit record to:

```text
~/.mempal/audit.jsonl
```

### Bootstrap Knowledge Lifecycle

P18 adds the explicit Stage-1 distillation entry point: create candidate knowledge
from existing evidence drawers.

```bash
mempal knowledge distill \
  --statement "Prefer evidence before asserting project facts" \
  --content "When answering project-specific questions, cite source-backed memory before making claims." \
  --tier dao_ren \
  --supporting-ref drawer_evidence
```

Distill always creates `status=candidate` and currently only allows `tier=dao_ren`
or `tier=qi`. `dao_tian` and `shu` are intentionally excluded from candidate
distill because the current P12 status policy does not allow candidate states
for those tiers. Use `promote` only after review.

P17 adds manual lifecycle commands for Stage-1 knowledge drawers. P19 hardens
those commands so lifecycle refs must be existing evidence drawers, not arbitrary
ids or other knowledge drawers:

P18 adds deterministic CLI distill. P22 exposes the same operation to MCP agents
as `mempal_knowledge_distill`: create candidate `dao_ren` / `qi` knowledge from
existing evidence refs without LLM summarization or auto-promotion. P23 exposes
the lifecycle mutation side as `mempal_knowledge_promote` and
`mempal_knowledge_demote`: MCP promotion is gate-enforced, and demotion requires
counterexample evidence.

Equivalent MCP distill request:

```json
{
  "statement": "Prefer evidence first",
  "content": "Use cited evidence before asserting project facts.",
  "tier": "dao_ren",
  "supporting_refs": ["drawer_evidence"]
}
```

P20 adds a read-only promotion gate report. P27 exposes the current Stage-1
policy table directly:

```bash
mempal knowledge policy --format json
```

Use `gate` before `promote` to check the minimum deterministic policy against a
specific drawer without changing status, refs, vectors, schema, or the audit
log. P21 exposes the same drawer-specific gate to MCP agents as
`mempal_knowledge_gate`, while P27 exposes the policy table as
`mempal_knowledge_policy`.

```bash
mempal knowledge gate drawer_knowledge --format json
```

P24 adds explicit anchor publication. This is separate from tier/status
promotion: it only moves an already active knowledge drawer outward across
anchor scope, without rewriting content or vectors.

```bash
mempal knowledge publish-anchor drawer_knowledge \
  --to repo \
  --reason "stable across this repository"
```

Supported publication chain is `worktree -> repo -> global`. Publishing to
`global` requires `domain=global` and an explicit `--target-anchor-id
global://...`. P25 exposes the same metadata-only operation to MCP agents as
`mempal_knowledge_publish_anchor`.

For `dao_tian -> canonical`, provide a reviewer for the advisory gate:

```bash
mempal knowledge gate drawer_dao_tian \
  --target-status canonical \
  --reviewer human \
  --format json
```

Equivalent MCP request:

```json
{
  "drawer_id": "drawer_dao_tian",
  "target_status": "canonical",
  "reviewer": "human"
}
```

```bash
mempal knowledge promote drawer_knowledge \
  --status promoted \
  --verification-ref drawer_evidence \
  --reason "validated across repeated runs" \
  --reviewer "human"
```

```bash
mempal knowledge demote drawer_knowledge \
  --status demoted \
  --evidence-ref drawer_counterexample \
  --reason "new evidence contradicts this heuristic" \
  --reason-type contradicted
```

Lifecycle commands only update existing `memory_kind=knowledge` drawers. They validate that `--verification-ref` / `--evidence-ref` values start with `drawer_`, exist, and point to `memory_kind=evidence`. They do not change content, re-embed vectors, bump schema, or add Phase-2 `knowledge_cards`. Successful distill and lifecycle changes append JSONL audit entries.

Phase-2 knowledge card tables exist in schema v8, but user-facing card runtime
APIs are not implemented yet. `drawers` remain the active evidence/citation
root, while `knowledge_cards`, `knowledge_evidence_links`, and
`knowledge_events` are reserved for the Phase-2 runtime.

### 4. Search

```bash
mempal search "auth decision clerk"
```

Structured JSON output:

```bash
mempal search "auth decision clerk" --json
```

Restrict to a wing:

```bash
mempal search "database decision" --wing myapp
```

Restrict to a wing and room:

```bash
mempal search "token refresh bug" --wing myapp --room auth
```

### 5. Generate a context refresh

```bash
mempal wake-up
```

Compact AAAK-formatted refresh:

```bash
mempal wake-up --format aaak
```

Use `mempal context` when the agent needs typed operating guidance such as
`dao_tian -> dao_ren -> shu -> qi`. `wake-up` may show selected knowledge
statements, but it keeps the L0/L1 refresh shape and does not assemble typed
tier sections or apply `dao_tian_limit`.

## Core Workflows

### Search

What a search result includes:

- `drawer_id`
- `content`
- `wing`
- `room`
- `source_file`
- `similarity`
- `route`

`route` explains whether the query used explicit filters or taxonomy routing.

`source_file` is stored relative to the ingest root, so citations stay stable whether the project was ingested via an absolute or relative path.

If you care about deterministic scope, pass `--wing` and optionally `--room` explicitly instead of relying on routing.

### Field Taxonomy

`field` is a mind-model metadata dimension used by typed memory search and
context assembly. It is separate from wing/room routing taxonomy. P28 exposes a
read-only recommended field list:

```bash
mempal field-taxonomy
mempal field-taxonomy --format json
```

The field taxonomy is guidance only. Custom fields remain valid for ingest,
distill, search, and context when the recommended Stage-1 fields are too coarse.

### Wake-Up and AAAK

`wake-up` emits a short memory summary for agent context refresh:

```bash
mempal wake-up
```

AAAK output:

```bash
mempal wake-up --format aaak
mempal compress "Kai recommended Clerk over Auth0 based on pricing and DX"
```

Example AAAK output:

```text
V1|manual|compress|1744156800|cli
0:KAI+CLK+AUT|kai_clerk_auth0|"Kai recommended Clerk over Auth0 based on pricing and DX"|★★★★|determ|DECISION
```

AAAK is an output formatter only:

- it does not affect how drawers are stored
- it is not required for ingest or search
- benchmark `--mode aaak` means "index AAAK-formatted retrieval text", not "change the storage layer"

### Chinese Text

AAAK supports Chinese and mixed Chinese-English text:

```bash
mempal compress "张三推荐Clerk替换Auth0，因为价格更优"
```

Chinese entities and topics are extracted with `jieba-rs` POS tagging. People, places, organizations, and content words are turned into entity/topic fields before AAAK formatting.

This section is about AAAK output formatting, not retrieval quality. Chinese AAAK support is currently stronger than Chinese search quality.

For the full format specification, see [`docs/aaak-dialect.md`](aaak-dialect.md).

### Taxonomy

List taxonomy entries:

```bash
mempal taxonomy list
```

Edit or add taxonomy keywords:

```bash
mempal taxonomy edit myapp auth --keywords "auth,login,clerk"
```

Use taxonomy when:

- you want routing to pick the right room more reliably
- your repo directory layout is not enough
- you want search behavior to reflect domain language instead of folder names

### Status

Show storage stats:

```bash
mempal status
```

The command reports:

- `schema_version`
- `drawer_count`
- `deleted_drawers` when soft-deleted content exists
- `taxonomy_entries`
- DB file size
- per-`wing` and per-`room` counts

Schema version is backed by SQLite `PRAGMA user_version`. On open, `mempal` applies bundled forward migrations needed to bring an older local database up to the current binary's schema.

### Agent Diary

mempal supports cross-session behavioral learning through a diary convention. Agents (or humans) record observations, lessons, and patterns that future sessions can learn from.

The diary uses existing tools — no special commands needed:

```bash
# Write a diary entry (via MCP or by asking your AI agent)
# Convention: wing="agent-diary", room=agent-name
# Prefix content with OBSERVATION:, LESSON:, or PATTERN:

# Search diary entries
mempal search "lesson" --wing agent-diary
mempal search "pattern infrastructure" --wing agent-diary --room claude

# Search all entries for a specific agent
mempal search "observation" --wing agent-diary --room codex
```

Example diary entry (written by AI agent via MCP `mempal_ingest`):

```json
{
  "content": "LESSON: always check repo docs before writing infrastructure code",
  "wing": "agent-diary",
  "room": "claude",
  "importance": 4
}
```

Content prefixes:

| Prefix | Use for |
|--------|---------|
| `OBSERVATION:` | Factual behavioral observations |
| `LESSON:` | Actionable takeaways from mistakes or successes |
| `PATTERN:` | Recurring behavioral patterns across sessions |

MEMORY_PROTOCOL Rule 5a tells AI agents to write diary entries after sessions. Human users can also write diary entries — use `room` to identify the author (e.g., `room="alex"`).

### Delete and Purge

These are destructive operations. Use them carefully.

Soft-delete one drawer:

```bash
mempal delete drawer_myapp_auth_1234abcd
```

Current behavior:

- looks up the drawer first
- soft-deletes it
- prints a short summary of what was deleted
- writes an audit log entry
- does not permanently remove it yet

Permanent removal:

```bash
mempal purge
```

Purge only drawers soft-deleted before an ISO timestamp:

```bash
mempal purge --before 2026-04-10T00:00:00Z
```

Important:

- `delete` is reversible only until `purge` runs
- `status` will tell you when deleted drawers are waiting to be purged

## Common Recipes

### Index a repo and search one subsystem

```bash
mempal init ~/code/myapp
mempal ingest ~/code/myapp --wing myapp
mempal search "token refresh bug" --wing myapp --room auth
```

### Preview a large ingest before committing disk and compute

```bash
mempal init ~/code/monorepo --dry-run
mempal ingest ~/code/monorepo --wing monorepo --dry-run
```

### Tune routing when search keeps landing in the wrong room

```bash
mempal taxonomy list
mempal taxonomy edit myapp deploy --keywords "render,railway,postgres,migration"
mempal search "postgres migration" --wing myapp
```

### Refresh an AI agent before continuing work

```bash
mempal wake-up
mempal wake-up --format aaak
```

### Run a fast benchmark sample instead of the full dataset

```bash
mempal bench longmemeval /path/to/longmemeval_s_cleaned.json \
  --limit 20 \
  --out benchmarks/results_longmemeval_20.jsonl
```

## MCP Server

Run stdio MCP explicitly:

```bash
mempal serve --mcp
```

If `mempal` was built without the `rest` feature, plain `mempal serve` behaves the same way.

The smoke-tested MCP baseline currently documents 19 verified tools. The
[architecture overview](architecture.md#mcp-tool-profiles) groups them into
conceptual profiles for agent discovery. Profiles are documentation-only; use
`mempal doctor` or protocol-level `tools/list` against `mempal serve --mcp`
for the runtime-advertised surface in a specific build.

Default agent profile:

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

Knowledge management profile:

- `mempal_kg` -- add, query, invalidate, timeline, or summarize knowledge graph triples.
- `mempal_taxonomy` -- inspect or edit wing/room routing keywords.
- `mempal_field_taxonomy` -- read recommended typed-memory `field` values.
- `mempal_tunnels` -- discover and manage cross-scope memory links.
- `mempal_timeline` -- inspect recent or important memory by project scope.
- `mempal_knowledge_distill` -- create candidate dao_ren/qi knowledge from evidence refs.
- `mempal_fact_check` -- detect offline name, relation, and stale-fact contradictions.

Fact-check workflow: call `mempal_fact_check` on draft memory text before
`mempal_ingest` when the draft asserts named-entity relationships or dated KG
facts. The check is deterministic, pattern-based, and does not use LLM or
network calls. It can flag bounded conflicts such as similar-name typos,
incompatible relationship predicates, and expired KG facts; it does not prove
truth, resolve ambiguity, or catch semantic contradictions outside those
patterns. Treat every issue as advisory evidence that needs human or agent
judgment before the draft is committed.

Workspace and skills profile:

- `mempal_skill` -- inspect skill and runtime guidance helpers for agents.
- `mempal_doctor` -- run install, schema, daemon, MCP, and runtime diagnostics.

The server also embeds MEMORY_PROTOCOL (behavioral rules) in the MCP `initialize.instructions` field so any MCP client learns the workflow on connect — zero configuration. The protocol treats `wake-up` as an L0/L1 refresh surface, `mempal_context` as typed guidance for choosing an approach, workflow, skill, or tool, `mempal_field_taxonomy` as guidance for choosing typed-memory `field` values, and `trigger_hints` as bias metadata only. These hints never override system, user, repo, or client-native skill rules.

Example request shapes:

```json
{
  "query": "auth decision clerk",
  "wing": "myapp",
  "room": "auth",
  "top_k": 5
}
```

```json
{
  "content": "decided to use Clerk for auth",
  "wing": "myapp",
  "room": "auth",
  "source": "/repo/README.md",
  "dry_run": false
}
```

Preview an ingest without writing (returns the predicted `drawer_id`):

```json
{
  "content": "decided to use Clerk for auth",
  "wing": "myapp",
  "dry_run": true
}
```

Soft-delete a drawer:

```json
{
  "drawer_id": "drawer_myapp_auth_1234abcd"
}
```

```json
{
  "action": "edit",
  "wing": "myapp",
  "room": "auth",
  "keywords": ["auth", "login", "clerk"]
}
```

`mempal_status` also returns the self-describing memory protocol and a dynamically generated AAAK spec so AI clients can learn the tool without a hardcoded prompt.

## Agent Cowork (peek + push)

Two coding agents running on the same repo — typically Claude Code and Codex — can collaborate through two primitives:

- **`mempal_peek_partner`** (P6) — read the partner's live session file without storing anything in mempal. Use for "what is partner currently doing" questions.
- **`mempal_cowork_push`** (P8) — send a short handoff (≤ 8 KB) to the partner's inbox. The partner sees it prepended to their next user prompt via a UserPromptSubmit hook. Use for "make sure partner notices X" status updates that are too transient for an ingest drawer.

### Install hooks

Hooks land the pushed message into the partner's next prompt automatically. Install once per repo (run at the repo root):

```bash
mempal cowork-install-hooks --global-codex
```

This writes two Claude-side artifacts (both required — Claude Code does not auto-discover bare hook scripts):

- `.claude/hooks/user-prompt-submit.sh` — the drain script
- an entry in `.claude/settings.json` under `hooks.UserPromptSubmit` registering the script

and merges the equivalent entry into `~/.codex/hooks.json` (top-level `hooks.UserPromptSubmit` with `{type:"command", command:"mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json"}`).

Re-running is idempotent and self-heals stale/wrong drain entries from prior mempal versions, preserving any unrelated hooks already in those files.

### Check current state

```bash
mempal cowork-status --cwd "$PWD"
```

Lists both inbox targets (`claude` and `codex`) for the given cwd along with message counts, byte sizes, and a preview. Read-only — does not drain.

### Known limitations

- **Codex `codex_hooks` feature flag**: Codex's hooks runtime is gated behind the `codex_hooks` feature flag ("under development" in current shipped `codex-cli`). If the flag is off, Codex silently ignores `~/.codex/hooks.json`. `install-hooks` detects this and prints an activation prompt (`codex features enable codex_hooks`).
- **TUI restart required on Codex side**: Codex caches `config.toml` + `hooks.json` at TUI startup only. After changing the feature flag or running `install-hooks`, fully quit and relaunch Codex before new hooks take effect.
- **MCP server re-spawn required in Claude Code**: Claude Code spawns the mempal MCP server at client startup. After upgrading the mempal binary, restart Claude Code so the MCP server respawns with the new tool list (notably `mempal_cowork_push`).
- **Claude ↔ Codex scope**: `mempal_cowork_push` requires the MCP client to identify itself as `claude-code` or `codex` (or their recognized aliases). Generic MCP clients cannot push because caller identity is required to fill the message `from` field and enforce self-push rejection. This is by design for the Claude ↔ Codex pair.
- **At-next-submit, not real-time**: a push is visible on the partner's *next* user prompt turn — never mid-turn. Codex's TUI will not re-render to inject a message on an external trigger.

## REST Server

Build with `--features rest` to enable REST:

```bash
cargo install --path . --locked --features rest --force --root ~/.local
mempal doctor rest
mempal serve
```

With REST enabled:

- MCP still runs over stdio
- REST listens on `127.0.0.1:3080` by default
- CORS only allows localhost origins

For daemon profiles, configure the REST address in `~/.mempal/config.toml`:

```toml
[api]
enabled = true
addr = "127.0.0.1:3080"
search_db_deadline_secs = 30
```

If you run multiple mempal daemons, assign each profile a different loopback
port such as `127.0.0.1:3081`. `mempal doctor rest` reports whether the binary
has REST support, whether the endpoint is reachable, which required routes are
present, and which process owns the configured port when there is a collision.

REST search runs inside the daemon process and uses the daemon-owned async
database pool. Explicit full-corpus searches should use `scope=global` (or
`scope=all_projects`) and remain bounded by `api.search_db_deadline_secs`: if a
database stage exceeds the deadline, the response returns partial/fallback
results with `mempal-warnings` and `search-mode` headers instead of hanging.
Automatic hooks and Hermes provider prefetches should continue to pass
project/profile filters and avoid implicit global searches.

Endpoints:

- `GET /api/status`
- `GET /api/search?q=...&wing=...&room=...&top_k=...&scope=project|all_wings|project_plus_global|global|all_projects`
- `POST /api/ingest`
- `GET /api/taxonomy`
- `GET /api/timeline`
- `GET /api/pinned_facts`

Examples:

```bash
curl 'http://127.0.0.1:3080/api/status'
curl 'http://127.0.0.1:3080/api/search?q=clerk&wing=myapp'
curl 'http://127.0.0.1:3080/api/search?q=Hermes+Agent+local+embedding&scope=global&top_k=10'
curl -X POST 'http://127.0.0.1:3080/api/ingest' \
  -H 'content-type: application/json' \
  -d '{"content":"decided to use Clerk","wing":"myapp","room":"auth"}'
curl 'http://127.0.0.1:3080/api/taxonomy'
```

## Benchmark LongMemEval

`mempal` includes a native LongMemEval harness. It reuses the dataset shape and retrieval metrics documented in `mempalace`, while indexing and searching through `mempal` itself.

Default session-granularity raw benchmark:

```bash
mempal bench longmemeval /path/to/longmemeval_s_cleaned.json
```

Other modes:

```bash
mempal bench longmemeval /path/to/longmemeval_s_cleaned.json --mode aaak
mempal bench longmemeval /path/to/longmemeval_s_cleaned.json --mode rooms
```

Turn granularity and results log:

```bash
mempal bench longmemeval /path/to/longmemeval_s_cleaned.json \
  --granularity turn \
  --out benchmarks/results_longmemeval.jsonl
```

Supported options:

- `--mode raw|aaak|rooms`
- `--granularity session|turn`
- `--limit N`
- `--skip N`
- `--top-k N`
- `--out path/to/results.jsonl`

What the benchmark does:

- loads the cleaned LongMemEval JSON
- builds a temporary benchmark DB per question
- indexes retrieval text using the configured embedder
- runs retrieval and reports `Recall@k` and `NDCG@k`

What it does not do:

- it does not generate final answers with an LLM
- it is not the same as the official answer-generation evaluation pipeline
- `raw` mode does not automatically mean zero API cost if your embedder backend is configured as `api`

For the current local benchmark snapshot in this repository, see [`benchmarks/longmemeval_s_summary.md`](../benchmarks/longmemeval_s_summary.md). That summary now separates the older 384d baseline from the newer model2vec 256d run.

## Recommended: Auto-Remind After Commit

mempal works best when AI agents save decision context after every commit — not just the code diff, but *why* the change was made, what was considered, and what's left to do. This is MEMORY_PROTOCOL Rule 4 (SAVE AFTER DECISIONS).

The problem: agents forget. The solution: a Claude Code hook that reminds the agent after every `git commit`.

### Setup for Claude Code

Create `.claude/settings.json` in your project root:

```json
{
  "hooks": {
    "afterToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "if echo \"$TOOL_INPUT\" | grep -q 'git commit'; then echo 'MEMPAL REMINDER: You just committed code. Call mempal_ingest to save the decision context (what was built, why, what was considered). Rule 4: SAVE AFTER DECISIONS.'; fi"
          }
        ]
      }
    ]
  }
}
```

After this, every time the agent runs `git commit`, it sees a reminder to save the decision to mempal. The agent still decides *what* to save — the hook just ensures it doesn't forget.

### What makes a good decision record

Bad (just restating the diff):
```
Added local gates
```

Good (captures context a future agent needs):
```
Replaced PR/push GitHub Actions with Linux-first `just local-gates` and
lefthook enforcement. The gate now runs format check, strict clippy, default
tests, REST feature tests, and release/package checks locally; exact-head
CSA/Codex review remains required before push/merge.
```

The difference: a future agent reading the good version knows what was omitted, why, and what to do next. The bad version tells them nothing they can't learn from `git log`.

### For other AI tools

- **Codex**: Configure in `~/.codex/instructions.md` — add "After every commit, call mempal_ingest with decision context"
- **Cursor**: Add to `.cursorrules` — same instruction
- **Any MCP client**: The MEMORY_PROTOCOL in `mempal_status` already contains Rule 4; the hook is a reinforcement for clients that sometimes skip it

## Auto-Dream Integration

Claude Code's auto-dream feature consolidates session memory while you're away — like REM sleep for AI. mempal integrates with this process to ensure project decisions survive across sessions.

### How it works

When auto-dream runs (automatically between sessions or manually via "dream"):

1. Claude reviews recent session transcripts
2. Extracts key decisions and knowledge
3. **With mempal**: verifies facts via `mempal_search`, saves consolidated insights via `mempal_ingest` with importance >= 3, and records a dream diary entry

### Setup

Add to your project's `CLAUDE.md`:

```markdown
## Auto-Dream Integration

When performing auto-dream or manual dream:
1. Call mempal_search to verify facts being consolidated
2. Save high-value insights to mempal (mempal_ingest, importance >= 3)
3. If MEMORY.md and mempal contradict, trust mempal (has citations)
4. Write dream summary as agent diary (wing="agent-diary", room="claude")
5. Check triples for expired relationships to invalidate
```

### What this gives you

Without mempal, auto-dream consolidates into MEMORY.md files that only Claude Code reads. With mempal, dream insights are stored in `palace.db` where **any** MCP-connected agent (Codex, Cursor, etc.) can find them. Dream becomes a cross-agent memory consolidation mechanism, not just a Claude Code internal process.

## Identity File

If you use `wake-up` regularly with AI agents, you can add a user-edited identity file:

```bash
mkdir -p ~/.mempal
$EDITOR ~/.mempal/identity.txt
```

Example:

```text
Role: Rust backend engineer at Acme.
Current focus: auth rewrite, Clerk migration.
Working style: small reversible edits, verify before asserting.
```

`wake-up` can include this as part of the agent context refresh.

## FAQ

### Search results look wrong or too broad

- Pass `--wing` explicitly. Global search is convenient, but it broadens retrieval.
- Pass `--room` when you already know the subsystem.
- Inspect taxonomy with `mempal taxonomy list` and add better keywords with `mempal taxonomy edit`.
- Check which embedder backend you are using. Different embedding models shift retrieval behavior.

### Search returns irrelevant results for Chinese (or other non-English) queries

The configured embedder can affect multilingual retrieval quality; English query normalization still tends to retrieve more reliably for Chinese and other non-English prompts in practice.

**For AI agents**: MEMORY_PROTOCOL rule 3a tells agents to translate queries to English before calling `mempal_search`. This is handled automatically by agents that read the protocol.

**For CLI users**: translate your query to English manually, or use the `--wing` filter to narrow scope:

```bash
# Poor results:
mempal search "它不再是一个高级原型"

# Good results:
mempal search "no longer just an advanced prototype"
```

This is mostly a retrieval-stack limitation, not a storage limitation:

- the embedder is multilingual but still stronger on English queries
- the search path does not currently use a Chinese-specific lexical tokenizer for FTS5
- AAAK uses `jieba-rs`, but the search path does not

So the practical guidance is still: translate the query to English first, or narrow scope with `--wing` / `--room`.

### Why did ingest store relative paths instead of absolute ones?

`mempal` stores `source_file` relative to the ingest root on purpose. This keeps citations stable if you ingest the same project through different absolute paths.

### Is `raw` benchmark mode always zero API cost?

No. `raw` only means raw retrieval text. API cost depends on the embedder backend:

- local `onnx` backend: zero external API calls
- `api` backend: embedding requests still go to the configured API

### Why is `--granularity turn` so much slower?

Because it expands one session into many more indexed items. On the current `LongMemEval s_cleaned` runs in this repository, `raw + turn` was dramatically slower than `raw + session` while not improving overall retrieval quality enough to justify being the default.

### Should I use `delete` freely because it is soft-delete?

Use it carefully anyway. `delete` is safer than hard removal, but once `mempal purge` runs, the data is permanently gone.

## Verify Changes

If you modify code or behavior in this repository, the current validation baseline is:

```bash
just local-gates
csa review --range main...HEAD --sa-mode true
```

GitHub Actions PR/push CI is not the project/agent gate for this repository.
Use local `just`/`lefthook` gates and exact-head CSA/Codex review. Do not poll
`gh pr checks` unless branch protection or the user explicitly requires it.

# Changelog

All notable changes to this project are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
(pre-1.0: MINOR bumps introduce new features, PATCH bumps are bug-fix only).

## [Unreleased]

### Added

- #872: daemon `/mcp`.
- Codex hooks inject citation-first briefs; BM25 stays AllProjects (#897).
- Deterministic cited-recall bench gates resume/compaction briefs for latest decision, citations, project isolation, and empty-evidence fallback (#898).
- Optional ADK-Rust v1.0.0 post-retrieval evidence workflow for `mempal_search` (#742).
- Interval daemon sleep/consolidation reuses the process-wide SQLite writer lease (#726).
- Baseline-aware `find-monolith-files` rejects new files above 800/8000 and ratchets monolith debt (#695).
- `mempal_projects_list`/`mempal_projects_resume` MCP tools for project discovery and resume (P109).
- `cowork_peek_command`/`mempal_peek_partner` cwd flag peeks a partner session per project (P108).
- Ingest tool/protocol text steers knowledge-only fields to distill before rejection (P107).
- `upstream-sync` skill with `.upstream-sync.json` for selective upstream ports.
- Reranker smoke and #652 latency note (hybrid/sqlite-vec/brute-force).

### Fixed

- Daemon SQLite writer-lease waits out a live maintenance holder past capped `remaining_secs` instead of exit 75; a live incompatible `mcp-ingest-worker` holder is still refused without takeover (#916, #849).
- Ingress: fsync before ACK; lease-fenced replay; REST bind/systemd READY precede daemon-ready (#945). Persist/spool before model and on SQLite/breaker-open; stall keeps REST (#986, #1000, #987).
- Cited-recall latest-decision walks the full successor chain, filters context the same way, and requires a live correction/continuation citation (#898).
- Codex snapshots atomically remove superseded turns/vectors and fail closed on ambiguity (#896).
- MCP search shares a deadline and releases reads before responding (#881).
- Typed/redacted MCP admission and audit-write diagnostics (#879).
- Daemon pidfile validates identity; scoped ingest release honors remaining retry budget (#885/#895).
- Queue byte admission retries transient profile-admission lock contention when opening a normal queue writer, preserving the byte-budget rejection contract under suite load (#893).
- Diagnostic readonly queue stats no longer inherit SQLite's default 5s busy wait: a `queue_stats_readonly` read under a held writer lock now returns a bounded lock diagnostic instead of stalling (#911).

- **Daemon readiness CLI tests**: shared Linux supervisor for bounded redacted lifecycle handling (#892).

- **Hermes receipts**: scoped smoke ingest inline; live-daemon polls `created_drawer_ids`; already-soft-deleted `mempal_delete` succeeds; cleanup IDs survive CLI/MCP/REST decode; breaker-open conclude replay is structured pending success, not tool error; open REST/Hermes breakers admit `mempal_search`/`mempal_profile` probes with typed/redacted payloads; daemon down with ≥100 pending → `doctor`/`status`/MCP doctor emit high-severity typed availability signal; unreadable config/queue stats/unverified PIDs → privacy-safe `unavailable`; recovery: drain, terminal failures, no DB edits; saturated MCP holder-budget no-write receipts and owner-bound smoke cleanup prevent false receipts/cross-operation cleanup IDs; MCP empty/invalid/failed roots resolve to no project (#871, #876, #888, #918, #921, #923, #924, #927, #936).

- **Daemon SQLite busy**: lease renew retries Busy/locked (#929); startup busy/lock exits 75; systemd avoids churn beside extra MCP holders (#931); post-merge REST install recycles daemon (#928, #940); Hermes writes authoritative; local conclusions avoid breaker retrips (#941).
- **MCP search deadlines**: test helper bounds embed/DB/route so fixtures cannot hang on 240s.
- **Admission fixture**: retry admission Busy in oversubscribe; suite waits aren't budget failures.
- **Hermetic live-daemon tests**: isolate REST/MCP/ingest-wait/mark-failed/dashboard so exact-gates skip live `:3080`; timeout wrapper reaps owned trees, authenticates idle sccache via exe, treats absent /proc as exit (#988, #989, #973, #991, #993, #1011).
- Lease/Busy/backoff/cancel (#882/#889/#890,#944/#956,#958/#961/#962/#965/#971/#975/#976/#968,#1013,#1010,#1009,#1008,#1023/#1024,#1006/#1007,#1027,#1005,#1029,#1004,#1031,#980,#1003,#1002,#1001,#1035,#999,#1037).
- **MCP delete retry fixture**: force observed SQLite Busy before synchronized lock release (#886).

- **Daemon supervisor cooldowns**: wait through active restart-budget
  cooldowns and retry bootstrap in-process; true temporary refusals retain
  `75` (`EX_TEMPFAIL`), while the canonical unit keeps
  `RestartPreventExitStatus=75` for thrash protection (#847, #868).

- **Daemon restart recovery**: keep the persisted restart cooldown anchored to
  its original deadline when a supervisor races replacement generations, freeze
  fault history for the full cooldown (including after the rolling window ages
  out), attribute faults to a monotonic admission epoch so same-second prior
  faults do not stick to a healthy replacement, keep same-generation
  post-admission faults charged, and replenish the rolling restart budget only
  after a daemon generation finishes startup without post-admission faults;
  cooldown-blocked retries can no longer extend an outage indefinitely (#844).

- **Daemon/MCP/CLI coexistence**: both typed temporary admission refusals—restart-budget cooldowns and still-held live `sqlite-writer` leases after bounded retry—map to exit `75` (`EX_TEMPFAIL`) to prevent systemd restart thrash (#843, #849, #850, #853, #856, #858, #859, #864, #869, #870).

- **MCP reads**: retry transient SQLite busy/locked errors on all query-only MCP reads (read_drawer, search, timeline, context, brief) under the existing search deadline, preventing opaque -32603 under daemon contention (#840).
- **SQLite mutation retries**: bound shared 10-second CLI/MCP retry backoff, writer admission, and SQLite busy waits; a mutation that commits reports success, while unstarted expired MCP deletes return structured retryable lock errors (#838).
- **MCP delete**: retry cached async-pool SQLite busy/locked soft deletes for 10 seconds, matching CLI content mutations (#836).
- **MCP ingest**: preserve queued `wait=true` recovery when daemon hook IPC and
  REST are unavailable, retaining an operation receipt for reconciliation (#834).
- **CLI search**: reserve BM25 fallback budget so hybrid search cannot consume
  the full CLI deadline (#833).
- **CLI writes/search**: retry pin, unpin, and delete SQLite locks for 10 seconds; enforce a 120-second search deadline, including embedder initialization and bounded runtime teardown (#831).
- **MCP full smoke**: follow accepted ingest wait-timeout operation receipts to
  recover cleanup-safe drawer IDs before classifying create or update as missing
  IDs (#829).
- **MCP context admission**: return an actionable structured `admission_blocked`
  diagnostic when a saturated profile holder budget prevents opening the
  query-only context pool (#824).
- **MCP context scope**: replace the shared `RetrievalScopeRequest` on
  `mempal_context` with a dedicated `deny_unknown_fields` `ContextScopeRequest`
  so search-only fields (`wing`, `room`, `session`, `memory_kind`, `tier`,
  `status`, `anchor_kind`, `include_global`) are rejected by schema instead of a
  runtime allow-then-reject path (#823).
- **DB admission fixture**: extend the `pid_namespace_mcp_holder` registration
  and reap deadlines from 10s to 30s and replace `yield_now()` busy-spins with a
  bounded 50ms readiness sleep so the test no longer starves its own fixture
  process under high host load (#823).
- **CLI/MCP ingest admission**: return a machine-readable `admission_blocked`
  receipt with holder/cache capacity and headroom plus empty cleanup IDs when
  create admission refuses; successful create receipts retain exact
  cleanup-safe drawer IDs (#821).
- **REST admission**: terminal `admission_blocked` receipt for `BudgetExceeded`;
  smoke recognizes dual `admission_blocked` no-write outcomes (#825).
- Add `maintenance rejudge rebind` CLI for safe checkpoint binding update (#819)
- Harden `test_daemon_sigterm_drains_running_ingest_async_before_reclaim` fixture isolation under full-suite admission contention
- **DB admission sidecars**: restrict lock/state files to owner-safe directories,
  reject symlink/hard-link/inode substitution, bound and version persisted state,
  reclaim only grammar-validated staged files, and fsync publication/removal
  ordering with deterministic crash recovery coverage (#796).
- **DB admission SQLite opens**: use the admitted canonical database path for
  raw and queue read-only connections, closing symlink-retarget TOCTOU windows
  between admission and SQLite open (#797).
- **Mind-model bootstrap tests**: use isolated SQLite/config fixtures and a
  fail-closed REST fallback address so they cannot reach a live daemon (#807).
- MCP ingest-worker tests: wait for I/O-burst telemetry before assertions; integration-visible `reset_io_burst_for_tests` isolates process-global observability (#808/#861).
- **Path-sensitive tests**: create daemon IPC, SQLite admission, and symlink
  identity fixtures below a bounded `/tmp` root (#810).
- **CLI integration tests**: route `ingest`/`delete` helpers through one
  Linux-only deadline-aware subprocess supervisor (spawn, bounded capture,
  pipe drain, TERM→KILL escalation, identity-safe reaping) so a hung CLI or
  pipe-holding descendant cannot stall `cargo test`/local gates indefinitely
  (#795). Stdin-write timeout paths now clean up the supervised child before
  panicking (no active-child Drop / `_exit(125)`), and timeout diagnostics
  measure wall time from before the helper call.
- **MCP admission**: reserve service holder seats for daemon/MCP, report
  reaped/reserved budget diagnostics, and fail closed on `wait=true`
  `mempal_ingest` when the MCP async pool cannot be admitted instead of
  accepting a queued receipt that can only time out (#809).
- **Rejudge apply**: add generation-bound artifact manifests, content-free typed
  OCC receipts, strict dry-run-to-execute receipt validation, and fail-closed
  schema/policy/file-hash/DB-generation checks while preserving soft-delete as
  the default (#794).
- **Rejudge**: confirmation backlog drain now circuit-breaks on the first
  retryable/cooldown failure instead of sweeping the entire backlog; adds
  `--max-confirmations` bound and sanitized aggregate diagnostics (#793).
- **MCP admission**: stale PID-namespace admission holders are now reaped on
  process exit; supervised test fixtures have bounded cleanup and output
  draining by deadline (#790).
- **Development gates**: accept receipt roots below an ignored, untracked
  checkout-local symlink without weakening Git tracking checks (#802).
- **DB admission**: preserve actionable SQLite diagnostics when a configured
  database path resolves to a directory, while retaining hard-link alias
  rejection (#791).
- **Development gates**: use fast read-only pre-commit checks, content-addressed
  exact-tree full-gate receipts, and exact-HEAD review validation before push;
  `push-reviewed` reuses receipts instead of rerunning expensive gates and
  reviews.
- **CLI/Delete**: scope exact-ID deletes to the resolved current project by default,
  add explicit `--project`, `--include-global`, and `--all-projects` selectors,
  and retain the project predicate in the soft-delete update itself (#712).
- **Daemon/hooks**: missing or unreadable hook payload handles are now
  dead-lettered as Terminal instead of retrying forever as
  `invalid_queue_payload` (#721).
- **install-from-source**: use `mise x rust@stable -- cargo` when mise is
  available, instead of raw `cargo` which requires a rustup default toolchain
  (#740).
- **DB admission**: enforce process-wide SQLite holder and page-cache budgets
  with cgroup-aware memory pressure diagnostics, canonical database file
  identity (symlink/hardlink rejection), PID-namespace-safe tri-state process
  liveness, writer-lease fenced mutations, and `args_os()` for panic-free
  non-UTF-8 argv handling (#680).
- **xurl/Hermes parser**: join every message to its matching session row so
  citation title, source, and cwd stay session-exact; filter rewound messages by
  enforcing `active = 1 OR compacted = 1` when Hermes `state.db` exposes those
  columns; and reconcile authoritative re-ingest snapshots by removing absent
  source rows from lexical and vector search within the requested scope. Legacy
  schemas without state columns preserve import-all behavior (#741).
- **Hermes/REST search**: end-to-end query deadline defaults to ~4 minutes
  (`api.search_query_deadline_secs = 240`), is hot-reloadable without a hard
  ceiling, is snapshotted at query admission, and is shared as remaining budget
  across embedding/DB/rerank/fallback. Hermes leaves the response-read timeout
  unbounded and lets each request's daemon-owned policy determine completion;
  stage defaults no longer truncate local models at 30s (#711).
- **Hermes/REST search**: when `bm25_fallback` is disabled, embedding timeout/exhausted
  primary budget and hybrid DB timeout again return `504 Gateway Timeout`
  instead of `200 []`, restoring the historical REST contract so backend
  failure is not indistinguishable from zero hits (#711).
- **Daemon/Hooks**: hook IPC queue admission now fails fast under SQLite write
  contention so clients can use durable fallback within their deadline, daemon
  claim polling backs off bounded SQLite lock streaks, and closed hook IPC
  clients are treated as expected disconnects instead of WARN noise (#703).
- **MCP/daemon writes**: detect a live daemon whose executable was deleted or
  replaced before write routing, and return redacted structured restart and
  retry-safety diagnostics through REST, MCP, and Hermes `mempal_conclude`
  instead of a generic unavailable error (#701).
- **Build/ONNX**: pin `ort` and `ort-sys` to `2.0.0-rc.12`, configure the ONNX
  feature for dynamic loading, and run its test gate against a checksum-pinned
  official ONNX Runtime 1.24.2 shared library. This keeps every ONNX-enabled
  build away from rc.12's prebuilt static archive, which references glibc 2.38
  `__isoc23_*` symbols and cannot link on glibc 2.36 with mold or lld (#698).
- **Tests**: initialize the embedder fallback fixture through the complete
  database migration path before queue admission (#699).
- **Tests**: keep daemon IPC and path-sensitive project/provenance fixtures
  under bounded external paths so the default test suite works with deeply
  nested, repository-backed `TMPDIR` values (#696).
- **Security/Dependencies**: upgraded `crossbeam-epoch`, `quick-xml`,
  `quinn-proto`, and `rmcp` past the five affected RustSec versions, and
  upgraded `anyhow` and `rand` to their patched releases. The remaining
  informational audit warnings are temporarily accepted because `daemonize`
  has no patched release and `core2`, `number_prefix`, `paste`, and
  `proc-macro-error2` arrive through upstream dependency chains that require
  broader replacements than this security-only update (#694).
- **Ingest/Admission**: MCP and REST ingest now reject content above a 10 MiB
  product limit before scrubbing or queue admission, active ingest queues enforce
  aggregate byte budgets, REST extractor-level body rejection returns the same
  structured `payload_too_large` error and metrics, failed-row retries preserve
  the active ingest byte budget, automatic endpoint recovery keeps retrying rows
  skipped by that budget after capacity frees, LLM gate copies are bounded, and
  status surfaces only content-free byte/rejection counters (#678).
- **CLI**: stdout writes now treat a closed downstream pipe as a successful
  early-consumer exit, preventing `mempal daemon status | head` and sibling
  CLI output paths from panicking on `BrokenPipe` (#690).
- **Daemon/Queue**: archived queue-failure stats now use a covering completion
  index, eliminating the idle stall detector's repeated full-table page-cache
  scans on large completion histories (#687).
- **Observability**: daemon stall checks now record Queue IO burst samples, so
  cached SQLite read regressions remain attributable without syscall tracing.
- **Daemon**: writer-lease recovery now binds daemon PIDs to process-start
  identities on every liveness path. Embedder status validates the Linux
  process-start identity plus daemon pidfile and fails closed elsewhere (#685).
- **MCP**: `mempal_ingest` now performs bounded synchronous self-recovery when
  queue admission is blocked by the current MCP server's own SQLite holder,
  avoiding self-deadlock without inventing a durable operation row (#681).
- **Hooks**: passive hook stdin admission now reads at most the 10 MiB inline
  limit plus one sentinel byte, records only aggregate oversized-payload
  diagnostics, spools accepted payloads above 64 KiB by handle, and keeps
  oversized or medium raw bodies out of queue/IPC envelopes (#676).
- **Queue**: `PendingMessageStore` now reuses cached writer and reader SQLite
  connections across enqueue, confirm, status, retry, and archive paths,
  eliminating non-claim connection churn under WAL mode (#674).
- **Daemon/Queue**: idle hook and LLM workers now reuse claim SQLite
  connections and exponentially back off empty queue polls, reducing idle
  daemon `rchar` from connection churn (#672).
- **MCP**: `mempal_context` and `mempal_brief` now use a reader-only async
  database pool, so read-only MCP surfaces stay available while the daemon owns
  the writer-capable pool (#670).
- **Telemetry**: operation_telemetry records now populate metadata_json with stage, search_mode, timed_out, and detached task lifecycle fields, enabling diagnosis of timed-out/detached DB reads without strace (#665)
- **Plugin**: Hermes mempal/mempal-hooks plugins now detect degraded daemon responses (200-OK with warnings/timeouts) and enter shared backoff cooldown. Prefetch uses cheap retrieval mode with reduced top_k. Ingest suppressed when breaker is open (#663)
- **API/Core**: Bounded REST/MCP DB reads now cancel the underlying SQLite operation via `progress_handler` when the deadline fires, preventing detached `spawn_blocking` tasks from continuing to scan `palace.db` after the async timeout (#661)
- **API**: Default `GET /api/status` now returns a cheap bounded snapshot. Expensive DB-wide status fields moved behind `?diagnostic=true` (#662)

- `mempal doctor`, `mempal daemon status`, and the full smoke runner now expose
  embedding degraded/write-refused state, sanitized endpoint details, and queue
  terminal failure counts so stale OpenAI-compatible embedding endpoints are
  actionable from standard diagnostics.
- `mempal brief` and MCP `mempal_brief` now bound query embedding by
  `embed.retry.search_deadline_secs`, fall back to BM25-only briefs when
  configured, and return deterministic non-empty no-results briefs instead of
  hanging or emitting blank output.
- MCP `mempal_ingest` `wait=true` now has subprocess regression coverage
  proving finite queued receipts under daemon and MCP worker writer ownership.
- `mempal ingest --stdin --wait` now uses daemon/queue admission when the
  daemon or MCP ingest worker owns the SQLite writer lease, returning the
  existing wait receipt instead of failing with a writer-lease conflict.
- Daemon/MCP ingest queue-claim idle and SQLite-lock retries now use the
  bounded backoff loop, record queue IO burst samples, and index stale-claim
  heartbeat checks so idle large databases stop sustained logical reads (#684).
- Project resume timestamp ordering now uses ISO 8601 string comparison
  instead of `CAST(added_at AS INTEGER)`, which collapsed RFC 3339 timestamps
  to their year prefix.
- Cowork peek now canonicalizes relative cwd paths to absolute before
  querying partner sessions, preventing silent no-session results.
- Shell injection prevention in `mempal_projects_resume` next-step command:
  drawer-controlled path and wing values are POSIX single-quote escaped.
- REST ingest now returns runtime-designated `created_drawer_ids` /
  `cleanup_drawer_ids` for newly-created drawers, so the full smoke runner
  can safely prove reversible cleanup without treating informational
  `drawer_id`/`drawer_ids` as cleanup authority.
- `mempal daemon status` now uses a bounded 2-second DB-holder scan instead
  of an unbounded `/proc/*/fd` traversal, preventing spurious 30-second
  timeouts under a healthy singleton daemon.
- The daemon-backed `mempal ingest --wait --json` regression test now
  terminates its foreground daemon with deterministic SIGTERM/SIGKILL cleanup
  and PID/log-tail diagnostics, reducing local-gates flakiness.

### Changed

- Default embedding, LLM, search, reranker, and daemon DB-holder scan timeouts
  are more generous for quality-first local model deployments on edge hardware.
- Scoped pre-push CHANGELOG guard that requires an `[Unreleased]` entry only
  when `src/**/*.rs` files changed on the branch.
- Current architecture overview at `docs/architecture.md`, linked from README,
  to document the post-fork module map, data flow, runtime surfaces, and the
  boundary between current behavior and historical specs.
- Product surface classification in `docs/architecture.md`, linked from README,
  to mark stable, advanced, and experimental CLI/MCP/REST compatibility
  expectations.
- MCP tool profiles in `docs/architecture.md` and `docs/usage.md`, grouping the
  19 verified MCP baseline tools into default agent, knowledge management, and
  workspace/skills profiles.
- Fact-check contract documentation in `docs/architecture.md` and
  `docs/usage.md`, clarifying that fact-check is a deterministic, pattern-based,
  advisory pre-ingest guard rather than a truth oracle.
- Cowork READ/SEND/PERSIST semantics in `docs/architecture.md` and
  `docs/usage.md`, clarifying live partner reads, ephemeral handoffs, and
  durable memory writes.
- Benchmark roadmap documentation in `docs/architecture.md`, covering current
  LongMemEval retrieval results and the missing write-quality,
  context-assembly, and end-to-end agent usefulness evaluation layers.
- Module boundary expectations in `docs/architecture.md`, documenting expected
  dependency direction and where to avoid business logic in surfaces.
- Context assembly model documentation in `docs/architecture.md`, covering
  pinned facts, tiered retrieval, budget enforcement, recency scoring,
  cognitive briefs, and timeline use.

### Changed

- **English-only documentation**: removed the Chinese README and local Chinese
  mdBook source, and updated book references to point to the English edition.
- **Documentation drift cleanup**: README and usage guide now document the same
  19 verified MCP baseline tools, cover the major current CLI command families,
  and mark the original design spec as a historical baseline where
  package-layout details are outdated.
- **Package identity**: `Cargo.toml` `repository`/`homepage`/`documentation` fields now point to `RyderFreeman4Logos/mempal` (the active fork) instead of upstream `ZhangHanDong/mempal`. README link references updated accordingly. Crate/binary names unchanged.

## [pre-fork Unreleased] — absorbed from upstream (fork remains 0.4.0)

This fork's package version remains `0.4.0`. The notes below record upstream
0.5.x/0.6.0 material absorbed during the 2026-06-05 sync; they are not released
versions of this fork.

### Upstream 0.6.0 material — 2026-06-05

Upstream feature release material. **P106: a read-only "distill signal" in
mind-model context.**

### Added

- **`mempal context` / `mempal_context` now carry `distill_suggestions`** — a
  read-only, deterministic signal that flags fields worth crystallizing into
  knowledge. The detector groups active drawers by `field` and surfaces a
  suggestion for each field with at least 5 active evidence drawers AND zero
  active promoted-or-canonical knowledge. It returns at most 3 suggestions
  (descending evidence count, then ascending field); each carries `field`,
  `evidence_count`, up to 3 `sample_evidence_drawer_ids`, and
  `suggested_tier="dao_ren"`. This is the "detector" layer of agent-driven
  mind-model construction: it makes "this is worth distilling" a client-agnostic,
  pull-based signal that appears where agents already look.
- On by default; disable per call with the CLI `--no-distill-suggestions` flag
  or the MCP `include_distill_suggestions=false` request field. `mempal brief`
  does not carry the signal.

### Notes

- Purely observational: the detector performs no database write, no LLM call, no
  auto-distill, and no auto-promotion. Acting on a suggestion stays the agent's
  explicit `mempal_knowledge_distill` plus the deterministic gate (governance per
  P77/P80 unchanged). It never alters the assembled tier sections.

### Upstream 0.5.4 material — 2026-05-30

Upstream bug-fix release material. **`purge_deleted` could silently drop triple
provenance when a hard delete was blocked by another foreign key** (a non-atomic
edge case left by 0.5.3's FK fix).

### Fixed

- **`purge_deleted` is now atomic.** 0.5.3 cleared `triples.source_drawer` before
  hard-deleting a drawer, but `purge_deleted` ran each statement outside a
  transaction. If the subsequent `DELETE FROM drawers` was blocked by another
  `RESTRICT` foreign key — e.g. `knowledge_evidence_links.evidence_drawer_id`,
  which protects a card's evidence — the `source_drawer = NULL` had already
  committed, silently dropping KG provenance for a drawer that was never purged.
  The purge loop is now wrapped in `BEGIN IMMEDIATE` / `COMMIT` / `ROLLBACK`, so
  a blocked delete rolls back the NULL too. The reindex replace path was already
  transactional and unaffected. Adds a regression test that blocks a purge with
  an evidence link and asserts the triple provenance survives.

### Upstream 0.5.3 material — 2026-05-29

Upstream bug-fix release material. **Reindex/purge crashed when deleting a
drawer referenced by a KG triple** (surfaced while self-healing the 0.5.2
duplicate cleanup).

### Fixed

- **Hard-deleting a drawer that a KG triple references no longer fails with
  `FOREIGN KEY constraint failed`.** `triples.source_drawer` is a `RESTRICT`
  FK to `drawers(id)` and mempal opens connections with `foreign_keys=ON`, so
  the across-rooms reindex replace (and `purge_deleted`) errored out and rolled
  back when a stale drawer was referenced by a triple. Both hard-delete paths
  now clear the dangling `source_drawer` pointer (`UPDATE triples SET
  source_drawer = NULL`) before deleting the drawer — the KG fact is kept, only
  its stale provenance link is dropped. Adds a regression test. Without this,
  `mempal reindex --stale` could not finish cleaning the 0.5.2-era duplicates.

### Upstream 0.5.2 material — 2026-05-29

Upstream bug-fix release material. **`reindex` left duplicate drawers when a
source re-routed to a different room.**

### Fixed

- **`reindex --stale` / `--force` no longer leave stale drawers behind when a
  source auto-routes to a new room.** Re-ingesting a source replaced its prior
  drawers only within the freshly resolved room
  (`replace_active_source_drawers` is keyed on `(source_file, wing, room)`). If
  taxonomy routing now sent the source to a different room than its existing
  drawers occupied, the old-room drawers were never deleted and coexisted with
  the new ones as duplicates — and their stale `normalize_version` could never
  be cleared. Reindex now deletes a source's prior drawers across **all** rooms
  via the new `Database::replace_active_source_drawers_across_rooms`, gated by a
  `replace_across_rooms` ingest option (reindex-only; normal ingest keeps
  room-scoped replace). Adds regression tests covering both the across-rooms
  delete and the room-scoped contrast. After upgrading, run `mempal reindex
  --stale` once to self-heal any duplicates left by an earlier version.

### Upstream 0.5.1 material — 2026-05-29

Upstream bug-fix release material. **0.5.0's MCP tool list failed to load in
strict clients.**

### Fixed

- **MCP tool list now loads in Claude Code (and other strict clients).** The
  `mempal_phase3` tool's `metadata` and `report` inputs are free-form JSON
  (`serde_json::Value`), for which schemars emits a boolean `true` property
  schema. Claude Code's Zod-based validator rejects a boolean property schema
  and then refuses the **entire** tool list (`tools[..].inputSchema.properties.
  {metadata,report}: Invalid input`), so all 23 tools silently disappeared in
  0.5.0. Both fields now advertise a concrete `{"type": "object"}` schema via a
  `schema_with` helper, and a regression test asserts they never revert to a
  boolean schema. CLI behavior is unchanged.

### Upstream 0.5.0 material — 2026-05-29

Upstream large feature release material covering P10–P105: the mind-model
knowledge layer, Phase-2 knowledge cards, Phase-3 runtime adoption evidence,
the multi-agent cowork bus, release/ops tooling, and the first Chinese mdBook.
Schema advances to **v9**. No breaking CLI removals; existing commands keep
their semantics.

### Added

- **Mind-model knowledge layer (P12–P29).** Typed drawers with `dao_tian /
  dao_ren / shu / qi / evidence` tiers and `global / repo / worktree` anchors;
  `mempal context` / `mempal_context` runtime context assembler; knowledge
  lifecycle CLI/MCP (`distill`, `gate`, `promote`, `demote`, `publish-anchor`);
  read-only promotion policy and field-taxonomy surfaces.
- **Phase-2 knowledge cards (P30–P48).** Schema v8 `knowledge_cards` /
  `knowledge_evidence_links` / `knowledge_events`; card core API, CLI, MCP
  read + gate/promote/demote/retrieve; Stage-1 → card backfill; card-aware
  context (`--include-cards`, opt-in).
- **Phase-3 runtime adoption evidence (P49–P82).** Schema v9
  `runtime_adoption_events`; `mempal phase3` (record/list/stats/gate/review/
  analytics/readiness/default-proposal/default-control/rollback-control),
  checked records, capture helpers, opt-in instrumentation wrapper, evaluator
  advisory API, research validate/ingest planning.
- **Cognitive brief (P83, P102).** `mempal brief` / `mempal_brief` —
  deterministic citation-first brief; no LLM, no DB writes.
- **Multi-agent cowork bus (P84–P96, P101).** Concrete `agent_id` registry +
  per-agent inbox, events log, delivery ack/status, presence/heartbeat,
  threads/channels, tmux transport + live peek, sessions, handoff summary, and
  explicit handoff-to-evidence capture; `mempal_cowork_bus` MCP surface.
- **Release & ops tooling (P97–P104).** `mempal doctor` / `mempal_doctor`,
  `mempal release-readiness`, `mempal maintenance guided-run`, maintenance &
  cowork runbooks, adoption analytics.
- **Book manuscript (P105).** Historical upstream material for a local mdBook
  manuscript is not retained in this fork's English-only documentation surface;
  use the public English book link from the README.

### Changed

- Storage schema advances to **v9**; `reindex --stale` migrates drawers behind
  the current `normalize_version`.
- `mempal_search` results carry AAAK-derived structured signals (`entities`,
  `topics`, `flags`, `emotions`, `importance_stars`); `content` stays raw.
- Published crate now also excludes `books/**` (mdBook manuscript + local
  Mermaid asset) alongside the existing `specs/**` and `docs/plans/**`.

### Notes

- Governance boundaries are deliberate: no silent promotion, evaluator stays
  advisory, research cannot define `dao` directly, and cowork runtime logs do
  not enter durable memory without explicit capture.

## [0.4.0] — 2026-04-20

First release with **write-safety** + **content-sanity** guarantees for the
Claude Code ↔ Codex cowork pair. Closes P9 (`specs/p9-fact-checker.spec.md` and
`specs/p9-ingest-lock.spec.md`).

### Added

- **`mempal_fact_check` MCP tool** (10th tool) and `mempal fact-check` CLI
  subcommand. Offline contradiction detection against the KG `triples` table
  and the AAAK entity registry. Flags three issue kinds:
  - `SimilarNameConflict` — mentioned name is ≤2 edit-distance from a known
    entity and not identical (typo / confusable).
  - `RelationContradiction` — text asserts a predicate that's in the
    incompatibility dictionary versus an existing KG triple with the same
    `(subject, object)` endpoints.
  - `StaleFact` — text asserts a triple whose KG row has `valid_to <
    now_unix_secs`.
  Pure read, zero LLM, zero network, deterministic.
- **Protocol Rule 11 "VERIFY BEFORE INGEST"** embedded in
  `mempal_status.memory_protocol`. Guides agents to call `mempal_fact_check`
  before ingesting decisions that assert entity relationships.
- **Per-source ingest lock** (advisory `flock` on Unix). Eliminates the
  TOCTOU race between concurrent `mempal_ingest` calls targeting the same
  source (Claude Code + Codex writing the same drawer simultaneously). Lock
  file lives at `~/.mempal/locks/<16-hex>.lock`; guard releases on drop.
- **`IngestStats.lock_wait_ms` / `IngestResponse.lock_wait_ms`** — optional
  field reporting how long the ingest call waited for the per-source lock.
  Non-zero values indicate observed concurrency with a peer agent. Omitted
  in dry-run and when the write path was bypassed.
- `IngestError::Lock` variant wrapping `ingest::lock::LockError`
  (`Timeout { path, timeout }` / `Io { path, source }` / `InvalidSourceKey`).

### Changed

- `MEMORY_PROTOCOL` tool list grew 9 → 10 entries; rule count 10 → 11.
- `src/aaak/mod.rs`: widened `codec` from `mod codec` to
  `pub(crate) mod codec` so the `factcheck` module can reuse
  `extract_entities` without duplicating logic. No external API change.

### Fixed

- Concurrent same-source ingest no longer produces duplicate drawers or
  mismatched `drawer_vectors` rows. Verified by the cross-thread
  `test_concurrent_ingest_same_source_single_drawer` integration test.

### Platform notes

- Linux and macOS have full lock enforcement via `flock(LOCK_EX | LOCK_NB)`
  implemented with inline `extern "C"` (no `libc` crate dependency).
- Windows currently runs a no-op fallback for the lock path — concurrent
  ingest on Windows is **not** race-protected in 0.4.0. Follow-up work will
  adopt `LockFileEx`.

### Compatibility

- Schema version unchanged (still `4`). Existing `~/.mempal/palace.db` files
  open without migration.
- No new runtime or dev-dependency in `Cargo.toml`.
- `mempal_ingest` response adds `lock_wait_ms` with
  `#[serde(skip_serializing_if = "Option::is_none")]`, so existing JSON
  consumers that ignore unknown fields see no change. Consumers that
  destructure the struct need to accept the new field.

### Internal

- New modules: `src/factcheck/{mod,names,relations,contradictions}.rs`,
  `src/ingest/lock.rs`.
- Tests added: 24 unit tests (18 factcheck + 6 ingest lock) and 18
  integration tests (10 `tests/fact_check.rs` + 8 `tests/ingest_lock.rs`),
  including a cross-thread concurrent-ingest race gate.
- Project spec index (`CLAUDE.md`) promoted `p9-fact-checker.spec.md` and
  `p9-ingest-lock.spec.md` to "completed" and registered five new draft
  specs (P10 explicit tunnels, P10 normalize_version, P11 diary daily
  rollup, P11 chunk neighbors, P11 transcript noise strip).

---

## [0.3.1] — 2026-04-16

### Fixed

- `mempal_cowork_push` now recognizes `codex-mcp-client` as a valid Codex
  MCP client identity (the actual string Codex sends per
  `codex-rs/codex-mcp/src/mcp_connection_manager.rs`). Previously, pushes
  from Codex were rejected with "cannot infer caller tool" even when
  Codex was correctly connected.

---

## [0.3.0] — 2026-04-14

First release shipping the full **Claude ↔ Codex cowork** stack (P6 + P7 +
P8) on top of hybrid search and the knowledge graph.

### Added

- **P6 — `mempal_peek_partner` MCP tool**: read the partner agent's live
  session log (Claude `.jsonl` transcripts, Codex rollout files) in place,
  without ingesting or mutating anything. Use for "what is the other agent
  doing right now" across Claude Code and Codex.
- **P6 — Memory Protocol Rules 8 & 9**: "PARTNER AWARENESS" and
  "DECISION CAPTURE" guidance embedded in `mempal_status`.
- **P7 — Structured AAAK-derived signals in search results**: every
  `mempal_search` hit now carries `entities`, `topics`, `flags`,
  `emotions`, `importance_stars` alongside raw `content`. Agents can
  filter by `DECISION` / `TECHNICAL` flags and rank by stars without
  parsing AAAK text.
- **P8 — `mempal_cowork_push` MCP tool**: send a short ephemeral handoff
  (≤ 8 KB, up to 16 pending / 32 KB per inbox) to the partner agent's
  inbox. Delivery is at-next-UserPromptSubmit, not real-time.
- **P8 — CLI commands**:
  - `mempal cowork-drain --target <claude|codex>` — drain inbox from a
    hook; exits 0 on any failure (graceful degrade).
  - `mempal cowork-status --cwd <PATH>` — read-only inbox inspection.
  - `mempal cowork-install-hooks [--global-codex]` — one-shot installer
    for the symmetric UserPromptSubmit hook on both Claude Code and
    Codex, idempotent and self-healing.
- **P8 — Memory Protocol Rule 10 "COWORK PUSH"**.
- Crate exclude list for `cargo package` — `.claude/**`, `.mcp.json`,
  `AGENTS.md`, `CLAUDE.md`, `hooks/**`, `specs/**`, `docs/plans/**` now
  stay out of the published tarball.

### Known limitations (see README)

- Codex `codex_hooks` feature flag must be enabled (`codex features
  enable codex_hooks`); `install-hooks` detects and warns.
- Codex TUI caches config at startup; restart after enabling the flag or
  re-running `install-hooks`.
- Claude Code spawns the mempal MCP server at client startup — restart
  Claude Code after upgrading the mempal binary so newly added tools
  (e.g. `mempal_cowork_push`, `mempal_fact_check` in 0.4.0) are visible.
- `mempal_cowork_push` requires the MCP client to identify as Claude or
  Codex via `ClientInfo.name` (by design for the Claude ↔ Codex pair).

---

## Earlier versions

Earlier releases (0.1.x, 0.2.x) are tracked only in Git history. Run
`git log --oneline` on the repository to inspect them.

[Unreleased]: https://github.com/RyderFreeman4Logos/mempal/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.4.0
[0.3.1]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.3.1
[0.3.0]: https://github.com/ZhangHanDong/mempal/releases/tag/v0.3.0

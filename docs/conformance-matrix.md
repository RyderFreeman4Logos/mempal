# Feature Preservation Conformance Matrix

This matrix is the compatibility inventory for the installed/live mempal
product surface. It is intentionally operational: every feature row names the
owner surface, current status, verification kind, exact verification handle,
acceptable skip or degraded reasons, and any intentional behavior change.

`skills/smoke-test/scripts/full_smoke.py` emits an aggregate
`conformance` object keyed by the feature groups in this document. The smoke
output is content-safe: it reports probe labels, counts, status, and error
classes only, never drawer content, prompts, snippets, headers, secrets, or raw
command output.

## Status and Verification Vocabulary

| Term | Meaning |
| --- | --- |
| `supported` | Part of the current compatibility contract. Regressions need fixes or explicit migration notes. |
| `experimental` | Implemented and usable, but command names, payload fields, policy, or storage behavior may change before 1.0. |
| `deprecated` | Intentionally retained only for transition, with a named replacement. |
| `known_broken` | A currently accepted breakage with issue link and expected replacement or repair path. |
| `unit` | Verified by deterministic tests that do not require the installed live daemon. |
| `integration` | Verified by cargo integration tests or feature-gated harnesses. |
| `installed-live smoke` | Verified by the installed `mempal` binary and live daemon through `skills/smoke-test/scripts/full_smoke.py`. |
| `manual-only` | Requires an operator because it is destructive, environment-specific, or not safe for routine smoke. |

Current known-broken count: **0**. The live CRUD regression tracked during the
Issue #647 release cycle is not classified as `known_broken`; CRUD remains
supported, and a live CRUD failure should be treated as a regression.

## Core CLI Memory Lifecycle

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| CLI-001 | Ingest memories from stdin or directory | `supported` | CLI, daemon queue, REST, MCP | installed-live smoke, integration | `python3 skills/smoke-test/scripts/full_smoke.py` probes `cli_create`, `cli_update`; `mise x rust@stable -- cargo test -p mempal --test write_wait_cli`; `mise x rust@stable -- cargo test -p mempal --features integration --test harness_smoke` | Embedder or daemon may return degraded/queued receipt; operation must remain observable and cleanup-safe. | Raw drawer storage remains the contract; derived vectors/cards are separate. |
| CLI-002 | Receipt-backed operation status and wait | `supported` | CLI, MCP | unit, installed-live smoke | `mempal operation status <OPERATION_ID> --json`; `mempal operation wait <OPERATION_ID> --timeout-secs 300 --json`; `full_smoke.py` helpers `wait_operation` and `recover_created_ids`; `write_wait_cli` | Skipped only when a create path returns direct drawer IDs instead of an operation receipt. | Async writes are not hidden behind best-effort fire-and-forget. |
| CLI-003 | Search indexed memories | `supported` | CLI, REST, MCP | installed-live smoke, integration | `mempal search <QUERY> --json`; `full_smoke.py` probes `cli_search_created_match`, `mcp_search_created_match`; `harness_smoke` | BM25 fallback is acceptable when vector embedder is unavailable, but warnings must be reported. | Search returns citations and drawer IDs; no uncited answer surface replaces it. |
| CLI-004 | Context assembly | `supported` | CLI, MCP | installed-live smoke, integration | `mempal context <QUERY> --format json --max-items 3 --no-distill-suggestions`; `full_smoke.py` probes `cli_context_created`, `mcp_context`; `harness_smoke` | Query embedding may degrade to lexical context only if warnings are surfaced. | `wake-up` is not the typed context replacement; use `context` or `brief` for dao/shu/qi guidance. |
| CLI-005 | Cognitive brief | `supported` | CLI, MCP | installed-live smoke, integration | `mempal brief <QUERY>`; `full_smoke.py` probe `mcp_brief`; `harness_smoke` | Skipped if MCP tool is not advertised in the installed binary. | Brief is deterministic summarization over assembled context, not a hidden LLM call. |
| CLI-006 | Drawer view/read | `supported` | CLI, MCP | installed-live smoke | `mempal view <DRAWER_ID> --all-projects`; MCP `mempal_read_drawer`, `mempal_read_drawers`; `full_smoke.py` probes `cli_read_view`, `cli_read_updated`, `mcp_read_drawer`, `mcp_read_drawers`, `mcp_read_updated` | Requires a drawer ID created by the same smoke run or an operator-provided ID. | Full raw read is explicit; search preview truncation is expanded only through read tools. |
| CLI-007 | Timeline and tail dashboard reads | `supported` | CLI, MCP | installed-live smoke | `mempal timeline --since 1h --format json`; `mempal tail --limit 3`; MCP `mempal_timeline`; `full_smoke.py` probes `timeline_json`, `tail_shape`, `mcp_read_timeline` | Empty database is acceptable if command shape succeeds. | CLI dashboard replaces any web UI requirement. |
| CLI-008 | Pinned facts read | `supported` | CLI, MCP | installed-live smoke | `mempal pinned --json`; MCP `mempal_pinned_facts`; `full_smoke.py` probes `pinned_json`, `cli_pinned_before`, `cli_pinned_after`, `mcp_read_pinned_facts` | Empty pinned list is acceptable. | Pinned recall is SQL-only and bypasses embedding lookup. |
| CLI-009 | Pin and unpin existing drawers | `supported` | CLI | installed-live smoke | `mempal pin <DRAWER_ID>`; `mempal unpin <DRAWER_ID>`; `full_smoke.py` probes `cli_pin`, `cli_unpin` | Requires cleanup-authorized smoke drawer ID. | MCP has a read-only pinned facts tool; mutation stays CLI/operator-owned for now. |
| CLI-010 | Delete and soft-delete | `supported` | CLI, MCP | installed-live smoke, unit | `mempal delete <DRAWER_ID>`; MCP `mempal_delete`; `full_smoke.py` probes `cli_delete_batch`, `mcp_delete_batch`, `cli_crud`, `mcp_crud`; `test_created_ids_from_accepts_only_cleanup_safe_fields` | Only IDs returned by the same smoke run may be cleaned automatically. | Delete is soft-delete with audit metadata, not permanent removal. |
| CLI-011 | Permanent purge of soft-deleted drawers | `supported` | CLI | manual-only | `mempal purge --before <REVIEWED_ISO_TIMESTAMP>`; availability check `mempal purge --help` | Skipped in automated smoke because it is destructive beyond the smoke-created ID set. | Purge remains operator-gated; routine smoke uses soft-delete cleanup only. |
| CLI-012 | Repair anti-pattern listing | `experimental` | CLI | installed-live smoke | `mempal repair list --json`; `full_smoke.py` probe `repair_json` | Empty findings are acceptable. | Repair is an experimental maintenance surface and is not a replacement for tests. |
| CLI-013 | Dashboard stats | `supported` | CLI | installed-live smoke | `mempal stats`; `full_smoke.py` probe `stats_shape` | Empty database is acceptable. | Stats output stays aggregate-only. |
| CLI-014 | Wake-up context refresh | `supported` | CLI | manual-only, integration | `mempal wake-up`; `mempal wake-up --format aaak`; covered indirectly by context/search harnesses | Skipped in content-safe live smoke because it can print memory text by design. | Wake-up remains L0/L1 refresh; typed guidance uses `mempal context`. |

## Typed and Project Metadata

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| META-001 | `wing` metadata | `supported` | CLI, REST, MCP, storage | installed-live smoke | `mempal ingest --wing smoke ...`; `full_smoke.py` probes `cli_create`, `mcp_create`, `cli_search_created_match`, `mcp_search_created_match` | None for smoke-created drawers. | Wing remains top-level routing scope. |
| META-002 | `room` metadata | `supported` | CLI, REST, MCP, storage | installed-live smoke | `mempal ingest --room cli ...`; MCP ingest `room=mcp`; `full_smoke.py` match probes verify room-scoped marker counts | None for smoke-created drawers. | Room remains optional sub-scope under wing. |
| META-003 | `project` / project scope | `supported` | CLI, REST, MCP, storage | integration, installed-live smoke | `mempal search <QUERY> --project <PROJECT>`; `mempal view <ID> --all-projects`; `harness_smoke`; `full_smoke.py` read probes use `all_projects` | Smoke may run all-projects to clean up its own IDs safely. | Project isolation is enforced at retrieval/storage boundaries; tunnel exceptions are explicit. |
| META-004 | `source` / `source_file` citation | `supported` | CLI, REST, MCP, search results | integration | `harness_smoke`; `mempal search <QUERY> --json` result fields `drawer_id` and `source_file` | Directory/stdin sources can have different source_file shapes; citation field must be present. | Source citation remains required for search results. |
| META-005 | `source_type` metadata | `supported` | CLI, REST, MCP, storage | installed-live smoke | `mempal ingest --source-type agent_inference ...`; `full_smoke.py` probes `cli_create`, `mcp_create` | None for smoke-created drawers. | Typed metadata is stored alongside raw content; it does not rewrite content. |
| META-006 | `memory_kind` metadata | `supported` | CLI, REST, MCP, storage | installed-live smoke | `mempal ingest --memory-kind evidence ...`; `full_smoke.py` probes `cli_create`, `mcp_create` | None for smoke-created drawers. | Evidence and governed knowledge have separate lifecycle rules. |
| META-007 | `domain` metadata | `supported` | CLI, REST, MCP, context/search | installed-live smoke | `mempal ingest --domain project ...`; `mempal search --domain project`; `full_smoke.py` probes `cli_create`, `mcp_brief` | None for smoke-created drawers. | Domain is a typed retrieval dimension, not wing/room routing. |
| META-008 | `field` metadata and recommended taxonomy | `supported` | CLI, MCP, context/search | installed-live smoke | `mempal field-taxonomy --format json`; MCP `mempal_field_taxonomy`; `full_smoke.py` probes `field_taxonomy_json`, `mcp_read_field_taxonomy` | Custom fields remain valid even if not in recommendations. | Field taxonomy is guidance only. |
| META-009 | Validity and confidence metadata | `supported` | CLI, storage, KG-adjacent search | manual-only, integration | `mempal ingest --confidence <N> --valid-from <TS> --valid-until <TS>`; search `--include-expired`; targeted cargo tests when changed | Skipped in broad smoke to avoid clock-sensitive assertions. | Expiry does not hard-delete drawers; it controls retrieval filters. |

## Daemon and Service Behavior

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| SVC-001 | Installed binary identity | `supported` | CLI | installed-live smoke | `mempal --version`; `command -v mempal`; `full_smoke.py` probe `version` | None. | Smoke uses installed binary, not `cargo run`. |
| SVC-002 | Daemon singleton and current binary | `supported` | daemon, CLI | installed-live smoke | `mempal daemon status`; `/proc/<pid>/exe` consistency; `full_smoke.py` probes `daemon_status_pre`, `binary_consistency` | Daemon not running may be classified, but a deleted/replaced daemon binary is a failure. | Singleton live daemon is preferred over unmanaged duplicate processes. |
| SVC-003 | DB holder classification | `supported` | daemon, CLI, doctor | installed-live smoke | `mempal daemon status`; `full_smoke.py` `holders_after`; status DB holder summaries | Extra holders may be reported as degraded if they are external to the smoke runner. | Diagnostics are aggregate: roles/counts/PIDs only, no command lines or payloads. |
| SVC-004 | Queue and operation receipts | `supported` | daemon, CLI, MCP | installed-live smoke, unit | `mempal status`; `mempal queue failed`; `mempal operation wait/status`; `full_smoke.py` operation receipt helpers; `write_wait_cli` | Failed terminal queue rows may exist, but output must stay payload-free. | Async write observation is receipt-backed. |
| SVC-005 | Schema compatibility and migrations | `supported` | core storage, doctor | installed-live smoke, integration | `mempal doctor --format json`; `full_smoke.py` probe `doctor_json_validation`; `harness_smoke` | Operational warnings may be non-fatal; schema mismatch is not acceptable. | SQLite `PRAGMA user_version` remains the upstream schema axis; fork extensions use separate metadata. |
| SVC-006 | Endpoint and degraded runtime status | `supported` | CLI, daemon, MCP, REST | installed-live smoke | `mempal status`; `mempal doctor --format json`; MCP `mempal_status`; `full_smoke.py` probes `doctor_json`, `mcp_status_last` | Embedding endpoint outage can degrade search, but warnings must be visible. | No silent cloud fallback is implied by status. |

## REST Availability and Key Routes

REST is feature-gated. When the installed binary lacks `rest` support or the
daemon REST endpoint is intentionally disabled, rows in this group may be
reported as skipped. When REST is enabled and configured, route availability is
part of the supported surface.

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| REST-001 | REST doctor and route inventory | `supported` | CLI, REST | installed-live smoke | `mempal doctor rest --format json`; `full_smoke.py` probe `doctor_rest` | REST feature disabled, daemon not running, or endpoint intentionally unreachable. | `doctor rest` is the route availability gate. |
| REST-002 | `GET /api/status` | `supported` | REST, Hermes plugin | installed-live smoke | `mempal doctor rest --format json`; manual `curl http://127.0.0.1:3080/api/status` | REST disabled or wrong loopback port. | Status route shares daemon-owned runtime state. |
| REST-003 | `GET /api/search` | `supported` | REST, Hermes plugin | installed-live smoke, integration | `mempal doctor rest --format json`; manual `curl 'http://127.0.0.1:3080/api/search?q=...&scope=project'`; `harness_smoke` | Embedder outage may return BM25/fallback warnings instead of failing. | Bounded REST search returns warnings instead of hanging. |
| REST-004 | `POST /api/ingest` | `supported` | REST, daemon queue, Hermes plugin | installed-live smoke | `full_smoke.py` fallback probes `cli_create_rest_fallback`, `cli_update_rest_fallback`, `mcp_create_rest_fallback`, `mcp_update_rest_fallback` when needed; manual `curl -X POST /api/ingest` | REST fallback probes are expected only when direct write path is blocked by daemon writer lease. | REST fallback keeps smoke cleanup possible but must not mask MCP ingest failure. |
| REST-005 | `GET /api/taxonomy` | `supported` | REST | installed-live smoke | `mempal doctor rest --format json`; manual `curl http://127.0.0.1:3080/api/taxonomy` | REST disabled. | Read-only route. |
| REST-006 | `GET /api/timeline` | `supported` | REST, Hermes compatibility | installed-live smoke | `mempal doctor rest --format json`; manual `curl http://127.0.0.1:3080/api/timeline` | REST disabled or empty database. | Timeline is aggregate/citation surface, not raw dump by default. |
| REST-007 | `GET /api/pinned_facts` | `supported` | REST | installed-live smoke | `mempal doctor rest --format json`; manual `curl http://127.0.0.1:3080/api/pinned_facts` | REST disabled or empty pinned set. | SQL-only pinned recall semantics match CLI/MCP. |
| REST-008 | Hermes compatibility delete route | `experimental` | REST, Hermes plugin | manual-only | `mempal doctor rest --format json`; targeted Hermes provider validation | Skipped unless validating Hermes integration. | Hermes plugin surface is experimental; CLI/MCP memory lifecycle remains canonical. |

## MCP Tool Surface

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| MCP-001 | Tool discovery baseline | `supported` | MCP | installed-live smoke | `mempal serve --mcp` protocol `tools/list`; `full_smoke.py` probe `mcp_tools_list` | Tool not advertised is a failure for required baseline tools, skip for optional newer tools. | Runtime-advertised list is source of truth for installed binary. |
| MCP-002 | `mempal_status` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_status_last` | Skipped only if tool is not advertised, which is a baseline regression. | Status includes warnings and protocol discovery. |
| MCP-003 | `mempal_search` | `supported` | MCP | installed-live smoke | `full_smoke.py` probes `mcp_search`, `mcp_search_created_match` | Search may degrade to BM25 with warnings. | Search results must include citations. |
| MCP-004 | `mempal_context` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_context` | Skipped only if tool is not advertised. | Context is typed guidance, not wake-up. |
| MCP-005 | `mempal_brief` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_brief` | Skipped only if tool is not advertised. | Brief uses deterministic assembled context. |
| MCP-006 | `mempal_read_drawer` / `mempal_read_drawers` | `supported` | MCP | installed-live smoke | `full_smoke.py` probes `mcp_read_drawer`, `mcp_read_drawers`, `mcp_read_updated` | Requires cleanup-authorized created drawer ID. | Full raw read is explicit and ID-scoped. |
| MCP-007 | `mempal_pinned_facts` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_read_pinned_facts` | Empty pinned facts are acceptable. | SQL-only recall. |
| MCP-008 | `mempal_timeline` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_read_timeline` | Empty recent timeline is acceptable. | Timeline complements, not replaces, context. |
| MCP-009 | `mempal_doctor` | `supported` | MCP | installed-live smoke | `full_smoke.py` probe `mcp_read_doctor` | Operational warnings can be non-fatal; schema mismatch is failure. | Diagnostics must stay aggregate-only. |
| MCP-010 | `mempal_ingest` | `supported` | MCP, daemon queue | installed-live smoke, unit | `full_smoke.py` probes `mcp_create`, `mcp_update`; MCP hard-timeout wrapper; operation receipt recovery; `test_mcp_error_reaps_child_and_checkpoints_before_fallback` | REST fallback may keep cleanup possible but does not make `mcp_create` pass; fallback is blocked until every runner-owned MCP child is reaped. | Current live CRUD failure is a regression if it returns. |
| MCP-011 | `mempal_delete` | `supported` | MCP | installed-live smoke | `full_smoke.py` probes `mcp_delete_batch`, `mcp_crud` | Requires cleanup-authorized created drawer IDs. | Soft-delete only. |
| MCP-012 | `mempal_operation_status` | `supported` | MCP | installed-live smoke, unit | `full_smoke.py` probe `mcp_operation_status`; `mcp_tools_list` required baseline; `write_wait_cli` covers receipt state behavior | Skipped when direct drawer IDs are returned and no operation receipt exists. | Receipt-backed observation remains the write contract. |
| MCP-013 | Knowledge, taxonomy, field, KG, and skill helper tools | `supported` | MCP | installed-live smoke | `full_smoke.py` probes `mcp_read_field_taxonomy`, `mcp_read_taxonomy`, `mcp_read_kg`, `mcp_read_skill`; targeted tests when mutating these areas | Empty state is acceptable. | Governance mutation tools still require their own policy gates. |

## Embedding and Search Behavior

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| SRCH-001 | Hybrid vector + BM25 retrieval | `supported` | CLI, REST, MCP, search core | installed-live smoke, integration | `mempal search <QUERY> --json`; `full_smoke.py` probes `cli_search_created_match`, `mcp_search_created_match`; `harness_smoke` | Vector endpoint outage may degrade to BM25. | Hybrid is the default search contract. |
| SRCH-002 | BM25 fallback with warnings | `supported` | CLI, REST, MCP, doctor/status | installed-live smoke, integration | `mempal status`; `mempal doctor --format json`; `harness_smoke` fallback assertions when changed | Acceptable only when warnings describe degraded vector/embedder state. | Fallback must not silently hide vector failures. |
| SRCH-003 | Degraded embedder/runtime warnings | `supported` | CLI, daemon, REST, MCP | installed-live smoke | `full_smoke.py` probes `doctor_json`, `doctor_json_validation`, `mcp_status_last`; `mempal status --full` | Endpoint health warnings are acceptable; schema or critical corruption warnings are not. | OpenAI-compatible/LAN endpoints are configurable, not hard-coded. |
| SRCH-004 | Bounded query and REST deadlines | `supported` | REST, CLI search core | integration, manual-only | `mempal doctor rest --format json`; targeted `harness_smoke`; manual bounded REST search | REST disabled. | Long DB stages return warnings/partial fallback rather than unbounded hangs. |
| SRCH-005 | Bounded context and brief behavior | `supported` | CLI, MCP | installed-live smoke | `mempal context <QUERY> --max-items 3`; `mempal brief <QUERY>`; `full_smoke.py` probes `cli_context_created`, `mcp_brief` | Empty database is acceptable; created smoke drawer should be found. | Brief/context do not mutate memory. |
| SRCH-006 | Progressive disclosure read path | `supported` | MCP, search/read core | installed-live smoke | MCP `mempal_read_drawer`, `mempal_read_drawers`; `full_smoke.py` read probes | Requires a drawer ID. | Truncated search previews must expose `content_truncated`/size metadata when active. |
| SRCH-007 | Reindex after embedder change | `supported` | CLI, embedder/storage | manual-only, integration | `mempal reindex`; targeted tests for reindex paths | Skipped in smoke because it is expensive and mutates all vectors. | Backend/model/dimension changes require explicit reindex; no automatic silent re-embed. |
| SRCH-008 | Search reranker endpoint, scoring, and fallback visibility | `supported` | CLI, search core, local reranker endpoint | installed-live smoke, unit | `full_smoke.py` conformance group `search_reranker_behavior` probes `reranker_endpoint_reachable`, `reranker_reorders_results`, `reranker_fallback_warning`; `mise x rust@stable -- cargo test -p mempal rerank` | Smoke skips when `[search.reranker] enabled=false`; fallback warning is unit-covered so smoke does not deliberately break the live endpoint. | Reranker is quality-first when enabled: endpoint shape must be valid, later relevant documents must score higher, and failures must warn while preserving original ranking. |

## Privacy, Diagnostics, and Cleanup Safety

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| SAFE-001 | No raw content in diagnostics | `supported` | CLI, daemon, REST, MCP, smoke runner | unit, installed-live smoke | `full_smoke.py` final JSON; `test_conformance_report_does_not_copy_raw_probe_fields`; smoke skill safety rules | None. A leak is a failure. | Shape/count summaries replace raw stdout/stderr in smoke reports. |
| SAFE-002 | Cleanup-safe ID authority and crash recovery | `supported` | CLI, MCP, smoke runner | unit, installed-live smoke | `test_created_ids_from_accepts_only_cleanup_safe_fields`; `test_manifest_is_private_atomic_and_contains_only_cleanup_ids`; `test_manifest_survives_partial_cleanup_and_deletes_after_verified_cleanup`; `test_pending_manifest_finalization_discloses_only_private_path`; `full_smoke.py` cleanup probes `cli_delete_batch`, `mcp_delete_batch`; failure JSON `cleanup_manifest_path` | No automatic cleanup if create failed before returning cleanup-authorized IDs; an unresolved-ID manifest is retained until cleanup is verified. | Search/read result IDs are not cleanup authority; diagnostics expose the mode-`0600` manifest path, never its IDs. |
| SAFE-003 | Queue diagnostics without payloads | `supported` | CLI, daemon | manual-only, installed-live status | `mempal queue failed`; `mempal status`; queue help text states aggregate-only output | Failed queue rows may exist; payloads must not be printed. | Queue recovery is filtered and dry-run by default. |
| SAFE-004 | Destructive cleanup guardrails | `supported` | CLI, MCP | installed-live smoke, manual-only | Soft-delete through exact IDs in `full_smoke.py`; purge manual row CLI-011 | Purge skipped in automated smoke. | Soft-delete is routine cleanup; hard purge is operator-reviewed. |
| SAFE-005 | Remote-call privacy status | `supported` | CLI, config, doctor/status | manual-only, installed-live status | `mempal cost status`; `mempal status`; `mempal doctor --format json` | Environment-specific endpoints may be unreachable. | No hidden remote LLM dependency is allowed by default. |

## Experimental and Adjacent Surfaces

| ID | Feature | Status | Owner surface(s) | Verification kind | Verification handle | Acceptable skip/degraded reason | Intentional change or replacement |
| --- | --- | --- | --- | --- | --- | --- | --- |
| EXP-001 | Hermes profile/session recall | `experimental` | CLI, Hermes plugin, REST | manual-only | `mempal hermes --help`; `mempal hermes search <QUERY>` in a configured Hermes profile; Hermes provider validation | Skipped unless Hermes data/config is present. | Preferred production path is Hermes provider/hooks against daemon REST, not long-lived stdio MCP. |
| EXP-002 | Cowork bus, channels, sessions, tmux transport | `experimental` | CLI, MCP | manual-only | `mempal cowork-runbook`; targeted cowork tests when changing `src/cowork/` | Requires multiple agent instances or configured tmux target. | SEND is ephemeral coordination, not durable memory. |
| EXP-003 | Sleep, reflect, crystallize, cards, phase3, insight, foresight, case | `experimental` | CLI, MCP where exposed | manual-only, targeted tests | Command-specific help and targeted tests for changed modules | Skipped in full smoke unless a row graduates to supported core. | Experimental governance surfaces do not replace raw evidence drawers. |
| EXP-004 | Hotpatch suggestions | `experimental` | CLI, filesystem | manual-only | `mempal hotpatch --help`; targeted hotpatch tests | Requires operator-reviewed target files. | Suggestions are gated; automatic apply is not a stable contract. |

## Intentional Changes and Deprecations

| Area | Classification | Rationale | Replacement or current path |
| --- | --- | --- | --- |
| Historical multi-crate workspace in early specs | Intentional change | Current repository is a single Cargo package and single installed binary. Historical specs/plans are design history. | Use `cargo install --path . --locked --features rest` and current `src/` module map. |
| Web UI for visualization | Intentional non-feature | Project policy is CLI dashboard and local tools, not a web UI. | `mempal tail`, `mempal timeline`, `mempal stats`, `mempal audit`, and REST only for API/Hermes integration. |
| `wake-up` as typed guidance | Intentional split | Wake-up is a compact L0/L1 refresh, not full dao/shu/qi assembly. | Use `mempal context` or `mempal brief` for typed runtime guidance. |
| Direct hard deletion in routine smoke | Intentional exclusion | Automated smoke must clean up only exact created IDs and must not permanently purge unrelated data. | Smoke uses `delete`; operators run `purge` manually after review. |
| REST always-on assumption | Intentional optionality | REST is feature-gated and daemon-configured. | `mempal doctor rest --format json` classifies availability; CLI/MCP remain canonical when REST is disabled. |
| Hermes plugin compatibility | Experimental | Hermes integration depends on external profile data and provider behavior. | Validate through Hermes-specific manual or targeted tests; stable memory lifecycle remains CLI/MCP/REST core. |

No row is currently `deprecated`. If a future change deprecates a feature, add
a `deprecated` row above before removing or changing the old behavior.

## Smoke Integration

Run the installed-live conformance smoke from the repository root:

```bash
python3 skills/smoke-test/scripts/full_smoke.py
```

The final JSON includes:

```json
{
  "conformance": {
    "schema": "mempal_conformance_smoke_v1",
    "matrix": "docs/conformance-matrix.md",
    "groups": {
      "core_cli_memory_lifecycle": {
        "status": "pass",
        "feature_count": 7,
        "failed_probes": null,
        "skipped_reason": null
      }
    },
    "summary": {"pass": 0, "fail": 0, "skipped": 0}
  }
}
```

Only the final JSON line should be parsed. Do not paste raw smoke logs into
issues or chats; summarize `overall_ok`, `failures`, `cleanup`, and
`conformance.summary`.

## Change Control Rule

Future automated PRs that change any core surface must either update this
matrix and the matching smoke/tests, or state why the affected feature status
and verification handle are unchanged. At minimum, review this file when a PR
touches:

- `src/main.rs`
- `src/mcp/`
- `src/api/`
- `src/search/`
- `src/ingest/`
- `src/core/queue.rs`
- `docs/usage.md`
- `docs/architecture.md`
- `skills/smoke-test/`

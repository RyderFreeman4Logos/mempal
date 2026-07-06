# Issue 670 MCP Async DB Caller Audit

Scope: remaining MCP-side calls to the writer-capable
`MempalMcpServer::async_db()` after moving bounded query-only reads to
`QueryOnlyAsyncDb`.

## Remaining Writer-Capable Call Sites

| Call site | Classification | Rationale |
| --- | --- | --- |
| `soft_delete_drawer_for_mcp` | writer required | `mempal_delete` mutates drawer state through `run_write`; it must keep the writer-capable pool and existing self-holder retry fallback. |
| `load_status_db_snapshot` | intentionally diagnostic/status | `mempal_status` reports writer-capable pool health, resource snapshots, and lock diagnostics. Hiding this behind `QueryOnlyAsyncDb` would remove the diagnostic signal that status is meant to expose. |
| `run_read_anyhow_bounded` | reader-only eligible, deferred | This generic helper still opens `AsyncDb` for non-query-only bounded search reads. It is eligible for a later migration, but #670 specifically removes the writer-pool dependency from `mempal_context` and `mempal_brief` via `run_query_only_read_bounded`. |
| `mempal_status` ingest-gating status read | intentionally diagnostic/status | The status response includes gating/runtime counters and may degrade into warnings when the writer-capable pool is unavailable; it is not a user query surface. |
| `resolve_replacement_target_async` | writer required | Ingest admission resolves supersede/replace targets before enqueueing writer work and intentionally preserves the existing lock retry behavior. |
| `test_mcp_delete_uses_current_server_writer_after_status_pool_load` | intentionally diagnostic/status test | Test-only direct call that preloads the current MCP writer-capable pool to verify self-holder delete behavior. |

## Query-Only Read Surfaces

The following MCP read paths now go through `run_query_only_read_bounded` or
`run_query_only_read_anyhow_bounded`, which opens `QueryOnlyAsyncDb` and never a
writer connection: `mempal_timeline`, `mempal_pinned_facts`,
`mempal_read_drawer`, `mempal_read_drawers`, `mempal_projects_list`,
`mempal_projects_resume`, `mempal_context`, `mempal_brief`,
`daemon_writer_lease_visible_for_ingest_wait`, and read-only KG/taxonomy/status
helper probes wired through the query-only helper.

## Issue 670 Boundary

#670 fixes the read-surface availability regression where `mempal_context` and
`mempal_brief` failed with `failed to open MCP async database pool` while a
daemon-owned writer lease made the writer-capable pool unavailable. The remaining
writer-capable call sites above either mutate state, intentionally expose
diagnostic pool health, or remain explicitly deferred outside the context/brief
regression scope.

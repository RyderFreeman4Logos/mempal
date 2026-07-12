# Reversible CLI and MCP CRUD Smoke

Use this reference when the smoke must exercise writes. Prefer `scripts/full_smoke.py` for broad coverage; use these manual steps for targeted diagnosis.

## Cleanup authority

Only delete IDs returned by the same smoke run as `created_drawer_ids` (or a documented alias that exactly mirrors that field). Never delete `drawer_id`, `drawer_ids`, search hits, read results, or marker matches unless they are also in the runtime-created cleanup-safe list.

If a create/update does not expose cleanup-safe IDs, fail closed: skip update/delete, classify the API gap, and avoid guessing.

Before each write or fallback, the automated runner atomically checkpoints its
current cleanup-safe IDs in a mode-`0600` file under `/tmp`. The manifest contains
only IDs returned by the current run; it never contains marker text, requests,
responses, or memory content. Verified cleanup removes IDs from the manifest and
deletes the file when no IDs remain. On failure with unresolved IDs, the final JSON
reports `cleanup_manifest_path` plus `cleanup_pending_count` without copying any IDs into diagnostics. Resume
cleanup only from that exact manifest; never reconstruct authority from search.

## Reversible CLI CRUD outline

1. Generate a unique marker.
2. Ingest via stdin with a smoke scope and JSON output:
   - `wing=smoke`, `room=cli`
   - `source_type=agent_inference`, `memory_kind=evidence`, `domain=project`, `field=smoke`
   - `--no-gate --wait --wait-timeout-secs 90 --json`
3. Extract only `created_drawer_ids`; if absent but an `operation_id` is returned, close any MCP stdio holder first, then follow with `mempal operation wait <id> --timeout-secs 300 --json` or `operation status`.
4. Read/query without printing content:
   - `mempal view <created_id> --all-projects` to temp file; summarize byte count only.
   - `mempal search <marker> --top-k 5 --json`; summarize count/shape only.
   - `mempal context <marker> --format json --max-items 3 --no-distill-suggestions`; summarize field names only.
   - `mempal pinned --json`; summarize type/count only.
5. Update by replacement semantics with `mempal ingest ... --supersedes <created_id> ... --json`; require new `created_drawer_ids`.
6. Pin/unpin exact smoke IDs, then `mempal delete <id>` for each created/update ID. Suppress raw delete output.
7. Re-run marker search and require no active `smoke/cli` matches, or classify tombstone visibility if only include-deleted surfaces show them.

## Reversible MCP CRUD outline

Use MCP tools already exposed to the active client when available. If not, `scripts/full_smoke.py` may start short-lived `mempal serve --mcp` stdio children because it owns shutdown and kills leftovers. Do not manually leave unmanaged MCP servers running.

An MCP timeout or JSON-RPC error must close and reap the current stdio child before
the runner attempts CLI observation, REST fallback, or CLI cleanup. If any
runner-owned MCP child survives the bounded shutdown sweep, the fallback is blocked
and the smoke fails with aggregate lifecycle counts and roles only.

Required MCP coverage when available:

| Step | Tool/method | Assertion |
|---|---|---|
| initialize/discover | JSON-RPC initialize + `tools/list` | server name present; expected tools listed |
| status | `mempal_status` | structured object; holder fields summarized only |
| create | `mempal_ingest` with `wing=smoke`, `room=mcp`, `smoke=true`, `wait=true` | terminal success and non-empty `created_drawer_ids` |
| read/search | `mempal_search`, `mempal_read_drawer`, optionally `mempal_read_drawers` | structured parse; no content printed |
| context/read-only | `mempal_context`, `mempal_pinned_facts`, `mempal_timeline`, `mempal_doctor`, optional taxonomy/brief/skill tools | shape-only success or classified skip/failure |
| update | `mempal_ingest` with `supersedes=<created_id>` and same smoke scope | replacement `created_drawer_ids` non-empty |
| delete | `mempal_delete` for exact create/update IDs | `deleted=true` or explicit already-deleted duplicate cleanup |
| post-delete | `mempal_search` marker query | no active `smoke/mcp` marker matches |
| shutdown | `shutdown` + exit notification | child exits; no extra MCP/DB holder remains |

Capture MCP responses to temp files and report only method/tool, JSON-RPC error class, latency, structuredContent top-level fields, result counts, and cleanup-created ID counts. Never print raw response bodies, content, snippets, previews, prompts, drawer text, or model output.

## Real memory write option

When the operator asks for a genuine write smoke, store a concise non-secret note that will be useful later. Keep it project-scoped and typed. Example metadata: `wing=project`, `room=mempal`, `source_type=agent_inference`, `memory_kind=knowledge`, `domain=project`, `field=ops`.

After writing, wait for completion, verify by exact safe query with shape/count only, and do **not** delete it because it is intentional durable memory.

---
name: smoke-test
description: "Project-local mempal exhaustive smoke test skill. Use after installing, restarting, merging, or changing mempal to verify daemon health, CLI/REST/MCP read surfaces, reversible CLI and MCP memory CRUD, memory growth, and database-holder safety without starting duplicate daemons or leaking drawer content."
---

# mempal Manual Smoke Test

Use this project-local skill when a main agent must manually verify that the installed `mempal` binary, live daemon, command-line surfaces, REST availability, and MCP tools still work after a merge, install, restart, MCP reconnect, or production-like debugging session.

Default to the installed CLI plus the MCP tools already exposed to the active client. Do not start extra daemons or long-lived REST/MCP servers unless the test explicitly requires them and the owner process is tracked for cleanup.

## Safety rules

1. Keep diagnostics aggregate-only. Do not print raw drawer content, prompts, model responses, raw process command lines, environment variables, connection strings, URLs, Authorization headers, bearer tokens, API keys, passwords, `drawer_content`, or prompt-like arguments.
2. Use the installed binary (`command -v mempal`, `mempal --version`) and the live user daemon. Do not run `cargo run` for smoke unless debugging source changes.
3. Maintain singleton ownership:
   - Prefer `systemctl --user restart mempal-daemon.service` for restart.
   - Verify exactly one `/usr/local/bin/mempal daemon --foreground` after restart.
   - Do not run `mempal serve` as a long-lived REST server for smoke unless a route test requires it; if started, track and kill it.
   - Do not spawn unmanaged `mempal serve --mcp` processes from the shell. The checked-in `scripts/full_smoke.py` runner may start short-lived MCP stdio servers because it owns their PID lifecycle, sends shutdown/exit, and kills leftovers.
4. Before declaring a lock failure, inspect DB holders with `mempal daemon status` and summarize only roles/counts/PIDs/commands.
5. Full smoke means read-only probes plus reversible memory CRUD through both CLI and MCP when the active client exposes MCP tools. If MCP tools are unavailable, record `mcp_crud=skipped_unavailable` and do not spawn an unmanaged MCP holder just to satisfy the matrix.
6. If using a synthetic write, use a unique marker, a `smoke/cli` or `smoke/mcp` wing-room, explicit typed metadata, and bounded wait/poll behavior, then clean up only exact drawer ID(s) returned by the create/update operation's `created_drawer_ids`. Never delete IDs discovered from generic search/read results. Report IDs only when cleanup fails; do not print content.
7. Update smoke must use replacement semantics (`--supersedes` / `supersedes`) against an exact drawer ID created earlier in the same smoke run. If the initial create did not expose a cleanup-safe created ID, skip update/delete and fail closed.
8. If there is already context that should become durable memory, it is acceptable to ingest a concise real note instead of synthetic content, but only when the note is genuinely useful and non-secret.

## Preferred automated full smoke

Use the checked-in runner first when the goal is broad coverage rather than one-off diagnosis:

```bash
python3 skills/smoke-test/scripts/full_smoke.py | tee /tmp/mempal-skill-full-smoke.log
```

The runner is intentionally part of this repo-local skill (`skills/smoke-test/scripts/full_smoke.py`) rather than `/tmp` so the exact CLI/MCP protocol, safety rules, and cleanup behavior stay versioned with the skill. It prints one aggregate JSON object and avoids raw memory content.

Runner behavior:

- CLI CRUD: JSON stdin `mempal ingest --stdin --json`, `search`, `view`, `context`, `pin`, `unpin`, `ingest --supersedes`, exact-ID `delete`, post-delete search verification.
- MCP read-only probes: short-lived isolated MCP stdio servers per optional probe to avoid one tool's SQLite read lock poisoning the rest of the matrix.
- MCP CRUD: `mempal_ingest`, `mempal_operation_status`, `mempal_search`, `mempal_read_drawer`, `mempal_read_drawers`, `mempal_context`, `mempal_brief`, `mempal_ingest` with `supersedes`, `mempal_delete`, post-delete search verification.
- Timed-out MCP ingest receipts: close the MCP server before following the returned `operation_id` with `mempal operation wait --json`; this avoids MCP self-holder SQLite lock false failures.
- Cleanup: if a later step fails after create exposed cleanup-safe IDs, delete those exact IDs before exiting.
- I/O telemetry: sample daemon `/proc/<pid>/io` before/after when the daemon PID stays stable, sample `/proc/<pid>/io` for CLI child commands and short-lived MCP stdio servers, and keep `resource.getrusage(RUSAGE_CHILDREN)` block counters as an aggregate fallback. The runner also appends content-free telemetry to `target/smoke-io-history.jsonl`.

If the runner exits nonzero, parse only the final JSON summary:

```bash
python3 - <<'PY'
import json, pathlib
line = [line for line in pathlib.Path('/tmp/mempal-skill-full-smoke.log').read_text(errors='replace').splitlines() if line.strip()][-1]
data = json.loads(line)
print({'overall_ok': data.get('overall_ok'), 'failures': data.get('failures'), 'cleanup': data.get('cleanup'), 'created_counts': data.get('created_counts'), 'io': data.get('io')})
for name in data.get('failures', []):
    print(name, data.get('groups', {}).get(name))
PY
```

Do not paste raw command output from the log into chat; the JSON summary is already redacted/aggregate-only.

## Preflight

Run from the repository root:

```bash
git status --short --branch --untracked-files=all
command -v mempal
mempal --version
systemctl --user is-active mempal-daemon.service || true
systemctl --user show mempal-daemon.service -p MainPID -p ActiveState -p SubState --value || true
ps -C mempal -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,comm --sort=-rss || true
mempal daemon status | awk '/^(status:|pid:|uptime_secs:|memory\.|live_daemons:|extra_holders:|stale_mcp_servers:|orphan_daemons:|search\.active:|rest\.health:|rest\.embedder_cache\.|embedder\.)/ {print}'
```

Record:

- installed version;
- daemon PID;
- `exe_deleted`;
- `live_daemons`;
- `extra_holders` / `stale_mcp_servers` / `orphan_daemons`;
- RSS/PSS/private dirty/anonymous memory;
- queue counts;
- `search.active`;
- REST health summary.

Do not record raw command lines, process arguments, environment variables, URLs, connection strings, bearer tokens, prompt-like arguments, or daemon status sections that contain drawer bodies.

## Restart procedure

When restart is allowed or needed:

```bash
systemctl --user restart mempal-daemon.service
sleep 3
main_pid=$(systemctl --user show mempal-daemon.service -p MainPID --value)
systemctl --user is-active mempal-daemon.service
ps -p "$main_pid" -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,comm
readlink "/proc/$main_pid/exe"
printf 'daemon_processes=%s\n' "$(pgrep -fc '^/usr/local/bin/mempal daemon --foreground$' || true)"
printf 'mcp_server_processes=%s\n' "$(pgrep -fc '^/usr/local/bin/mempal serve --mcp$' || true)"
```

Pass criteria:

- service is `active`;
- exactly one daemon process exists;
- daemon exe resolves to `/usr/local/bin/mempal` and is not deleted;
- no untracked extra `mempal serve --mcp` processes were created by the smoke;
- RSS is reasonable immediately after restart (normally hundreds of MB, not multi-GB).

## Coverage target

Run the broadest safe matrix the current environment supports. A full report should classify each group as `pass`, `fail`, or `skipped_<reason>`.

| Group | Surface | Required coverage |
|---|---|---|
| Runtime | CLI/systemd/process | installed binary, daemon singleton, current executable, DB holders, RSS/resource summary |
| Read-only memory | CLI | status, stats, search, context, timeline/tail shape, pinned shape, knowledge/card/repair/pattern/skill surfaces where supported |
| Reversible CRUD | CLI | create via `mempal ingest`, read via `search` + `view`, update via `ingest --supersedes`, pin/unpin, delete via exact created IDs, post-delete verification |
| REST | CLI doctor or tracked REST server | route/health shape and degraded warning categories |
| Read-only MCP | MCP tools | `mempal_status`, `mempal_search`, `mempal_context`, `mempal_pinned_facts`, `mempal_timeline`, `mempal_doctor`, plus optional knowledge/brief/taxonomy/skill tools exposed by the client |
| Reversible CRUD | MCP tools | create via `mempal_ingest`, read via `mempal_search` + `mempal_read_drawer`, update via `mempal_ingest` with `supersedes`, delete via `mempal_delete`, post-delete search verification |
| Cleanup | CLI/process | no extra daemon/MCP/REST holders, all exact-created smoke IDs soft-deleted or explicitly reported |

Never downgrade a failed CRUD path to pass because another surface worked. Report CLI CRUD and MCP CRUD separately.

## Read-only CLI smoke matrix

Do not paste raw stdout or stderr from `mempal` commands into chat, issues, or logs unless the command is explicitly listed as safe-direct below. For every other command, capture stdout/stderr to temporary files and report only:

- exit code;
- latency;
- stdout/stderr byte counts;
- JSON/NDJSON parse success/failure;
- top-level JSON type, field names, line counts, and item counts;
- health/status/warning strings only after redaction and only when they do not include drawer/card/prompt content.

Treat these commands as content-bearing by default and never print their raw stdout in chat: `mempal status`, `mempal status --full`, `mempal timeline`, `mempal tail`, `mempal pinned`, `mempal knowledge-card list`, `mempal cards --pending`, `mempal reflect`, `mempal prime`, `mempal wake-up`, `mempal context`, `mempal search`, `mempal recall hermes search`, and any command that returns drawers, cards, recall snippets, evidence, timeline entries, prompts, model responses, or previews.

Simple non-content identity commands may be displayed directly only when their output is known not to include memory content, for example `mempal --version`. For `doctor`, `doctor rest`, `daemon status`, stats, cost, and gating commands, still prefer summarized fields and redacted warning categories over raw output.

Use a capture harness for all content-bearing or uncertain commands:

```bash
run_mempal_probe() {
  name="$1"
  expect_json="$2"
  shift 2
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/mempal-smoke.XXXXXX")"
  stdout_file="$tmpdir/stdout"
  stderr_file="$tmpdir/stderr"
  start_ms="$(date +%s%3N)"
  "$@" >"$stdout_file" 2>"$stderr_file"
  status=$?
  end_ms="$(date +%s%3N)"
  python3 - "$name" "$expect_json" "$status" "$start_ms" "$end_ms" "$stdout_file" "$stderr_file" <<'PY'
import json
import sys
from pathlib import Path

name, expect_json, status, start_ms, end_ms, stdout_path, stderr_path = sys.argv[1:]
stdout = Path(stdout_path).read_bytes()
stderr = Path(stderr_path).read_bytes()
summary = {
    "name": name,
    "exit_code": int(status),
    "latency_ms": max(0, int(end_ms) - int(start_ms)),
    "stdout_bytes": len(stdout),
    "stderr_bytes": len(stderr),
}

if expect_json == "json":
    try:
        parsed = json.loads(stdout.decode("utf-8") or "null")
    except Exception as exc:
        lines = [line for line in stdout.decode("utf-8", errors="replace").splitlines() if line.strip()]
        try:
            parsed_lines = [json.loads(line) for line in lines]
        except Exception:
            summary["json"] = {"ok": False, "error_type": type(exc).__name__}
        else:
            field_names = sorted({
                key
                for item in parsed_lines
                if isinstance(item, dict)
                for key in item.keys()
            })
            summary["json"] = {
                "ok": True,
                "type": "ndjson",
                "line_count": len(parsed_lines),
                "fields": field_names,
            }
    else:
        if isinstance(parsed, dict):
            summary["json"] = {
                "ok": True,
                "type": "object",
                "fields": sorted(parsed.keys()),
                "field_count": len(parsed),
            }
        elif isinstance(parsed, list):
            summary["json"] = {"ok": True, "type": "array", "count": len(parsed)}
        else:
            summary["json"] = {"ok": True, "type": type(parsed).__name__}

print(json.dumps(summary, sort_keys=True))
PY
  rm -rf "$tmpdir"
  return "$status"
}
```

The harness prints structure only. It must not print command stdout/stderr, JSON values, drawer IDs from content-bearing output, search snippets, previews, prompts, or model responses.

| Group | Command | Output handling | Pass criteria |
|---|---|---|---|
| Identity | `mempal --version` | safe-direct | exits 0, expected version string |
| Daemon | `mempal daemon status` | summarize/redact | exits 0, running, singleton/current binary |
| Doctor | `mempal doctor` | summarize/redact | exits 0; warning categories summarized |
| REST doctor | `mempal doctor rest --format json` | harness JSON/NDJSON | exits 0, JSON parses, routes reported; degraded is allowed only with warning recorded |
| Dashboard | `mempal status` | content-bearing harness | exits 0 |
| Stats | `mempal stats` | summarize/redact | exits 0 |
| Config | `mempal config intelligence` | summarize/redact | exits 0 |
| Cost | `mempal cost status` | summarize/redact | exits 0 |
| Gating | `mempal gating stats` | summarize/redact | exits 0 |
| Timeline | `mempal timeline --since 1h --format json` | content-bearing harness JSON/NDJSON | exits 0, empty output or valid JSON/NDJSON accepted |
| Tail | `mempal tail --limit 3` | content-bearing harness | exits 0 |
| Pinned | `mempal pinned --json` | content-bearing harness JSON/NDJSON | exits 0, JSON parses |
| KG | `mempal kg stats` | summarize/redact | exits 0 |
| Cards | `mempal knowledge-card list --format json` and `mempal cards --pending --format json` | content-bearing harness JSON/NDJSON | exits 0, JSON parses |
| Reflection | `mempal reflect --json --limit 3` | content-bearing harness JSON/NDJSON | exits 0, JSON parses |
| Prime | `mempal prime --format json --token-budget 512 --no-stats` | content-bearing harness JSON/NDJSON | exits 0, JSON parses |
| Wake-up | `mempal wake-up --format protocol` | content-bearing harness | exits 0 |
| Taxonomy | `mempal field-taxonomy --format json` | harness JSON/NDJSON | exits 0, JSON parses |
| Integrations | `mempal integrations status` | summarize/redact | exits 0 |
| Checkpoint | `mempal checkpoint status` | summarize/redact | exits 0 |
| Patterns/skills/repair | `mempal patterns list`, `mempal skills list`, `mempal repair list` | summarize/redact | exits 0 |
| Cowork | `mempal cowork-status --cwd "$PWD"` | summarize/redact | exits 0 |
| Maintenance | `mempal maintenance guided-run --format json` | harness JSON/NDJSON | exits 0, JSON parses |
| Release | `mempal release-readiness --format json` | harness JSON/NDJSON | exits 0, JSON parses |
| xurl | `mempal xurl stats` | summarize/redact | exits 0 |
| Benchmark | `mempal bench matrix --mode no-llm --top-k 3 --format json` | harness JSON/NDJSON | exits 0, JSON parses |
| Recall help | `mempal recall hermes --help` | safe-direct | exits 0 |

For expensive semantic queries, run them only when needed and bound time explicitly:

```bash
run_mempal_probe search json timeout 180s mempal search '<known safe query>' --top-k 3 --json
run_mempal_probe context json timeout 120s mempal context '<known safe query>' --format json --max-items 3 --no-distill-suggestions
```

If these are slow, report latency and memory growth; do not treat slowness as pass unless the task only asked for availability.

## Reversible CLI memory CRUD smoke

Run this by default for full smoke unless the operator explicitly requests read-only mode. The flow must exercise create, read, update, pin/unpin, search, context, and delete through the command line.

1. Generate a unique marker:

   ```bash
   marker="mempal-smoke-$(date +%s)-$RANDOM"
   ```

2. Ingest via stdin with a smoke scope, capture the response, and extract only cleanup-safe IDs explicitly returned by ingest:

   ```bash
   ingest_json="$(mktemp)"
   operation_json=""
   smoke_ids="$(mktemp)"
   if python3 - "$marker" <<'PY' \
     | mempal ingest --stdin --wing smoke --room cli --source-type agent_inference --memory-kind evidence --domain project --field smoke --no-gate --wait --wait-timeout-secs 90 --json \
     > "$ingest_json"; then
import json, sys
marker = sys.argv[1]
print(json.dumps({
    "content": f"{marker} reversible CLI smoke drawer; safe to delete",
    "wing": "smoke",
    "room": "cli",
    "source_type": "agent_inference",
    "memory_kind": "evidence",
    "domain": "project",
    "field": "smoke",
}))
PY
     :
   fi
   python3 - "$ingest_json" > "$smoke_ids" <<'PY'
import json
import sys
from pathlib import Path

obj = json.loads(Path(sys.argv[1]).read_text() or "{}")
ids = []
drawer_ids = obj.get("created_drawer_ids")
if isinstance(drawer_ids, list):
    ids.extend(item for item in drawer_ids if isinstance(item, str) and item)
for item in dict.fromkeys(ids):
    print(item)
PY
   if [ ! -s "$smoke_ids" ]; then
     operation_id="$(python3 - "$ingest_json" <<'PY'
import json
import sys
from pathlib import Path

obj = json.loads(Path(sys.argv[1]).read_text() or "{}")
operation_id = obj.get("operation_id")
state = obj.get("state")
if isinstance(operation_id, str) and operation_id and state not in {"completed", "rejected", "failed"}:
    print(operation_id)
PY
)"
     if [ -n "$operation_id" ]; then
       operation_json="$(mktemp)"
       if mempal operation wait "$operation_id" --timeout-secs 300 --json > "$operation_json"; then
         wait_status=0
       else
         wait_status=$?
       fi
       if [ "$wait_status" -ne 0 ]; then
         mempal operation status "$operation_id" --json > "$operation_json" || true
       fi
       python3 - "$operation_json" > "$smoke_ids" <<'PY'
import json
import sys
from pathlib import Path

obj = json.loads(Path(sys.argv[1]).read_text() or "{}")
if obj.get("state") != "completed":
    raise SystemExit(0)
ids = []
drawer_ids = obj.get("created_drawer_ids")
if isinstance(drawer_ids, list):
    ids.extend(item for item in drawer_ids if isinstance(item, str) and item)
for item in dict.fromkeys(ids):
    print(item)
PY
     fi
   fi
   if [ ! -s "$smoke_ids" ]; then
     echo "cleanup requires manual operator action: terminal response did not expose created_drawer_ids; do not delete drawer_id, drawer_ids, or cleanup_drawer_ids" >&2
     rm -f "$ingest_json" "$operation_json" "$smoke_ids"
     exit 1
   fi
   ```

   Do not print the ingest response, operation response, or smoke IDs unless cleanup fails. `created_drawer_ids` from the terminal ingest JSON, or from terminal `mempal operation wait/status --json`, is the cleanup authority. `cleanup_drawer_ids` is only a human-readable alias when it mirrors the created list. `drawer_id` and `drawer_ids` are informational because they may name pre-existing, deduplicated, dropped, or merge-target drawers. If `created_drawer_ids` is empty after the terminal wait/status step, fail closed.

3. Read and query the created memory without printing content:

   ```bash
   created_id="$(head -n 1 "$smoke_ids")"
   view_out="$(mktemp)"
   search_json="$(mktemp)"
   context_json="$(mktemp)"
   pinned_json="$(mktemp)"

   mempal view "$created_id" --all-projects > "$view_out"
   mempal search "$marker" --top-k 5 --json > "$search_json"
   mempal context "$marker" --format json --max-items 3 --no-distill-suggestions > "$context_json"
   mempal pinned --json > "$pinned_json"

   python3 - "$view_out" "$search_json" "$context_json" "$pinned_json" <<'PY'
import json, sys
from pathlib import Path
view_out, search_path, context_path, pinned_path = [Path(p) for p in sys.argv[1:]]
results = json.loads(search_path.read_text() or "[]")
context = json.loads(context_path.read_text() or "{}")
pinned = json.loads(pinned_path.read_text() or "[]")
summary = {
    "view_stdout_bytes": view_out.stat().st_size,
    "search_count": len(results) if isinstance(results, list) else None,
    "context_fields": sorted(context.keys()) if isinstance(context, dict) else [],
    "pinned_type": type(pinned).__name__,
}
print(json.dumps(summary, sort_keys=True))
PY
   rm -f "$view_out" "$search_json" "$context_json" "$pinned_json"
   ```

   The summary is aggregate-only. Do not print `view`, search result content, context sections, pinned text, snippets, or previews. Search result IDs are not cleanup authority.

4. Update by replacement semantics, then verify the replacement ID:

   ```bash
   created_id="$(head -n 1 "$smoke_ids")"
   update_json="$(mktemp)"
   update_ids="$(mktemp)"
   if python3 - "$marker" <<'PY' \
     | mempal ingest --stdin --wing smoke --room cli --source-type agent_inference --memory-kind evidence --domain project --field smoke --no-gate --supersedes "$created_id" --wait --wait-timeout-secs 90 --json \
     > "$update_json"; then
import json, sys
marker = sys.argv[1]
print(json.dumps({
    "content": f"{marker} reversible CLI smoke drawer updated; safe to delete",
    "wing": "smoke",
    "room": "cli",
    "source_type": "agent_inference",
    "memory_kind": "evidence",
    "domain": "project",
    "field": "smoke",
}))
PY
     :
   fi
   python3 - "$update_json" > "$update_ids" <<'PY'
import json, sys
from pathlib import Path
obj = json.loads(Path(sys.argv[1]).read_text() or "{}")
for item in dict.fromkeys(x for x in obj.get("created_drawer_ids", []) if isinstance(x, str) and x):
    print(item)
PY
   if [ ! -s "$update_ids" ]; then
     echo "update smoke did not expose cleanup-safe created_drawer_ids; fail closed" >&2
     exit 1
   fi
   cat "$update_ids" >> "$smoke_ids"
   updated_id="$(head -n 1 "$update_ids")"
   updated_view="$(mktemp)"
   mempal view "$updated_id" --all-projects > "$updated_view"
   rm -f "$updated_view" "$update_json" "$update_ids"
   ```

5. Verify marker visibility again, then pin/unpin and delete exact smoke IDs from create/update responses. Suppress command output so `mempal delete` cannot print drawer summaries:

   ```bash
   verify_json="$(mktemp)"
   mempal search "$marker" --top-k 5 --json > "$verify_json"
   python3 - "$verify_json" "$marker" <<'PY'
import json
import sys
from pathlib import Path

results = json.loads(Path(sys.argv[1]).read_text() or "[]")
marker = sys.argv[2]
matches = [
    item for item in results
    if isinstance(item, dict)
    and item.get("wing") == "smoke"
    and item.get("room") == "cli"
    and marker in str(item.get("content", ""))
]
print(f"active_cli_smoke_marker_matches={len(matches)}")
PY
   rm -f "$verify_json"

   sort -u "$smoke_ids" -o "$smoke_ids"
   while IFS= read -r smoke_id; do
     mempal pin "$smoke_id" >/dev/null || true
     mempal unpin "$smoke_id" >/dev/null || true
     if ! mempal delete "$smoke_id" >/dev/null; then
       echo "cleanup failed for smoke drawer id: $smoke_id" >&2
       exit 1
     fi
   done < "$smoke_ids"
   rm -f "$ingest_json" "$operation_json" "$smoke_ids"
   ```

6. Re-run aggregate marker verification and confirm no active `smoke/cli` marker match remains, or record soft-delete behavior if tombstones remain visible only with explicit include-deleted flags.

## Reversible MCP memory CRUD smoke

Run this by default for full smoke. Prefer MCP tools already exposed to the active client. If the active client has no mempal MCP tools, it is acceptable to start a short-lived tracked stdio child with `mempal serve --mcp`, call the tools, then shut it down and verify no extra holder remains.

Required MCP tool coverage when available:

| Step | MCP method/tool | Required assertion |
|---|---|---|
| initialize | JSON-RPC `initialize` then `notifications/initialized` | server name present |
| discover | `tools/list` | includes `mempal_ingest`, `mempal_operation_status`, `mempal_search`, `mempal_read_drawer`, `mempal_delete` |
| status | `mempal_status` | structured object, db holder fields summarized only |
| create | `mempal_ingest` with `wing="smoke"`, `room="mcp"`, `smoke=true`, `wait=true`, typed metadata | completed or terminal success; `created_drawer_ids` non-empty |
| read/search | `mempal_search`, `mempal_read_drawer`, optionally `mempal_read_drawers` | structured responses parse; do not print content |
| context/read-only | `mempal_context`, `mempal_pinned_facts`, `mempal_timeline`, `mempal_doctor`, optionally `mempal_taxonomy`, `mempal_field_taxonomy`, `mempal_kg`, `mempal_skill`, `mempal_brief` | shape-only success or classified skip/failure |
| update | `mempal_ingest` with `supersedes=<created_id>`, same smoke scope, `smoke=true`, `wait=true` | replacement `created_drawer_ids` non-empty |
| delete | `mempal_delete` for every exact create/update ID | `deleted=true` or explicit already-deleted for duplicate cleanup attempt |
| post-delete | `mempal_search` marker query | no active `smoke/mcp` marker matches, or soft-delete visibility classified |
| shutdown | `shutdown` then `notifications/exit`; kill only if needed | child exits; no extra MCP/DB holder remains |

MCP stdio uses newline-delimited JSON-RPC messages in this project. A minimal client pattern is:

```python
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"mempal-smoke","version":"0"}}})
send({"jsonrpc":"2.0","method":"notifications/initialized"})
send({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
send({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mempal_ingest","arguments":{"content":"<marker> reversible MCP smoke drawer; safe to delete","wing":"smoke","room":"mcp","source_type":"agent_inference","memory_kind":"evidence","domain":"project","field":"smoke","smoke":True,"wait":True,"wait_timeout_secs":90}}})
```

Capture MCP responses to temp files and report only: method/tool name, JSON-RPC error class if any, latency, structuredContent top-level fields, result counts, and cleanup-created ID counts. Never print `content`, snippets, previews, prompts, drawer text, or raw response bodies.

Cleanup authority is the same as CLI: only `created_drawer_ids` from `mempal_ingest` or `mempal_operation_status` may be deleted automatically. Do not delete `drawer_id`, search hits, read results, or marker matches.

Public MCP `mempal_ingest` exposes a constrained smoke path through `smoke=true`. The server accepts it only for small writes under `wing="smoke"` and `room="mcp"`, rejects diary rollup, and internally bypasses gating/novelty so automated smoke can receive cleanup-authoritative `created_drawer_ids`. If a terminal MCP smoke write still has no `created_drawer_ids`, classify it as `inconclusive_no_cleanup_id`, do not attempt update/delete, and do not use search result IDs for cleanup.

## Dangerous or non-default surfaces

Default smoke must skip these unless the task explicitly asks for them and an exact cleanup plan exists:

- `mempal_rollback`: dry-run only; never execute real rollback in smoke.
- Purge/delete-all style commands: forbidden for smoke.
- `mempal_peek_partner`: may read live session text; skip.
- `mempal_cowork_push` and `mempal_cowork_bus send/broadcast/drain/capture`: skip because they mutate or expose cowork traffic.
- Taxonomy/tunnel/knowledge graph mutations such as taxonomy edit, tunnel add/delete, and `mempal_kg add/invalidate`: skip unless exact synthetic IDs and cleanup are proven.
- Knowledge/profile promotion workflows (`promote`, `adopt`, `reject`, `retire`, `publish`, card promote/demote): read/list/gate summaries only by default.

For content-bearing read-only tools (`mempal_context`, `mempal_brief`, `mempal_pinned_facts`, `mempal_timeline`, search/read tools), summarize counts/fields/bytes only and redact or omit values named `content`, `text`, `preview`, `statement`, `narrative`, `messages`, `facts.content`, snippets, prompts, and model output.

## REST checks

Prefer `mempal doctor rest --format json` for route availability. If an actual REST route smoke is required:

1. First check whether an endpoint is already running (`doctor rest`).
2. If not running, start `mempal serve` as a tracked background process and ensure it listens before probing.
3. Probe only aggregate/status endpoints or synthetic content.
4. Kill the temporary server and verify no extra `mempal serve` process remains.

Do not confuse daemon service health with REST server availability; they are separate surfaces.

## Memory growth guard

Capture daemon memory before and after smoke:

```bash
main_pid=$(systemctl --user show mempal-daemon.service -p MainPID --value)
ps -p "$main_pid" -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,comm
mempal daemon status | awk '/^(memory\.|live_daemons:|extra_holders:|search\.active:|rest\.embedder_cache\.|embedder\.)/ {print}'
```

If read-only commands push RSS into multi-GB territory, file or update an issue with aggregate evidence. Restarting is only mitigation; the product fix should bound/lazily load/evict caches.

## Done when

- Preflight state recorded without leaking content.
- Daemon restart, if performed, leaves one current-binary daemon.
- Read-only matrix exits 0 or failures are classified by command/stderr class.
- Raw stdout/stderr from content-bearing `mempal` commands was captured, structurally summarized, and not pasted into chat or log summaries.
- CLI CRUD smoke creates, reads/searches/contexts, updates, pin/unpins, deletes, and verifies cleanup using only exact `created_drawer_ids`.
- MCP CRUD smoke creates, reads/searches/contexts, updates, deletes, and verifies cleanup through MCP tools using the constrained `smoke=true` write path. If a terminal MCP smoke write lacks cleanup-safe `created_drawer_ids`, classify it as `inconclusive_no_cleanup_id` and keep the MCP CRUD group non-pass. Use `skipped_unavailable` only when no MCP client/tooling is available.
- REST/MCP checks do not leave extra processes or DB holders.
- Memory before/after is recorded.
- Worktree remains clean except intentional skill/code changes.

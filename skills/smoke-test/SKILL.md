---
name: smoke-test
description: "Project-local mempal manual smoke test skill. Use after installing, restarting, merging, or changing mempal to verify daemon health, CLI/REST/MCP/read-only surfaces, optional reversible write/delete paths, memory growth, and database-holder safety without starting duplicate daemons or leaking drawer content."
---

# mempal Manual Smoke Test

Use this project-local skill when a main agent must manually verify that the installed `mempal` binary and live daemon still work after a merge, install, restart, MCP reconnect, or production-like debugging session.

Default to direct installed-CLI probes. Do not start extra daemons or long-lived REST/MCP servers unless the test explicitly requires them and the owner process is tracked for cleanup.

## Safety rules

1. Keep diagnostics aggregate-only. Do not print raw drawer content, prompts, model responses, raw process command lines, environment variables, connection strings, URLs, Authorization headers, bearer tokens, API keys, passwords, `drawer_content`, or prompt-like arguments.
2. Use the installed binary (`command -v mempal`, `mempal --version`) and the live user daemon. Do not run `cargo run` for smoke unless debugging source changes.
3. Maintain singleton ownership:
   - Prefer `systemctl --user restart mempal-daemon.service` for restart.
   - Verify exactly one `/usr/local/bin/mempal daemon --foreground` after restart.
   - Do not run `mempal serve` as a long-lived REST server for smoke unless a route test requires it; if started, track and kill it.
   - Do not spawn extra `mempal serve --mcp` processes from the shell. MCP reconnect is a client action.
4. Before declaring a lock failure, inspect DB holders with `mempal daemon status` and summarize only roles/counts/PIDs/commands.
5. Prefer read-only CLI smoke first. Use reversible write/delete only when the requested task requires proving write paths or when read-only probes expose a write-path risk.
6. If using a synthetic write, use a unique marker, a `smoke/manual` wing-room, `--no-gate`, and `--wait`, then clean up only the exact drawer ID(s) returned by the ingest response. Never delete IDs discovered from generic search results. Report IDs only when cleanup fails; do not print content.
7. If there is already context that should become durable memory, it is acceptable to ingest a concise real note instead of synthetic content, but only when the note is genuinely useful and non-secret.

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

## Optional reversible write smoke

Use only when write-path validation is required.

1. Generate a unique marker:

   ```bash
   marker="mempal-smoke-$(date +%s)-$RANDOM"
   ```

2. Ingest via stdin with a smoke scope, capture the response, and extract only cleanup-safe IDs explicitly returned by ingest:

   ```bash
   ingest_json="$(mktemp)"
   smoke_ids="$(mktemp)"
   printf '{"content":"%s reversible smoke drawer; safe to delete","wing":"smoke","room":"manual"}\n' "$marker" \
     | mempal ingest --stdin --wing smoke --room manual --no-gate --wait --wait-timeout-secs 60 --json \
     > "$ingest_json"
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
     echo "cleanup requires manual operator action: ingest response did not expose created_drawer_ids; do not delete drawer_id, drawer_ids, or cleanup_drawer_ids" >&2
     rm -f "$ingest_json" "$smoke_ids"
     exit 1
   fi
   ```

   Do not print the ingest response or smoke IDs unless cleanup fails. `created_drawer_ids` is the cleanup authority; `cleanup_drawer_ids` is only a human-readable alias when it mirrors the created list. `drawer_id` and `drawer_ids` are informational because they may name pre-existing, deduplicated, dropped, or merge-target drawers. If the ingest response timed out and returned only an `operation_id`, inspect it with `mempal operation wait <operation_id> --timeout-secs <seconds>` or `mempal operation status <operation_id>`, then delete only IDs from `created_drawer_ids`; if that list is empty, fail closed.

3. Optionally search for the marker as a verification probe only. Keep output in a temporary file, print aggregate counts only, never print search result bodies/snippets, and never use search result IDs for deletion:

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
    and item.get("room") == "manual"
    and marker in str(item.get("content", ""))
]
print(f"active_smoke_marker_matches={len(matches)}")
PY
   rm -f "$verify_json"
   ```

4. For each exact smoke ID from the ingest response, optionally test pin/unpin, then delete. Suppress command output so `mempal delete` cannot print drawer summaries:

   ```bash
   while IFS= read -r smoke_id; do
     mempal pin "$smoke_id" >/dev/null || true
     mempal unpin "$smoke_id" >/dev/null || true
     if ! mempal delete "$smoke_id" >/dev/null; then
       echo "cleanup failed for smoke drawer id: $smoke_id" >&2
       exit 1
     fi
   done < "$smoke_ids"
   rm -f "$ingest_json" "$smoke_ids"
   ```

5. Re-run the aggregate marker verification from step 3 and confirm no active `smoke/manual` marker match remains, or record soft-delete behavior if tombstones remain visible only with explicit include-deleted flags.

## REST checks

Prefer `mempal doctor rest --format json` for route availability. If an actual REST route smoke is required:

1. First check whether an endpoint is already running (`doctor rest`).
2. If not running, start `mempal serve` as a tracked background process and ensure it listens before probing.
3. Probe only aggregate/status endpoints or synthetic content.
4. Kill the temporary server and verify no extra `mempal serve` process remains.

Do not confuse daemon service health with REST server availability; they are separate surfaces.

## MCP checks

Do not spawn MCP servers manually for smoke. If MCP tool validation is required, ask the interactive client to reconnect MCP (for example `/mcp`) and then call available read-only MCP tools. Pass criteria are shape/status only; do not print raw memory content.

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
- Optional write smoke cleans up its own drawers.
- REST/MCP checks do not leave extra processes.
- Memory before/after is recorded.
- Worktree remains clean except intentional skill/code changes.

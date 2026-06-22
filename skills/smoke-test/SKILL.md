---
name: smoke-test
description: "Project-local mempal manual smoke test skill. Use after installing, restarting, merging, or changing mempal to verify daemon health, CLI/REST/MCP/read-only surfaces, optional reversible write/delete paths, memory growth, and database-holder safety without starting duplicate daemons or leaking drawer content."
---

# mempal Manual Smoke Test

Use this project-local skill when a main agent must manually verify that the installed `mempal` binary and live daemon still work after a merge, install, restart, MCP reconnect, or production-like debugging session.

Default to direct installed-CLI probes. Do not start extra daemons or long-lived REST/MCP servers unless the test explicitly requires them and the owner process is tracked for cleanup.

## Safety rules

1. Keep diagnostics aggregate-only. Do not print raw drawer content, prompts, model responses, Authorization headers, bearer tokens, API keys, passwords, `drawer_content`, or secret-bearing URLs.
2. Use the installed binary (`command -v mempal`, `mempal --version`) and the live user daemon. Do not run `cargo run` for smoke unless debugging source changes.
3. Maintain singleton ownership:
   - Prefer `systemctl --user restart mempal-daemon.service` for restart.
   - Verify exactly one `/usr/local/bin/mempal daemon --foreground` after restart.
   - Do not run `mempal serve` as a long-lived REST server for smoke unless a route test requires it; if started, track and kill it.
   - Do not spawn extra `mempal serve --mcp` processes from the shell. MCP reconnect is a client action.
4. Before declaring a lock failure, inspect DB holders with `mempal daemon status` and summarize only roles/counts/PIDs/commands.
5. Prefer read-only CLI smoke first. Use reversible write/delete only when the requested task requires proving write paths or when read-only probes expose a write-path risk.
6. If using a synthetic write, use a unique marker, a `smoke/manual` wing-room, `--no-gate`, `--wait`, then immediately find the created drawer ID(s), pin/unpin if needed, and `mempal delete` them. Report IDs only if needed; do not print content.
7. If there is already context that should become durable memory, it is acceptable to ingest a concise real note instead of synthetic content, but only when the note is genuinely useful and non-secret.

## Preflight

Run from the repository root:

```bash
git status --short --branch --untracked-files=all
command -v mempal
mempal --version
systemctl --user is-active mempal-daemon.service || true
systemctl --user show mempal-daemon.service -p MainPID -p ActiveState -p SubState --value || true
ps -eo pid,ppid,stat,etime,%cpu,%mem,rss,vsz,args --sort=-rss | awk '/[m]empal/ {print}'
mempal daemon status | sed -E 's/(authorization|bearer|token|password|secret|api[_-]?key|drawer_content)[^[:space:]]*/\1=[REDACTED]/Ig' | sed -n '1,180p'
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

## Restart procedure

When restart is allowed or needed:

```bash
systemctl --user restart mempal-daemon.service
sleep 3
main_pid=$(systemctl --user show mempal-daemon.service -p MainPID --value)
systemctl --user is-active mempal-daemon.service
ps -p "$main_pid" -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,args
readlink "/proc/$main_pid/exe"
pgrep -af '^/usr/local/bin/mempal (daemon --foreground|serve --mcp)$' || true
```

Pass criteria:

- service is `active`;
- exactly one daemon process exists;
- daemon exe resolves to `/usr/local/bin/mempal` and is not deleted;
- no untracked extra `mempal serve --mcp` processes were created by the smoke;
- RSS is reasonable immediately after restart (normally hundreds of MB, not multi-GB).

## Read-only CLI smoke matrix

Run these commands directly. Capture exit code, latency, stdout byte count, stderr byte count, and JSON shape/field names where applicable. Do not print raw drawer bodies.

| Group | Command | Pass criteria |
|---|---|---|
| Identity | `mempal --version` | exits 0, expected version string |
| Daemon | `mempal daemon status` | exits 0, running, singleton/current binary |
| Doctor | `mempal doctor` | exits 0; warnings summarized |
| REST doctor | `mempal doctor rest --format json` | exits 0, JSON parses, routes reported; degraded is allowed only with warning recorded |
| Dashboard | `mempal status` | exits 0 |
| Stats | `mempal stats` | exits 0 |
| Config | `mempal config intelligence` | exits 0 |
| Cost | `mempal cost status` | exits 0 |
| Gating | `mempal gating stats` | exits 0 |
| Timeline | `mempal timeline --since 1h --format json` | exits 0, empty output or valid JSON accepted |
| Tail | `mempal tail --limit 3` | exits 0 |
| Pinned | `mempal pinned --json` | exits 0, JSON parses |
| KG | `mempal kg stats` | exits 0 |
| Cards | `mempal knowledge-card list --format json` and `mempal cards --pending --format json` | exits 0, JSON parses |
| Reflection | `mempal reflect --json --limit 3` | exits 0, JSON parses |
| Prime | `mempal prime --format json --token-budget 512 --no-stats` | exits 0, JSON parses |
| Wake-up | `mempal wake-up --format protocol` | exits 0 |
| Taxonomy | `mempal field-taxonomy --format json` | exits 0, JSON parses |
| Integrations | `mempal integrations status` | exits 0 |
| Checkpoint | `mempal checkpoint status` | exits 0 |
| Patterns/skills/repair | `mempal patterns list`, `mempal skills list`, `mempal repair list` | exits 0 |
| Cowork | `mempal cowork-status --cwd "$PWD"` | exits 0 |
| Maintenance | `mempal maintenance guided-run --format json` | exits 0, JSON parses |
| Release | `mempal release-readiness --format json` | exits 0, JSON parses |
| xurl | `mempal xurl stats` | exits 0 |
| Benchmark | `mempal bench matrix --mode no-llm --top-k 3 --format json` | exits 0, JSON parses |
| Recall help | `mempal recall hermes --help` | exits 0 |

For expensive semantic queries, run them only when needed and bound time explicitly:

```bash
timeout 180s mempal search '<known safe query>' --top-k 3 --json
timeout 120s mempal context '<known safe query>' --format json --max-items 3 --no-distill-suggestions
```

If these are slow, report latency and memory growth; do not treat slowness as pass unless the task only asked for availability.

## Optional reversible write smoke

Use only when write-path validation is required.

1. Generate a unique marker:

   ```bash
   marker="mempal-smoke-$(date +%s)-$RANDOM"
   ```

2. Ingest via stdin with a smoke scope:

   ```bash
   printf '{"content":"%s reversible smoke drawer; safe to delete","wing":"smoke","room":"manual"}\n' "$marker" \
     | mempal ingest --stdin --format json --wing smoke --room manual --no-gate --wait --wait-timeout-secs 60 --json
   ```

3. Search only for the marker and parse IDs from JSON without printing content:

   ```bash
   mempal search "$marker" --top-k 5 --json > /tmp/mempal-smoke-search.json
   python3 - <<'PY'
import json
from pathlib import Path
obj=json.loads(Path('/tmp/mempal-smoke-search.json').read_text() or '[]')
ids=[]
def walk(x):
    if isinstance(x, dict):
        for k,v in x.items():
            if k in ('id','drawer_id','drawerId') and isinstance(v,(str,int)):
                ids.append(str(v))
            else:
                walk(v)
    elif isinstance(x, list):
        for y in x:
            walk(y)
walk(obj)
print('\n'.join(dict.fromkeys(ids)))
PY
   ```

4. For each smoke ID, optionally test pin/unpin, then delete:

   ```bash
   mempal pin "$id" || true
   mempal unpin "$id" || true
   mempal delete "$id"
   ```

5. Re-run marker search and confirm no active smoke drawer remains, or record soft-delete behavior if tombstones remain visible only with explicit include-deleted flags.

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
ps -p "$main_pid" -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,args
mempal daemon status | awk '/^(memory\.|live_daemons:|extra_holders:|search\.active:|rest\.embedder_cache\.|embedder\.)/ {print}'
```

If read-only commands push RSS into multi-GB territory, file or update an issue with aggregate evidence. Restarting is only mitigation; the product fix should bound/lazily load/evict caches.

## Done when

- Preflight state recorded without leaking content.
- Daemon restart, if performed, leaves one current-binary daemon.
- Read-only matrix exits 0 or failures are classified by command/stderr class.
- Optional write smoke cleans up its own drawers.
- REST/MCP checks do not leave extra processes.
- Memory before/after is recorded.
- Worktree remains clean except intentional skill/code changes.

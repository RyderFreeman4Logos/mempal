# Preflight and Read-only CLI Smoke

Use this reference when the smoke needs manual preflight or read-only CLI probes beyond the automated `scripts/full_smoke.py` runner.

## Preflight

Run from repo root. Capture content-bearing output to temp files; report only shapes/counts/status classes.

```bash
git status --short --branch --untracked-files=all
command -v mempal
mempal --version
systemctl --user is-active mempal-daemon.service || true
systemctl --user show mempal-daemon.service -p MainPID -p ActiveState -p SubState --value || true
mempal daemon status > /tmp/mempal-daemon-status-smoke.txt
```

Summarize only:

- installed path/version;
- daemon service state/PID;
- daemon executable current vs `/usr/local/bin/mempal (deleted)`;
- live daemon count;
- extra holders / stale MCP servers / orphan daemons;
- RSS/PSS/private dirty/anonymous memory;
- queue counts and search active flag;
- REST health class.

Do not report raw command lines, environment variables, URLs, connection strings, bearer tokens, prompt-like arguments, or drawer/card bodies.

## Structure-only capture harness

Use for content-bearing or uncertain commands:

```bash
run_mempal_probe() {
  name="$1"; expect_json="$2"; shift 2
  tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/mempal-smoke.XXXXXX")"
  stdout_file="$tmpdir/stdout"; stderr_file="$tmpdir/stderr"
  start_ms="$(date +%s%3N)"
  "$@" >"$stdout_file" 2>"$stderr_file"; status=$?
  end_ms="$(date +%s%3N)"
  python3 - "$name" "$expect_json" "$status" "$start_ms" "$end_ms" "$stdout_file" "$stderr_file" <<'PY'
import json, sys
from pathlib import Path
name, expect_json, status, start_ms, end_ms, stdout_path, stderr_path = sys.argv[1:]
stdout = Path(stdout_path).read_bytes(); stderr = Path(stderr_path).read_bytes()
summary = {
    'name': name,
    'exit_code': int(status),
    'latency_ms': max(0, int(end_ms) - int(start_ms)),
    'stdout_bytes': len(stdout),
    'stderr_bytes': len(stderr),
}
if expect_json == 'json':
    text = stdout.decode('utf-8', errors='replace')
    try:
        parsed = json.loads(text or 'null')
        if isinstance(parsed, dict):
            summary['json'] = {'ok': True, 'type': 'object', 'fields': sorted(parsed.keys()), 'field_count': len(parsed)}
        elif isinstance(parsed, list):
            summary['json'] = {'ok': True, 'type': 'array', 'count': len(parsed)}
        else:
            summary['json'] = {'ok': True, 'type': type(parsed).__name__}
    except Exception as exc:
        lines=[line for line in text.splitlines() if line.strip()]
        try:
            parsed_lines=[json.loads(line) for line in lines]
            fields=sorted({k for item in parsed_lines if isinstance(item, dict) for k in item})
            summary['json']={'ok': True, 'type': 'ndjson', 'line_count': len(parsed_lines), 'fields': fields}
        except Exception:
            summary['json']={'ok': False, 'error_type': type(exc).__name__}
print(json.dumps(summary, sort_keys=True))
PY
  rm -rf "$tmpdir"
  return "$status"
}
```

The harness must not print command stdout/stderr, JSON values, drawer IDs, search snippets, previews, prompts, or model responses.

## Read-only CLI matrix

Treat these commands as content-bearing by default: `status`, `status --full`, `timeline`, `tail`, `pinned`, `knowledge-card list`, `cards --pending`, `reflect`, `prime`, `wake-up`, `context`, `search`, `recall`, and any command returning drawers/snippets/evidence/prompts/previews.

Suggested probes:

| Group | Command | Handling | Pass criteria |
|---|---|---|---|
| Identity | `mempal --version` | safe direct | exit 0 |
| Daemon | `mempal daemon status` | captured summary | exit 0, singleton/current binary classified |
| Doctor | `mempal doctor` | captured summary | exit 0 or warnings classified |
| REST doctor | `mempal doctor rest --format json` | JSON harness | JSON parses; degraded allowed only if recorded |
| Dashboard | `mempal status` | harness | exit 0 |
| Stats | `mempal stats` | captured summary | exit 0 |
| Config/cost/gating | `mempal config intelligence`, `mempal cost status`, `mempal gating stats` | captured summary | exit 0 |
| Timeline/tail/pinned | `mempal timeline --since 1h --format json`, `mempal tail --limit 3`, `mempal pinned --json` | harness | exit 0, JSON parses where requested |
| KG/cards/reflect | `mempal kg stats`, `mempal knowledge-card list --format json`, `mempal cards --pending --format json`, `mempal reflect --json --limit 3` | harness | exit 0/JSON parses |
| Context/search | `mempal search <safe-query> --top-k 3 --json`, `mempal context <safe-query> --format json --max-items 3 --no-distill-suggestions` | harness; timeout 120-180s | shape-only success |
| Maintenance/release/xurl | `mempal maintenance guided-run --format json`, `mempal release-readiness --format json`, `mempal xurl stats` | harness/summary | exit 0 |

If semantic queries are slow, report latency and memory growth. Do not treat slowness as pass unless the task only asked for availability.

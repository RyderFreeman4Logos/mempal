---
name: smoke-test
description: "Project-local mempal smoke test. Use after installing, restarting, merging, or changing mempal to verify installed CLI, daemon singleton/current binary, REST/MCP availability, read-only memory surfaces, reversible CLI/MCP CRUD, cleanup, and resource safety without leaking drawer content or starting duplicate daemons."
---

# mempal Manual Smoke Test

Use this project-local skill when validating the installed `mempal` binary and live daemon after a merge, install, restart, MCP reconnect, or production-like debugging session.

Keep this file as the high-level runbook. Load references only when the current task needs manual detail:

- `references/preflight-and-readonly-cli.md` — preflight, singleton/holder checks, read-only CLI probe matrix, and structure-only capture harness.
- `references/reversible-crud.md` — reversible CLI and MCP memory CRUD, exact-created-ID cleanup authority, and MCP stdio lifecycle.
- `references/rest-resource-and-done.md` — REST checks, dangerous/non-default surfaces, memory/I/O guardrails, and done criteria.
- `scripts/full_smoke.py` — preferred automated full smoke runner; execute it instead of reimplementing the matrix when broad coverage is requested.
- `../../docs/conformance-matrix.md` — feature-preservation conformance matrix; `scripts/full_smoke.py` emits aggregate pass/fail/skipped status for its feature groups under the final JSON `conformance` key.

## Safety rules

1. Keep diagnostics aggregate-only. Do not print raw drawer content, prompts, model responses, raw process command lines, environment variables, connection strings, URLs, Authorization headers, bearer tokens, API keys, passwords, `drawer_content`, snippets, previews, or prompt-like arguments.
2. Use the installed binary (`command -v mempal`, `mempal --version`) and the live user daemon. Do not use `cargo run` for smoke unless debugging source changes.
3. Maintain singleton ownership: prefer the existing `mempal-daemon.service`; verify one current `/usr/local/bin/mempal` daemon; do not start unmanaged long-lived REST/MCP servers. The checked-in `scripts/full_smoke.py` may start short-lived MCP stdio children because it owns shutdown/cleanup.
4. Before classifying lock failures, inspect DB holders with aggregate-only daemon status and report roles/counts/PIDs only.
5. For write smoke, clean up only exact IDs returned as `created_drawer_ids` by that same smoke run. Never delete IDs discovered through search/read results.
6. If there is genuinely useful non-secret context to preserve, one real memory write is allowed instead of purely synthetic content. Keep it concise, typed, and project-scoped.

## Preferred automated full smoke

Run from repo root:

```bash
python3 skills/smoke-test/scripts/full_smoke.py > /tmp/mempal-skill-full-smoke.log
```

The runner prints one aggregate JSON object and avoids raw memory content. It covers installed CLI identity/status, read-only surfaces, reversible CLI CRUD, short-lived MCP read/CRUD, cleanup, and coarse I/O telemetry.
It also reports feature-preservation conformance by group according to `docs/conformance-matrix.md`.

If it exits nonzero or times out, parse only the final JSON summary:

```bash
python3 - <<'PY'
import json, pathlib
p = pathlib.Path('/tmp/mempal-skill-full-smoke.log')
lines = [line for line in p.read_text(errors='replace').splitlines() if line.strip()]
data = json.loads(lines[-1]) if lines else {}
print({
    'overall_ok': data.get('overall_ok'),
    'failures': data.get('failures'),
    'cleanup': data.get('cleanup'),
    'created_counts': data.get('created_counts'),
    'conformance': data.get('conformance', {}).get('summary'),
    'group_count': len(data.get('groups', {})),
})
for name, info in (data.get('groups') or {}).items():
    if isinstance(info, dict) and not info.get('ok'):
        print(name, {k: v for k, v in info.items() if k not in {'content', 'text', 'preview', 'statement', 'narrative'}})
PY
```

Do not paste raw command stdout/stderr from the smoke log into chat.

## Minimal preflight

Use this before any write smoke or full smoke:

```bash
git status --short --branch --untracked-files=all
command -v mempal
mempal --version
systemctl --user is-active mempal-daemon.service || true
systemctl --user show mempal-daemon.service -p MainPID -p ActiveState -p SubState --value || true
mempal daemon status > /tmp/mempal-daemon-status-smoke.txt
```

Summarize only: installed version/path, daemon active state/PID, daemon executable current vs deleted, live daemon count, extra holders/stale MCP/orphans, RSS/PSS/private dirty/anonymous, queue counts, search active, and REST health class. For field-level details, load `references/preflight-and-readonly-cli.md`.

## Coverage target

Classify each group as `pass`, `fail`, or `skipped_<reason>`:

| Group | Required coverage |
|---|---|
| Runtime | installed binary, daemon singleton, **daemon binary consistency** (exe vs installed), DB holders, RSS/resource summary |
| Read-only CLI | status/stats/**doctor (schema/embedding health)**/search/context/timeline/tail/pinned/knowledge/card/repair/pattern/skill shapes where supported |
| Reversible CLI CRUD | create, search/read/context, update via `--supersedes`, pin/unpin, exact-ID delete, post-delete verification |
| REST | doctor/route availability, degraded warning categories if any |
| MCP read-only | status/search/context/pinned/timeline/doctor plus optional skill/brief/taxonomy tools exposed by the client |
| Reversible MCP CRUD | create, read/search/context, update, exact-ID delete, post-delete verification |
| Cleanup/resource | no extra REST/MCP holders, created IDs cleaned, daemon PID stable or restart classified, I/O/RSS summarized |

Never downgrade a failed CRUD path to pass because another surface worked. Report CLI and MCP CRUD separately.

## Manual smoke pointers

- For read-only CLI probes and shape-only output handling, load `references/preflight-and-readonly-cli.md`.
- For CLI/MCP write/update/delete details, load `references/reversible-crud.md`.
- For REST, memory growth, dangerous surfaces, and final checklist, load `references/rest-resource-and-done.md`.

## Verification checklist

- [ ] Preflight recorded without leaking content.
- [ ] Installed binary and daemon are current; singleton/holder state is classified.
- [ ] **Daemon binary consistency check passes** (exe path matches installed binary, not deleted).
- [ ] Read-only matrix exits 0 or failures are classified by command/error class.
- [ ] **`doctor --format json` probe verifies schema version match and no critical warnings.**
- [ ] CLI CRUD creates, reads/searches/contexts, updates, pin/unpins, deletes, and verifies cleanup using only `created_drawer_ids`.
- [ ] MCP CRUD passes or is explicitly skipped/unavailable; failures are not hidden by CLI success.
- [ ] **MCP ingest external-fallback count is reported as a degradation signal.** The legacy `mcp_ingest_fallback_to_cli` key counts both CLI operation-wait recovery and REST fallback; non-zero means MCP ingest did not return cleanup IDs directly.
- [ ] **Every MCP write fallback runs only after every runner-owned MCP stdio child is closed and reaped.** A surviving owned holder blocks fallback and fails the run.
- [ ] **Unresolved exact cleanup IDs remain in a mode-`0600` atomic `/tmp` manifest.** Failure JSON reports only `cleanup_manifest_path` and `cleanup_pending_count`; verified cleanup removes the file.
- [ ] REST/MCP checks do not leave extra processes or DB holders.
- [ ] Memory/I/O before/after is summarized aggregate-only.
- [ ] Worktree remains clean except intentional skill/code changes.

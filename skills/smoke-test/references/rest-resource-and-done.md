# REST, Resource Safety, and Done Criteria

Use this reference for non-default smoke surfaces, resource guards, and final reporting.

## REST checks

Prefer `mempal doctor rest --format json` for route availability. It is cheaper and avoids unmanaged server lifetimes.

If an actual REST route smoke is required:

1. Check whether an endpoint is already running with `doctor rest`.
2. If no route is available and the test truly requires REST, start `mempal serve` as a tracked background process.
3. Probe only aggregate/status endpoints or synthetic content.
4. Kill the temporary server and verify no extra `mempal serve` process remains.

Do not confuse daemon service health with REST server availability; they are separate surfaces.

## Dangerous or non-default surfaces

Skip these unless the task explicitly asks and an exact cleanup plan exists:

- rollback/rollback-like mutation except dry-run;
- purge/delete-all style commands;
- partner/session peek tools that may expose live text;
- cowork push/bus send/broadcast/drain/capture;
- taxonomy/tunnel/KG mutations without exact synthetic IDs and cleanup;
- knowledge/profile promotion workflows (`promote`, `adopt`, `reject`, `retire`, `publish`, card promote/demote).

For content-bearing read-only tools (`context`, `brief`, `pinned`, `timeline`, search/read tools), summarize counts/fields/bytes only and omit/redact values named `content`, `text`, `preview`, `statement`, `narrative`, `messages`, snippets, prompts, and model output.

## Memory and I/O guard

Capture daemon memory before/after broad smoke:

```bash
main_pid=$(systemctl --user show mempal-daemon.service -p MainPID --value)
ps -p "$main_pid" -o pid,ppid,stat,etime,%cpu,%mem,rss,vsz,comm
mempal daemon status > /tmp/mempal-daemon-status-smoke.txt
```

Report only aggregate RSS/PSS/anonymous/private dirty, queue/holder counts, and route/health classes. If read-only commands push RSS into multi-GB territory or tiny smoke writes cause large SSD reads, capture PID-stable `/proc/<pid>/io` deltas and file/fix a resource-governance issue; restarting is mitigation, not the product fix.

## Final report checklist

A smoke report is complete when:

- preflight state is recorded without leaking content;
- installed binary and daemon executable are current, singleton, and holder state is classified;
- read-only matrix exits 0 or failures are classified by command/error class;
- CLI CRUD creates, reads/searches/contexts, updates, pin/unpins, deletes, and verifies cleanup using exact `created_drawer_ids` only;
- MCP CRUD passes or is explicitly skipped/unavailable; MCP failures are not hidden by CLI success;
- REST/MCP checks leave no extra processes or DB holders;
- all exact-created smoke IDs are cleaned, while intentional real-memory writes are preserved;
- memory and I/O before/after are summarized aggregate-only;
- worktree is clean except intentional skill/code changes.

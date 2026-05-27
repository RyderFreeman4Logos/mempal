# Smoke Test: mempal Read-Only Regression Check

Non-destructive smoke test covering every side-effect-free CLI feature.
Run after a new binary install or MCP reconnect to catch regressions.

## Prerequisites

1. **Restart the daemon** so the new binary is loaded:
   ```
   mempal daemon restart
   ```
2. **Ask the user to run `/mcp`** (or reconnect MCP manually) so the MCP
   server picks up the new binary. **Wait for user confirmation before
   proceeding** — MCP reconnect is a user-side action.

## Test Matrix

Run every command below. A test **passes** if it exits 0 and produces
non-empty, structurally valid output (not just an error message).
Collect results into a markdown table at the end.

### Group 1 — System Health

| # | Command | Pass criteria |
|---|---------|---------------|
| 1 | `mempal status` | Shows `schema_version`, `drawer_count > 0`, `daemon running: true` |
| 2 | `mempal daemon status` | Shows `status: running`, valid PID, `uptime_secs > 0` |
| 3 | `mempal stats` | Shows `drawers total > 0`, at least one scope entry |

### Group 2 — Search & Retrieval

| # | Command | Pass criteria |
|---|---------|---------------|
| 4 | `mempal search '<chinese-query>' --top-k 3` | Returns >= 1 result with score |
| 5 | `mempal search '<english-query>' --top-k 3` | Returns >= 1 result with score |
| 6 | `mempal context '<query>'` | Exits 0 (may return "no context" — that is OK) |
| 7 | `mempal context '<query>' --include-cards` | Exits 0 |

For search queries, pick terms known to exist in the palace (e.g. a recent
commit message, a wing name, or a domain concept from the project).

### Group 3 — Timeline & History

| # | Command | Pass criteria |
|---|---------|---------------|
| 8  | `mempal timeline` | Returns dated entries |
| 9  | `mempal tail --limit 5` | Returns 5 drawer previews |
| 10 | `mempal prime` | Returns timeline overview with star ratings |

### Group 4 — Knowledge & Topology

| # | Command | Pass criteria |
|---|---------|---------------|
| 11 | `mempal knowledge-card list --format json` | Exits 0, returns `[]` or valid JSON array |
| 12 | `mempal tunnels list` | Returns tunnel entries or "0 tunnel(s)" |

### Group 5 — MCP Tools (post-reconnect only)

After MCP reconnect, test each read-only MCP tool if available:

| # | Tool | Pass criteria |
|---|------|---------------|
| 13 | `mempal_status()` | Returns status object |
| 14 | `mempal_search(query="test")` | Returns results array |
| 15 | `mempal_context(query="test")` | Returns context or empty |
| 16 | `mempal_timeline()` | Returns timeline entries |

## Execution

1. Run `mempal daemon restart` and verify `mempal daemon status` shows running.
2. Tell the user: "Please run `/mcp` to reconnect the MCP server, then confirm."
3. Wait for user confirmation.
4. Execute Groups 1-4 via CLI (all in parallel where independent).
5. Execute Group 5 via MCP tool calls.
6. Collect all results into a summary table:

```
| # | Test | Result | Notes |
|---|------|--------|-------|
| 1 | status | PASS/FAIL | ... |
```

7. If any test FAILs, report the failing command, exit code, and stderr.

## Done When

- All 12+ CLI tests executed
- MCP tools tested (if available after reconnect)
- Summary table printed
- Zero FAIL = "No regression detected"
- Any FAIL = list affected features for investigation

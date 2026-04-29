# CSA integration

This integration lets CSA (cli-sub-agent) use mempal as its persistent memory
backend instead of maintaining a separate JSONL plus Tantivy/BM25 memory store.
CSA keeps producing session summaries, review findings, merge summaries, tags,
and extracted facts; mempal stores those records as raw drawers and makes them
available through the normal CLI and MCP search paths.

The integration is intentionally additive. CSA writes a new source of drawers
under project-specific wings and CSA-specific rooms, while existing Claude Code,
Codex, Gemini, MCP, hook, and search consumers continue to use mempal unchanged.

## Hook configuration templates

CSA can call `mempal ingest --stdin --json` from `hooks.toml` lifecycle hooks.
The stdin payload for mempal ingest must be a JSON object. If CSA renders the
environment variables below as complete JSON strings, these minimal templates
are sufficient:

```toml
[session_complete]
command = "echo '${SESSION_SUMMARY}' | mempal ingest --stdin --wing ${PROJECT_NAME} --room csa-session --json"
timeout_seconds = 30

[post_review]
command = "echo '${REVIEW_FINDINGS}' | mempal ingest --stdin --wing ${PROJECT_NAME} --room csa-review --json"
timeout_seconds = 30

[merge_completed]
command = "echo '${MERGE_SUMMARY}' | mempal ingest --stdin --wing ${PROJECT_NAME} --room csa-merge --json"
timeout_seconds = 30
```

For raw text environment variables, wrap them as JSON before invoking mempal.
The `jq -Rs` form preserves newlines and quotes:

```toml
[session_complete]
command = "printf '%s' \"${SESSION_SUMMARY}\" | jq -Rs --arg wing \"${PROJECT_NAME}\" --arg project \"${PROJECT_NAME}\" --arg source \"${CSA_SESSION_ID}\" '{content: ., wing: $wing, room: \"csa-session\", project: $project, source: $source}' | mempal ingest --stdin --json"
timeout_seconds = 30

[post_review]
command = "printf '%s' \"${REVIEW_FINDINGS}\" | jq -Rs --arg wing \"${PROJECT_NAME}\" --arg project \"${PROJECT_NAME}\" --arg source \"${CSA_SESSION_ID}\" '{content: ., wing: $wing, room: \"csa-review\", project: $project, source: $source}' | mempal ingest --stdin --json"
timeout_seconds = 30

[merge_completed]
command = "printf '%s' \"${MERGE_SUMMARY}\" | jq -Rs --arg wing \"${PROJECT_NAME}\" --arg project \"${PROJECT_NAME}\" --arg source \"${CSA_SESSION_ID}\" '{content: ., wing: $wing, room: \"csa-merge\", project: $project, source: $source}' | mempal ingest --stdin --json"
timeout_seconds = 30
```

Recommended rooms:

| CSA event | mempal wing | mempal room | Stored content |
|---|---|---|---|
| `session_complete` | `${PROJECT_NAME}` | `csa-session` | Session summary and decisions |
| `post_review` | `${PROJECT_NAME}` | `csa-review` | Findings, severity, files, and resolution notes |
| `merge_completed` | `${PROJECT_NAME}` | `csa-merge` | Merge summary, PR metadata, and follow-up context |

## Field mapping

| CSA MemoryEntry field | mempal drawer field | Notes |
|---|---|---|
| `content` | `content` | Verbatim |
| `project` | `project` | Maps to mempal project filter |
| `source` | `source_file` | CSA session ID or path |
| `facts[]` | extracted into `content` | CSA fact extraction stored inline |
| `created_at` | `added_at` | ISO-8601 |
| `tags[]` | metadata in `content` | Stored in content body |

CSA should place structured data that has no first-class mempal drawer field
inside the content body. A compact Markdown or JSON block is fine, as long as the
original wording remains available for citation.

## Stdin ingest JSON format

`mempal ingest --stdin --json` reads one JSON object from stdin. `content` is
required. `wing` may be provided either in the JSON object or through
`--wing`; `room`, `project`, `source`, and `source_file` are optional but useful
for filtering and citations.

```json
{
  "content": "Session summary: implemented auth middleware...",
  "wing": "my-project",
  "room": "csa-session",
  "project": "my-project",
  "source": "csa-session-01ABCDEF"
}
```

Equivalent CLI invocation:

```bash
cat csa-session.json | mempal ingest --stdin --json
```

Expected JSON output contains the created drawer IDs and ingest counters:

```json
{
  "drawer_ids": ["drawer_my_project_csa_session_..."],
  "stats": {
    "dry_run": false,
    "files": 1,
    "chunks": 1,
    "skipped": 0,
    "dropped_by_gate": 0
  }
}
```

## Migration guide: JSONL to mempal

Existing CSA JSONL memories can be replayed into mempal without a schema
migration. The example below maps each legacy record into one stdin ingest JSON
object and stores it in a `csa-migrated` room.

```bash
# Export existing memories
cat ~/.local/state/cli-sub-agent/memory/*.jsonl | \
  jq -c '{content: .content, wing: .project, room: "csa-migrated", project: .project, source: "csa-legacy"}' | \
  while IFS= read -r line; do
    echo "$line" | mempal ingest --stdin --json
  done
```

For large migrations, run a small batch first and verify search quality:

```bash
cat ~/.local/state/cli-sub-agent/memory/*.jsonl | \
  head -n 20 | \
  jq -c '{content: .content, wing: .project, room: "csa-migrated", project: .project, source: "csa-legacy"}' | \
  while IFS= read -r line; do
    echo "$line" | mempal ingest --stdin --dry-run --json
  done
```

## Non-breaking constraints

- mempal's existing MCP interface, hook system, and search remain unchanged.
- CSA adds itself as a new source and wing alongside existing data.
- No schema migrations are required.
- Existing Claude Code, Codex, and Gemini consumers are unaffected.
- CSA should not rewrite existing mempal drawers during migration; import legacy
  records as new `csa-migrated` drawers.
- CSA should keep facts and tags inline in `content` unless a future mempal
  schema adds dedicated fields.

## Retrieval

CSA memories are normal mempal drawers, so retrieval uses the existing CLI and
MCP interfaces.

```bash
# Search CSA memories
mempal search "auth middleware decision" --wing my-project --room csa-session --top-k 5
```

Via MCP:

```text
mempal_search(query="auth middleware", wing="my-project", room="csa-session")
```

Useful retrieval patterns:

```bash
# Review findings for a project
mempal search "missing tests" --wing my-project --room csa-review --top-k 10

# Merge decisions and follow-ups
mempal search "follow-up refactor" --wing my-project --room csa-merge --top-k 10

# Legacy CSA memories after migration
mempal search "queue retry policy" --wing my-project --room csa-migrated --top-k 10
```

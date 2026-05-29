spec: task
name: "P16: xurl conversation recall — screen-visible content indexing + semantic search"
tags: [feature, conversation, search, cross-tool, recall, cli]
estimate: 3d
---

## Intent

Replace the full-JSONL conversation ingestion approach (PR #236 / issue #235) with a
targeted system that indexes **only screen-visible content** (user input + assistant
text output) from agent tool transcripts. Provide `mempal xurl` CLI subcommand for
semantic search and paginated recall across sessions and tools.

This is the mempal equivalent of `csa xurl recall` — but with embeddings for
semantic search rather than keyword/sequential browsing.

**Core principle**: most tool_use/tool_result content is noise. The gating mechanism
exists precisely to filter low-value content. Conversation turns that the user saw
on screen are the high-signal content worth indexing.

## Supersedes

- Issue #235 full-JSONL conversation indexing approach
- PR #236 `ingest-conversation` implementation
- Rework tracked by issue #239

## Decisions

### CSA sessions vs user-facing sessions

CSA frequently spawns tool sessions (CC, Codex, Hermes) as sub-agents — these are
agent-to-agent conversations the user never sees on screen. CSA may also use tmux
to create interactive sessions that bypass non-interactive limitations. Only
**user-facing sessions** (where the human typed and read responses) should be indexed.

**Two-level filtering**:

**Level 1 — Session-level**: Is this a user-facing session or a CSA sub-agent session?
- **CC**: CSA-spawned sessions live under CSA's state directory
  (`~/.local/state/cli-sub-agent/.../sessions/`), not `~/.claude/projects/`.
  Only JSONL under `~/.claude/projects/` is user-facing.
- **Codex**: CSA-spawned Codex sessions are tracked in CSA's state dir.
  User-facing sessions live in `$CODEX_HOME/sessions/`.
- **Hermes**: Check session directory location or parent process metadata.

**Level 2 — Turn-level**: Even within a user-facing session, not all "user" turns
are human-typed. The orchestrating model (main agent) generates prompts to CSA
sub-agents, and these appear as `type:"user"` in the JSONL. These model-generated
prompts must NOT be indexed as human input — they may contain errors, hallucinations,
or misinterpretations of user intent (e.g., the #235 full-JSONL approach was a model
decision, not user instruction).

Turn-level heuristics for CC:
- `userType: "external"` = human typed (reliable signal)
- `userType: "internal"` or absent = model/system generated
- Turns that are `tool_result` wrapped in `type:"user"` = tool feedback, skip
- Adjacent `tool_use` → `user(tool_result)` pairs = agent-internal loop, skip both

For assistant turns, only index `content[].type:"text"` blocks — these are what
the user saw on screen. Skip `tool_use` blocks (internal actions).

Schema tracks provenance:
- `is_csa_delegated BOOLEAN DEFAULT FALSE` — session-level: CSA sub-agent session
- `provenance TEXT NOT NULL DEFAULT 'human'` — turn-level: 'human' (user typed) |
  'agent' (model-generated prompt) | 'system' (hook/automated injection)

Default `mempal xurl search/timeline`:
- Excludes CSA-delegated sessions (`is_csa_delegated = false`)
- Excludes agent-generated turns (`provenance = 'human'` for user role)
- Includes all assistant text (screen output is always relevant)
- `--include-csa` and `--include-agent-prompts` flags to override

### Dedicated table over drawers

A new `conversation_turns` table rather than overloading the `drawers` table.
Rationale: conversation turns have distinct query patterns (by session, by tool,
chronological ordering, cross-tool timeline) that don't map well to wing/room
taxonomy. Search path is separate (`mempal xurl` not `mempal search`).

### Schema: `conversation_turns`

```sql
CREATE TABLE conversation_turns (
    id              TEXT PRIMARY KEY,   -- ulid
    session_id      TEXT NOT NULL,      -- tool-native session identifier
    tool            TEXT NOT NULL,      -- 'cc' | 'codex' | 'hermes'
    turn_index      INTEGER NOT NULL,   -- 0-based within session
    role            TEXT NOT NULL,      -- 'user' | 'assistant'
    content         TEXT NOT NULL,      -- raw screen-visible text
    timestamp_epoch REAL NOT NULL,      -- unix epoch seconds (float)
    token_count     INTEGER,            -- estimated token count
    project_path    TEXT,               -- working directory / project context
    git_branch      TEXT,               -- branch at time of turn
    is_csa_delegated BOOLEAN NOT NULL DEFAULT FALSE, -- true if CSA sub-agent session
    provenance       TEXT NOT NULL DEFAULT 'human', -- 'human' | 'agent' | 'system'
    UNIQUE(session_id, tool, turn_index)
);

CREATE TABLE conversation_turn_vectors (
    turn_id    TEXT PRIMARY KEY REFERENCES conversation_turns(id),
    vector     BLOB NOT NULL
);

CREATE INDEX idx_ct_session ON conversation_turns(session_id, tool);
CREATE INDEX idx_ct_timestamp ON conversation_turns(timestamp_epoch DESC);
CREATE INDEX idx_ct_project ON conversation_turns(project_path);
```

Uses `fork_ext_version` axis (not upstream PRAGMA). Target: `ext_v6`.

> Note: `fork_ext_version` is a sequential counter **independent of the milestone
> label**. This is milestone **P16**, but it lands as `ext_v6` — *not* `ext_v16` —
> because the existing fork-ext chain ends at `ext_v5` (vector-isolation) and this is
> the next migration in sequence (ext_v1=queue … ext_v5=vector-iso → ext_v6=this).

### Per-tool JSONL parsers

Three format-specific extractors, each returning `Vec<RawTurn>`:

```rust
struct RawTurn {
    session_id: String,
    tool: Tool,           // Cc | Codex | Hermes
    role: Role,           // User | Assistant
    content: String,
    timestamp_epoch: f64,
    project_path: Option<String>,
    git_branch: Option<String>,
}
```

**CC parser** — reads `~/.claude/projects/<id>/<conv>.jsonl`:
- Filter: `type IN ("user", "assistant")`
- Extract: `message.content[].type == "text"` → concatenate text blocks
- Skip: `tool_use`, `tool_result`, `progress`, `file-history-snapshot`, `queue-operation`
- Timestamp: `timestamp` field (ISO 8601) → epoch

**Codex parser** — reads `$CODEX_HOME/sessions/.../rollout-*.jsonl`:
- Filter: `response_item` with `payload.role == "assistant"` + `event_msg` with `payload.type == "agent_message"`
- User turns: inferred from request boundaries or explicit `role:"user"` entries
- Timestamp: `ts` field (RFC 3339) → epoch

**Hermes parser** — reads `~/.hermes/state.db` SQLite:
- Query: `SELECT * FROM messages WHERE role IN ('user', 'assistant') ORDER BY timestamp, id`
- Skip: `role = 'tool'`
- Apply `sanitize_context()` equivalent stripping on content
- Timestamp: `timestamp` column (Unix epoch float, already correct)

### Embedding pipeline

Reuse existing mempal embedder infrastructure (`OpenAiCompatibleEmbedder` with
model2vec fallback). Conversation turns are embedded individually (not chunked) —
most turns are already within the ~512 token embedding window.

For long assistant outputs (>512 tokens): split into overlapping chunks at
sentence boundaries, store multiple vectors per turn with a `chunk_index` column
(mirrors drawer chunking).

### CLI: `mempal xurl`

```
mempal xurl search <query>         # semantic search across all turns
mempal xurl search <query> --tool cc --since 7d
mempal xurl timeline               # chronological, newest first, paginated
mempal xurl timeline --tool codex --session <id>
mempal xurl ingest                 # scan all known tool data dirs, index new turns
mempal xurl ingest --tool cc --path <jsonl-path>  # manual single-file
mempal xurl stats                  # per-tool turn counts, date ranges
```

Output format: markdown with `[session] [tool] [timestamp] [role]` header per turn,
content below. `--format json` for structured output.

### Timestamp normalization

All tools' timestamps are normalized to Unix epoch float on ingest:
- CC: `chrono::DateTime::parse_from_rfc3339(timestamp)` → epoch
- Codex: same RFC 3339 parsing on `ts` field
- Hermes: already Unix epoch float

Cross-tool timeline queries order by `timestamp_epoch DESC`.

### Deduplication

On `mempal xurl ingest`, check `(session_id, tool, turn_index)` uniqueness.
If a turn already exists with identical content hash, skip. If content differs
(e.g., session was replayed/edited), update in place.

## Boundaries

- **No LLM API calls** — embedding only, no summarization or classification
- **No gating** — conversation turns bypass the gating pipeline (they are
  already filtered to screen-visible content; the "gating" happened at the
  parser level by excluding tool calls)
- **No daemon auto-ingest** — `mempal xurl ingest` is manual (or cron).
  SessionEnd hook from #235 should be removed or rewired to call xurl ingest
  for the completed session only
- **No MCP tool** — CLI-only for now. MCP `mempal_xurl_search` can be added
  later if agents need programmatic access
- **Read-only for Hermes DB** — open `state.db` in read-only mode, never write

## Scenarios

### S1: Index CC session turns

```
GIVEN a CC JSONL file at ~/.claude/projects/-foo/abc123.jsonl
  containing 50 entries (20 user text, 15 assistant text, 15 tool_use/tool_result)
WHEN mempal xurl ingest --tool cc
THEN conversation_turns has 35 rows (20 user + 15 assistant)
  AND each row has a non-null embedding vector
  AND timestamps are monotonically increasing within session
```

### S2: Semantic search across tools

```
GIVEN indexed turns from CC, Codex, and Hermes sessions
WHEN mempal xurl search "database migration strategy"
THEN results are ranked by embedding similarity
  AND results span multiple tools
  AND each result shows [tool] [session] [timestamp] [role]
  AND results are paginated (default 10 per page)
```

### S3: Timeline view with tool filter

```
GIVEN indexed turns from CC and Codex
WHEN mempal xurl timeline --tool cc --since 7d --limit 20
THEN shows the 20 most recent CC turns from last 7 days
  AND ordered newest-first
  AND user/assistant turns interleaved chronologically
```

### S4: Incremental re-ingest is idempotent

```
GIVEN a session already fully indexed
WHEN mempal xurl ingest runs again
THEN no duplicate rows created
  AND no embedding recomputation for unchanged content
```

### S5: Long assistant output chunked

```
GIVEN an assistant turn with 2000 tokens of text
WHEN indexed
THEN conversation_turn_vectors has multiple rows for this turn_id
  AND chunks overlap by ~64 tokens at sentence boundaries
  AND semantic search can match any chunk
```

### S6: Hermes SQLite read-only

```
GIVEN ~/.hermes/state.db exists with WAL mode
WHEN mempal xurl ingest --tool hermes
THEN opens state.db in SQLITE_OPEN_READONLY mode
  AND never creates -wal or -shm files
  AND turns extracted correctly with sanitized content
```

### S7: CSA-delegated sessions excluded by default

```
GIVEN CC JSONL files from:
  - ~/.claude/projects/-foo/user-session.jsonl (user-facing)
  - ~/.local/state/cli-sub-agent/.../sessions/.../output.log (CSA sub-agent)
WHEN mempal xurl ingest --tool cc
THEN user-session.jsonl turns have is_csa_delegated = false
  AND CSA session turns have is_csa_delegated = true
WHEN mempal xurl search "anything"
THEN only user-facing turns appear (CSA-delegated excluded)
WHEN mempal xurl search "anything" --include-csa
THEN both user-facing and CSA-delegated turns appear
```

### S8: Agent-generated prompts excluded from user recall

```
GIVEN a user-facing CC session where:
  - Turn 0: user types "sync upstream" (userType: "external")
  - Turn 1: assistant prints "Starting upstream sync..."
  - Turn 2: assistant calls csa run with a long prompt (tool_use)
  - Turn 3: user message is tool_result from csa (userType absent or "internal")
  - Turn 4: assistant prints "Sync complete, PR #238 created"
WHEN mempal xurl ingest --tool cc
THEN Turn 0 has provenance = 'human', role = 'user'
  AND Turn 1 has provenance = 'human', role = 'assistant'
  AND Turn 2 is skipped (tool_use, not screen text)
  AND Turn 3 is skipped (tool_result, not human input)
  AND Turn 4 has provenance = 'human', role = 'assistant'
WHEN mempal xurl search "upstream sync"
THEN returns Turn 0, Turn 1, Turn 4
  AND does NOT return the CSA prompt or tool_result
```

### S9: Cross-tool chronological alignment

```
GIVEN CC turns at 14:00-14:30 and Codex turns at 14:15-14:45
WHEN mempal xurl timeline (no tool filter)
THEN turns interleaved by timestamp_epoch across both tools
```

## DONE WHEN

1. `conversation_turns` and `conversation_turn_vectors` tables exist at ext_v6
2. CC, Codex, Hermes parsers each extract screen-visible content only
3. `mempal xurl ingest` indexes turns from all three tools
4. `mempal xurl search <query>` returns semantically relevant turns
5. `mempal xurl timeline` shows paginated chronological view
6. `mempal xurl stats` shows per-tool counts
7. Incremental re-ingest is idempotent (S4)
8. Old `ingest-conversation` CLI subcommand removed or redirected to `xurl ingest`
9. SessionEnd daemon hook removed or rewired
10. All scenarios pass

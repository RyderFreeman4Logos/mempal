# mempal hermes-agent plugin

Plugs mempal into hermes-agent as a drop-in MemoryProvider.
Replaces mem0 with a fully local BM25 + vector hybrid backend — no cloud API calls.

## Prerequisites

1. mempal built with `--features rest` and running:
   ```bash
   cargo build --features rest
   mempal serve   # starts MCP + REST on 127.0.0.1:3080
   ```
   Or start the daemon (auto-starts REST when built with `rest` feature):
   ```bash
   mempal daemon start
   ```

2. Python 3.9+ with no extra pip dependencies required (uses stdlib `urllib`).

## Setup

1. Copy or symlink this directory into hermes-agent's plugin search path:
   ```bash
   cp -r contrib/hermes-agent-plugin/mempal  /path/to/hermes-agent/plugins/memory/mempal
   ```

2. Configure hermes-agent to use the mempal provider:
   ```yaml
   # cli-config.yaml
   memory:
     provider: mempal
   ```

3. Optionally set environment variables:
   ```bash
   export MEMPAL_BASE_URL=http://127.0.0.1:3080   # default
   export MEMPAL_USER_ID=your-username              # default: hermes-user
   ```
   Or write `$HERMES_HOME/mempal.json`:
   ```json
   {
     "base_url": "http://127.0.0.1:3080",
     "user_id": "your-username"
   }
   ```

## Tools exposed to the model

| Tool | Description |
|------|-------------|
| `mempal_profile` | Recent memories via `/api/timeline` |
| `mempal_search` | Hybrid BM25+vector search via `/api/search` |
| `mempal_conclude` | Store a fact verbatim via `/api/ingest` |

## Memory routing

All memories are profile-scoped under `wing="hermes-user/{user_id}/{profile}"`.
The default profile is `default`, and Hermes may pass it as `agent_identity`
or `profile`.

- Turns → `room="turns"` or `room="turns/{platform}/{chat_id}[/{thread_id}]"`
- Explicit facts → `room="facts"` shared across chats for the same profile
- Session summaries → `room="sessions/{session_id}"`
- Built-in memory mirrors → `room="facts"`, scoped turns/session rooms, or `room="memory-mirror/{target}"`

When Hermes provides `project_id`, the plugin forwards it to `/api/search`,
`/api/timeline`, and `/api/ingest` for mempal project isolation. When only
`cwd` is available, the plugin derives the project scope from the directory
basename.

## Intelligence modes

Optional LLM-enhanced memory classification. Configure in `$HERMES_HOME/mempal.json`:

```json
{
  "base_url": "http://127.0.0.1:3080",
  "memory_intelligence": {
    "mode": "local_llm",
    "llm": {
      "base_url": "http://127.0.0.1:18009/v1",
      "model": "qwen3.6-27b-decensor-by-aeon",
      "timeout_secs": 30,
      "extra_body": {
        "chat_template_kwargs": { "enable_thinking": false }
      }
    }
  }
}
```

| Mode | Behavior |
|------|----------|
| `deterministic` | No text LLM calls (default). Explicit writes, BM25+vector search only. |
| `local_llm` | Use a local OpenAI-compatible endpoint for metadata extraction, fact extraction from turns, and session summaries. |
| `cloud_llm` | Use a paid/cloud endpoint for the same enhancements. |
| `auto` | Try configured LLM, fall back to deterministic on failure or missing config. |

All LLM output passes deterministic validation gates before acceptance.
Failed/slow LLM calls fall back to deterministic behavior without blocking writes.

## Circuit breaker

After 5 consecutive REST failures the provider pauses for 120 seconds to avoid
hammering a down server. Tool calls return a clear error message during cooldown.

## Readiness suite

Run the provider test suite to verify all features work correctly:

```bash
python3 contrib/hermes-agent-plugin/test_mempal_provider.py -v
```

The suite covers 67 tests across 12 test classes:

| Area | Tests | What it verifies |
|------|-------|-----------------|
| Scope isolation | 9 | Profiles, wings, turn rooms, project_id, session switch, prefetch keying |
| Write queue | 3 | Drain on shutdown, config-based availability, enqueue-not-thread |
| Write semantics | 6 | add/replace/remove, drawer_id tracking, supersession, typed metadata |
| Search results | 4 | Typed fields, None stripping, drawer_id in conclude/profile |
| Pinned facts | 4 | System prompt injection, TTL cache, empty handling, session invalidation |
| Intelligence modes | 9 | Mode switching, deterministic default, auto fallback, LLM breaker |
| LLM client | 4 | Unconfigured returns None, breaker status, extra_body preserved |
| Metadata validation | 6 | JSON parsing, code fence stripping, invalid kind/domain rejection |
| Fact extraction | 5 | Grounded facts accepted, hallucination rejected, cap at 10 |
| Provider activation | 6 | No-URL unavailable, tool schemas, breaker blocks/resets, health cache |
| Durable memory | 5 | Single add, replace supersedes, remove deletes, verbatim conclude |
| Reliability | 6 | 20-write drain, stale state clearing, breaker trip/reset, empty results |

### Readiness checklist

Before recommending `memory.provider=mempal` as authoritative backend:

- [x] Scope isolation: profiles, chats, threads, projects do not leak
- [x] Write semantics: add/replace/remove map correctly to ingest/supersede/delete
- [x] Typed metadata: search and profile results include drawer_id and provenance
- [x] Pinned facts: always-on context injected into system prompt
- [x] Write queue: single worker, drains on shutdown, no data loss
- [x] Intelligence modes: deterministic default, LLM optional, validation gates
- [x] Circuit breaker: trips after failures, resets after cooldown
- [x] Config: base_url, user_id, intelligence modes all configurable
- [x] All blocker issues closed: #212, #213, #214, #215, #216, #191

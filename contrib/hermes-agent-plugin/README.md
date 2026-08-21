# mempal hermes-agent integration

Three complementary integration paths — use any combination:

| Path | Plugin type | What it does | Requires hermes core changes? |
|------|-------------|-------------|------|
| **MemoryProvider** | Memory plugin | Mirror/sync hermes built-in memory to mempal, expose search/conclude tools | No |
| **Hooks** | General plugin | Inject deep mempal context per turn, capture tool observations | No |
| **MCP** | Config entry | Debug-only direct access to all mempal tools | No |

All three work without forking hermes-agent. When hermes upstream resolves
[#25526](https://github.com/NousResearch/hermes-agent/issues/25526) and
[#25527](https://github.com/NousResearch/hermes-agent/issues/25527)
(authoritative provider mode), the integration can be simplified to a single
MemoryProvider — the hooks plugin gracefully becomes redundant.

## Prerequisites

1. mempal built with `--features rest` and running:
   ```bash
   cargo build --features rest
   mempal doctor rest
   mempal serve   # starts MCP + REST on 127.0.0.1:3080 for local testing
   ```
   Or start the daemon (auto-starts REST when built with `rest` feature):
   ```bash
   mempal daemon start
   ```

2. Python 3.9+ with no extra pip dependencies required (uses stdlib `urllib`).

## Path 1: MemoryProvider (mirror/sync)

Plugs mempal into hermes-agent as a drop-in MemoryProvider.
Replaces mem0 with a fully local BM25 + vector hybrid backend — no cloud API calls.

### Setup

1. Check or explicitly refresh hermes-agent's user plugin copies:
   ```bash
   python3 contrib/hermes-agent-plugin/install_plugins.py \
     --check --hermes-home "$HERMES_HOME"
   python3 contrib/hermes-agent-plugin/install_plugins.py \
     --refresh --hermes-home "$HERMES_HOME"
   ```
   `--check` is read-only and returns non-zero for missing, changed, or extra
   plugin files. `--refresh` is an explicit opt-in: it stages and verifies both
   `mempal` and `mempal-hooks`, then replaces them with rollback protection.
   Tests must pass a temporary `--hermes-home`; neither command silently uses
   an alternate path after a filesystem error.

   The provider keeps Hermes' `MemoryProvider` / `register_memory_provider`
   discovery markers in the first 8192 bytes of `__init__.py`, so user plugin
   discovery works without copying into Hermes' bundled provider directory.

2. Configure hermes-agent:
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
     "user_id": "your-username",
     "memory_intelligence": { "mode": "deterministic" },
     "safe_mode": {
       "enabled": true,
       "min_importance": 3,
       "memory_kinds": ["knowledge", "profile_fact"],
       "context_budget_chars": 4000,
       "include_raw_turns": false
     }
   }
   ```

### Tools exposed to the model

| Tool | Description |
|------|-------------|
| `mempal_profile` | Recent memories via `/api/timeline` |
| `mempal_search` | Bounded hybrid/BM25 search via `/api/search`; partial/timeouts include redacted correlation and stage metadata |
| `mempal_conclude` | Store a fact verbatim via a durable ingest receipt |

`mempal_conclude` reports success only after the receipt reaches `completed`
with a non-empty `drawer_id`. A pending response includes `operation_id` and
`operation_key`; retry with that same `operation_key` to query or resume the
same durable operation without creating a second drawer.

### Memory routing

All memories are profile-scoped under `wing="hermes-user/{user_id}/{profile}"`.

- Turns → `room="turns"` or `room="turns/{platform}/{chat_id}[/{thread_id}]"`
- Explicit facts → `room="facts"` shared across chats for the same profile
- Session summaries → `room="sessions/{session_id}"`
- Built-in memory mirrors → `room="facts"`, scoped turns/session rooms, or `room="memory-mirror/{target}"`

### Intelligence modes

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

### Safe mode for coding-agent workflows

Safe mode is enabled by default. It keeps Hermes on the low-risk path:

- deterministic memory intelligence unless explicitly configured otherwise
- pinned/canonical facts are shown first and labeled authoritative
- search/profile hits keep `drawer_id`, source, importance, typed metadata, and
  are labeled `evidence/background` unless pinned or canonical
- low-importance raw evidence is filtered unless it passes `min_importance` or
  `memory_kinds`
- raw-turn retrieval is disabled unless `safe_mode.include_raw_turns=true`
- injected prefetch/pinned context is capped by `safe_mode.context_budget_chars`

Do not enable the separate `mempal-hooks` plugin for per-turn injection unless
you explicitly want raw turn capture/injection behavior beyond the conservative
MemoryProvider path.

### Multi-profile REST ports

Each mempal daemon profile should use a distinct local REST address. Keep the
host loopback-only:

```toml
[api]
enabled = true
addr = "127.0.0.1:3081"
```

Run `mempal doctor rest --addr 127.0.0.1:3081` to verify the binary feature,
endpoint reachability, required routes, and port owner before pointing Hermes at
that profile.

## Path 2: Hooks plugin (deep context injection)

General hermes plugin that registers lifecycle hooks. Works standalone or
alongside the MemoryProvider — they complement each other:

- **MemoryProvider**: mirrors hermes memory to mempal, provides 3 tools
- **Hooks plugin**: injects deep mempal context into every turn, captures tool observations

### Setup

1. Use Path 1's `install_plugins.py --check` or explicit `--refresh` command.
   The verified transaction installs both `mempal` and `mempal-hooks`; do not
   overlay a stale target with `cp -r`.

2. No extra config needed — shares `$HERMES_HOME/mempal.json` and env vars with the MemoryProvider.

### Hooks registered

| Hook | When | What it does |
|------|------|-------------|
| `pre_llm_call` | Before each LLM turn | Searches mempal for relevant memories matching the user message, injects them as turn context |
| `post_tool_call` | After each tool returns | Captures interesting tool results (shell, web search, code) as observation drawers in mempal |
| `on_session_start` | Session begins | Fetches mempal brief (or recent search) and caches session warmup injected on the next pre_llm_call |

### Context injection

On each turn, the `pre_llm_call` hook:
1. Searches mempal with the user's message (`/api/search`, top 8 results)
2. Formats results with memory_kind and importance tags
3. Returns as `{"context": "## Relevant memories (mempal)\n..."}` appended to the user message

### Tool observation capture

After allowlisted tool calls (bash, web_search, python, etc.), the `post_tool_call` hook:
1. Filters out mempal's own tools (no loops) and errored results
2. Truncates result to 2000 chars
3. Ingests to mempal as `room="tool-observations"`, `memory_kind="observation"`, `importance=1`

These low-importance observations enrich future searches without cluttering high-priority memory.

## Path 3: MCP server (full tool access)

For routine Hermes use, prefer Path 1 (MemoryProvider) and Path 2 (hooks) against
the daemon REST API. That path is daemon-owned, keeps automatic searches scoped,
and does not create extra SQLite holders.

Direct MCP registration gives the LLM access to **all** mempal tools (context,
knowledge cards, timeline, kg, etc.). The supported agent path is daemon-owned
loopback HTTP MCP at `/mcp` (build with `rest`, then run the daemon). Long-lived
stdio MCP is an extra holder/debug path: use it only for short debugging sessions
because it can hold `palace.db` open in a second mempal process.

### Temporary stdio setup

Only use this for a short debugging session, then remove it from
`~/.hermes/config.yaml` so Hermes does not keep a second mempal server process
alive:

```yaml
mcp_servers:
  mempal:
    transport: stdio
    command: "mempal"
    args: ["mcp"]
```

Hermes discovers all mempal MCP tools at startup and makes them available to
the LLM alongside built-in tools. Key tools the LLM gains:

| Tool | Description |
|------|-------------|
| `mempal_context` | Tiered context assembly (dao_tian → qi layers) |
| `mempal_search` | Hybrid BM25+vector search |
| `mempal_knowledge_cards` | Phase-2 knowledge retrieval |
| `mempal_timeline` | Chronological memory timeline |
| `mempal_kg` | Knowledge graph queries |
| `mempal_ingest` | Store new evidence |
| `mempal_pinned_facts` | Always-active canonical facts |
| `mempal_status` | System health and wing inventory |

## Recommended combination

For maximum enhancement before hermes authoritative mode lands:

```
MemoryProvider  →  mirror hermes writes, basic tools, system prompt facts
Hooks plugin    →  per-turn context injection, observation capture
MCP server      →  full tool palette for deep mempal operations
```

All three share the same mempal REST backend and `mempal.json` config.

## Circuit breaker

Both plugins use independent circuit breakers: after 5 consecutive REST
failures, the plugin pauses for 120 seconds to avoid hammering a down server.

## Readiness suite

Run the provider test suite to verify MemoryProvider features:

```bash
python3 contrib/hermes-agent-plugin/test_mempal_provider.py -v
```

## Future: authoritative mode

When hermes upstream merges #25526 (full memory bridge) and #25527
(authoritative provider mode), the integration simplifies to:

```yaml
memory:
  provider: mempal
  provider_mode: authoritative
```

At that point the hooks plugin becomes optional — the MemoryProvider gets
full memory operation pass-through and the LLM uses mempal as THE memory
backend. The hooks plugin can be kept for observation capture if desired.

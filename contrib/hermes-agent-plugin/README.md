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

When Hermes provides `project_id` (or `cwd` as a fallback), the plugin forwards
it to `/api/search`, `/api/timeline`, and `/api/ingest` for mempal project
isolation.

## Circuit breaker

After 5 consecutive REST failures the provider pauses for 120 seconds to avoid
hammering a down server. Tool calls return a clear error message during cooldown.

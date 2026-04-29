# Hook Payload Schema

This document describes the stdin payload interface for `mempal hook`.
It is intended for external integrators that want to emit Claude Code-style
session events into mempal without linking to mempal internals.

## Supported Events

`mempal hook` accepts four event commands. Each command also has a stable
snake-case alias used as the queue `kind`.

| Event command | Alias / queue kind | When to emit |
| --- | --- | --- |
| `PostToolUse` | `hook_post_tool` | After each tool call completes. |
| `UserPromptSubmit` | `hook_user_prompt` | When the user submits a prompt. |
| `SessionStart` | `hook_session_start` | At the beginning of an agent session. |
| `SessionEnd` | `hook_session_end` | At the end of an agent session. |

Examples:

```bash
mempal hook PostToolUse
mempal hook hook_post_tool
```

## Input Contract

All hook events read raw bytes from stdin. The input does not need to match a
fixed schema: JSON, plain text, and arbitrary metadata are all accepted. mempal
stores the payload as-is inside a capture envelope, except that invalid UTF-8 is
decoded lossily with the Unicode replacement character.

Small payloads are stored inline in the queued envelope. Payloads larger than
10 MiB are written to disk and represented by a path plus a preview.

## Captured Envelope

The hook command serializes the stdin payload into a `CapturedHookEnvelope` and
enqueues that JSON string in `pending_messages.payload`.

```json
{
  "event": "SessionEnd",
  "kind": "hook_session_end",
  "agent": "claude",
  "captured_at": "2026-04-29T12:34:56Z",
  "claude_cwd": "/path/to/working/directory",
  "payload": "{\"session_id\":\"abc\",\"summary\":\"Fixed auth bug\"}",
  "payload_path": null,
  "payload_preview": null,
  "original_size_bytes": 55,
  "truncated": false
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `event` | string | Canonical event name: `PostToolUse`, `UserPromptSubmit`, `SessionStart`, or `SessionEnd`. |
| `kind` | string | Queue kind and command alias, such as `hook_session_end`. |
| `agent` | string | Inferred agent name. Current recognized values are `claude`, `codex`, and `gemini`; consumers should treat the field as an open string. |
| `captured_at` | string | Capture timestamp in ISO-8601 format. |
| `claude_cwd` | string | `CLAUDE_PROJECT_CWD` when set; otherwise the hook process current directory; otherwise `"."`. |
| `payload` | string or null | Inline stdin payload when `original_size_bytes <= 10 MiB`. |
| `payload_path` | string or null | Absolute path to the oversize payload file when `original_size_bytes > 10 MiB`. |
| `payload_preview` | string or null | First 4 KiB preview for oversize payloads, truncated at a valid UTF-8 boundary. |
| `original_size_bytes` | number | Original stdin byte length before UTF-8 decoding or overflow handling. |
| `truncated` | boolean | `true` when the payload was stored out-of-line because it exceeded 10 MiB. |

Agent detection first checks JSON string fields named `agent`, `originator`,
and `model`, then falls back to substring matching in the payload text. If no
known agent name is found, the current implementation emits `claude`.

## Size Limits and Overflow

Inline payload limit: `10 * 1024 * 1024` bytes (`MAX_INLINE_PAYLOAD_BYTES`).

Preview limit: `4 * 1024` bytes (`PREVIEW_MAX_BYTES`).

When stdin exceeds the inline limit:

1. mempal writes the original bytes to `~/.mempal/hook-oversize/<blake3-hash>.json`.
2. `payload` is set to `null`.
3. `payload_path` points to the oversize file.
4. `payload_preview` contains the first 4 KiB of the payload.
5. `truncated` is set to `true`.

The oversize directory is derived from the configured database path. With the
default database, this is `~/.mempal/hook-oversize/`.

## Queueing and Processing

The hook path is designed to be fast. It loads config, opens the SQLite-backed
`PendingMessageStore`, performs one queue insert into `pending_messages`, and
returns without doing ingest work. Typical hook enqueue latency should stay
below 50 ms on a healthy local SQLite database.

The daemon processes queued hook envelopes asynchronously:

| Event | Default daemon target |
| --- | --- |
| `PostToolUse` | `wing=hooks-raw`, `room=<tool_name>` or `unknown-tool`. |
| `UserPromptSubmit` | `wing=hooks-raw`, `room=user-prompt`. |
| `SessionStart` | `wing=hooks-raw`, `room=session-lifecycle`. |
| `SessionEnd` | `wing=hooks-raw`, `room=session-lifecycle`; may also trigger session self-review extraction when enabled. |

`mempal hook` itself writes no stdout on success. For oversized payloads, it may
write a short diagnostic to stderr indicating that the payload was envelope-wrapped.

## Usage Examples

Session end with JSON metadata:

```bash
echo '{"session_id":"abc","summary":"Fixed auth bug"}' | mempal hook SessionEnd
```

Session start with plain text:

```bash
echo "Starting session for project X" | mempal hook SessionStart
```

Post-tool event with tool details:

```bash
echo '{"tool_name":"Read","input":{"file_path":"/src/main.rs"},"exit_code":0}' \
  | mempal hook PostToolUse
```

User prompt event with arbitrary integration metadata:

```bash
cat <<'JSON' | mempal hook hook_user_prompt
{
  "agent": "codex",
  "session_id": "abc",
  "prompt": "Continue the integration work",
  "metadata": {
    "source": "external-runner",
    "cwd": "/repo"
  }
}
JSON
```


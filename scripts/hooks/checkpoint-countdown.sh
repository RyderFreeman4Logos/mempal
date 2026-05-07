#!/usr/bin/env bash
# Session checkpoint Stop hook.
#
# Fires on Claude Code SessionStop. Directly saves the last assistant message
# from the current session JSONL as a mempal checkpoint — no agent involvement
# required (Stop hook output is not injected back into agent context).
#
# Guards:
#   - Sub-agents (CLAUDE_CODE_ENTRYPOINT=task) skip
#   - CSA sessions (CSA_SESSION_ID set) skip
#   - Non-interactive (no TERM, no CLAUDECODE) skip
#   - Dedup via atomic mkdir lock (TTL: 300s — Stop-event dedup window)
set -euo pipefail

# Guard: sub-agents should not trigger checkpoints
# NOTE: env var is CLAUDE_CODE_ENTRYPOINT (no underscore), not CLAUDE_CODE_ENTRY_POINT
if [ "${CLAUDE_CODE_ENTRYPOINT:-}" = "task" ]; then
    exit 0
fi

# Guard: CSA sessions should not trigger checkpoints
if [ -n "${CSA_SESSION_ID:-}" ]; then
    exit 0
fi

# Guard: non-interactive sessions (headless CI, cron, pipe)
# NOTE: Cannot use `[ -t 0 ]` — Claude Code hooks always run as subprocesses
# without a TTY. CLAUDECODE=1 is set in all Claude Code sessions.
if [ -z "${TERM:-}" ] && [ -z "${CLAUDECODE:-}" ]; then
    exit 0
fi

PROJECT_HASH=$(echo -n "$(pwd)" | sha256sum | cut -c1-12)
LOCKFILE="/tmp/mempal-checkpoint-${PROJECT_HASH}.lock"
LOCK_TTL_SECS=300
LOCK_HELD=0
SAVED=0

cleanup_lock() {
    # Release lock only when no checkpoint was saved, so the 300s dedup window
    # suppresses duplicate Stop events after a successful save.
    if [ "$LOCK_HELD" -eq 1 ] && [ "$SAVED" -eq 0 ]; then
        rmdir "$LOCKFILE" 2>/dev/null || true
        LOCK_HELD=0
    fi
}

lock_is_stale() {
    local mtime now age
    mtime=$(stat -c %Y "$LOCKFILE" 2>/dev/null) || return 1
    now=$(date +%s)
    age=$((now - mtime))
    [ "$age" -ge "$LOCK_TTL_SECS" ]
}

trap cleanup_lock EXIT INT TERM

# Dedup: mkdir is atomic — if it succeeds, we hold the lock
if ! mkdir "$LOCKFILE" 2>/dev/null; then
    if lock_is_stale; then
        rmdir "$LOCKFILE" 2>/dev/null || true
        if ! mkdir "$LOCKFILE" 2>/dev/null; then
            exit 0
        fi
    else
        exit 0
    fi
fi
LOCK_HELD=1

PROJECT_NAME="${MEMPAL_PROJECT:-$(basename "$(pwd)")}"

# Locate session JSONL: try CLAUDE_CODE_SESSION_ID first, fall back to most
# recent *.jsonl by mtime (CLAUDE_CODE_SESSION_ID is often not in hook env).
# Project dir in ~/.claude/projects/ is cwd with / replaced by -.
PROJECT_DIR_NAME=$(pwd | sed 's|/|-|g')
PROJECT_DIR="$HOME/.claude/projects/${PROJECT_DIR_NAME}"
SESSION_JSONL=""
if [ -n "${CLAUDE_CODE_SESSION_ID:-}" ]; then
    SESSION_JSONL="${PROJECT_DIR}/${CLAUDE_CODE_SESSION_ID}.jsonl"
    [ -f "$SESSION_JSONL" ] || SESSION_JSONL=""
fi
if [ -z "$SESSION_JSONL" ]; then
    SESSION_JSONL=$(ls -t "$PROJECT_DIR"/*.jsonl 2>/dev/null | head -1)
fi

if [ -z "$SESSION_JSONL" ] || [ ! -f "$SESSION_JSONL" ]; then
    exit 0
fi

# Extract last assistant message; exit silently if empty or extraction fails
CONTENT=$(mempal checkpoint extract "$SESSION_JSONL" --last 1 2>/dev/null) || exit 0
if [ -z "$CONTENT" ]; then
    exit 0
fi

# Save checkpoint directly; 10s timeout prevents blocking next session start.
# Hold lock for 300s after success to suppress duplicate Stop events.
if printf '%s' "$CONTENT" | timeout 10s mempal checkpoint save \
        --project "$PROJECT_NAME" >/dev/null 2>&1; then
    SAVED=1
else
    printf 'mempal checkpoint save failed for project %s\n' "$PROJECT_NAME" >&2
fi

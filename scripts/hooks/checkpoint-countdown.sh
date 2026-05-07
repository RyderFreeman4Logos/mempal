#!/usr/bin/env bash
# Session checkpoint Stop hook.
#
# Fires on Claude Code Stop. Directly saves the last assistant message
# from the current session JSONL as a mempal checkpoint — no agent involvement
# required (Stop hook output is not injected back into agent context).
#
# Guards:
#   - Sub-agents (CLAUDE_CODE_ENTRYPOINT=task) skip
#   - CSA sessions (CSA_SESSION_ID set) skip
#   - Non-interactive (no TERM, no CLAUDECODE) skip
#   - Dedup via atomic mkdir lock (TTL: 180s — testing interval)
#
# Debug log: /tmp/mempal-checkpoint-debug.log
set -eu

LOG="/tmp/mempal-checkpoint-debug.log"
log() { printf '[%s] %s\n' "$(date -Iseconds)" "$*" >> "$LOG"; }

log "=== Stop hook fired. PID=$$ CWD=$(pwd)"
log "  CLAUDE_CODE_ENTRYPOINT=${CLAUDE_CODE_ENTRYPOINT:-<unset>}"
log "  CSA_SESSION_ID=${CSA_SESSION_ID:-<unset>}"
log "  CLAUDE_CODE_SESSION_ID=${CLAUDE_CODE_SESSION_ID:-<unset>}"
log "  CLAUDECODE=${CLAUDECODE:-<unset>} TERM=${TERM:-<unset>}"

# Guard: sub-agents should not trigger checkpoints
if [ "${CLAUDE_CODE_ENTRYPOINT:-}" = "task" ]; then
    log "SKIP: sub-agent (ENTRYPOINT=task)"
    exit 0
fi

# Guard: CSA sessions should not trigger checkpoints
if [ -n "${CSA_SESSION_ID:-}" ]; then
    log "SKIP: CSA session"
    exit 0
fi

# Guard: non-interactive sessions
if [ -z "${TERM:-}" ] && [ -z "${CLAUDECODE:-}" ]; then
    log "SKIP: non-interactive (no TERM, no CLAUDECODE)"
    exit 0
fi

PROJECT_HASH=$(echo -n "$(pwd)" | sha256sum | cut -c1-12)
LOCKFILE="/tmp/mempal-checkpoint-${PROJECT_HASH}.lock"
LOCK_TTL_SECS=180
LOCK_HELD=0
SAVED=0

cleanup_lock() {
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

# Dedup: mkdir is atomic
if ! mkdir "$LOCKFILE" 2>/dev/null; then
    if lock_is_stale; then
        rmdir "$LOCKFILE" 2>/dev/null || true
        if ! mkdir "$LOCKFILE" 2>/dev/null; then
            log "SKIP: lock contention after stale cleanup"
            exit 0
        fi
    else
        log "SKIP: lock held (not stale)"
        exit 0
    fi
fi
LOCK_HELD=1
log "Lock acquired: $LOCKFILE"

PROJECT_NAME="${MEMPAL_PROJECT:-$(basename "$(pwd)")}"

# Locate session JSONL
PROJECT_DIR_NAME=$(pwd | sed 's|/|-|g')
PROJECT_DIR="$HOME/.claude/projects/${PROJECT_DIR_NAME}"
SESSION_JSONL=""

if [ -n "${CLAUDE_CODE_SESSION_ID:-}" ]; then
    SESSION_JSONL="${PROJECT_DIR}/${CLAUDE_CODE_SESSION_ID}.jsonl"
    [ -f "$SESSION_JSONL" ] || SESSION_JSONL=""
fi

if [ -z "$SESSION_JSONL" ]; then
    # Fall back to most recent JSONL by mtime. Use find to avoid SIGPIPE from ls|head.
    SESSION_JSONL=$(find "$PROJECT_DIR" -maxdepth 1 -name '*.jsonl' -printf '%T@ %p\n' 2>/dev/null \
        | sort -rn | head -1 | cut -d' ' -f2-)
fi

if [ -z "$SESSION_JSONL" ] || [ ! -f "$SESSION_JSONL" ]; then
    log "SKIP: no session JSONL found in $PROJECT_DIR"
    exit 0
fi
log "JSONL: $SESSION_JSONL"

# Extract last assistant message
CONTENT=$(mempal checkpoint extract "$SESSION_JSONL" --last 1 2>>"$LOG") || {
    log "SKIP: mempal checkpoint extract failed (exit $?)"
    exit 0
}
if [ -z "$CONTENT" ]; then
    log "SKIP: extract returned empty content"
    exit 0
fi
log "Extracted ${#CONTENT} bytes"

# Save checkpoint; 30s timeout (embedding can be slow on first call)
if printf '%s' "$CONTENT" | timeout 30s mempal checkpoint save \
        --project "$PROJECT_NAME" >>"$LOG" 2>&1; then
    SAVED=1
    log "OK: checkpoint saved for project=$PROJECT_NAME"
else
    log "FAIL: mempal checkpoint save failed (exit $?)"
fi

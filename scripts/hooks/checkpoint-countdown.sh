#!/usr/bin/env bash
# Session checkpoint countdown hook (SessionStop trigger).
#
# Injected into the main Claude Code agent's prompt via session stop hook.
# The agent reads this output, then runs a background sleep 3000 (50 min)
# countdown. When it fires, the agent checks for user activity and writes
# a checkpoint to mempal if idle — this also refreshes KV cache TTL.
#
# Guards:
#   - Sub-agents (CLAUDE_CODE_ENTRY_POINT=task) skip
#   - CSA sessions (CSA_SESSION_ID set) skip
#   - Non-interactive (stdin not TTY) skip
#   - Dedup via flag file prevents stacked countdowns
set -euo pipefail

# Guard: sub-agents should not trigger checkpoints
if [ "${CLAUDE_CODE_ENTRY_POINT:-}" = "task" ]; then
    exit 0
fi

# Guard: CSA sessions should not trigger checkpoints
if [ -n "${CSA_SESSION_ID:-}" ]; then
    exit 0
fi

# Guard: non-interactive sessions (piped stdin)
if [ ! -t 0 ] 2>/dev/null; then
    exit 0
fi

PROJECT_HASH=$(echo -n "$(pwd)" | sha256sum | cut -c1-12)
LOCKFILE="/tmp/mempal-checkpoint-${PROJECT_HASH}.lock"
LOCK_TTL_SECS=3600
LOCK_HELD=0
PROMPT_EMITTED=0

cleanup_lock() {
    if [ "$LOCK_HELD" -eq 1 ]; then
        rmdir "$LOCKFILE" 2>/dev/null || true
        LOCK_HELD=0
    fi
}

lock_is_stale() {
    local now mtime age
    mtime=$(stat -c %Y "$LOCKFILE" 2>/dev/null) || return 1
    now=$(date +%s)
    age=$((now - mtime))
    [ "$age" -ge "$LOCK_TTL_SECS" ]
}

on_exit() {
    if [ "$PROMPT_EMITTED" -eq 0 ]; then
        cleanup_lock
    fi
}

trap on_exit EXIT INT TERM

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
LOCKFILE_SHELL=$(printf '%q' "$LOCKFILE")
PROJECT_NAME_SHELL=$(printf '%q' "$PROJECT_NAME")

cat <<CHECKPOINT_PROMPT

[SESSION CHECKPOINT] A tool stop was detected. To preserve your session state
before KV cache expires (1h TTL), run this background countdown:

\`\`\`bash
export MEMPAL_CHECKPOINT_LOCKFILE=$LOCKFILE_SHELL
export MEMPAL_CHECKPOINT_PROJECT=$PROJECT_NAME_SHELL
bash -lc 'trap '\''rmdir "$MEMPAL_CHECKPOINT_LOCKFILE" 2>/dev/null || true'\'' EXIT; sleep 3000; mempal checkpoint save --project "$MEMPAL_CHECKPOINT_PROJECT" --content "Session checkpoint auto-saved. Summarize current work state here."'
\`\`\`

Run the countdown command with run_in_background: true. When the notification fires
(~50 min), check if the user has been active. If no activity, write a checkpoint
summarizing: (1) what you were working on, (2) key decisions made, (3) next steps.
The write itself generates tokens that refresh the KV cache.

CHECKPOINT_PROMPT
PROMPT_EMITTED=1

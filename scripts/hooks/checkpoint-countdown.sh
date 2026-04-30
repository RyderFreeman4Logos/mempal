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

LOCKFILE="/tmp/mempal-checkpoint-pending-$$"

# Dedup: if a countdown is already pending for this session, skip
if ls /tmp/mempal-checkpoint-pending-* 1>/dev/null 2>&1; then
    exit 0
fi

PROJECT_NAME="${MEMPAL_PROJECT:-$(basename "$(pwd)")}"

cat <<CHECKPOINT_PROMPT

[SESSION CHECKPOINT] A tool stop was detected. To preserve your session state
before KV cache expires (1h TTL), run this background countdown:

\`\`\`bash
touch $LOCKFILE && sleep 3000 && mempal checkpoint save --project "$PROJECT_NAME" --content "$(echo 'Session checkpoint auto-saved. Summarize current work state here.')" && rm -f $LOCKFILE
\`\`\`

Run the sleep command with run_in_background: true. When the notification fires
(~50 min), check if the user has been active. If no activity, write a checkpoint
summarizing: (1) what you were working on, (2) key decisions made, (3) next steps.
The write itself generates tokens that refresh the KV cache.

CHECKPOINT_PROMPT

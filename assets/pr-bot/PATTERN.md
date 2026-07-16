# pr-bot Executor Pattern

Canonical **tracked** packaging for mempal agent workflows.

This path is the install/runtime source of truth for pr-bot executor steps in
this repository. Do **not** rely on gitignored agent-local paths such as
`.claude/PATTERN.md` (covered by `~/.gitignore_noai`).

Full CSA skill packaging (SKILL + workflow.toml + scripts) remains available via
the pinned `cli-sub-agent` package in `weave.lock` after `weave` install, under
`patterns/pr-bot/` (local symlink tree). Fresh clones without weave still need
this tracked file so executor sub-agents can resolve a PATTERN without hand-
created local files.

## Steps

1. **Commit check**: Ensure all changes are committed. Record `WORKFLOW_BRANCH`.
2. **Local pre-PR review** (SYNCHRONOUS): use SHA-verified fast-path first. If mismatched/missing, run full `csa review --branch "${DEFAULT_BRANCH}" --fix --max-rounds 3`. Sets `REVIEW_COMPLETED=true`.
3. **Push and ensure PR** (PRECONDITION: `REVIEW_COMPLETED=true`): push with `--force-with-lease`, resolve PR by owner-aware lookup. Create if missing.
3a. Check `csa config get pr_review.cloud_bot --default true`. If `false` → skip Steps 4-9.
4. **Trigger cloud bot**: Round 0 follows `cloud_bot_trigger`. Round 1+ posts explicit retrigger. Wait then poll via `patterns/pr-bot/scripts/pr-bot-wait.sh` (when weave package is installed).
5. **Evaluate bot comments**: Classify A (fixed), B (false positive), C (real issue).
6. **Staleness filter**: check if referenced code changed since comment.
7. **Arbitrate** Category B via `csa debate`.
8. **Fix** Category C via `csa review --fix`. Commit, review gate, push.
9. **Continue loop** until `REVIEW_ROUND >= MAX_REVIEW_ROUNDS` (default 10).
10. **Clean resubmission** if fixes accumulated.
11. **Merge**: `gh pr merge --${MERGE_STRATEGY}`, sync local default branch.
12. **Design-opportunity pass** (post-merge, only after successful merge): for non-trivial
    `dev2merge` / `issue-drain` work, ask whether a reusable design insight should be
    recorded. If yes, call:

    ```
    mempal insight record \
      --source review-finding \
      --scope issue \
      --target github-issue \
      --evidence <issue-or-session-ref> \
      --summary <content-free-insight> \
      --rule <acceptance-or-reusable-rule> \
      --priority 4
    ```

    Summary and rule must be content-free (no secrets, no private repo paths that leak
    credentials). If no reusable insight, explicitly note `no reusable insight` in the
    agent closeout and skip `mempal insight record`. Drain later via
    `mempal insight list --status open --min-priority 4` and
    `mempal insight resolve <id> --actor <agent> --note <target-ref>` (see
    `mempal insight runbook`).

## Path resolution contract

Executor resolution order for mempal:

1. `assets/pr-bot/PATTERN.md` (this file — always present in a fresh clone)
2. `patterns/pr-bot/PATTERN.md` (optional; weave-installed full package)
3. Agent-local copies under `.claude/` are **not** packaging and must not be
   treated as complete fixes for missing tracked content

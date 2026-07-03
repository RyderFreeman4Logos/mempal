---
name: upstream-sync
description: >
  Selectively port new features and bug fixes from upstream to a heavily diverged fork.
  Uses a tracking file (.upstream-sync.json) to resume from the last synced commit.
  Classifies each upstream commit as port/skip, then delegates implementation to CSA
  subagents to keep the main agent context clean. Handles massively diverged codebases
  where direct merge/rebase would produce hundreds of unresolvable conflicts.
  Trigger: "sync upstream", "merge upstream", "同步 upstream", "拉取上游", "port upstream".
---

# Upstream Sync (Selective Porting)

Orchestrate selective upstream-to-fork synchronization for heavily diverged codebases.

## When to Use

User says: "sync upstream" / "merge upstream" / "upstream sync" / "同步 upstream" / "拉取上游" / "port upstream"

## Core Architecture

- **You (main agent)** = orchestrator. Parse tracking file, read recon JSON, make port/skip decisions, talk to user. NEVER read large diffs yourself.
- **CSA/subagent** = executor. Heavy lifting: commit analysis, code porting, conflict resolution.
- **Rule**: Keep main agent context CLEAN. Delegate all code reading and writing to subagents.
- **Tracking file**: `.upstream-sync.json` at repo root — records the last synced upstream commit.

## Why This Exists

This fork is **971+ commits ahead** of upstream with massive architectural divergence
(shared files differ 5-25x in line count). A direct `git merge` or `git rebase` would
produce hundreds of conflicts across every shared file, most of which are irrelevant
because the fork has rewritten those sections entirely.

Instead, this skill:
1. Reads `.upstream-sync.json` to find where the last sync stopped
2. Lists only NEW upstream commits since last sync
3. Classifies each as port/skip (delegate analysis to subagent)
4. Ports the valuable ones one-by-one via CSA
5. Updates the tracking file

## Variables

| Variable | Source |
|----------|--------|
| `LAST_SYNCED_COMMIT` | `.upstream-sync.json` → `last_synced_commit` |
| `UPSTREAM_TIP` | `git rev-parse upstream/{UPSTREAM_DEFAULT}` |
| `UPSTREAM_DEFAULT` | `git symbolic-ref refs/remotes/upstream/HEAD` |
| `LOCAL_DEFAULT` | `git symbolic-ref refs/remotes/origin/HEAD` |
| `BRANCH` | `sync/upstream-{date}` |

---

## Phase 0: Pre-flight

### 0.1 Dirty Tree Check

```bash
git status --porcelain
```

Non-empty → **STOP**. Ask user to stash/commit.

### 0.2 Fetch Upstream

```bash
git fetch upstream
```

### 0.3 Read Tracking File

```bash
cat .upstream-sync.json
```

If missing, warn user and attempt to detect the last sync point:
```bash
LAST_SYNCED_COMMIT=$(git merge-base HEAD upstream/main)
```
Create the tracking file with this as the initial value. Tell the user.

Extract `last_synced_commit` → `LAST_SYNCED_COMMIT`.

### 0.4 Detect Branches

```bash
UPSTREAM_DEFAULT=$(git symbolic-ref refs/remotes/upstream/HEAD 2>/dev/null | sed 's|refs/remotes/upstream/||' || echo main)
LOCAL_DEFAULT=$(git symbolic-ref refs/remotes/origin/HEAD 2>/dev/null | sed 's|refs/remotes/origin/||' || echo main)
```

### 0.5 Quick Check: Anything New?

```bash
UPSTREAM_TIP=$(git rev-parse upstream/$UPSTREAM_DEFAULT)
NEW_COMMIT_COUNT=$(git rev-list --count $LAST_SYNCED_COMMIT..$UPSTREAM_TIP)
```

- `0` → Tell user "✅ Already synced through {UPSTREAM_TIP}" → **STOP**
- `>0` → Show user:
  ```
  📊 Upstream has {N} new commits since last sync ({LAST_SYNCED_COMMIT}):
  {git log --oneline LAST_SYNCED_COMMIT..UPSTREAM_TIP}
  ```
  Wait for user to say "继续" / "proceed" / "go".

---

## Phase 1: Reconnaissance (Subagent)

### Goal

Classify each new upstream commit as port or skip, producing a structured JSON the main agent can triage without reading diffs.

### 1.1 Delegate to Subagent

Write the recon prompt (from `prompts/recon.md`) to a temp file with variables filled in, then launch a CSA session:

```bash
csa run --sa-mode false --tool codex --prompt-file /tmp/upstream-sync-recon.md
```

The subagent reads each commit diff, compares against the fork's current code, and outputs a JSON classification.

**Why subagent**: Reading N commit diffs would flood the main agent's context. The subagent returns a compact JSON summary.

**Fallback** (if CSA unavailable): Use `delegate_task` with a leaf subagent to do the same analysis. Or ask user permission for main agent to do it directly (only if <3 commits).

### 1.2 Parse Recon Results

Read the subagent output JSON. Present a summary table to the user:

```
📊 Recon Results:
SHA      | Category         | Complexity | Action
abc1234  | port_clean       | trivial    | cherry-pick
def5678  | port_adapt       | moderate   | manual port
ghi9012  | skip_already_have| -          | skip (fork has X)
...
```

Ask user: "Which commits should I port? (all/recommended/none/select)"

**Default recommendation**: port all `port_clean` + `port_adapt`, skip the rest.

---

## Phase 2: Porting (One Commit at a Time)

### 2.1 Create Sync Branch

Use the project's standard branch creation workflow (per AGENTS.md).
```bash
BRANCH="sync/upstream-$(date +%Y-%m-%d)"
git checkout -b "$BRANCH" "$LOCAL_DEFAULT"
```

### 2.2 For Each Selected Commit

**CRITICAL: Port ONE commit at a time.** This keeps conflicts isolated and reviewable.

For each commit, determine the strategy from recon data:

#### Strategy: clean_cherry_pick (new files or non-conflicting additive changes)

Delegate to CSA:

```bash
csa run --sa-mode false --tool codex "Port upstream commit {SHA} to this fork via cherry-pick. Strategy: clean_cherry_pick. Read skills/upstream-sync/prompts/port.md for instructions."
```

#### Strategy: manual_adapt (shared files with massive divergence)

**This is the most common case for this fork.** The subagent must:
1. Read the upstream diff to understand the *semantic change*
2. Grep the fork for where this concept lives now
3. Implement the equivalent change in the fork's architecture
4. Build + test

Delegate to CSA with explicit context:

```bash
csa run --sa-mode false --tool codex "Port upstream commit {SHA} ({MESSAGE}) to this fork. The fork has heavily rewritten the affected files. Read skills/upstream-sync/prompts/port.md for instructions. Strategy: manual_adapt. Upstream commit diff: $(git show {SHA})"
```

#### Strategy: new_file_copy (upstream adds entirely new files)

```bash
# Simplest case: copy file from upstream, add module declaration
git checkout upstream/$UPSTREAM_DEFAULT -- src/new_file.rs
# Then manually add to mod.rs / lib.rs as needed
```

Can be done by main agent if trivial (1-2 files), or delegated to CSA.

### 2.3 Post-Port AI-Config Reject

After each port, ensure no AI-config files leaked into the staging area.
Do NOT use `git reset` or `git checkout` on AI-config files — those are
never tracked in the fork and live as symlinks. Instead, simply unstage
them if CSA accidentally staged them:

```bash
# Only unstage — never checkout/reset AI-config paths
git restore --staged AGENTS.md CLAUDE.md GEMINI.md .claude/ .codex/ .gemini/ .agents/ weave.lock 2>/dev/null || true
```

### 2.4 Build + Test After Each Commit

After porting each commit, build and test before moving to the next:

```bash
cargo fmt --all --check && cargo clippy --workspace -- -D warnings && cargo test --workspace
```

If a port breaks the build, delegate the fix to CSA (max 2 rounds). If unfixable, revert the port commit following the project's standard workflow and report.

---

## Phase 3: Quality Gates

### 3.1 Full Build + Test

After all commits are ported:

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

### 3.2 CSA Review

```bash
csa review --range main...HEAD --sa-mode true
```

Fix any findings (max 2 rounds via CSA `--fix-finding`).

---

## Phase 4: Delivery

### 4.1 Update Tracking File

**CRITICAL**: Update `.upstream-sync.json` to record the new sync point:

```bash
python3 -c "
import json, datetime
with open('.upstream-sync.json') as f:
    data = json.load(f)
data['last_synced_commit'] = '$UPSTREAM_TIP'
data['last_synced_date'] = datetime.date.today().isoformat()
data['syncs'].append({
    'date': datetime.date.today().isoformat(),
    'commit': '$UPSTREAM_TIP',
    'method': 'selective_port',
    'ported': [<list of ported SHAs>],
    'skipped': [<list of skipped SHAs with reasons>],
    'note': 'Ported N commits via selective porting'
})
with open('.upstream-sync.json', 'w') as f:
    json.dump(data, f, indent=2)
"
# Commit the tracking file update following the project's standard
# commit workflow (pre-commit hooks, quality gates, etc.). Do NOT
# use raw git commit — invoke the project's commit skill or toolchain.
```

### 4.2 Push + PR

Follow the project's standard push/PR workflow (per AGENTS.md Practice 015).
Use `gh pr create` with appropriate flags. The PR body template:

```
## Summary
- Ported {N} upstream commits via selective porting
- Skipped {M} commits (already have / obsolete / conflict risk)
- Tracked in .upstream-sync.json

## Ported Commits
{list with SHAs and messages}

## Skipped Commits
{list with SHAs and skip reasons}

## Quality Gates
- [x] Build passed
- [x] Tests passed
- [x] CSA review {PASS/rounds}
"
```

### 4.3 Merge

Ask user: "PR created: {url}. 合并？"

---

## Token Efficiency Design

| Operation | Who Does It | Why |
|-----------|-------------|-----|
| Read tracking file | Main agent | Tiny file, <500 bytes |
| List new commits | Main agent | `git log --oneline`, compact |
| Read commit diffs | **Subagent** | Avoids flooding context with diffs |
| Classify port/skip | **Subagent** | Returns compact JSON, not raw diffs |
| Implement ports | **CSA** | Heavy code reading + writing |
| Build/test | Main agent | Just reads exit codes |
| Update tracking file | Main agent | Simple JSON edit |

**Main agent never reads a raw diff**. All diff analysis is delegated to subagents that return structured summaries.

---

## .upstream-sync.json Format

```json
{
  "last_synced_commit": "abc1234...",
  "last_synced_date": "2026-05-20",
  "upstream_remote": "upstream",
  "upstream_branch": "main",
  "syncs": [
    {
      "date": "2026-05-20",
      "commit": "abc1234...",
      "method": "selective_port|manual_merge|cherry_pick",
      "ported": ["sha1", "sha2"],
      "skipped": [{"sha": "sha3", "reason": "already_have"}],
      "note": "free text"
    }
  ]
}
```

---

## Edge Cases

### Fork already has an equivalent feature

When recon classifies a commit as `skip_already_have`, record in the tracking file *why* it was skipped. If later recon shows the upstream version has improvements over the fork's version, flag it for the user: "Upstream commit X improves feature Y. Your fork has a different implementation in {file}. Port the improvement?"

### Upstream changes a file the fork deleted

If upstream modifies `src/old_module.rs` but the fork deleted it, skip the commit. The fork's architecture may have moved that functionality elsewhere.

### Upstream adds a new dependency

Check `Cargo.toml` changes. If upstream adds a new crate, decide whether the fork needs it. If the fork uses a different crate for the same purpose, skip.

### Upstream changes database schema

**High risk.** Always flag for user review. The fork has its own schema migrations (fork_ext tables, etc.). Porting upstream schema changes requires careful integration.

---

## Subagent Delegation Patterns

### Pattern 1: CSA for code analysis + implementation

```bash
csa run --sa-mode false --tool codex --prompt-file /tmp/recon-filled.md
```

Best for: multi-file analysis, complex porting, conflict resolution.

### Pattern 2: delegate_task for parallel recon

```python
# Launch multiple leaf subagents to analyze different commits in parallel
delegate_task(tasks=[
    {"goal": "Analyze upstream commit abc123 for portability...", ...},
    {"goal": "Analyze upstream commit def456 for portability...", ...},
])
```

Best for: when there are many commits to analyze and they're independent.

### Pattern 3: Main agent for trivial operations

Only for: reading tracking file, git log, creating branches, updating JSON.
Never for: reading code diffs, resolving conflicts, implementing features.

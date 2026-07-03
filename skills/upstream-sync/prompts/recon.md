# Recon Prompt Template

You are analyzing upstream commits to decide the porting strategy for a heavily diverged fork.

## Context

- Repo: `{REPO_DIR}`
- Local default branch: `{LOCAL_DEFAULT}`
- Upstream branch: `{UPSTREAM_DEFAULT}`
- Last synced upstream commit: `{LAST_SYNCED_COMMIT}`
- Target upstream commit: `{UPSTREAM_TIP}`

## Task

For each commit in range `{LAST_SYNCED_COMMIT}..{UPSTREAM_TIP}`:

1. Read the commit message, diff, and changed files.
2. Classify each commit into ONE of these categories:

| Category | Description | Porting Strategy |
|----------|-------------|-----------------|
| `port_clean` | New file or additive change to a file the fork hasn't modified | `git cherry-pick` or direct copy |
| `port_adapt` | Changes a file the fork also modified, but the concept applies | Manual reimplementation referencing upstream diff |
| `skip_already_have` | Fork already has this feature (possibly different impl) | Skip, document why |
| `skip_obsolete` | Upstream feature made obsolete by fork's own changes | Skip, document why |
| `skip_conflict_risk` | Touches core architecture that fork completely rewrote | Skip unless user explicitly wants it |
| `docs_only` | Only docs/specs/changelog, no code | `git cherry-pick` docs only (optional) |

3. For each `port_adapt` commit, identify:
   - Which specific upstream functions/blocks changed
   - What the fork's equivalent code looks like (grep for the function/type name)
   - Whether the fork already has an equivalent feature

## Output Format

Output ONLY this JSON, no other text:

```json
{
  "upstream_tip": "{UPSTREAM_TIP}",
  "commit_count": N,
  "commits": [
    {
      "sha": "abc1234",
      "message": "short subject",
      "category": "port_clean|port_adapt|skip_already_have|skip_obsolete|skip_conflict_risk|docs_only",
      "files_changed": ["path/to/file.rs"],
      "fork_equivalent": "description of what fork already has, or null",
      "port_complexity": "trivial|moderate|hard",
      "port_notes": "specific guidance for porting"
    }
  ],
  "port_clean_count": N,
  "port_adapt_count": N,
  "skip_count": N,
  "docs_only_count": N,
  "recommended_action": "proceed|needs_user_decision|nothing_to_port"
}
```

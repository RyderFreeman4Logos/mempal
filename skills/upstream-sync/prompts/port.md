# Port Prompt Template

You are porting ONE upstream commit to a heavily diverged fork.

## Context

- Repo: `{REPO_DIR}`
- Branch: `{BRANCH}`
- Upstream commit SHA: `{COMMIT_SHA}`
- Upstream commit message: `{COMMIT_MESSAGE}`
- Porting strategy: `{STRATEGY}` (clean_cherry_pick | new_file_copy | manual_adapt)

## Rules

1. **Never blindly merge.** The fork has rewritten most shared files 5-25x.
   If a file diverged massively (e.g. upstream 139 lines, fork 3547 lines),
   do NOT try to apply the upstream diff mechanically. Find the equivalent
   code location in the fork and apply the *concept*, not the diff hunks.

2. **Preserve fork architecture.** The fork has features upstream doesn't:
   fork_ext tables, async_db, hot_reload, cowork, API layer, etc.
   Upstream changes must integrate with these, not replace them.

3. **AI-config files are never tracked.** Files matching `~/.gitignore_noai`
   (AGENTS.md, CLAUDE.md, .claude/, etc.) must NOT be added to git.

4. **Quality gates after porting:**
   - `cargo fmt --all --check`
   - `cargo clippy --workspace -- -D warnings`
   - `cargo test --workspace`

## Task

### If strategy is `clean_cherry_pick`:
```bash
git cherry-pick --no-commit {COMMIT_SHA}
# Resolve any conflicts manually
# Remove any AI-config files from staging
git checkout --ours AGENTS.md CLAUDE.md .claude/ .codex/ .gemini/ 2>/dev/null || true
git rm --cached AGENTS.md CLAUDE.md 2>/dev/null || true
git add -A
git commit -m "sync: port {COMMIT_SHA} from upstream"
```

### If strategy is `new_file_copy`:
Copy the new file(s) from upstream. Adapt `mod.rs` declarations.
Check if `lib.rs` or `main.rs` need new module declarations.

### If strategy is `manual_adapt`:
1. Read the upstream diff: `git show {COMMIT_SHA}`
2. Identify the *semantic change* (what feature/fix does it add?)
3. Grep the fork for where this concept lives now
4. Implement the same concept in the fork's codebase
5. Commit with reference to upstream SHA

## Output

After committing, report:
```
PORT_RESULT: success|partial|failed
COMMIT_SHA: {COMMIT_SHA}
FILES_CHANGED: list
NOTES: any adaptation notes
```

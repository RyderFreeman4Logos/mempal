# pr-bot packaging (tracked)

| Path | Role |
|------|------|
| `assets/pr-bot/PATTERN.md` | **Canonical tracked** executor pattern for this repo (includes post-merge design-opportunity / `mempal insight record`) |
| `patterns/pr-bot/` | Optional full CSA skill after weave install (not git packaging) |
| `.claude/PATTERN.md` | Agent-local only; gitignored — **not** a deliverable |

Fresh clone requirement: executor must resolve `assets/pr-bot/PATTERN.md`
without creating files under `.claude/`.

See also `weave.lock` for the pinned `cli-sub-agent` commit that supplies the
full optional package.

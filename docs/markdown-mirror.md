# Markdown Memory Mirror

`mempal export md <dir>` exports active SQLite drawers into deterministic
Markdown files for human review, diffs, and Git-friendly snapshots.

SQLite remains the canonical memory store. The Markdown directory is generated
output, not a second source of truth. Re-run the export command to refresh it
from SQLite.

The first export initializes an empty directory with a
`.mempal-markdown-mirror.toml` manifest. A non-empty directory that does not
already contain a mempal mirror manifest is refused by default so unrelated user
files are not overwritten. On later exports, mempal only overwrites paths listed
in the manifest and removes generated paths that are no longer present in the
current SQLite export scope. Symlinked manifests and symlinked generated parent
directories are refused so a refresh cannot escape the mirror tree.

## Command

```bash
mempal export md ./memory-mirror
```

By default the command uses the current project scope when a project ID can be
resolved. Use `--project <id>`, `--include-global`, or `--all-projects` to make
the export scope explicit.

The default export redacts secret-like values at the export boundary. This does
not mutate raw SQLite drawer content. `--no-redact` exists for controlled local
debugging only.

## File Semantics

Each drawer file has stable YAML frontmatter with:

- `canonical_source: sqlite`
- `mirror_semantics: generated_read_only`
- `drawer_id`
- project, wing, room, timestamps, source/provenance fields
- typed memory metadata such as `memory_kind`, `domain`, `field`, `tier`, and
  `status` when present
- reference arrays such as `supporting_refs`, `counterexample_refs`,
  `teaching_refs`, and `verification_refs`

Paths are deterministic and derived from wing, room, and drawer ID with a short
hash suffix to avoid collisions after filesystem sanitization.

The manifest is the ownership boundary for generated files. Extra files placed
in the mirror directory are preserved unless a future export would need to write
the same relative path, in which case the command stops instead of overwriting
the unmanaged file. Markdown edits remain local edits to generated files; SQLite
is still canonical until a separate explicit import/watch flow exists.

## Import And Watch Policy

Markdown import/watch sync is intentionally not implemented by this export
surface. Future import or watch behavior must be opt-in and conflict-safe:

- SQLite wins by default.
- Markdown files must preserve `drawer_id` and `canonical_source: sqlite`.
- A Markdown edit must be compared against the current SQLite drawer before any
  write.
- Conflicts must stop with an explicit error unless the user chooses a conflict
  policy.
- Import/watch must not make Markdown the only source of truth or introduce a
  second authoritative persistence layer.

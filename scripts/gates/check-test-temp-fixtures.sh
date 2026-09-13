#!/usr/bin/env bash
set -euo pipefail

readonly pattern='^[[:space:]]*(let[[:space:]]+[^=]+=[[:space:]]*)?((tempfile::)?TempDir::new_in|tempfile::tempdir_in)\([[:space:]]*(r#*)?"/(var/)?tmp"(#*)?[[:space:]]*\)'

violations="$(git grep --cached -nE "$pattern" -- '*.rs')" && rc=0 || rc=$?
if [ "$rc" -eq 0 ]; then
    printf '%s\n' "$violations"
    printf 'ERROR: test temp fixtures must honor the configured temporary directory.\n' >&2
    exit 1
fi

[ "$rc" -eq 1 ] || exit "$rc"

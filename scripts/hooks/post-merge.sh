#!/usr/bin/env bash
# Auto-install mempal after merges that change the binary source.
# Uses `just install` so the post-merge path stays feature-identical to manual installs.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0

if ! git rev-parse --verify ORIG_HEAD >/dev/null 2>&1; then
  exit 0
fi

changed_files=$(git diff-tree -r --name-only ORIG_HEAD HEAD 2>/dev/null || true)
if ! grep -Eq '^(src/|Cargo\.toml$|Cargo\.lock$|justfile$|scripts/hooks/post-merge\.sh$|crates/)' <<<"$changed_files"; then
  exit 0
fi

cargo_home=/usr/local
install_bin_dir="${cargo_home}/bin"
log_file=/tmp/mempal-post-merge-install.log

if [ ! -w "$cargo_home" ] || { [ -d "$install_bin_dir" ] && [ ! -w "$install_bin_dir" ]; }; then
  echo "[mempal] Binary update skipped: ${cargo_home} is not writable (requires sudo)" >&2
  exit 0
fi

nohup bash -c '
set -euo pipefail
repo_root=$1

printf "\n[%s] post-merge install started in %s\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$repo_root"
cd "$repo_root"
just install
printf "[%s] post-merge install finished\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
' mempal-post-merge-install "$repo_root" >>"$log_file" 2>&1 </dev/null &

echo "[mempal] Binary update triggered (background install)"

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

install_root="${CARGO_INSTALL_ROOT:-/usr/local}"
install_bin_dir="${install_root}/bin"
log_file=/tmp/mempal-post-merge-install.log

if [ ! -w "$install_root" ] || { [ -d "$install_bin_dir" ] && [ ! -w "$install_bin_dir" ]; }; then
  echo "[mempal] Binary update skipped: ${install_root} is not writable (requires sudo)" >&2
  exit 0
fi

nohup bash -c '
set -euo pipefail
repo_root=$1
install_root=$2
install_bin=$3

printf "\n[%s] post-merge install started in %s\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$repo_root"
cd "$repo_root"
CARGO_INSTALL_ROOT="$install_root" just install
"$install_bin" daemon restart
printf "[%s] post-merge install and daemon restart finished\n" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
' mempal-post-merge-install "$repo_root" "$install_root" "$install_bin_dir/mempal" >>"$log_file" 2>&1 </dev/null &

echo "[mempal] Binary update triggered (background install)"

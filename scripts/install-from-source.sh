#!/usr/bin/env bash
# Install mempal from local source -- safe across schema bumps.
#
# Why this script exists: `cargo install --git <fork> --branch main --force mempal`
# is unreliable. `--force` only forces *installation*, not source re-fetch, so cargo's
# git source cache can return a stale ref and silently skip the rebuild ("0 deps compiled").
# After a CURRENT_SCHEMA_VERSION bump, the resulting binary will fail with a
# schema mismatch error that tells you to update the mempal binary and, for MCP
# servers, verify the MCP client command/path configuration.
# See https://github.com/RyderFreeman4Logos/mempal/issues/76.
#
# This script always pulls fresh source and uses --path, which forces a real rebuild.

set -euo pipefail
cd "$(dirname "$0")/.."

git pull --ff-only origin main
install_root="${CARGO_INSTALL_ROOT:-/usr/local}"
cargo install --path . --force --locked --features rest --root "$install_root"

echo "--- verifying schema match ---"
"$install_root/bin/mempal" status | grep -E "schema_version|fork_ext_version"

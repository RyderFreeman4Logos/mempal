#!/usr/bin/env bash
# Install mempal from local source -- safe across schema bumps.
#
# Why this script exists: `cargo install --git <fork> --branch main --force mempal`
# is unreliable. `--force` only forces *installation*, not source re-fetch, so cargo's
# git source cache can return a stale ref and silently skip the rebuild ("0 deps compiled").
# After a CURRENT_SCHEMA_VERSION bump, the resulting binary will fail with
# `database schema version N is newer than supported version N-1`.
# See https://github.com/RyderFreeman4Logos/mempal/issues/76.
#
# This script always pulls fresh source and uses --path, which forces a real rebuild.

set -euo pipefail
cd "$(dirname "$0")/.."

git pull --ff-only origin main
CARGO_HOME="${CARGO_HOME:-/usr/local}" cargo install --path crates/mempal-cli --force --locked

echo "--- verifying schema match ---"
"${CARGO_HOME:-/usr/local}/bin/mempal" status | grep -E "schema_version|fork_ext_version"

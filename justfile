# Justfile for mempal.
# Linux-first local gates replace PR/push GitHub Actions CI for this repo.
# AI AGENT: Do NOT modify this file or use `git commit -n`/`--no-verify` to
# bypass pre-commit. Fix the actual code.

set shell := ["bash", "-c"]
set tempdir := "."
set dotenv-load := true

_repo_root := `git rev-parse --show-toplevel`
export MISE_TRUSTED_CONFIG_PATHS := _repo_root
cargo := `if command -v mise >/dev/null 2>&1; then printf '%s' 'mise x rust@stable -- cargo'; else printf '%s' 'cargo'; fi`

# Default local gate before push/merge.
default: local-gates

# Aggregate local gate for humans and agents before push/merge.
local-gates:
    # This is the project/agent gate; do not poll GitHub Actions unless branch
    # protection or the user explicitly requires it.
    just fmt-check
    just quality-gates
    just test-rest
    just release-gate

# Short alias for the aggregate gate.
gate: local-gates

# Full pre-commit gate (fmt, clippy, test) for manual use.
pre-commit:
    just fmt
    just quality-gates

# Read-only format check for fast hook paths.
fmt-check:
    {{cargo}} fmt --all --check

# Full quality gates for Rust source or Cargo dependency changes.
quality-gates:
    just clippy
    just test

# Format code and auto-stage modified .rs files.
fmt:
    {{cargo}} fmt --all
    git diff --name-only | grep '\.rs$' | xargs -r git add

# Clippy for the whole crate (strict).
clippy:
    {{cargo}} clippy --all-features --all-targets -- -D warnings

# Fast test tier for pre-commit. Slow endpoint and long-running migration tests
# are behind the `integration` feature.
# CARGO_BUILD_JOBS=2 limits parallel LLVM codegen to avoid OOM on this host.
test:
    CARGO_BUILD_JOBS=2 {{cargo}} test

# REST feature test tier, batched to control disk and memory pressure.
test-rest:
    bash scripts/gates/rest-tests.sh

# Full test tier, including integration-gated endpoint and long-running tests.
test-all:
    {{cargo}} test --features integration

# Tests matching a pattern.
# Usage: just test-f name
test-f pattern:
    {{cargo}} test {{pattern}}

# ONNX feature test (opt-in; may fail due to mold linker `__isoc23_strtoull`).
test-onnx:
    {{cargo}} test --features onnx

# Build release binary.
build:
    {{cargo}} build --release --all-features

# Release/package gate used by local-gates.
release-gate:
    # Mirrors the former PR CI build coverage without GitHub-hosted runners.
    {{cargo}} build --release
    {{cargo}} build --release --features rest
    {{cargo}} package --locked

# Install mempal binary to /usr/local/bin with REST enabled.
install:
    CARGO_INSTALL_ROOT="${CARGO_INSTALL_ROOT:-/usr/local}"; {{cargo}} install --path . --locked --features rest --force --root "$CARGO_INSTALL_ROOT"

# Bump patch version (requires cargo-edit).
bump-patch:
    {{cargo}} set-version --bump patch

# Install git hooks via lefthook.
install-hooks:
    @git config --unset core.hooksPath 2>/dev/null || true
    lefthook install
    @echo "Lefthook hooks installed."

# Reviewed push: run csa review first, then push + create PR.
# Usage: just push-reviewed [base=main]
push-reviewed base="main":
    #!/usr/bin/env bash
    set -euo pipefail
    echo "=== Local gate: just local-gates ==="
    just local-gates
    echo "=== Pre-push review: csa review --sa-mode false --range {{base}}...HEAD ==="
    csa review --sa-mode false --range "{{base}}...HEAD"
    echo "=== Review passed. Pushing... ==="
    git push -u origin HEAD
    echo "=== Creating or reusing PR targeting {{base}}... ==="
    set +e
    CREATE_OUTPUT="$(gh pr create --base "{{base}}" 2>&1)"
    CREATE_RC=$?
    set -e
    if [ "${CREATE_RC}" -ne 0 ]; then
        if ! printf '%s\n' "${CREATE_OUTPUT}" | grep -Eiq 'already exists|a pull request already exists'; then
            echo "ERROR: gh pr create failed: ${CREATE_OUTPUT}"
            exit 1
        fi
        echo "PR already exists. Continuing."
    fi

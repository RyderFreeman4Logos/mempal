# Justfile for mempal
# Single-crate Rust project. Adapted from ../cli-sub-agent/justfile.
# AI AGENT: Do NOT modify this file or use `git commit -n`/`--no-verify` to
# bypass pre-commit. Fix the actual code.

set shell := ["bash", "-c"]
set tempdir := "."
set dotenv-load := true

_repo_root := `git rev-parse --show-toplevel`
export MISE_TRUSTED_CONFIG_PATHS := _repo_root

# Default recipe
default: pre-commit

# Full pre-commit gate (fmt, clippy, test).
pre-commit:
    just fmt
    just clippy
    just test

# Format code and auto-stage modified .rs files.
fmt:
    cargo fmt --all
    git diff --name-only | grep '\.rs$' | xargs -r git add

# Clippy for the whole crate (strict).
clippy:
    cargo clippy --all-features --all-targets -- -D warnings

# All tests (default features + `rest`; `onnx` intentionally excluded — the
# bundled onnxruntime C++ libs reference `__isoc23_strtoull`, which mold on
# this host fails to resolve. `just test-onnx` is an opt-in for runs where
# that toolchain is fixed.)
test:
    cargo test --features rest

# Tests matching a pattern.
# Usage: just test-f name
test-f pattern:
    cargo test --features rest {{pattern}}

# ONNX feature test (opt-in; may fail due to mold linker `__isoc23_strtoull`).
test-onnx:
    cargo test --features onnx

# Build release binary.
build:
    cargo build --release --all-features

# Bump patch version (requires cargo-edit).
bump-patch:
    cargo set-version --bump patch

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

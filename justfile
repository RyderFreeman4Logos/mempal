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

# Detect tracked text files that exceed the monolith line or token budget.
# Existing debt is ratcheted by scripts/monolith/baseline.toml; new debt fails.
# Env: MONOLITH_TOKEN_THRESHOLD (default 8000), MONOLITH_LINE_THRESHOLD (default 800), TOKUIN_MODEL (default gpt-4)
find-monolith-files:
    #!/usr/bin/env bash
    set -euo pipefail

    threshold_tokens="${MONOLITH_TOKEN_THRESHOLD:-8000}"
    threshold_lines="${MONOLITH_LINE_THRESHOLD:-800}"
    model="${TOKUIN_MODEL:-gpt-4}"
    baseline="scripts/monolith/baseline.toml"

    for command in git jq parallel python3 tokuin; do
        if ! command -v "$command" >/dev/null 2>&1; then
            echo "ERROR: find-monolith-files requires '$command' in PATH." >&2
            exit 2
        fi
    done
    case "$threshold_tokens" in
        ''|*[!0-9]*) echo "ERROR: MONOLITH_TOKEN_THRESHOLD must be a positive integer." >&2; exit 2 ;;
    esac
    case "$threshold_lines" in
        ''|*[!0-9]*) echo "ERROR: MONOLITH_LINE_THRESHOLD must be a positive integer." >&2; exit 2 ;;
    esac
    if [ "$threshold_tokens" -le 0 ] || [ "$threshold_lines" -le 0 ]; then
        echo "ERROR: monolith thresholds must be positive integers." >&2
        exit 2
    fi
    if [ ! -f "$baseline" ]; then
        echo "ERROR: monolith baseline not found: $baseline" >&2
        exit 2
    fi

    baseline_tsv="$(mktemp)"
    trap 'rm -f "$baseline_tsv"' EXIT
    python3 - "$baseline" >"$baseline_tsv" <<'PY'
    import sys

    try:
        import tomllib
    except ModuleNotFoundError:
        print("ERROR: Python 3.11+ tomllib is required to parse the monolith baseline.", file=sys.stderr)
        sys.exit(2)

    baseline_path = sys.argv[1]
    try:
        with open(baseline_path, "rb") as baseline_file:
            entries = tomllib.load(baseline_file).get("files", [])
    except Exception as error:
        print(f"ERROR: failed to parse {baseline_path}: {error}", file=sys.stderr)
        sys.exit(2)

    if not isinstance(entries, list):
        print("ERROR: baseline key 'files' must be an array of tables.", file=sys.stderr)
        sys.exit(2)

    seen = set()
    for index, entry in enumerate(entries, start=1):
        if not isinstance(entry, dict):
            print(f"ERROR: baseline entry #{index} must be a table.", file=sys.stderr)
            sys.exit(2)
        path = entry.get("path")
        kind = entry.get("kind")
        tokens = entry.get("baseline_tokens")
        lines = entry.get("baseline_lines")
        issue = entry.get("issue")
        rationale = entry.get("rationale")
        if not isinstance(path, str) or not path or "\t" in path or "\n" in path:
            print(f"ERROR: baseline entry #{index} has an invalid path.", file=sys.stderr)
            sys.exit(2)
        if path in seen:
            print(f"ERROR: duplicate baseline entry for {path}.", file=sys.stderr)
            sys.exit(2)
        seen.add(path)
        if kind not in {"source", "test", "doc", "config", "other"}:
            print(f"ERROR: baseline entry for {path} has an invalid kind.", file=sys.stderr)
            sys.exit(2)
        if not isinstance(tokens, int) or tokens < 0:
            print(f"ERROR: baseline entry for {path} has invalid baseline_tokens.", file=sys.stderr)
            sys.exit(2)
        if not isinstance(lines, int) or lines < 0:
            print(f"ERROR: baseline entry for {path} has invalid baseline_lines.", file=sys.stderr)
            sys.exit(2)
        if not isinstance(issue, str) or not issue:
            print(f"ERROR: baseline entry for {path} must name an issue.", file=sys.stderr)
            sys.exit(2)
        if not isinstance(rationale, str) or not rationale:
            print(f"ERROR: baseline entry for {path} must include a rationale.", file=sys.stderr)
            sys.exit(2)
        print(f"{path}\t{kind}\t{tokens}\t{lines}\t{issue}")
    PY

    monolith_error() {
        local category="$1" file="$2" tokens="$3" lines="$4" limits="$5"
        echo ""
        echo "=========================================="
        echo "ERROR: $category"
        echo "  File: $file"
        echo "  Actual: $tokens tokens, $lines lines"
        echo "  Limit: $limits"
        echo "=========================================="
        echo "Split the module in separately reviewed work, or document an unavoidable"
        echo "baseline change with the active issue and a precise rationale."
        echo "=========================================="
    }

    check_file() {
        local file="$1" token_limit="$2" line_limit="$3" tokenizer_model="$4" baseline_data="$5"
        case "$file" in
            *.lock|*lock.json|*lock.yaml) return 0 ;;  # generated dependency locks
            *.min.*) return 0 ;;                       # generated minified assets
            */AGENTS.md|*/FACTORY.md) return 0 ;;      # generated rule aggregations
            scripts/monolith/baseline.toml) return 0 ;; # guard metadata, not a module
        esac
        [ -f "$file" ] || return 0
        grep -Iq '' "$file" 2>/dev/null || return 0

        local lines bytes limits tokens=0
        lines="$(wc -l <"$file" | tr -d '[:space:]')"
        bytes="$(wc -c <"$file" | tr -d '[:space:]')"
        limits="$(awk -F '\t' -v path="$file" '$1 == path { print $3 "\t" $4 "\t" $5; exit }' "$baseline_data")"

        # One token cannot exceed its UTF-8 byte length, so small files can skip
        # tokenizer startup unless a baseline entry requires a ratchet check.
        if [ -n "$limits" ] || [ "$lines" -gt "$line_limit" ] || [ "$bytes" -gt "$token_limit" ]; then
            if ! tokens="$(tokuin estimate --model "$tokenizer_model" --format json "$file" 2>/dev/null \
                | jq -er '(.tokens // .total) | select(type == "number" and floor == . and . >= 0)')"; then
                echo "ERROR: tokuin failed or returned invalid JSON for $file." >&2
                return 2
            fi
        fi

        if [ -n "$limits" ]; then
            local cap_tokens cap_lines issue
            IFS=$'\t' read -r cap_tokens cap_lines issue <<<"$limits"
            if [ "$tokens" -gt "$cap_tokens" ] || [ "$lines" -gt "$cap_lines" ]; then
                monolith_error "Baseline ratchet exceeded (#$issue)" "$file" "$tokens" "$lines" \
                    "$cap_tokens tokens, $cap_lines lines"
                return 1
            fi
            return 0
        fi

        if [ "$tokens" -gt "$token_limit" ] || [ "$lines" -gt "$line_limit" ]; then
            monolith_error "New monolith detected" "$file" "$tokens" "$lines" \
                "$token_limit tokens, $line_limit lines"
            return 1
        fi
    }
    export -f check_file monolith_error

    git ls-files -z --recurse-submodules \
        | parallel -0 --jobs "${MONOLITH_JOBS:-4}" --halt now,fail=1 \
            check_file {} "$threshold_tokens" "$threshold_lines" "$model" "$baseline_tsv"

# Full quality gates for Rust source or Cargo dependency changes.
quality-gates:
    just find-monolith-files
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
    CARGO_BUILD_JOBS=2 bash scripts/gates/cargo-test-with-timeout.sh {{cargo}} test

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

# ONNX feature test using the checksum-pinned official shared runtime.
test-onnx:
    bash scripts/gates/onnx-tests.sh

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

#!/usr/bin/env bash
set -euo pipefail

REST_TEST_TARGETS_PER_BATCH="${REST_TEST_TARGETS_PER_BATCH:-12}"
REST_GATE_DRY_RUN="${REST_GATE_DRY_RUN:-0}"

if command -v mise >/dev/null 2>&1; then
    cargo_cmd=(mise x rust@stable -- cargo)
else
    cargo_cmd=(cargo)
fi

test_timeout_wrapper="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)/scripts/gates/cargo-test-with-timeout.sh"

if ! [[ "${REST_TEST_TARGETS_PER_BATCH}" =~ ^[1-9][0-9]*$ ]]; then
    echo "REST_TEST_TARGETS_PER_BATCH must be a positive integer" >&2
    exit 2
fi

run_cmd() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'

    if [[ "${REST_GATE_DRY_RUN}" != "1" ]]; then
        "$@"
    fi
}

clean_mempal_artifacts() {
    run_cmd "${cargo_cmd[@]}" clean -p mempal
}

run_cargo_test() {
    run_cmd bash "${test_timeout_wrapper}" "${cargo_cmd[@]}" test "$@"
}

run_integration_batch() {
    local batch_index="$1"
    shift

    local target_args=()
    local target
    for target in "$@"; do
        target_args+=(--test "${target}")
    done

    printf 'rest integration batch %s: %s target(s)\n' "${batch_index}" "$#"
    run_cargo_test --workspace --features rest "${target_args[@]}"
    clean_mempal_artifacts
}

echo "rest target batch size: ${REST_TEST_TARGETS_PER_BATCH}"

run_cargo_test --workspace --features rest --lib --bins
run_cargo_test --workspace --features rest --doc
clean_mempal_artifacts

mapfile -t integration_tests < <(
    find tests -maxdepth 1 -type f -name '*.rs' -printf '%f\n' \
        | sed 's/\.rs$//' \
        | sort
)

if ((${#integration_tests[@]} == 0)); then
    echo "no integration test targets found"
    exit 0
fi

batch=()
batch_index=1
for target in "${integration_tests[@]}"; do
    batch+=("${target}")
    if ((${#batch[@]} == REST_TEST_TARGETS_PER_BATCH)); then
        run_integration_batch "${batch_index}" "${batch[@]}"
        batch=()
        ((batch_index += 1))
    fi
done

if ((${#batch[@]} > 0)); then
    run_integration_batch "${batch_index}" "${batch[@]}"
fi

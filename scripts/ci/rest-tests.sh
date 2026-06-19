#!/usr/bin/env bash
set -euo pipefail

REST_TEST_TARGETS_PER_BATCH="${REST_TEST_TARGETS_PER_BATCH:-12}"
REST_CI_DRY_RUN="${REST_CI_DRY_RUN:-0}"

if ! [[ "${REST_TEST_TARGETS_PER_BATCH}" =~ ^[1-9][0-9]*$ ]]; then
    echo "REST_TEST_TARGETS_PER_BATCH must be a positive integer" >&2
    exit 2
fi

run_cmd() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'

    if [[ "${REST_CI_DRY_RUN}" != "1" ]]; then
        "$@"
    fi
}

clean_mempal_artifacts() {
    run_cmd cargo clean -p mempal
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
    run_cmd cargo test --workspace --features rest "${target_args[@]}"
    clean_mempal_artifacts
}

echo "rest target batch size: ${REST_TEST_TARGETS_PER_BATCH}"

run_cmd cargo test --workspace --features rest --lib --bins
run_cmd cargo test --workspace --features rest --doc
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

#!/usr/bin/env bash
set -euo pipefail

REST_TEST_TARGETS_PER_BATCH="${REST_TEST_TARGETS_PER_BATCH:-12}"
REST_GATE_DRY_RUN="${REST_GATE_DRY_RUN:-0}"
REST_GATE_LOCK_TIMEOUT_SECS="${REST_GATE_LOCK_TIMEOUT_SECS:-1800}"

if ! command -v flock >/dev/null 2>&1; then
    echo "REST gate requires 'flock' in PATH" >&2
    exit 2
fi

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
rest_target_dir="${REST_GATE_TARGET_DIR:-${repo_root}/target/rest-gate}"

if ! [[ "${REST_GATE_LOCK_TIMEOUT_SECS}" =~ ^[1-9][0-9]*$ ]]; then
    echo "REST_GATE_LOCK_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
fi
# Reject values longer than 5 digits before any bash arithmetic to prevent
# signed-64-bit overflow (e.g. 18446744073709551615 wraps to -1).
if ((${#REST_GATE_LOCK_TIMEOUT_SECS} > 5)) || ((10#${REST_GATE_LOCK_TIMEOUT_SECS} > 86400)); then
    echo "REST_GATE_LOCK_TIMEOUT_SECS must be a positive integer <= 86400" >&2
    exit 2
fi

monotonic_centiseconds() {
    local uptime_seconds
    local whole_seconds
    local fractional_seconds

    read -r uptime_seconds _ </proc/uptime
    whole_seconds="${uptime_seconds%%.*}"
    fractional_seconds="${uptime_seconds#*.}00"
    fractional_seconds="${fractional_seconds:0:2}"
    printf '%s\n' "$((10#${whole_seconds} * 100 + 10#${fractional_seconds}))"
}

# REST batches clean package artifacts between phases. Keep that cleanup in a
# dedicated target directory so it cannot delete another Cargo process's files.
mkdir -p -- "${rest_target_dir}"
rest_target_dir="$(cd -- "${rest_target_dir}" && pwd -P)"
mkdir -p -- "${repo_root}/target"
shared_target_dir="$(cd -- "${repo_root}/target" && pwd -P)"
if [[ "${rest_target_dir}" == "${shared_target_dir}" ]]; then
    echo "REST_GATE_TARGET_DIR must not use the shared Cargo target: ${shared_target_dir}" >&2
    exit 2
fi

rest_lock_file="${rest_target_dir}.lock"
mkdir -p -- "$(dirname -- "${rest_lock_file}")"
exec {rest_lock_fd}>"${rest_lock_file}"
# The top-level shell owns this descriptor. Every non-lock child closes its
# copy so a detached test descendant cannot extend the gate's lock lifetime.
if ! flock -n "${rest_lock_fd}"; then
    # Use one subsecond monotonic deadline for diagnostics and acquisition.
    # `SECONDS` is integer-quantized, so it can charge a subsecond diagnostic
    # a full second when it crosses a tick boundary.
    rest_lock_wait_deadline_centiseconds=$((
        $(monotonic_centiseconds) + 10#${REST_GATE_LOCK_TIMEOUT_SECS} * 100
    ))
    echo "rest gate waiting for lock: ${rest_lock_file} (pid=$$)" >&2
    rest_lock_wait_remaining_centiseconds=$((
        rest_lock_wait_deadline_centiseconds - $(monotonic_centiseconds)
    ))
    if ((rest_lock_wait_remaining_centiseconds > 0)) \
        && command -v fuser >/dev/null 2>&1 \
        && command -v timeout >/dev/null 2>&1; then
        rest_lock_wait_remaining="$(printf '%d.%02d' \
            "$((rest_lock_wait_remaining_centiseconds / 100))" \
            "$((rest_lock_wait_remaining_centiseconds % 100))")"
        (
            exec {rest_lock_fd}>&-
            timeout --signal=KILL "${rest_lock_wait_remaining}s" \
                fuser -v "${rest_lock_file}" >&2
        ) || true
    fi
    rest_lock_wait_remaining_centiseconds=$((
        rest_lock_wait_deadline_centiseconds - $(monotonic_centiseconds)
    ))
    rest_lock_wait_remaining="$(printf '%d.%02d' \
        "$((rest_lock_wait_remaining_centiseconds / 100))" \
        "$((rest_lock_wait_remaining_centiseconds % 100))")"
    if ((rest_lock_wait_remaining_centiseconds <= 0)) \
        || ! flock -w "${rest_lock_wait_remaining}" "${rest_lock_fd}"; then
        echo "rest gate lock timed out after ${REST_GATE_LOCK_TIMEOUT_SECS}s: ${rest_lock_file}" >&2
        exit 75
    fi
    echo "rest gate acquired lock: ${rest_lock_file} (pid=$$)" >&2
fi

export CARGO_TARGET_DIR="${rest_target_dir}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

if command -v mise >/dev/null 2>&1; then
    cargo_cmd=(mise x rust@stable -- cargo)
else
    cargo_cmd=(cargo)
fi

test_timeout_wrapper="${repo_root}/scripts/gates/cargo-test-with-timeout.sh"

if ! [[ "${REST_TEST_TARGETS_PER_BATCH}" =~ ^[1-9][0-9]*$ ]]; then
    echo "REST_TEST_TARGETS_PER_BATCH must be a positive integer" >&2
    exit 2
fi

if ! [[ "${CARGO_BUILD_JOBS}" =~ ^[1-9][0-9]*$ ]]; then
    echo "CARGO_BUILD_JOBS must be a positive integer" >&2
    exit 2
fi

run_cmd() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'

    if [[ "${REST_GATE_DRY_RUN}" != "1" ]]; then
        (
            exec {rest_lock_fd}>&-
            "$@"
        )
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
echo "rest cargo target dir: ${CARGO_TARGET_DIR}"
echo "rest cargo build jobs: ${CARGO_BUILD_JOBS}"

# #1052: rest-lib test_worker_* observer/contention tests get their own cargo
# process instead of running inside the parallel --lib suite.
run_cargo_test --workspace --features rest --lib --bins -- --skip test_worker_ --skip recovery_publication_failure_returns_error_without_ready_or_child_leaks
run_cargo_test --workspace --features rest --lib test_worker_
run_cargo_test --workspace --features rest --lib recovery_publication_failure_returns_error_without_ready_or_child_leaks
run_cargo_test --workspace --features rest --doc
clean_mempal_artifacts

mapfile -t integration_tests < <(
    exec {rest_lock_fd}>&-
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

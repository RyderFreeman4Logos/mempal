#!/usr/bin/env bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
timeout_wrapper="${script_dir}/cargo-test-with-timeout.py"

run_phase() {
    printf '+'
    printf ' %q' "$@"
    printf '\n'
    python3 "${timeout_wrapper}" "$@"
}

# Reuse #1052's process split for the default `just test` invocation. REST
# phases pass arguments after `test` and keep their existing process layout.
if (( $# > 0 )) && [[ "${!#}" == "test" ]]; then
    run_phase "$@" -- --skip test_worker_ --skip historical_rejudge
    run_phase "$@" test_worker_
    run_phase "$@" historical_rejudge -- --skip historical_rejudge_paired_spark_exhausted_persists_confirm_backlog_and_continues_qwen
    run_phase "$@" historical_rejudge_paired_spark_exhausted_persists_confirm_backlog_and_continues_qwen
else
    exec python3 "${timeout_wrapper}" "$@"
fi

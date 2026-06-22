#!/usr/bin/env bash
set -euo pipefail

TIMEOUT_SECS="${MEMPAL_CARGO_TEST_TIMEOUT_SECS:-1800}"
KILL_GRACE_SECS="${MEMPAL_CARGO_TEST_KILL_GRACE_SECS:-30}"

if (($# == 0)); then
    echo "usage: $0 <cargo-test-command> [args...]" >&2
    exit 2
fi

if ! [[ "${TIMEOUT_SECS}" =~ ^[1-9][0-9]*$ ]]; then
    echo "MEMPAL_CARGO_TEST_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
fi

if ! [[ "${KILL_GRACE_SECS}" =~ ^[1-9][0-9]*$ ]]; then
    echo "MEMPAL_CARGO_TEST_KILL_GRACE_SECS must be a positive integer" >&2
    exit 2
fi

print_command() {
    printf ' %q' "$@"
}

print_process_context() {
    local pgid="$1"

    echo "process tree:" >&2
    if command -v ps >/dev/null 2>&1; then
        ps -o pid,ppid,pgid,stat,etime,pcpu,pmem,comm,args --forest -g "${pgid}" >&2 \
            || ps -o pid,ppid,pgid,stat,etime,pcpu,pmem,comm,args -g "${pgid}" >&2 \
            || true
    else
        echo "ps unavailable; cannot print process tree" >&2
    fi
}

terminate_group() {
    local pgid="$1"
    local pid="$2"

    kill -TERM "-${pgid}" 2>/dev/null || kill -TERM "${pid}" 2>/dev/null || true
    sleep "${KILL_GRACE_SECS}"
    if kill -0 "${pid}" 2>/dev/null; then
        kill -KILL "-${pgid}" 2>/dev/null || kill -KILL "${pid}" 2>/dev/null || true
    fi
}

timeout_marker="$(mktemp "${TMPDIR:-/tmp}/mempal-cargo-test-timeout.XXXXXX")"
rm -f "${timeout_marker}"

if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
else
    "$@" &
fi
cmd_pid="$!"
cmd_pgid="$cmd_pid"

cleanup() {
    local exit_code="$1"
    if [[ -n "${watchdog_pid:-}" ]]; then
        kill "${watchdog_pid}" 2>/dev/null || true
        wait "${watchdog_pid}" 2>/dev/null || true
    fi
    if kill -0 "${cmd_pid}" 2>/dev/null; then
        terminate_group "${cmd_pgid}" "${cmd_pid}"
    fi
    rm -f "${timeout_marker}"
    exit "${exit_code}"
}

trap 'cleanup 130' INT
trap 'cleanup 143' TERM

(
    sleep_pid=""
    cleanup_watchdog() {
        if [[ -n "${sleep_pid}" ]]; then
            kill "${sleep_pid}" 2>/dev/null || true
            wait "${sleep_pid}" 2>/dev/null || true
        fi
        exit 0
    }
    trap cleanup_watchdog INT TERM HUP

    sleep "${TIMEOUT_SECS}" &
    sleep_pid="$!"
    wait "${sleep_pid}" || exit 0
    sleep_pid=""

    if kill -0 "${cmd_pid}" 2>/dev/null; then
        {
            echo "cargo test command timed out after ${TIMEOUT_SECS}s"
            printf 'active command:'
            print_command "$@"
            printf '\n'
            print_process_context "${cmd_pgid}"
        } >&2
        : >"${timeout_marker}"
        terminate_group "${cmd_pgid}" "${cmd_pid}"
    fi
) &
watchdog_pid="$!"

set +e
wait "${cmd_pid}"
status="$?"
set -e

kill "${watchdog_pid}" 2>/dev/null || true
wait "${watchdog_pid}" 2>/dev/null || true

if [[ -f "${timeout_marker}" ]]; then
    rm -f "${timeout_marker}"
    exit 124
fi

rm -f "${timeout_marker}"
exit "${status}"

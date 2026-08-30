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

process_stat_fields() {
    local pid="$1"
    local stat_path="/proc/${pid}/stat"
    [[ -r "${stat_path}" ]] || return 1
    local stat
    stat="$(<"${stat_path}")" || return 1
    read -r -a REPLY <<<"${stat##*') '}"
    ((${#REPLY[@]} >= 20))
}

process_start_time() {
    process_stat_fields "$1" || return 1
    printf '%s\n' "${REPLY[19]}"
}

process_identity_matches() {
    local pid="$1"
    local expected_start_time="$2"
    local actual_start_time
    actual_start_time="$(process_start_time "${pid}")" || return 1
    [[ "${actual_start_time}" == "${expected_start_time}" ]]
}

process_is_running() {
    local pid="$1"
    process_stat_fields "${pid}" || return 1
    [[ "${REPLY[0]}" != Z && "${REPLY[0]}" != X ]]
}

capture_group_identities() {
    local pgid="$1"
    local stat_path pid
    owned_group_identities=""
    for stat_path in /proc/[0-9]*/stat; do
        [[ -r "${stat_path}" ]] || continue
        pid="${stat_path#/proc/}"
        pid="${pid%/stat}"
        if ! process_stat_fields "${pid}"; then
            continue
        fi
        if [[ "${REPLY[2]:-}" == "${pgid}" ]]; then
            owned_group_identities+="${pid}:${REPLY[19]}"$'\n'
        fi
    done
}

group_has_owned_member() {
    local pgid="$1"
    local pid start_time
    while IFS=: read -r pid start_time; do
        [[ -n "${pid}" && -n "${start_time}" ]] || continue
        if ! process_stat_fields "${pid}"; then
            continue
        fi
        if [[ "${REPLY[2]:-}" == "${pgid}" && "${REPLY[19]:-}" == "${start_time}" ]] && \
            process_is_running "${pid}"; then
            return 0
        fi
    done <<<"${owned_group_identities}"
    return 1
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

wait_for_owned_group_exit() {
    local pgid="$1"
    local attempts=$((KILL_GRACE_SECS * 100))
    local attempt
    for ((attempt = 0; attempt < attempts; attempt++)); do
        if ! group_has_owned_member "${pgid}"; then
            return 0
        fi
        sleep 0.01
    done
    return 0
}

terminate_group() {
    local pgid="$1"
    local pid="$2"
    local isolated_group="$3"
    local signaled=0

    if [[ "${isolated_group}" == 1 ]] && group_has_owned_member "${pgid}"; then
        kill -TERM "-${pgid}" 2>/dev/null || true
        signaled=1
    elif process_identity_matches "${pid}" "${cmd_start_time}"; then
        kill -TERM "${pid}" 2>/dev/null || true
        signaled=1
    fi

    sleep "${KILL_GRACE_SECS}"
    if ((signaled)); then
        if [[ "${isolated_group}" == 1 ]] && group_has_owned_member "${pgid}"; then
            kill -KILL "-${pgid}" 2>/dev/null || true
        elif process_identity_matches "${pid}" "${cmd_start_time}"; then
            kill -KILL "${pid}" 2>/dev/null || true
        fi
    fi
    wait_for_owned_group_exit "${pgid}"
}

timeout_marker="$(mktemp "${TMPDIR:-/tmp}/mempal-cargo-test-timeout.XXXXXX")"
rm -f "${timeout_marker}"

if command -v setsid >/dev/null 2>&1; then
    setsid "$@" &
    isolated_group=1
else
    "$@" &
    isolated_group=0
fi
cmd_pid="$!"
cmd_pgid="$cmd_pid"
cmd_start_time="$(process_start_time "${cmd_pid}" 2>/dev/null || true)"

cleanup() {
    local exit_code="$1"
    if [[ -n "${watchdog_pid:-}" ]]; then
        kill "${watchdog_pid}" 2>/dev/null || true
        wait "${watchdog_pid}" 2>/dev/null || true
    fi
    if process_identity_matches "${cmd_pid}" "${cmd_start_time}"; then
        capture_group_identities "${cmd_pgid}"
        terminate_group "${cmd_pgid}" "${cmd_pid}" "${isolated_group}"
        wait "${cmd_pid}" 2>/dev/null || true
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

    if process_identity_matches "${cmd_pid}" "${cmd_start_time}"; then
        capture_group_identities "${cmd_pgid}"
        {
            echo "cargo test command timed out after ${TIMEOUT_SECS}s"
            printf 'active command:'
            print_command "$@"
            printf '\n'
            print_process_context "${cmd_pgid}"
        } >&2
        : >"${timeout_marker}"
        terminate_group "${cmd_pgid}" "${cmd_pid}" "${isolated_group}"
    fi
) &
watchdog_pid="$!"

set +e
wait "${cmd_pid}"
command_status="$?"
set -e

if [[ -f "${timeout_marker}" ]]; then
    wait "${watchdog_pid}" 2>/dev/null || true
    rm -f "${timeout_marker}"
    exit 124
fi

kill "${watchdog_pid}" 2>/dev/null || true
wait "${watchdog_pid}" 2>/dev/null || true
rm -f "${timeout_marker}"
exit "${command_status}"

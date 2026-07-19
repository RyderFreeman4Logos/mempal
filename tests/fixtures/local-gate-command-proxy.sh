#!/bin/bash
set -euo pipefail

# Immutable proxy for local-gate PATH fixtures. The symlink basename selects
# the command; each test provides behavior-specific state through its child
# process environment.
case "${0##*/}" in
    mise)
        if [[ "$#" -lt 4 || "$1" != "x" || "$3" != "--" || "$4" != "cargo" ]]; then
            exit 64
        fi
        shift 4
        exec cargo "$@"
        ;;
    cargo)
        if [[ "${1:-}" == "test" && ! -e "${FAKE_CARGO_TRIGGERED:?}" ]]; then
            : >"${FAKE_CARGO_TRIGGERED}"
            (
                trap '' HUP
                printf '%s\n' "${BASHPID}" >"${HOLDER_PID_FILE:?}"
                : >"${HOLDER_READY_FILE:?}"
                exec /bin/sleep 30
            ) </dev/null >/dev/null 2>&1 &
            while [[ ! -s "${HOLDER_PID_FILE}" || ! -e "${HOLDER_READY_FILE}" ]]; do
                sleep 0.01
            done
            kill -TERM "$$"
        fi
        exit 0
        ;;
    fuser)
        if [[ -n "${REST_GATE_FUSER_READY_FILE:-}" ]]; then
            : >"${REST_GATE_FUSER_READY_FILE}"
            while [[ ! -e "${REST_GATE_FUSER_RELEASE_FILE:?}" ]]; do
                sleep 0.01
            done
            : >"${REST_GATE_FUSER_RELEASED_FILE:?}"
            : >"${REST_GATE_LOCK_HOLDER_RELEASE_FILE:?}"
        fi
        exit 0
        ;;
    csa)
        printf '%s\n' "$*" >>"${REVIEW_CHECK_FIXTURE_LOG:?}"
        exit 42
        ;;
    git)
        if [[ -n "${LOCAL_GATE_RECEIPT_STALLED_GIT_PID_FILE:-}" ]]; then
            start_time="$(awk '{print $22}' /proc/$$/stat)"
            printf '%s %s\n' "$$" "${start_time}" >"${LOCAL_GATE_RECEIPT_STALLED_GIT_PID_FILE}"
            exec /bin/sleep 60
        fi

        case "${LOCAL_GATE_RECEIPT_FAULT_KIND:-}" in
            status)
                if [[ "${1:-}" == "status" ]]; then
                    if [[ -n "${LOCAL_GATE_RECEIPT_FAULT_ARM:-}" && ! -e "${LOCAL_GATE_RECEIPT_FAULT_ARM}" ]]; then
                        exec "${LOCAL_GATE_RECEIPT_REAL_GIT:?}" "$@"
                    fi
                    printf '%s\n' 'faulted-status' >>"${LOCAL_GATE_RECEIPT_FAULT_LOG:?}"
                    exit 42
                fi
                ;;
            git-dir)
                if [[ "${1:-}" == "rev-parse" && "${2:-}" == "--git-dir" ]]; then
                    "${LOCAL_GATE_RECEIPT_REAL_GIT:?}" "$@"
                    printf '%s\n' 'faulted-git-dir' >>"${LOCAL_GATE_RECEIPT_FAULT_LOG:?}"
                    exit 42
                fi
                ;;
            git-common-dir)
                if [[ "${1:-}" == "rev-parse" && "${2:-}" == "--git-common-dir" ]]; then
                    "${LOCAL_GATE_RECEIPT_REAL_GIT:?}" "$@"
                    printf '%s\n' 'faulted-git-common-dir' >>"${LOCAL_GATE_RECEIPT_FAULT_LOG:?}"
                    exit 42
                fi
                ;;
            check-ignore)
                if [[ "${1:-}" == "check-ignore" ]]; then
                    printf '%s\n' 'faulted-check-ignore' >>"${LOCAL_GATE_RECEIPT_FAULT_LOG:?}"
                    exit 42
                fi
                ;;
        esac

        exec "${LOCAL_GATE_RECEIPT_REAL_GIT:?}" "$@"
        ;;
    *)
        exit 64
        ;;
esac

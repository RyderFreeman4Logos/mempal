#!/usr/bin/env bash
# Produce and validate a checkout-local PASS receipt for the full local gate.

set -euo pipefail

readonly RECEIPT_SCHEMA="local-gate-receipt-v1"
readonly RECEIPT_ROOT="target/local-gates/receipts"

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

push_blocked() {
    printf 'ERROR: local gate receipt is not valid for this exact tree: %s\n' "$*" >&2
    printf 'Run: just local-gates (on a clean committed tree) before pushing.\n' >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "local gate receipt requires '$1' in PATH"
}

ensure_clean_tracked_tree() {
    local status_output
    if ! status_output="$(git status --porcelain=v1 --untracked-files=no 2>/dev/null)"; then
        printf 'ERROR: cannot determine tracked index and worktree cleanliness.\n' >&2
        return 1
    fi
    [ -z "$status_output" ] || return 1
}

hash_record() {
    sha256sum | awk '{print $1}'
}

snapshot_identity() {
    SNAPSHOT_HEAD="$(git rev-parse --verify HEAD^{commit})" || die "cannot resolve committed HEAD"
    SNAPSHOT_TREE="$(git rev-parse --verify HEAD^{tree})" || die "cannot resolve HEAD tree"

    local repo_root git_dir_output git_dir common_dir_output common_dir contract_blobs path blob
    repo_root="$(git rev-parse --show-toplevel)" || die "cannot resolve repository root"
    repo_root="$(realpath -e "$repo_root")" || die "cannot canonicalize repository root"
    git_dir_output="$(git rev-parse --git-dir 2>/dev/null)" || die "cannot resolve Git directory"
    git_dir="$(realpath -e "$git_dir_output" 2>/dev/null)" || die "cannot canonicalize Git directory"
    common_dir_output="$(git rev-parse --git-common-dir 2>/dev/null)" || die "cannot resolve Git common directory"
    common_dir="$(realpath -e "$common_dir_output" 2>/dev/null)" || die "cannot canonicalize Git common directory"

    SNAPSHOT_REPOSITORY="$(printf '%s\0' "$common_dir" | hash_record)" \
        || die "cannot hash repository identity"
    SNAPSHOT_CHECKOUT="$(printf '%s\0%s\0' "$repo_root" "$git_dir" | hash_record)" \
        || die "cannot hash checkout identity"

    contract_blobs=""
    for path in justfile lefthook.yml scripts/gates/local-gate-receipt.sh; do
        blob="$(git rev-parse --verify "${SNAPSHOT_TREE}:${path}")" \
            || die "gate contract path is absent from committed tree: ${path}"
        contract_blobs+="${path}=${blob}"$'\n'
    done
    SNAPSHOT_CONTRACT="$(printf '%s' "$contract_blobs" | hash_record)" \
        || die "cannot hash gate contract"
    SNAPSHOT_RECEIPT_ID="$(
        printf '%s\0%s\0%s\0%s\0%s\0' \
            "$SNAPSHOT_REPOSITORY" \
            "$SNAPSHOT_CHECKOUT" \
            "$SNAPSHOT_HEAD" \
            "$SNAPSHOT_TREE" \
            "$SNAPSHOT_CONTRACT" \
            | hash_record
    )" || die "cannot hash receipt identity"
}

receipt_path() {
    printf '%s/%s.pass\n' "$RECEIPT_ROOT" "$SNAPSHOT_RECEIPT_ID"
}

ensure_receipt_location_is_ignored() {
    local component ancestor
    local -a components

    IFS=/ read -r -a components <<<"$RECEIPT_ROOT"
    ancestor=""
    for component in "${components[@]}"; do
        ancestor="${ancestor:+${ancestor}/}${component}"
        if [ -L "$ancestor" ]; then
            git check-ignore -q -- "$ancestor" \
                || die "receipt directory must be ignored: ${RECEIPT_ROOT}"
            return
        fi
    done

    git check-ignore -q -- "$RECEIPT_ROOT" \
        || die "receipt directory must be ignored: ${RECEIPT_ROOT}"
}

run_literal_aggregate() {
    # fixture-aggregate-start
    just fmt-check
    just quality-gates
    just test-rest
    just release-gate
    # fixture-aggregate-end
}

publish_receipt() {
    local receipt started_at completed_at temp_receipt
    receipt="$(receipt_path)"
    started_at="$1"
    completed_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

    mkdir -p "$RECEIPT_ROOT"
    temp_receipt="$(mktemp "${RECEIPT_ROOT}/.${SNAPSHOT_RECEIPT_ID}.tmp.XXXXXX")" \
        || die "cannot create temporary receipt"
    umask 077
    {
        printf 'schema=%s\n' "$RECEIPT_SCHEMA"
        printf 'status=PASS\n'
        printf 'repository=%s\n' "$SNAPSHOT_REPOSITORY"
        printf 'checkout=%s\n' "$SNAPSHOT_CHECKOUT"
        printf 'head=%s\n' "$SNAPSHOT_HEAD"
        printf 'tree=%s\n' "$SNAPSHOT_TREE"
        printf 'contract=%s\n' "$SNAPSHOT_CONTRACT"
        printf 'started_at=%s\n' "$started_at"
        printf 'completed_at=%s\n' "$completed_at"
        printf 'receipt_id=%s\n' "$SNAPSHOT_RECEIPT_ID"
    } >"$temp_receipt"
    mv -f "$temp_receipt" "$receipt"
    printf 'PASS local gate receipt: %s\n' "$receipt"
}

produce() {
    ensure_clean_tracked_tree \
        || die "local-gates requires a clean tracked index and worktree before it begins"
    ensure_receipt_location_is_ignored
    snapshot_identity

    local started_at gate_rc before_head before_tree before_contract
    started_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    before_head="$SNAPSHOT_HEAD"
    before_tree="$SNAPSHOT_TREE"
    before_contract="$SNAPSHOT_CONTRACT"
    set +e
    (
        set -euo pipefail
        run_literal_aggregate
    )
    gate_rc=$?
    set -e
    if [ "$gate_rc" -ne 0 ]; then
        printf 'ERROR: literal local gate failed with exit code %s; no PASS receipt was published.\n' "$gate_rc" >&2
        exit "$gate_rc"
    fi

    ensure_clean_tracked_tree \
        || die "local gate changed tracked bytes; no PASS receipt was published"
    snapshot_identity
    [ "$SNAPSHOT_HEAD" = "$before_head" ] \
        && [ "$SNAPSHOT_TREE" = "$before_tree" ] \
        && [ "$SNAPSHOT_CONTRACT" = "$before_contract" ] \
        || die "HEAD, tree, or gate contract drifted during the local gate; no PASS receipt was published"
    publish_receipt "$started_at"
}

validate_receipt() {
    ensure_clean_tracked_tree || push_blocked "tracked index or worktree is dirty"
    ensure_receipt_location_is_ignored
    snapshot_identity

    local receipt line key value
    receipt="$(receipt_path)"
    [ -f "$receipt" ] || push_blocked "receipt is missing"

    declare -A fields=()
    while IFS= read -r line || [ -n "$line" ]; do
        [[ "$line" == *=* ]] || push_blocked "receipt is malformed"
        key="${line%%=*}"
        value="${line#*=}"
        case "$key" in
            schema|status|repository|checkout|head|tree|contract|started_at|completed_at|receipt_id) ;;
            *) push_blocked "receipt has an unknown field" ;;
        esac
        [ -z "${fields[$key]+present}" ] || push_blocked "receipt has a duplicate field"
        [ -n "$value" ] || push_blocked "receipt has an empty field"
        fields["$key"]="$value"
    done <"$receipt"

    [ "${fields[schema]:-}" = "$RECEIPT_SCHEMA" ] || push_blocked "receipt schema is stale"
    [ "${fields[status]:-}" = "PASS" ] || push_blocked "receipt status is not PASS"
    [ "${fields[repository]:-}" = "$SNAPSHOT_REPOSITORY" ] || push_blocked "repository identity changed"
    [ "${fields[checkout]:-}" = "$SNAPSHOT_CHECKOUT" ] || push_blocked "checkout identity changed"
    [ "${fields[head]:-}" = "$SNAPSHOT_HEAD" ] || push_blocked "committed HEAD changed"
    [ "${fields[tree]:-}" = "$SNAPSHOT_TREE" ] || push_blocked "committed tree changed"
    [ "${fields[contract]:-}" = "$SNAPSHOT_CONTRACT" ] || push_blocked "gate contract changed"
    [ "${fields[receipt_id]:-}" = "$SNAPSHOT_RECEIPT_ID" ] || push_blocked "receipt identity changed"
    [[ "${fields[started_at]:-}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] \
        || push_blocked "receipt start timestamp is malformed"
    [[ "${fields[completed_at]:-}" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] \
        || push_blocked "receipt completion timestamp is malformed"
    printf 'PASS local gate receipt reused: %s\n' "$receipt"
}

case "${1:-}" in
    produce) produce ;;
    validate) validate_receipt ;;
    *)
        printf 'Usage: %s {produce|validate}\n' "$0" >&2
        exit 64
        ;;
esac

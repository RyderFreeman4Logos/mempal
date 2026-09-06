"""Strict aggregate-only classification for smoke create attempts."""
from __future__ import annotations

from typing import Any

__all__ = [
    "classify_create_attempt",
    "cleanup_ids_from",
    "followable_update_without_terminal_ids",
    "holder_budget_no_write_receipt",
    "operation_id_from",
    "operation_state_from",
    "receipt_dicts_from",
    "update_missing_reason",
]

_MAX_RUST_WIRE_INTEGER = (1 << 64) - 1
_OPERATION_STATES = frozenset({"queued", "running", "completed", "rejected", "failed"})
_CAPACITY_FIELDS = frozenset({"holders", "cache_bytes"})
_UNKNOWN_HOLDER_REASONS = frozenset({
    "unknown_lease_version",
    "lease_open_unavailable",
    "lease_lock_unavailable",
    "legacy_process_identity_unverifiable",
})
_CLI_PROFILE_FIELDS = frozenset({
    "active_holders", "configured_holder_limit", "active_cache_bytes",
    "configured_cache_bytes", "reaped_stale_holders_this_snapshot",
    "reserved_service_holders", "service_holders", "requested_cache_bytes",
    "budget_reason",
})
_MCP_PROFILE_FIELDS = _CLI_PROFILE_FIELDS | {
    "available_cache_bytes", "capacity", "headroom", "unknown_holders",
    "unknown_holder_diagnostics", "async_pool_loaded",
}
_CLI_REFUSAL_FIELDS = frozenset({
    "outcome", "reason", "action", "created_drawer_ids", "cleanup_drawer_ids",
    "capacity", "headroom", "profile_admission",
})
_MCP_REFUSAL_FIELDS = _CLI_REFUSAL_FIELDS | {
    "async_pool_loaded", "database_diagnostic",
}
_MCP_DIAGNOSTIC_FIELDS = frozenset({"path", "source", "failure_kind", "summary", "hint"})
_REST_ERROR_FIELDS = _CLI_REFUSAL_FIELDS | {"message", "status", "kind", "retryable"}
_REST_HOLDER_BUDGET_MESSAGE = (
    "mempal profile holder budget is exhausted; write was refused before queueing"
)


def receipt_dicts_from(value: Any) -> list[dict[str, Any]]:
    """Return operation-style receipts without parsing raw text payloads."""
    receipts: list[dict[str, Any]] = []
    if isinstance(value, dict):
        receipts.append(value)
        for key in ("structuredContent", "result", "payload", "response", "error", "data"):
            nested = value.get(key)
            if isinstance(nested, (dict, list)):
                receipts.extend(receipt_dicts_from(nested))
    elif isinstance(value, list):
        for item in value:
            receipts.extend(receipt_dicts_from(item))
    return receipts


def _drawer_id_evidence(
    value: Any, *, expected_operation_id: str | None = None
) -> tuple[list[str], list[str], bool, bool]:
    created_ids: list[str] = []
    cleanup_ids: list[str] = []
    malformed = False
    conflicting_failure = False
    for receipt in receipt_dicts_from(value):
        if "returncode" in receipt:
            returncode = receipt["returncode"]
            if (
                not isinstance(returncode, int)
                or isinstance(returncode, bool)
                or returncode != 0
            ):
                conflicting_failure = True
        if (
            "error" in receipt
            or receipt.get("state") in {"rejected", "failed"}
            or receipt.get("outcome") == "admission_blocked"
            or receipt.get("action") == "write_refused"
            or receipt.get("ok") is False
            or receipt.get("success") is False
        ):
            conflicting_failure = True
        for key in ("created_drawer_ids", "cleanup_drawer_ids"):
            if key not in receipt:
                continue
            values = receipt[key]
            if not isinstance(values, list):
                malformed = True
                continue
            for item in values:
                if isinstance(item, str) and item:
                    if expected_operation_id is not None and (
                        not isinstance(expected_operation_id, str)
                        or not expected_operation_id
                        or receipt.get("operation_id") != expected_operation_id
                    ):
                        continue
                    cleanup_ids.append(item)
                    if key == "created_drawer_ids":
                        created_ids.append(item)
                else:
                    malformed = True
    return (
        list(dict.fromkeys(created_ids)),
        list(dict.fromkeys(cleanup_ids)),
        malformed,
        conflicting_failure,
    )


def cleanup_ids_from(value: Any) -> list[str]:
    """Return validated explicit create/cleanup IDs from protocol receipt fields."""
    return _drawer_id_evidence(value)[1]


def _operation_evidence(
    value: Any, *, expected_operation_id: str | None = None
) -> tuple[str | None, str | None, bool, bool]:
    """Return one coherent operation ID, its state/timeout, and invalidity."""
    records: list[tuple[str, str | None, bool]] = []
    malformed = False
    for receipt in receipt_dicts_from(value):
        if {"operation_id", "state", "timed_out"}.isdisjoint(receipt):
            continue
        operation_id = receipt.get("operation_id")
        if not isinstance(operation_id, str) or not operation_id:
            malformed = True
            continue
        state: str | None = None
        if "state" in receipt:
            state = receipt["state"]
            if not isinstance(state, str) or state not in _OPERATION_STATES:
                malformed = True
                state = None
        timed_out = False
        if "timed_out" in receipt:
            timeout = receipt["timed_out"]
            if not isinstance(timeout, bool):
                malformed = True
            else:
                timed_out = timeout
        records.append((operation_id, state, timed_out))
    unique_ids = list(dict.fromkeys(record[0] for record in records))
    terminal_states = {
        state
        for _operation_id, state, _timed_out in records
        if state in {"completed", "rejected", "failed"}
    }
    if (
        malformed
        or len(unique_ids) > 1
        or len(terminal_states) > 1
        or (
            expected_operation_id is not None
            and (
                not isinstance(expected_operation_id, str)
                or not expected_operation_id
                or unique_ids != [expected_operation_id]
            )
        )
    ):
        return None, None, False, True
    states = [state for _operation_id, state, _timed_out in records if state is not None]
    return (
        unique_ids[0] if unique_ids else None,
        next(iter(terminal_states)) if terminal_states else (states[-1] if states else None),
        any(record[2] for record in records),
        False,
    )


def operation_id_from(
    value: Any, *, expected_operation_id: str | None = None
) -> str | None:
    """Return the sole coherent operation ID from the complete attempt."""
    return _operation_evidence(
        value, expected_operation_id=expected_operation_id
    )[0]


def operation_state_from(
    value: Any, *, expected_operation_id: str | None = None
) -> str | None:
    """Return state only when the complete attempt has coherent operation evidence."""
    return _operation_evidence(
        value, expected_operation_id=expected_operation_id
    )[1]


def _is_nonnegative_rust_wire_integer(value: Any) -> bool:
    return (
        isinstance(value, int)
        and not isinstance(value, bool)
        and 0 <= value <= _MAX_RUST_WIRE_INTEGER
    )


def _closed_capacity(value: Any) -> dict[str, int] | None:
    if not isinstance(value, dict) or set(value) != _CAPACITY_FIELDS:
        return None
    if not all(_is_nonnegative_rust_wire_integer(amount) for amount in value.values()):
        return None
    return value


def _saturating_rust_wire_add(left: int, right: int) -> int:
    return min(_MAX_RUST_WIRE_INTEGER, left + right)


def _coherent_holder_budget_refusal(receipt: Any, *, mcp: bool) -> bool:
    expected_fields = _MCP_REFUSAL_FIELDS if mcp else _CLI_REFUSAL_FIELDS
    if not isinstance(receipt, dict) or set(receipt) != expected_fields:
        return False
    if (
        receipt.get("outcome") != "admission_blocked"
        or receipt.get("reason") != "holder_budget_exceeded"
        or receipt.get("action") != "write_refused"
        or receipt.get("created_drawer_ids") != []
        or receipt.get("cleanup_drawer_ids") != []
    ):
        return False
    capacity = _closed_capacity(receipt["capacity"])
    headroom = _closed_capacity(receipt["headroom"])
    admission = receipt["profile_admission"]
    profile_fields = _MCP_PROFILE_FIELDS if mcp else _CLI_PROFILE_FIELDS
    if capacity is None or headroom is None or not isinstance(admission, dict) or set(admission) != profile_fields:
        return False
    if mcp:
        diagnostic = receipt["database_diagnostic"]
        admission_capacity = _closed_capacity(admission["capacity"])
        admission_headroom = _closed_capacity(admission["headroom"])
        if (
            receipt["async_pool_loaded"] is not False
            or not isinstance(diagnostic, dict)
            or set(diagnostic) != _MCP_DIAGNOSTIC_FIELDS
            or not all(isinstance(value, str) for value in diagnostic.values())
            or diagnostic["source"] != "async_db"
            or diagnostic["failure_kind"] != "holder_budget_exceeded"
            or admission["async_pool_loaded"] is not False
            or admission_capacity is None
            or admission_headroom is None
            or admission_capacity != capacity
            or admission_headroom != headroom
            or admission["available_cache_bytes"] != headroom["cache_bytes"]
            or not isinstance(admission["unknown_holder_diagnostics"], list)
            or any(
                not isinstance(item, dict)
                or set(item) != {"generation", "reason"}
                or not _is_nonnegative_rust_wire_integer(item["generation"])
                or not isinstance(item["reason"], str)
                or item["reason"] not in _UNKNOWN_HOLDER_REASONS
                for item in admission["unknown_holder_diagnostics"]
            )
            or admission["unknown_holders"] != len(admission["unknown_holder_diagnostics"])
        ):
            return False
    numeric_fields = (
        "active_holders", "configured_holder_limit", "active_cache_bytes",
        "configured_cache_bytes", "reaped_stale_holders_this_snapshot",
        "reserved_service_holders", "service_holders", "requested_cache_bytes",
    )
    if mcp:
        numeric_fields += ("available_cache_bytes", "unknown_holders")
    if not all(_is_nonnegative_rust_wire_integer(admission[field]) for field in numeric_fields):
        return False
    holders, cache_bytes = capacity["holders"], capacity["cache_bytes"]
    active_holders, active_cache = admission["active_holders"], admission["active_cache_bytes"]
    requested_cache = admission["requested_cache_bytes"]
    budget_reason = admission["budget_reason"]
    allowed_budget_reasons = (
        {"holder_limit", "cache_budget"}
        if mcp
        else {"holder_limit", "cache_budget", "reserved_service_slots"}
    )
    if (
        holders == 0
        or cache_bytes == 0
        or requested_cache == 0
        or admission["configured_holder_limit"] != holders
        or admission["configured_cache_bytes"] != cache_bytes
        or headroom["holders"] != max(0, holders - active_holders)
        or headroom["cache_bytes"] != max(0, cache_bytes - active_cache)
        or admission["service_holders"] > active_holders
        or admission["reserved_service_holders"] > holders
        or not isinstance(budget_reason, str)
        or budget_reason not in allowed_budget_reasons
    ):
        return False
    if budget_reason == "holder_limit":
        return active_holders >= holders
    if budget_reason == "cache_budget":
        return (
            active_holders < holders
            and _saturating_rust_wire_add(active_cache, requested_cache) > cache_bytes
        )
    return (
        active_holders < holders
        and _saturating_rust_wire_add(active_cache, requested_cache) <= cache_bytes
        and admission["reserved_service_holders"] > 0
        and _saturating_rust_wire_add(
            _saturating_rust_wire_add(active_holders, 1),
            admission["reserved_service_holders"],
        ) > holders
    )


def _mcp_holder_budget_no_write_receipt(value: Any) -> dict[str, Any] | None:
    if not isinstance(value, dict) or set(value) != {"jsonrpc", "id", "error"}:
        return None
    error = value["error"]
    if (
        value["jsonrpc"] != "2.0"
        or not _is_nonnegative_rust_wire_integer(value["id"])
        or not isinstance(error, dict)
        or set(error) != {"code", "message", "data"}
        or not isinstance(error["code"], int)
        or isinstance(error["code"], bool)
        or error["code"] != -32603
        or not isinstance(error["message"], str)
        or not _coherent_holder_budget_refusal(error["data"], mcp=True)
    ):
        return None
    return {
        "outcome": "admission_blocked",
        "reason": "holder_budget_exceeded",
        "cleanup_required": False,
    }


def _rest_holder_budget_no_write_receipt(value: Any) -> dict[str, Any] | None:
    if not isinstance(value, dict) or set(value) != {"error"}:
        return None
    error = value["error"]
    if not isinstance(error, dict) or set(error) != _REST_ERROR_FIELDS:
        return None
    if (
        error["message"] != _REST_HOLDER_BUDGET_MESSAGE
        or not isinstance(error["status"], int)
        or isinstance(error["status"], bool)
        or error["status"] != 503
        or error["kind"] != "admission_blocked"
        or error["retryable"] is not False
    ):
        return None
    receipt = {key: error[key] for key in _CLI_REFUSAL_FIELDS}
    if not _coherent_holder_budget_refusal(receipt, mcp=False):
        return None
    return {
        "outcome": "admission_blocked",
        "reason": "holder_budget_exceeded",
        "cleanup_required": False,
    }


def _cli_holder_budget_no_write_receipt(value: Any) -> dict[str, Any] | None:
    entries = value if isinstance(value, list) else [value]
    if not entries:
        return None
    receipt_count = 0
    for entry in entries:
        if not isinstance(entry, dict):
            return None
        if "returncode" in entry:
            if (
                set(entry) != {"returncode"}
                or not isinstance(entry["returncode"], int)
                or isinstance(entry["returncode"], bool)
                or entry["returncode"] == 0
            ):
                return None
            continue
        if not _coherent_holder_budget_refusal(entry, mcp=False):
            return None
        receipt_count += 1
    if receipt_count != 1:
        return None
    return {
        "outcome": "admission_blocked",
        "reason": "holder_budget_exceeded",
        "cleanup_required": False,
    }


def holder_budget_no_write_receipt(value: Any) -> dict[str, Any] | None:
    """Return a strict no-write receipt only after whole-attempt classification."""
    _created_ids, cleanup_ids, malformed_ids, _conflicting_failure = (
        _drawer_id_evidence(value)
    )
    if cleanup_ids or malformed_ids:
        return None
    return (
        _mcp_holder_budget_no_write_receipt(value)
        or _rest_holder_budget_no_write_receipt(value)
        or _cli_holder_budget_no_write_receipt(value)
    )


def classify_create_attempt(
    value: Any, *, expected_operation_id: str | None = None
) -> dict[str, Any]:
    """Classify a complete create attempt before producing smoke-safe summaries."""
    created_ids, cleanup_ids, malformed_ids, conflicting_failure = (
        _drawer_id_evidence(value, expected_operation_id=expected_operation_id)
    )
    cleanup_evidence = {"cleanup_drawer_ids": cleanup_ids} if cleanup_ids else {}
    operation_id, state, timed_out, malformed_operation = _operation_evidence(
        value, expected_operation_id=expected_operation_id
    )
    if malformed_operation or malformed_ids:
        return {"kind": "inconclusive", **cleanup_evidence}
    if operation_id is not None and state not in {"completed", "rejected", "failed"}:
        queued = {
            "kind": "queued",
            "operation_id": operation_id,
            "state": state,
            **cleanup_evidence,
        }
        if timed_out:
            queued["timed_out"] = True
        return queued
    if state in {"queued", "running", "rejected", "failed"}:
        return {"kind": "inconclusive", **cleanup_evidence}
    receipt = (
        holder_budget_no_write_receipt(value)
        if expected_operation_id is None
        else None
    )
    if receipt is not None:
        return {"kind": "proven_no_write", "receipt": receipt}
    if conflicting_failure:
        return {"kind": "inconclusive", **cleanup_evidence}
    if created_ids:
        return {
            "kind": "created",
            "created_drawer_ids": created_ids,
            **cleanup_evidence,
        }
    return {"kind": "inconclusive", **cleanup_evidence}


def followable_update_without_terminal_ids(info: dict[str, Any]) -> bool:
    """True when a queued/running follow must not be labeled update-missing."""
    if info.get("kind") != "queued" or not info.get("operation_id_present"):
        return False
    state = info.get("recovered_state") or info.get("operation_state")
    return state in {"queued", "running"}


def update_missing_reason(info: dict[str, Any]) -> str:
    if followable_update_without_terminal_ids(info):
        return "update_followable_not_terminal"
    return "update_missing_created_drawer_ids"

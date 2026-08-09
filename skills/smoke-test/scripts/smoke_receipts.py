"""Strict aggregate-only classification for smoke create attempts."""
from __future__ import annotations

from typing import Any

__all__ = [
    "classify_create_attempt",
    "created_ids_from",
    "holder_budget_no_write_receipt",
    "operation_id_from",
    "operation_state_from",
    "receipt_dicts_from",
]

_MAX_RUST_WIRE_INTEGER = (1 << 64) - 1
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


def _created_id_evidence(value: Any) -> tuple[list[str], bool]:
    ids: list[str] = []
    malformed = False
    for receipt in receipt_dicts_from(value):
        for key in ("created_drawer_ids", "cleanup_drawer_ids"):
            if key not in receipt:
                continue
            values = receipt[key]
            if not isinstance(values, list):
                malformed = True
                continue
            for item in values:
                if isinstance(item, str) and item:
                    ids.append(item)
                else:
                    malformed = True
    return list(dict.fromkeys(ids)), malformed


def created_ids_from(value: Any) -> list[str]:
    """Return validated explicit create/cleanup IDs from protocol receipt fields."""
    return _created_id_evidence(value)[0]


def operation_id_from(value: Any) -> str | None:
    """Return the first explicit operation ID from the complete attempt."""
    for receipt in receipt_dicts_from(value):
        operation_id = receipt.get("operation_id")
        if isinstance(operation_id, str) and operation_id:
            return operation_id
    return None


def operation_state_from(value: Any) -> str | None:
    """Return a terminal state first, otherwise the latest explicit state."""
    last_state: str | None = None
    for receipt in receipt_dicts_from(value):
        state = receipt.get("state")
        if isinstance(state, str) and state:
            if state in {"completed", "rejected", "failed"}:
                return state
            last_state = state
    return last_state


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
    created_ids, malformed_ids = _created_id_evidence(value)
    if created_ids or malformed_ids:
        return None
    return (
        _mcp_holder_budget_no_write_receipt(value)
        or _rest_holder_budget_no_write_receipt(value)
        or _cli_holder_budget_no_write_receipt(value)
    )


def classify_create_attempt(value: Any) -> dict[str, Any]:
    """Classify a complete create attempt before producing smoke-safe summaries."""
    created_ids, malformed_ids = _created_id_evidence(value)
    id_evidence = {"created_drawer_ids": created_ids} if created_ids else {}
    operation_id = operation_id_from(value)
    state = operation_state_from(value)
    if operation_id is not None and state not in {"completed", "rejected", "failed"}:
        return {
            "kind": "queued",
            "operation_id": operation_id,
            "state": state,
            **id_evidence,
        }
    if state in {"queued", "running", "rejected", "failed"}:
        return {"kind": "inconclusive", **id_evidence}
    mcp_ids_are_cleanup_only = (
        isinstance(value, dict)
        and value.get("jsonrpc") == "2.0"
        and (
            "error" in value
            or ("result" in value and not isinstance(value["result"], dict))
        )
    )
    if malformed_ids or (created_ids and mcp_ids_are_cleanup_only):
        return {"kind": "inconclusive", **id_evidence}
    if created_ids:
        return {"kind": "created", "created_drawer_ids": created_ids}
    if operation_id is not None:
        return {"kind": "queued", "operation_id": operation_id, "state": state}
    receipt = holder_budget_no_write_receipt(value)
    if receipt is not None:
        return {"kind": "proven_no_write", "receipt": receipt}
    return {"kind": "inconclusive"}

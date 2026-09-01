"""Durable terminal-readback contract for explicit Hermes conclusions."""

from __future__ import annotations

import logging
import secrets
import time
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional

from ._write_spool import (
    OperationKeyConflictError,
    WriteSpool,
    classify_replay_error,
    classify_write_error,
    valid_control_token,
)

logger = logging.getLogger(__name__)


@dataclass(frozen=True)
class ConcludeResult:
    stored: bool
    payload: Dict[str, Any]


_TERMINAL_KINDS = {
    "operation_key_conflict",
    "invalid_control_fields",
    "malformed_spool_row",
}


def conclude_side_effects(
    provider: Any,
    payload: Dict[str, Any],
) -> None:
    """Apply failure/wake bookkeeping for a non-stored conclusion.

    One terminal-kind classification owns both the breaker-failure decision
    and the worker-wake decision for conclusion outcomes, so no ad hoc set
    can drift out of sync with the classifications in this module.
    """
    details = payload.get("error_details", {})
    local_admission = (
        payload.get("state") == "local_admitted"
        or payload.get("durability", {}).get("kind") == "durable_replay_deferred"
        or (
            details.get("kind") == "durable_admission_deferred"
            and details.get("error_class") == "breaker_open"
        )
        or details.get("kind")
        in {"durable_replay_deferred", "durable_operation_pending"}
    )
    if not local_admission and details.get("kind") not in _TERMINAL_KINDS:
        try:
            provider._record_failure()
        except Exception:
            logger.warning("mempal conclude failure bookkeeping failed")
    if (
        details.get("kind") not in _TERMINAL_KINDS
        and details.get("kind") != "local_durable_admission_failed"
    ):
        try:
            provider._wake_spool_worker()
        except Exception:
            logger.warning("mempal conclude replay wake failed")


def conclusion_request(
    conclusion: str,
    wing: str,
    room: str,
    importance: int,
    project_id: Optional[str],
) -> Dict[str, Any]:
    """Build the verbatim durable request without exposing transport details."""
    request = {
        "content": conclusion,
        "wing": wing,
        "room": room,
        "memory_kind": "profile_fact",
        "importance": importance,
        "source_type": "user_explicit",
        "source": "hermes-session-conclusion",
    }
    if project_id:
        request["project_id"] = project_id
    return request


def submit_conclusion(
    spool: Optional[WriteSpool],
    post: Callable[[str, Dict[str, Any]], Any],
    get: Callable[[str], Any],
    request: Dict[str, Any],
    *,
    operation_key: Optional[str],
    wait_timeout: float,
    transport_allowed: bool = True,
    replay_allowed: Optional[Callable[[], bool]] = None,
) -> ConcludeResult:
    """Admit once and report success only after authoritative completion."""
    if not valid_control_token(operation_key):
        return ConcludeResult(False, _invalid_control_payload())
    key = operation_key or secrets.token_urlsafe(32)
    if spool is None:
        return ConcludeResult(False, _retry_payload(
            "local_durable_admission_failed",
            None,
            key,
            "local_admission_failed",
            "spool_uninitialized",
        ))
    try:
        spool.admit("ingest", request, action="conclude", operation_key=key)
    except OperationKeyConflictError:
        return ConcludeResult(False, _retry_payload(
            "operation_key_conflict",
            None,
            key,
            "operation_key_conflict",
            "operation_key_conflict",
            retry_safe=False,
        ))
    except Exception as exc:
        return ConcludeResult(False, _retry_payload(
            "local_durable_admission_failed",
            None,
            key,
            "local_admission_failed",
            classify_write_error(exc),
        ))

    if not transport_allowed:
        return ConcludeResult(False, _retry_payload(
            "durable_admission_deferred",
            None,
            key,
            "local_admitted",
            "breaker_open",
        ))

    deadline = time.monotonic() + max(0.0, wait_timeout)
    operation_id: Optional[str] = None
    state = "local_admitted"
    while True:
        outcome = spool.replay_operation_key(
            key,
            post,
            get,
            ignore_retry_delay=True,
            replay_allowed=replay_allowed,
        )
        if outcome is None:
            return ConcludeResult(False, _retry_payload(
                "durable_operation_pending",
                operation_id,
                key,
                state,
            ))
        operation_id = outcome.operation_id or operation_id
        if outcome.error_class == "breaker_open":
            return ConcludeResult(False, _retry_payload(
                "durable_admission_deferred",
                operation_id,
                key,
                "local_admitted",
                "breaker_open",
            ))
        if outcome.completed and outcome.drawer_id:
            return ConcludeResult(True, {
                "result": "Fact stored.",
                "operation_id": operation_id or "",
                "operation_key": key,
                "drawer_id": outcome.drawer_id,
            })
        error_class = outcome.error_class
        if error_class and error_class.startswith("terminal_"):
            state = error_class.removeprefix("terminal_")
            return ConcludeResult(False, _retry_payload(
                f"durable_operation_{state}",
                operation_id,
                key,
                state,
                error_class,
                outcome.error_details,
                retry_safe=False,
            ))
        if error_class in {"operation_key_conflict", "malformed_spool_row"}:
            return ConcludeResult(False, _retry_payload(
                error_class,
                operation_id,
                key,
                error_class,
                error_class,
                outcome.error_details,
                retry_safe=False,
            ))
        if error_class and error_class.startswith("status_"):
            state = error_class.removeprefix("status_")
            kind = (
                "durable_operation_pending"
                if state in {"queued", "running"}
                else "durable_status_invalid"
            )
        elif operation_id:
            kind = "durable_status_unavailable"
        else:
            kind = "durable_admission_deferred"
        classification = classify_replay_error(error_class) if error_class else None
        retry_safe = bool(
            classification.retryable if classification is not None else True
        ) or kind == "durable_status_unavailable"
        if (
            kind == "durable_admission_deferred"
            or kind == "durable_status_invalid"
            or time.monotonic() >= deadline
        ):
            return ConcludeResult(False, _retry_payload(
                kind,
                operation_id,
                key,
                state,
                error_class,
                outcome.error_details,
                retry_safe=retry_safe,
            ))
        time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))


def _invalid_control_payload() -> Dict[str, Any]:
    return {
        "error": "Memory request was rejected.",
        "error_details": {
            "kind": "invalid_control_fields",
            "retry_safe": False,
        },
    }


def _retry_payload(
    kind: str,
    operation_id: Optional[str],
    operation_key: str,
    state: str,
    error_class: Optional[str] = None,
    transport_details: Optional[Dict[str, Any]] = None,
    *,
    retry_safe: bool = True,
) -> Dict[str, Any]:
    details = {
        "kind": kind,
        "operation_key": operation_key,
        "retry_operation_id": operation_key,
        "state": state,
        "retry_safe": retry_safe,
    }
    if operation_id:
        details["operation_id"] = operation_id
    if error_class:
        details["error_class"] = error_class
    if transport_details:
        details["transport"] = dict(transport_details)
    return {
        "error": "Memory is not yet confirmed stored.",
        "error_details": details,
    }

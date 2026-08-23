"""Durable terminal-readback contract for explicit Hermes conclusions."""

from __future__ import annotations

import secrets
import time
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional

from ._write_spool import OperationKeyConflictError, WriteSpool, classify_write_error


@dataclass(frozen=True)
class ConcludeResult:
    stored: bool
    payload: Dict[str, Any]


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
        return ConcludeResult(False, _local_admission_pending_payload(key))

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
            return ConcludeResult(
                False,
                _local_admission_pending_payload(
                    key,
                    operation_id=operation_id,
                ),
            )
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
                f"durable_operation_{state}", operation_id, key, state
            ))
        if error_class and error_class.startswith("status_"):
            state = error_class.removeprefix("status_")
            kind = "durable_status_invalid" if state == "unknown" else "durable_operation_pending"
        elif operation_id:
            kind = "durable_status_unavailable"
        else:
            kind = "durable_admission_deferred"
        if kind == "durable_admission_deferred" or time.monotonic() >= deadline:
            return ConcludeResult(False, _retry_payload(
                kind,
                operation_id,
                key,
                state,
                error_class,
                outcome.error_details,
            ))
        time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))


def _local_admission_pending_payload(
    operation_key: str,
    *,
    operation_id: Optional[str] = None,
) -> Dict[str, Any]:
    payload = {
        "result": "Fact admitted locally; durable storage pending.",
        "operation_key": operation_key,
        "retry_operation_id": operation_key,
        "state": "local_admitted",
        "retry_safe": True,
        "durability": {
            "state": "pending",
            "kind": "durable_replay_deferred",
            "deferred_reason": "breaker_open",
        },
    }
    if operation_id:
        payload["operation_id"] = operation_id
    return payload


def _retry_payload(
    kind: str,
    operation_id: Optional[str],
    operation_key: str,
    state: str,
    error_class: Optional[str] = None,
    transport_details: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    details = {
        "kind": kind,
        "operation_key": operation_key,
        "retry_operation_id": operation_key,
        "state": state,
        "retry_safe": True,
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

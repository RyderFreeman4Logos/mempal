"""Durable terminal-readback contract for explicit Hermes conclusions."""

from __future__ import annotations

import secrets
import time
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional

from ._write_spool import classify_write_error


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
    }
    if project_id:
        request["project_id"] = project_id
    return request


def submit_conclusion(
    post: Callable[[str, Dict[str, Any]], Any],
    get: Callable[[str], Any],
    request: Dict[str, Any],
    *,
    operation_key: Optional[str],
    wait_timeout: float,
) -> ConcludeResult:
    """Admit once and report success only after authoritative completion."""
    key = operation_key or secrets.token_urlsafe(32)
    receipt = post(
        "/api/ingest/durable",
        {"idempotency_key": key, "request": request},
    )
    if not isinstance(receipt, dict) or not receipt.get("operation_id"):
        raise RuntimeError("durable conclusion admission omitted operation_id")
    operation_id = str(receipt["operation_id"])
    deadline = time.monotonic() + max(0.0, wait_timeout)
    state = str(receipt.get("state") or "queued")
    while True:
        try:
            status = get(f"/api/operations/{operation_id}")
        except Exception as exc:
            return ConcludeResult(False, _retry_payload(
                "durable_status_unavailable",
                operation_id,
                key,
                state,
                classify_write_error(exc),
            ))
        if not isinstance(status, dict):
            return ConcludeResult(False, _retry_payload(
                "durable_status_invalid", operation_id, key, state
            ))
        state = str(status.get("state") or "")
        drawer_id = str(status.get("drawer_id") or "")
        if state == "completed" and drawer_id:
            return ConcludeResult(True, {
                "result": "Fact stored.",
                "operation_id": operation_id,
                "operation_key": key,
                "drawer_id": drawer_id,
            })
        if state in {"failed", "rejected"}:
            return ConcludeResult(False, _retry_payload(
                f"durable_operation_{state}", operation_id, key, state
            ))
        if time.monotonic() >= deadline:
            return ConcludeResult(False, _retry_payload(
                "durable_operation_pending", operation_id, key, state
            ))
        time.sleep(min(0.05, max(0.0, deadline - time.monotonic())))


def _retry_payload(
    kind: str,
    operation_id: str,
    operation_key: str,
    state: str,
    error_class: Optional[str] = None,
) -> Dict[str, Any]:
    details = {
        "kind": kind,
        "operation_id": operation_id,
        "operation_key": operation_key,
        "state": state,
        "retry_safe": True,
    }
    if error_class:
        details["error_class"] = error_class
    return {
        "error": "Memory is not yet confirmed stored.",
        "error_details": details,
    }

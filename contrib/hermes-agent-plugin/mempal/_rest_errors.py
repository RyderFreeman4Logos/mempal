"""Redacted, actionable REST error classification for the Hermes plugin."""

from __future__ import annotations

import http.client
import json
import urllib.error
from http import HTTPStatus
from typing import Any, Dict, Optional

__all__ = ["rest_error_payload"]

_REST_ERROR_BODY_MAX_BYTES = 64 * 1024


def _route_class(path: str) -> str:
    if path in {
        "/api/ingest",
        "/api/ingest/durable",
        "/api/delete",
        "/api/delete/durable",
    }:
        return "write"
    if path in {"/api/search", "/api/timeline", "/api/pinned_facts"}:
        return "read"
    return "other"


def _http_status_text(status: int) -> str:
    try:
        return HTTPStatus(status).phrase
    except ValueError:
        return "HTTP error"


def _is_retryable_http_status(status: int) -> bool:
    return status in {408, 429} or 500 <= status <= 599


def _rest_recovery_hint(status: Optional[int], path: str) -> str:
    if status is None:
        return "Confirm the local mempal REST daemon is running and reachable, then retry."
    if _is_retryable_http_status(status):
        return f"Retry the request; if it persists, inspect mempal daemon logs for {path}."
    if 400 <= status <= 499:
        return "Check the tool request fields; mempal rejected the write before storage."
    return f"Inspect mempal daemon logs for {path}."


def _stale_daemon_http_error_details(exc: Any) -> Dict[str, Any]:
    """Read only the bounded, allowlisted stale-daemon REST error contract."""
    try:
        raw = exc.read(_REST_ERROR_BODY_MAX_BYTES + 1)
    except (AttributeError, OSError, ValueError, http.client.HTTPException):
        return {}
    if not isinstance(raw, bytes) or len(raw) > _REST_ERROR_BODY_MAX_BYTES:
        return {}
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return {}
    error = payload.get("error") if isinstance(payload, dict) else None
    if not isinstance(error, dict) or error.get("kind") != "stale_daemon":
        return {}
    daemon_pid = error.get("daemon_pid")
    if (
        error.get("stale_daemon") is not True
        or error.get("exe_deleted") is not True
        or error.get("retryable") is not False
        or error.get("retry_safe_after_restart") is not True
        or not isinstance(daemon_pid, int)
        or isinstance(daemon_pid, bool)
        or daemon_pid <= 0
    ):
        return {}
    return {
        "kind": "stale_daemon",
        "boundary": "daemon_executable",
        "action": "restart_daemon_then_retry",
        "stale_daemon": True,
        "daemon_pid": daemon_pid,
        "exe_deleted": True,
        "retryable": False,
        "retry_safe_after_restart": True,
        "recovery_hint": "Run `mempal daemon restart`, then retry the write once.",
    }


def rest_error_payload(message: str, path: str, exc: Exception) -> Dict[str, Any]:
    """Classify a REST failure without exposing request or response content."""
    status: Optional[int] = None
    details: Dict[str, Any] = {
        "route": path,
        "route_class": _route_class(path),
    }
    if isinstance(exc, urllib.error.HTTPError):
        status = int(exc.code)
        details.update({
            "kind": "rest_http_error",
            "http_status": status,
            "status_text": _http_status_text(status),
            "retryable": _is_retryable_http_status(status),
        })
        if status == 503:
            details.update(_stale_daemon_http_error_details(exc))
    elif isinstance(exc, urllib.error.URLError):
        details.update({
            "kind": "rest_transport_error",
            "error_class": exc.__class__.__name__,
            "retryable": True,
        })
    else:
        details.update({
            "kind": "plugin_exception",
            "error_class": exc.__class__.__name__,
        })
    details.setdefault("recovery_hint", _rest_recovery_hint(status, path))
    return {"error": message, "error_details": details}

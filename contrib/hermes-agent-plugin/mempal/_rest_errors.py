"""Redacted, actionable REST error classification for the Hermes plugin."""

from __future__ import annotations

import http.client
import json
import urllib.error
from http import HTTPStatus
from typing import Any, Dict, Optional

__all__ = [
    "rest_error_payload",
    "search_metadata_from_headers",
    "search_timeout_metadata_from_http_error",
]

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


def _read_http_error_json(exc: Any) -> Dict[str, Any]:
    try:
        raw = exc.read(_REST_ERROR_BODY_MAX_BYTES + 1)
    except (AttributeError, OSError, ValueError, http.client.HTTPException):
        return {}
    finally:
        try:
            exc.close()
        except (AttributeError, OSError, ValueError, http.client.HTTPException):
            pass
    if not isinstance(raw, bytes) or len(raw) > _REST_ERROR_BODY_MAX_BYTES:
        return {}
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return {}
    return payload if isinstance(payload, dict) else {}


def _stale_daemon_http_error_details(exc: Any) -> Dict[str, Any]:
    """Read only the bounded, allowlisted stale-daemon REST error contract."""
    payload = _read_http_error_json(exc)
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


def _sanitize_search_metadata(
    source: Any,
    correlation_id: str,
    default_deadline_ms: Optional[int],
) -> Dict[str, Any]:
    if not isinstance(source, dict) or source.get("correlation_id") != correlation_id:
        return {}
    allowed_stages = {
        "embedding", "routing", "hybrid_db", "bm25_fallback_db", "bm25_db", "rerank",
    }
    allowed_boundaries = {
        "daemon.embedding", "daemon.search_db", "daemon.reranker",
    }
    allowed_fallbacks = {"bm25", "original_ranking", "route_defaults"}
    timeouts = []
    raw_timeouts = source.get("timeouts")
    if isinstance(raw_timeouts, list):
        for item in raw_timeouts:
            if not isinstance(item, dict):
                continue
            stage = item.get("stage")
            boundary = item.get("boundary")
            if stage in allowed_stages and boundary in allowed_boundaries:
                timeouts.append({"stage": stage, "boundary": boundary})
    raw_fallbacks = source.get("fallback_used")
    fallback_used = []
    if isinstance(raw_fallbacks, list):
        fallback_used = [
            value for value in raw_fallbacks if value in allowed_fallbacks
        ]
    elapsed_ms = source.get("elapsed_ms")
    deadline_ms = source.get("deadline_ms")
    if not isinstance(elapsed_ms, int) or isinstance(elapsed_ms, bool) or elapsed_ms < 0:
        elapsed_ms = 0
    if (
        not isinstance(deadline_ms, int)
        or isinstance(deadline_ms, bool)
        or deadline_ms <= 0
    ):
        deadline_ms = default_deadline_ms
    metadata = {
        "correlation_id": correlation_id,
        "elapsed_ms": elapsed_ms,
        "partial": source.get("partial") is True,
        "retry_safe": source.get("retry_safe") is True,
        "fallback_used": fallback_used,
        "timeouts": timeouts,
    }
    if isinstance(deadline_ms, int) and not isinstance(deadline_ms, bool) and deadline_ms > 0:
        metadata["deadline_ms"] = deadline_ms
    return metadata


def search_metadata_from_headers(
    headers: Any,
    correlation_id: str,
    default_deadline_ms: Optional[int] = None,
) -> Dict[str, Any]:
    normalized = {
        str(key).lower(): str(value)
        for key, value in dict(headers or {}).items()
        if value is not None
    }
    raw = normalized.get("mempal-search-metadata")
    if not raw:
        return {}
    try:
        source = json.loads(raw)
    except (TypeError, ValueError):
        return {}
    return _sanitize_search_metadata(source, correlation_id, default_deadline_ms)


def search_timeout_metadata_from_http_error(
    exc: Exception,
    correlation_id: str,
    default_deadline_ms: Optional[int] = None,
) -> Dict[str, Any]:
    """Parse only the allowlisted terminal-search timeout contract."""
    if not isinstance(exc, urllib.error.HTTPError) or int(exc.code) != 504:
        return {}
    header_metadata = search_metadata_from_headers(
        getattr(exc, "headers", {}), correlation_id, default_deadline_ms,
    )
    payload = _read_http_error_json(exc)
    error = payload.get("error") if isinstance(payload, dict) else None
    if not isinstance(error, dict) or error.get("kind") != "search_timeout":
        return header_metadata
    body_metadata = _sanitize_search_metadata(
        error.get("search_metadata"), correlation_id, default_deadline_ms,
    )
    return body_metadata or header_metadata


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

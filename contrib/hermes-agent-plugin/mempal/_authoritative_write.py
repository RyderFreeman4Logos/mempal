"""Hermes authoritative memory writes through the durable local spool."""

from __future__ import annotations

import json
import logging

logger = logging.getLogger(__name__)


def authoritative_memory_write(provider, request, **kwargs):
    """Persist one Hermes core memory operation through the durable spool."""
    self = provider
    del kwargs
    if not isinstance(request, dict):
        return json.dumps({
            "success": False,
            "error_class": "invalid_request",
            "retryable": False,
        })
    action = request.get("action")
    target = str(request.get("target") or "memory")
    content = request.get("content")
    old_text = request.get("old_text")
    operations = request.get("operations")

    def validation_error(item_action, item_content, item_old_text):
        if item_action not in {"add", "replace", "remove"}:
            return "invalid_action"
        if item_action in {"add", "replace"} and (
            not isinstance(item_content, str) or not item_content
        ):
            return "missing_content"
        if item_action in {"replace", "remove"} and (
            not isinstance(item_old_text, str) or not item_old_text
        ):
            return "missing_old_text"
        return None

    if operations is not None:
        if not isinstance(operations, list) or not operations:
            return json.dumps({
                "success": False,
                "error_class": "invalid_operations",
                "retryable": False,
            })
        normalized = []
        for item in operations:
            if not isinstance(item, dict):
                return json.dumps({
                    "success": False,
                    "error_class": "invalid_request",
                    "retryable": False,
                })
            item_action = item.get("action")
            item_content = item.get("content")
            item_old_text = item.get("old_text")
            error_class = validation_error(item_action, item_content, item_old_text)
            if error_class is not None:
                return json.dumps({
                    "success": False,
                    "error_class": error_class,
                    "retryable": False,
                })
            normalized.append({
                "action": item_action,
                "target": target,
                "content": item_content,
                "old_text": item_old_text,
            })

        completed_ids = []
        operation_keys = []
        for item in normalized:
            receipt = json.loads(self.authoritative_memory_write(item))
            if receipt.get("operation_key"):
                operation_keys.append(receipt["operation_key"])
            if receipt.get("success"):
                if receipt.get("operation_id"):
                    completed_ids.append(receipt["operation_id"])
                continue
            error_class = receipt.get("error_class") or "durable_write_pending"
            return json.dumps({
                "success": False,
                "partial_write": bool(completed_ids),
                "operation_ids": completed_ids,
                "operation_keys": operation_keys,
                "error_class": error_class,
                "last_error_class": error_class,
                "retryable": receipt.get("retryable", True),
            })
        return json.dumps({
            "success": True,
            "partial_write": False,
            "operation_ids": completed_ids,
            "operation_keys": operation_keys,
        })
    error_class = validation_error(action, content, old_text)
    if error_class is not None:
        return json.dumps({
            "success": False,
            "error_class": error_class,
            "retryable": False,
        })

    track_key = f"{target}:{self._wing}"
    room = self._memory_room_for_target(target)
    if action == "remove":
        body = self._with_project_id({})
        kind = "delete"
    else:
        body = self._with_project_id({
            "content": content,
            "wing": self._wing,
            "room": room,
        })
        if action == "replace":
            body["replace_text"] = old_text
        if room == self._facts_room:
            body.update({
                "memory_kind": "profile_fact",
                "importance": self._safe_min_importance,
                "source_type": "user_explicit",
            })
        kind = "ingest"

    try:
        operation = self._spool_write(
            kind,
            body,
            track_key=track_key,
            action="delete" if action == "remove" else str(action),
            wake=False,
        )
    except Exception:
        return json.dumps({
            "success": False,
            "error_class": "local_durable_admission_failed",
            "retryable": True,
        })

    key = operation.operation_key
    if self._is_breaker_open():
        try:
            self._wake_spool_worker()
        except Exception:
            logger.warning("mempal authoritative write replay wake failed")
        return json.dumps({
            "success": False,
            "error_class": "breaker_open",
            "retryable": True,
            "operation_key": key,
        })
    try:
        outcome = self._write_spool.replay_operation_key(
            key,
            self._post,
            self._get,
            ignore_retry_delay=True,
            replay_allowed=lambda: not self._is_breaker_open(),
        )
    except Exception:
        outcome = None
    if outcome is not None and outcome.completed and outcome.drawer_id:
        try:
            self._record_success()
        except Exception:
            logger.warning("mempal authoritative write success bookkeeping failed")
        return json.dumps({
            "success": True,
            "drawer_id": outcome.drawer_id,
            "operation_id": outcome.operation_id or "",
            "operation_key": key,
        })
    error_class = (
        outcome.error_class if outcome is not None and outcome.error_class
        else "durable_write_pending"
    )
    if error_class == "breaker_open":
        retryable = True
    else:
        retryable = not error_class.startswith("terminal_") and error_class != "target_unresolved"
        try:
            self._record_failure()
        except Exception:
            logger.warning("mempal authoritative write failure bookkeeping failed")
    payload = {
        "success": False,
        "error_class": error_class,
        "retryable": retryable,
        "operation_key": key,
    }
    if outcome is not None and outcome.operation_id:
        payload["operation_id"] = outcome.operation_id
    return json.dumps(payload)

"""Hermes authoritative memory writes through the durable local spool."""

from __future__ import annotations

import json
import logging
from typing import Dict, Mapping, Optional, Protocol

from ._write_spool import (
    OperationKeyConflictError,
    SpoolOperation,
    WriteSpool,
    classify_replay_error,
    valid_control_token,
    valid_target,
)

logger = logging.getLogger(__name__)

__all__ = ["authoritative_memory_write"]


def _invalid_control_receipt() -> str:
    return json.dumps({
        "success": False,
        "error_class": "invalid_control_fields",
        "retryable": False,
        "retry_safe": False,
    })


class _AuthoritativeWriteProvider(Protocol):
    _write_spool: WriteSpool
    _wing: str
    _facts_room: str
    _safe_min_importance: int

    def _track_key(self, target: str) -> str: ...

    def _memory_room_for_target(self, target: str) -> str: ...

    def _with_project_id(self, payload: Dict[str, object]) -> Dict[str, object]: ...

    def _spool_write(
        self,
        kind: str,
        body: Dict[str, object],
        *,
        track_key: Optional[str],
        action: str,
        operation_key: Optional[str],
        wake: bool,
    ) -> SpoolOperation: ...

    def _is_breaker_open(self) -> bool: ...

    def _wake_spool_worker(self) -> None: ...

    def _post(self, path: str, body: Dict[str, object]) -> object: ...

    def _get(self, path: str) -> object: ...

    def _record_success(self) -> None: ...

    def _record_failure(self) -> None: ...

    def authoritative_memory_write(
        self, request: Mapping[str, object], **kwargs: object
    ) -> str: ...


def authoritative_memory_write(
    provider: _AuthoritativeWriteProvider,
    request: Mapping[str, object],
    **kwargs: object,
) -> str:
    """Persist one Hermes core memory operation through the durable spool."""
    self = provider
    retry_operation_key = kwargs.get("operation_key")
    if not valid_control_token(retry_operation_key):
        return _invalid_control_receipt()
    if not isinstance(retry_operation_key, str):
        retry_operation_key = None
    if not isinstance(request, Mapping):
        return json.dumps({
            "success": False,
            "error_class": "invalid_request",
            "retryable": False,
        })
    request = dict(request)
    if "target" in request and not valid_target(request["target"]):
        return _invalid_control_receipt()
    request_operation_key = request.get("operation_key")
    if not valid_control_token(request_operation_key):
        return _invalid_control_receipt()
    if not isinstance(request_operation_key, str):
        request_operation_key = None
    action = request.get("action")
    target = request.get("target", "memory")
    if not isinstance(target, str):
        return _invalid_control_receipt()
    content = request.get("content")
    old_text = request.get("old_text")
    operations = request.get("operations")
    if isinstance(request_operation_key, str):
        retry_operation_key = request_operation_key
    batch_operation_keys = request.get("operation_keys")

    def validation_error(
        item_action: object,
        item_content: object,
        item_old_text: object,
    ) -> Optional[str]:
        if not isinstance(item_action, str) or item_action not in {
            "add",
            "replace",
            "remove",
        }:
            return "invalid_action"
        if item_action in {"add", "replace"} and (
            not isinstance(item_content, str) or not item_content
        ):
            return "missing_content"
        if item_action in {"replace", "remove"} and (
            not isinstance(item_old_text, str) or not item_old_text
        ):
            return "missing_old_text"
        if item_action == "remove" and item_content is not None and item_content != "":
            return "ambiguous_remove_payload"
        return None

    def remove_shape_error(item: Dict[str, object]) -> Optional[str]:
        if item.get("action") != "remove":
            return None
        allowed = {"action", "content", "old_text", "operation_key", "target"}
        if any(key not in allowed for key in item):
            return "unsupported_remove_payload"
        return None

    if operations is not None:
        if not isinstance(operations, list) or not operations:
            return json.dumps({
                "success": False,
                "error_class": "invalid_operations",
                "retryable": False,
            })
        if batch_operation_keys is not None and (
            not isinstance(batch_operation_keys, list)
            or len(batch_operation_keys) != len(operations)
        ):
            return json.dumps({
                "success": False,
                "error_class": "invalid_operations",
                "retryable": False,
            })
        if isinstance(batch_operation_keys, list) and any(
            not valid_control_token(key) for key in batch_operation_keys
        ):
            return _invalid_control_receipt()
        normalized: list[Dict[str, object]] = []
        for index, item in enumerate(operations):
            if not isinstance(item, dict):
                return json.dumps({
                    "success": False,
                    "error_class": "invalid_request",
                    "retryable": False,
                })
            if "target" in item:
                return json.dumps({
                    "success": False,
                    "error_class": "unsupported_batch_item_target",
                    "retryable": False,
                })
            shape_error = remove_shape_error(item)
            if shape_error is not None:
                return json.dumps({
                    "success": False,
                    "error_class": shape_error,
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
            item_operation_key = item.get("operation_key")
            if not valid_control_token(item_operation_key):
                return _invalid_control_receipt()
            if item_operation_key is None and batch_operation_keys is not None:
                item_operation_key = batch_operation_keys[index]
            normalized.append({
                "action": item_action,
                "target": target,
                "content": item_content,
                "old_text": item_old_text,
                "operation_key": item_operation_key,
            })

        completed_ids: list[str] = []
        operation_keys: list[str] = []
        pending_durability: Optional[Dict[str, object]] = None
        for item in normalized:
            receipt = json.loads(self.authoritative_memory_write(item))
            receipt_operation_key = receipt.get("operation_key")
            if isinstance(receipt_operation_key, str) and receipt_operation_key:
                operation_keys.append(receipt_operation_key)
            is_completed = bool(receipt.get("success")) and receipt.get("state") != "local_admitted"
            if is_completed:
                operation_id = receipt.get("operation_id")
                if isinstance(operation_id, str) and operation_id:
                    completed_ids.append(operation_id)
                continue
            durability = receipt.get("durability")
            if (
                receipt.get("state") == "local_admitted"
                and isinstance(durability, dict)
                and durability.get("kind") == "durable_replay_deferred"
            ):
                pending_durability = durability
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
        if pending_durability is not None:
            error_class = pending_durability.get("deferred_reason") or "durable_write_pending"
            return json.dumps({
                "success": False,
                "partial_write": bool(completed_ids),
                "operation_ids": completed_ids,
                "operation_keys": operation_keys,
                "error_class": error_class,
                "last_error_class": error_class,
                "retryable": True,
                "retry_safe": True,
                "state": "local_admitted",
                "durability": pending_durability,
            })
        return json.dumps({
            "success": True,
            "partial_write": False,
            "operation_ids": completed_ids,
            "operation_keys": operation_keys,
        })
    shape_error = (
        remove_shape_error(request)
        if isinstance(request, dict)
        else None
    )
    error_class = shape_error or validation_error(action, content, old_text)
    if error_class is not None:
        return json.dumps({
            "success": False,
            "error_class": error_class,
            "retryable": False,
        })

    track_key = self._track_key(target)
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
            operation_key=retry_operation_key,
            wake=False,
        )
    except OperationKeyConflictError:
        return json.dumps({
            "success": False,
            "error_class": "operation_key_conflict",
            "retryable": False,
        })
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
    classification = classify_replay_error(error_class)
    if classification.count_failure:
        try:
            self._record_failure()
        except Exception:
            logger.warning("mempal authoritative write failure bookkeeping failed")
    if classification.retryable:
        try:
            self._wake_spool_worker()
        except Exception:
            logger.warning("mempal authoritative write replay wake failed")
        payload = {
            "success": True,
            "state": "local_admitted",
            "retryable": True,
            "retry_safe": True,
            "operation_key": key,
            "retry_operation_id": key,
            "durability": {
                "state": "pending",
                "kind": "durable_replay_deferred",
                "deferred_reason": (
                    error_class.removeprefix("status_")
                    if error_class.startswith("status_")
                    else error_class
                ) or "pending",
            },
        }
        if outcome is not None and outcome.operation_id:
            payload["operation_id"] = outcome.operation_id
        return json.dumps(payload)
    retryable = classification.retryable
    payload = {
        "success": False,
        "error_class": error_class,
        "retryable": retryable,
        "operation_key": key,
    }
    if outcome is not None and outcome.operation_id:
        payload["operation_id"] = outcome.operation_id
    return json.dumps(payload)

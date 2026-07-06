"""Shared circuit-breaker state for mempal Hermes plugins."""

from __future__ import annotations

import json
import os
import threading
import time
import uuid
from dataclasses import dataclass
from typing import Any, Dict, Optional


@dataclass(frozen=True)
class BackoffState:
    failure_count: int = 0
    open_until_epoch: float = 0.0

    @property
    def is_open(self) -> bool:
        return self.failure_count > 0 and time.time() < self.open_until_epoch


class SharedPluginBackoff:
    """Small file-backed breaker shared by mempal and mempal-hooks."""

    def __init__(
        self,
        *,
        path: Optional[str] = None,
        threshold: int = 5,
        cooldown_secs: float = 120.0,
    ) -> None:
        self._path = path or self._default_path()
        self._threshold = threshold
        self._cooldown_secs = cooldown_secs
        self._write_lock = threading.Lock()

    @staticmethod
    def _default_path() -> str:
        configured = os.environ.get("MEMPAL_PLUGIN_BACKOFF_PATH", "").strip()
        if configured:
            return configured
        return os.path.join(os.path.expanduser("~"), ".mempal", ".plugin_backoff")

    @property
    def path(self) -> str:
        return self._path

    def is_open(self) -> bool:
        state = self._read_state()
        if state.failure_count < self._threshold:
            return False
        if time.time() >= state.open_until_epoch:
            self.record_success()
            return False
        return True

    def record_success(self) -> BackoffState:
        state = BackoffState()
        self._write_state(state)
        return state

    def record_failure(self) -> BackoffState:
        state = self._read_state()
        failure_count = state.failure_count + 1
        open_until_epoch = state.open_until_epoch
        if failure_count >= self._threshold:
            open_until_epoch = time.time() + self._cooldown_secs
        next_state = BackoffState(
            failure_count=failure_count,
            open_until_epoch=open_until_epoch,
        )
        self._write_state(next_state)
        return next_state

    def _read_state(self) -> BackoffState:
        try:
            with open(self._path, encoding="utf-8") as handle:
                raw = json.loads(handle.read() or "{}")
        except (OSError, json.JSONDecodeError, ValueError):
            return BackoffState()
        if not isinstance(raw, dict):
            return BackoffState()
        return BackoffState(
            failure_count=self._positive_int(raw.get("failure_count")),
            open_until_epoch=self._positive_float(raw.get("open_until_epoch")),
        )

    def _write_state(self, state: BackoffState) -> None:
        with self._write_lock:
            directory = os.path.dirname(self._path)
            if directory:
                os.makedirs(directory, exist_ok=True)
            tmp_path = f"{self._path}.tmp.{uuid.uuid4().hex}"
            payload: Dict[str, Any] = {
                "failure_count": state.failure_count,
                "open_until_epoch": state.open_until_epoch,
                "updated_at_epoch": time.time(),
            }
            with open(tmp_path, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, separators=(",", ":"))
            os.replace(tmp_path, self._path)

    @staticmethod
    def _positive_int(value: Any) -> int:
        try:
            parsed = int(value)
        except (TypeError, ValueError):
            return 0
        return max(0, parsed)

    @staticmethod
    def _positive_float(value: Any) -> float:
        try:
            parsed = float(value)
        except (TypeError, ValueError):
            return 0.0
        return max(0.0, parsed)

"""Replay and terminal projection for durable Hermes mempal writes."""

from __future__ import annotations

import json
import sqlite3
import time
import urllib.error
from contextlib import closing
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional

from ._rest_errors import rest_error_payload

_MAX_REPLAY_ATTEMPTS = 5
_RETRY_BACKOFF_SECS = (0.25, 0.5, 1.0, 2.0, 5.0)
_RETRYABLE_HTTP_4XX = {408, 425, 429}
_RETRYABLE_STATES = {"queued", "running"}
_TERMINAL_STATES = {"failed", "rejected"}
_NON_RETRYABLE_REPLAY_ERRORS = {
    "status_invalid",
    "status_completed_missing_drawer",
    "terminal_failed",
    "terminal_rejected",
}

JsonObject = Dict[str, Any]
PostCallback = Callable[[str, JsonObject], Any]
GetCallback = Callable[[str], Any]

class OperationKeyConflictError(ValueError):
    """An operation key was reused for a different durable operation."""


class ClaimLostError(RuntimeError):
    """The SQLite lease expired or was settled by another worker."""


@dataclass(frozen=True)
class ReplayClassification:
    retryable: bool
    count_failure: bool

def legacy_track_key(track_key: str) -> Optional[str]:
    """Derive the pre-upgrade unscoped track key for a current-format key.

    Legacy track_drawers rows were written as f"{target}:{wing}" and carry no
    project identity.  The current format is an injective JSON tuple that
    preserves project isolation.  On exact current-format miss, the unscoped
    legacy row for the same target/wing is a best-effort match: the legacy
    namespace is project-agnostic by construction, and acting on such a row is
    the documented ambiguity policy for legacy unscoped rows.  The fallback
    only ever produces legacy-format keys, so it cannot alias two
    current-format keys.
    """
    try:
        parts = json.loads(track_key)
    except (TypeError, ValueError):
        return None
    if not isinstance(parts, list) or len(parts) != 4 or parts[0] != "track":
        return None
    target, wing = parts[1], parts[2]
    if not isinstance(target, str) or not isinstance(wing, str):
        return None
    return f"{target}:{wing}"


def classify_write_error(exc: Exception) -> str:
    """Return a content-free transport/storage failure class."""
    if isinstance(exc, urllib.error.HTTPError):
        if exc.code == 409:
            return "operation_key_conflict"
        return f"http_{exc.code}"
    if isinstance(exc, (TimeoutError, urllib.error.URLError)):
        return "network_timeout"
    if isinstance(exc, OSError):
        return f"os_error_{exc.errno or 'unknown'}"
    return "network_or_protocol_error"


def classify_replay_error(error_class: Optional[str]) -> ReplayClassification:
    """Classify replay outcomes once for direct and background callers."""
    if not error_class:
        return ReplayClassification(retryable=True, count_failure=False)
    if error_class in {
        "breaker_open",
        "claim_busy",
        "claim_lost",
        "durable_write_pending",
        "fifo_blocked",
        "retry_not_due",
    }:
        return ReplayClassification(retryable=True, count_failure=False)
    if error_class in {"status_queued", "status_running"}:
        return ReplayClassification(retryable=True, count_failure=False)
    if error_class == "operation_key_conflict":
        return ReplayClassification(retryable=False, count_failure=False)
    if error_class.startswith("http_"):
        try:
            status = int(error_class.removeprefix("http_"))
        except ValueError:
            return ReplayClassification(retryable=False, count_failure=True)
        return ReplayClassification(
            retryable=status in _RETRYABLE_HTTP_4XX or 500 <= status <= 599,
            count_failure=True,
        )
    if error_class == "malformed_spool_row":
        return ReplayClassification(retryable=False, count_failure=False)
    if error_class == "target_unresolved":
        return ReplayClassification(retryable=False, count_failure=True)
    if error_class.startswith("network_") or error_class.startswith("os_error_"):
        return ReplayClassification(retryable=True, count_failure=True)
    if error_class in _NON_RETRYABLE_REPLAY_ERRORS or error_class.startswith("status_"):
        return ReplayClassification(retryable=False, count_failure=True)
    if error_class.startswith("terminal_"):
        return ReplayClassification(retryable=False, count_failure=True)
    return ReplayClassification(retryable=False, count_failure=True)


def _is_retryable_replay_error(error_class: Optional[str]) -> bool:
    return classify_replay_error(error_class).retryable
@dataclass(frozen=True)
class SpoolOperation:
    sequence: int
    operation_key: str
    kind: str
    body: JsonObject
    track_key: Optional[str]
    action: Optional[str]
    receipt_operation_id: Optional[str]
    attempt_count: int
    last_error_class: Optional[str]
    next_attempt_at: float
    quarantined_at: Optional[float]
    quarantine_reason: Optional[str]
    settled_at: Optional[float] = None
    result_drawer_id: Optional[str] = None
    claim_token: Optional[str] = None
    claim_expires_at: Optional[float] = None


@dataclass(frozen=True)
class ReplayOutcome:
    operation: SpoolOperation
    completed: bool
    error_class: Optional[str] = None
    drawer_id: Optional[str] = None
    operation_id: Optional[str] = None
    quarantined: bool = False
    error_details: Optional[JsonObject] = None


class WriteSpoolReplay:
    def _metadata_operation(self, row: sqlite3.Row) -> SpoolOperation:
        """Build an operation from terminal metadata without decoding body."""

        def safe_int(value: Any) -> int:
            try:
                return int(value)
            except (TypeError, ValueError, OverflowError):
                return 0

        def safe_float(value: Any) -> Optional[float]:
            try:
                return float(value)
            except (TypeError, ValueError, OverflowError):
                return None

        return SpoolOperation(
            sequence=safe_int(row["sequence"]),
            operation_key=str(row["operation_key"]),
            kind=str(row["kind"]),
            body={},
            track_key=None,
            action=None,
            receipt_operation_id=row["receipt_operation_id"],
            attempt_count=safe_int(row["attempt_count"]),
            last_error_class=row["last_error_class"],
            next_attempt_at=safe_float(row["next_attempt_at"]) or 0.0,
            quarantined_at=safe_float(row["quarantined_at"]),
            quarantine_reason=row["quarantine_reason"],
            settled_at=safe_float(row["settled_at"]),
            result_drawer_id=row["result_drawer_id"],
            claim_token=row["claim_token"],
            claim_expires_at=safe_float(row["claim_expires_at"]),
        )

    def _terminal_replay_outcome(self, row: sqlite3.Row) -> ReplayOutcome:
        """Project a terminal row without decoding malformed body content."""
        operation = self._metadata_operation(row)
        if operation.settled_at is not None:
            return ReplayOutcome(
                operation,
                completed=True,
                drawer_id=operation.result_drawer_id,
                operation_id=operation.receipt_operation_id,
            )
        reason = operation.quarantine_reason or "malformed_spool_row"
        return ReplayOutcome(
            operation,
            completed=False,
            error_class=reason,
            operation_id=operation.receipt_operation_id,
            quarantined=True,
        )

    def _quarantine_keyed_row(
        self, operation_key: str, now: float
    ) -> ReplayOutcome:
        """Transactionally quarantine an undecodable row on the keyed path."""
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute(
                """
                UPDATE write_operations
                SET quarantined_at = ?2,
                    quarantine_reason = ?3,
                    claim_token = NULL,
                    claim_expires_at = NULL,
                    updated_at = ?2
                WHERE operation_key = ?1
                  AND settled_at IS NULL
                """,
                (operation_key, now, "malformed_spool_row"),
            )
            row = connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (operation_key,),
            ).fetchone()
            connection.execute("COMMIT")
        return self._terminal_replay_outcome(row)
    def replay_operation_key(
        self,
        operation_key: str,
        post: PostCallback,
        get: GetCallback,
        *,
        ignore_retry_delay: bool = False,
        replay_allowed: Optional[Callable[[], bool]] = None,
    ) -> Optional[ReplayOutcome]:
        """Claim one producer-owned operation without holding a Python lock."""
        row = self._select_row(operation_key)
        if row is None:
            return None
        if row["settled_at"] is not None or row["quarantined_at"] is not None:
            return self._terminal_replay_outcome(row)
        try:
            operation = self._row_to_operation(row)
        except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
            return self._quarantine_keyed_row(operation_key, time.time())
        if not ignore_retry_delay and operation.next_attempt_at > time.time():
            return ReplayOutcome(
                operation,
                completed=False,
                error_class=operation.last_error_class or "retry_not_due",
                operation_id=operation.receipt_operation_id,
            )
        if not self._fifo_available(operation_key):
            return ReplayOutcome(
                operation,
                completed=False,
                error_class="fifo_blocked",
                operation_id=operation.receipt_operation_id,
            )
        claimed = self._claim_operation(
            operation_key,
            ignore_retry_delay=ignore_retry_delay,
        )
        if claimed is None:
            current_row = self._select_row(operation_key)
            if current_row is None:
                return None
            if (
                current_row["settled_at"] is not None
                or current_row["quarantined_at"] is not None
            ):
                return self._terminal_replay_outcome(current_row)
            try:
                current = self._row_to_operation(current_row)
            except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
                return self._quarantine_keyed_row(operation_key, time.time())
            if current.claim_token and (
                current.claim_expires_at is not None
                and current.claim_expires_at > time.time()
            ):
                return ReplayOutcome(
                    current,
                    completed=False,
                    error_class="claim_busy",
                    operation_id=current.receipt_operation_id,
                )
            return ReplayOutcome(
                current,
                completed=False,
                error_class="fifo_blocked",
                operation_id=current.receipt_operation_id,
            )
        return self._replay_claimed_operation(
            claimed,
            post,
            get,
            replay_allowed=replay_allowed,
        )

    def _replay_operation(
        self,
        operation: SpoolOperation,
        post: PostCallback,
        get: GetCallback,
        *,
        replay_allowed: Optional[Callable[[], bool]] = None,
    ) -> ReplayOutcome:
        claimed = self._claim_operation(operation.operation_key, ignore_retry_delay=True)
        if claimed is None:
            current_row = self._select_row(operation.operation_key)
            if current_row is not None and (
                current_row["settled_at"] is not None
                or current_row["quarantined_at"] is not None
            ):
                return self._terminal_replay_outcome(current_row)
            if current_row is None:
                current = operation
            else:
                try:
                    current = self._row_to_operation(current_row)
                except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
                    return self._quarantine_keyed_row(
                        operation.operation_key, time.time()
                    )
            return ReplayOutcome(
                current,
                completed=False,
                error_class="claim_busy",
                operation_id=current.receipt_operation_id,
            )
        return self._replay_claimed_operation(
            claimed,
            post,
            get,
            replay_allowed=replay_allowed,
        )

    def _replay_claimed_operation(
        self,
        operation: SpoolOperation,
        post: PostCallback,
        get: GetCallback,
        *,
        replay_allowed: Optional[Callable[[], bool]] = None,
    ) -> ReplayOutcome:
        if operation.quarantined_at is not None:
            return ReplayOutcome(
                operation,
                completed=False,
                error_class=operation.quarantine_reason or "malformed_spool_row",
                quarantined=True,
            )
        claim_token = operation.claim_token
        if not claim_token:
            return ReplayOutcome(operation, completed=False, error_class="claim_lost")
        request = dict(operation.body)
        if operation.track_key and operation.action in {"replace", "delete"}:
            if not (operation.action == "replace" and request.get("replace_text")):
                target = self.drawer_for_track(operation.track_key)
                if not target:
                    try:
                        quarantined = self.record_attempt(
                            operation.operation_key,
                            "target_unresolved",
                            retryable=True,
                            claim_token=claim_token,
                        )
                    except ClaimLostError:
                        return ReplayOutcome(
                            operation,
                            completed=False,
                            error_class="claim_lost",
                        )
                    return ReplayOutcome(
                        operation,
                        completed=False,
                        error_class="target_unresolved",
                        quarantined=quarantined,
                    )
                request[
                    "supersedes" if operation.action == "replace" else "drawer_id"
                ] = target
        operation_id = operation.receipt_operation_id
        route = "/api/ingest/durable"
        try:
            if replay_allowed is not None and not replay_allowed():
                self.release_claim(operation.operation_key, claim_token)
                return ReplayOutcome(
                    operation,
                    completed=False,
                    error_class="breaker_open",
                    operation_id=operation_id,
                )
            if operation_id:
                route = f"/api/operations/{operation_id}"
                status = get(route)
            else:
                route = (
                    "/api/delete/durable"
                    if operation.kind == "delete"
                    else "/api/ingest/durable"
                )
                receipt = post(
                    route,
                    {
                        "idempotency_key": operation.operation_key,
                        "request": request,
                    },
                )
                if not isinstance(receipt, dict):
                    raise RuntimeError("durable admission returned an invalid receipt")
                operation_id = str(receipt.get("operation_id") or "")
                if not operation_id:
                    raise RuntimeError("durable admission omitted operation_id")
                self.record_receipt(operation.operation_key, operation_id, claim_token)
                route = f"/api/operations/{operation_id}"
                status = get(route)
            if not isinstance(status, dict):
                raise RuntimeError("durable status returned an invalid response")
            state = status.get("state")
            state = state if isinstance(state, str) else ""
            drawer_value = status.get("drawer_id")
            drawer_id = drawer_value if isinstance(drawer_value, str) else ""
            if state == "completed" and drawer_id:
                self.complete(
                    operation.operation_key,
                    track_key=operation.track_key,
                    drawer_id=drawer_id,
                    delete_track=operation.action == "delete",
                    claim_token=claim_token,
                )
                return ReplayOutcome(
                    operation,
                    completed=True,
                    drawer_id=drawer_id,
                    operation_id=operation_id,
                )
            if state == "completed":
                error_class = "status_completed_missing_drawer"
                retryable = False
            elif state in _TERMINAL_STATES:
                error_class = f"terminal_{state}"
                retryable = False
            elif state in _RETRYABLE_STATES:
                error_class = f"status_{state}"
                retryable = True
            else:
                error_class = "status_invalid"
                retryable = False
            quarantined = self.record_attempt(
                operation.operation_key,
                error_class,
                retryable=retryable,
                claim_token=claim_token,
            )
            return ReplayOutcome(
                operation,
                completed=False,
                error_class=error_class,
                operation_id=operation_id,
                quarantined=quarantined,
            )
        except ClaimLostError:
            return ReplayOutcome(
                operation,
                completed=False,
                error_class="claim_lost",
                operation_id=operation_id,
            )
        except Exception as exc:
            error_class = classify_write_error(exc)
            error_details = rest_error_payload(
                "Durable write replay failed.",
                route,
                exc,
            )["error_details"]
            classification = classify_replay_error(error_class)
            try:
                quarantined = self.record_attempt(
                    operation.operation_key,
                    error_class,
                    retryable=classification.retryable,
                    claim_token=claim_token,
                )
            except ClaimLostError:
                return ReplayOutcome(
                    operation,
                    completed=False,
                    error_class="claim_lost",
                    operation_id=operation_id,
                )
            return ReplayOutcome(
                operation,
                completed=False,
                error_class=error_class,
                operation_id=operation_id,
                quarantined=quarantined,
                error_details=error_details,
            )

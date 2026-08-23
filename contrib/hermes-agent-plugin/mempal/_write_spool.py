"""Crash-safe local write spool for Hermes mempal authoritative writes."""

from __future__ import annotations

import json
import os
import secrets
import sqlite3
import time
import urllib.error
from contextlib import closing
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional

from ._rest_errors import rest_error_payload
from ._write_spool_claims import WriteSpoolClaims

_DB_RELATIVE_PATH = os.path.join("state", "mempal", "write-spool.sqlite3")
_CONNECT_TIMEOUT_SECS = 0.5
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


def make_track_key(target: str, wing: str, project_id: Optional[str]) -> str:
    """Encode tracking scope as a typed, injective JSON tuple."""
    return json.dumps(
        ["track", target, wing, project_id],
        ensure_ascii=False,
        separators=(",", ":"),
    )


def classify_write_error(exc: Exception) -> str:
    """Return a content-free transport/storage failure class."""
    if isinstance(exc, urllib.error.HTTPError):
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
    if error_class.startswith("http_"):
        try:
            status = int(error_class.removeprefix("http_"))
        except ValueError:
            return ReplayClassification(retryable=False, count_failure=True)
        return ReplayClassification(
            retryable=status in _RETRYABLE_HTTP_4XX or 500 <= status <= 599,
            count_failure=True,
        )
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


class WriteSpool(WriteSpoolClaims):
    """Durable global-FIFO writes with SQLite claims before network I/O.

    A pending predecessor blocks every later operation in this shared spool.
    """

    def __init__(self, hermes_home: str) -> None:
        if not hermes_home:
            raise ValueError("hermes_home is required for durable mempal writes")
        self.path = os.path.abspath(os.path.join(hermes_home, _DB_RELATIVE_PATH))
        self._prepare_parent()
        self._initialize_schema()

    def _prepare_parent(self) -> None:
        parent = os.path.dirname(self.path)
        previous_umask = os.umask(0o077)
        try:
            os.makedirs(parent, mode=0o700, exist_ok=True)
        finally:
            os.umask(previous_umask)
        os.chmod(parent, 0o700)

    def _connect(self) -> sqlite3.Connection:
        previous_umask = os.umask(0o077)
        try:
            connection = sqlite3.connect(
                self.path,
                timeout=_CONNECT_TIMEOUT_SECS,
                isolation_level=None,
            )
        finally:
            os.umask(previous_umask)
        os.chmod(self.path, 0o600)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA busy_timeout = 500")
        connection.execute("PRAGMA journal_mode = WAL")
        connection.execute("PRAGMA synchronous = FULL")
        return connection

    def _chmod_sqlite_files(self) -> None:
        for path in (self.path, f"{self.path}-wal", f"{self.path}-shm"):
            if os.path.exists(path):
                os.chmod(path, 0o600)

    def admit(
        self,
        kind: str,
        body: JsonObject,
        *,
        track_key: Optional[str] = None,
        action: Optional[str] = None,
        operation_key: Optional[str] = None,
    ) -> SpoolOperation:
        key = secrets.token_urlsafe(32) if operation_key is None else operation_key
        encoded = json.dumps(
            body,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            try:
                cursor = connection.execute(
                    """
                    INSERT INTO write_operations (
                        operation_key, kind, body_json, track_key, action,
                        created_at, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                    """,
                    (key, kind, encoded, track_key, action, now),
                )
                sequence = int(cursor.lastrowid)
            except sqlite3.IntegrityError:
                connection.execute("ROLLBACK")
                existing = self.get(key)
                if existing is None:
                    raise
                existing_encoded = json.dumps(
                    existing.body,
                    ensure_ascii=False,
                    separators=(",", ":"),
                    sort_keys=True,
                )
                if (
                    existing.kind != kind
                    or existing_encoded != encoded
                    or existing.track_key != track_key
                    or existing.action != action
                ):
                    raise OperationKeyConflictError("operation_key_conflict")
                return existing
            connection.execute("COMMIT")
        self._chmod_sqlite_files()
        return SpoolOperation(
            sequence=sequence,
            operation_key=key,
            kind=kind,
            body=dict(body),
            track_key=track_key,
            action=action,
            receipt_operation_id=None,
            attempt_count=0,
            last_error_class=None,
            next_attempt_at=0.0,
            quarantined_at=None,
            quarantine_reason=None,
        )

    def next_operation(self) -> Optional[SpoolOperation]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                """
                SELECT * FROM write_operations
                WHERE settled_at IS NULL
                ORDER BY sequence LIMIT 1
                """
            ).fetchone()
        return self._row_to_operation(row) if row is not None else None

    def next_replayable_operation(self) -> Optional[SpoolOperation]:
        now = time.time()
        with closing(self._connect()) as connection:
            row = connection.execute(
                """
                SELECT *
                FROM write_operations AS candidate
                WHERE candidate.settled_at IS NULL
                  AND candidate.quarantined_at IS NULL
                  AND candidate.next_attempt_at <= ?1
                  AND (
                    candidate.claim_token IS NULL
                    OR candidate.claim_expires_at <= ?1
                  )
                  AND NOT EXISTS (
                    SELECT 1
                    FROM write_operations AS earlier
                    WHERE earlier.sequence < candidate.sequence
                      AND earlier.settled_at IS NULL
                      AND earlier.quarantined_at IS NULL
                  )
                ORDER BY candidate.sequence
                LIMIT 1
                """,
                (now,),
            ).fetchone()
        return self._row_to_operation(row) if row is not None else None

    def get(self, operation_key: str) -> Optional[SpoolOperation]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (operation_key,),
            ).fetchone()
        return self._row_to_operation(row) if row is not None else None

    def count(self) -> int:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT COUNT(*) FROM write_operations WHERE settled_at IS NULL"
            ).fetchone()
        return int(row[0]) if row is not None else 0

    def record_receipt(
        self, operation_key: str, operation_id: str, claim_token: str
    ) -> None:
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            updated = connection.execute(
                """
                UPDATE write_operations
                SET receipt_operation_id = ?2, updated_at = ?3
                WHERE operation_key = ?1
                  AND claim_token = ?4
                  AND claim_expires_at > ?3
                  AND settled_at IS NULL
                """,
                (operation_key, operation_id, now, claim_token),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                raise ClaimLostError("durable spool claim lost")
            connection.execute("COMMIT")

    def record_attempt(
        self,
        operation_key: str,
        error_class: Optional[str],
        *,
        retryable: bool = True,
        claim_token: str,
    ) -> bool:
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute(
                """
                SELECT attempt_count
                FROM write_operations
                WHERE operation_key = ?1
                  AND claim_token = ?2
                  AND claim_expires_at > ?3
                  AND settled_at IS NULL
                """,
                (operation_key, claim_token, now),
            ).fetchone()
            if row is None:
                connection.execute("ROLLBACK")
                raise ClaimLostError("durable spool claim lost")
            next_count = int(row["attempt_count"]) + 1
            delay = _RETRY_BACKOFF_SECS[
                min(next_count - 1, len(_RETRY_BACKOFF_SECS) - 1)
            ]
            quarantined = (not retryable) or (
                error_class == "target_unresolved" and next_count >= _MAX_REPLAY_ATTEMPTS
            )
            updated = connection.execute(
                """
                UPDATE write_operations
                SET attempt_count = ?2,
                    last_error_class = ?3,
                    next_attempt_at = ?4,
                    quarantined_at = CASE
                        WHEN ?5 THEN ?6
                        ELSE quarantined_at
                    END,
                    quarantine_reason = CASE
                        WHEN ?5 THEN ?3
                        ELSE quarantine_reason
                    END,
                    claim_token = NULL,
                    claim_expires_at = NULL,
                    updated_at = ?6
                WHERE operation_key = ?1
                  AND claim_token = ?7
                  AND claim_expires_at > ?6
                  AND settled_at IS NULL
                """,
                (
                    operation_key,
                    next_count,
                    error_class,
                    now + delay,
                    quarantined,
                    now,
                    claim_token,
                ),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                raise ClaimLostError("durable spool claim lost")
            connection.execute("COMMIT")
        return quarantined

    def replace_body(self, operation_key: str, body: JsonObject) -> None:
        """Atomically refine an admitted operation before delivery begins."""
        encoded = json.dumps(
            body,
            ensure_ascii=False,
            separators=(",", ":"),
            sort_keys=True,
        )
        self._update_operation(
            operation_key,
            "body_json = ?2, updated_at = ?3",
            (encoded, time.time()),
        )

    def complete(
        self,
        operation_key: str,
        *,
        track_key: Optional[str] = None,
        drawer_id: Optional[str] = None,
        delete_track: bool = False,
        claim_token: Optional[str] = None,
    ) -> None:
        if claim_token is None:
            claimed = self._claim_operation(operation_key, ignore_retry_delay=True)
            if claimed is None or claimed.claim_token is None:
                raise ClaimLostError("durable spool claim unavailable")
            claim_token = claimed.claim_token
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            updated = connection.execute(
                """
                UPDATE write_operations
                SET settled_at = ?2,
                    result_drawer_id = ?3,
                    claim_token = NULL,
                    claim_expires_at = NULL,
                    updated_at = ?2
                WHERE operation_key = ?1
                  AND claim_token = ?4
                  AND claim_expires_at > ?2
                  AND settled_at IS NULL
                """,
                (operation_key, now, drawer_id, claim_token),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                raise ClaimLostError("durable spool claim lost")
            if track_key and drawer_id:
                connection.execute(
                    """
                    INSERT INTO track_drawers(track_key, drawer_id, updated_at)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT(track_key) DO UPDATE SET
                        drawer_id = excluded.drawer_id,
                        updated_at = excluded.updated_at
                    """,
                    (track_key, drawer_id, now),
                )
            elif track_key and delete_track:
                connection.execute(
                    "DELETE FROM track_drawers WHERE track_key = ?1", (track_key,)
                )
            connection.execute("COMMIT")

    def drawer_for_track(self, track_key: str) -> Optional[str]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT drawer_id FROM track_drawers WHERE track_key = ?1",
                (track_key,),
            ).fetchone()
        return str(row[0]) if row is not None else None

    def replay_one(
        self,
        post: PostCallback,
        get: GetCallback,
        *,
        replay_allowed: Optional[Callable[[], bool]] = None,
    ) -> Optional[ReplayOutcome]:
        """Claim the earliest global-FIFO operation before network I/O."""
        operation = self._claim_next()
        if operation is None:
            return None
        return self._replay_claimed_operation(
            operation,
            post,
            get,
            replay_allowed=replay_allowed,
        )

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
        operation = self.get(operation_key)
        if operation is None:
            return None
        if operation.settled_at is not None:
            return self._settled_outcome(operation)
        if operation.quarantined_at is not None:
            return ReplayOutcome(
                operation,
                completed=False,
                error_class=operation.quarantine_reason or operation.last_error_class,
                operation_id=operation.receipt_operation_id,
                quarantined=True,
            )
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
            current = self.get(operation_key)
            if current is None:
                return None
            if current.settled_at is not None:
                return self._settled_outcome(current)
            if current.quarantined_at is not None:
                return ReplayOutcome(
                    current,
                    completed=False,
                    error_class=current.quarantine_reason or current.last_error_class,
                    operation_id=current.receipt_operation_id,
                    quarantined=True,
                )
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
            current = self.get(operation.operation_key) or operation
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

    @staticmethod
    def _settled_outcome(operation: SpoolOperation) -> ReplayOutcome:
        return ReplayOutcome(
            operation,
            completed=operation.settled_at is not None,
            drawer_id=operation.result_drawer_id,
            operation_id=operation.receipt_operation_id,
            quarantined=False,
        )

    def _update_operation(
        self, operation_key: str, assignment: str, values: tuple[Any, ...]
    ) -> None:
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            updated = connection.execute(
                f"UPDATE write_operations SET {assignment} WHERE operation_key = ?1",
                (operation_key, *values),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                raise KeyError("spool operation is no longer present")
            connection.execute("COMMIT")

    @staticmethod
    def _row_to_operation(row: sqlite3.Row) -> SpoolOperation:
        body = json.loads(str(row["body_json"]))
        if not isinstance(body, dict):
            raise sqlite3.DatabaseError("spool operation body is not a JSON object")
        return SpoolOperation(
            sequence=int(row["sequence"]),
            operation_key=str(row["operation_key"]),
            kind=str(row["kind"]),
            body=body,
            track_key=row["track_key"],
            action=row["action"],
            receipt_operation_id=row["receipt_operation_id"],
            attempt_count=int(row["attempt_count"]),
            last_error_class=row["last_error_class"],
            next_attempt_at=float(row["next_attempt_at"]),
            quarantined_at=(
                None if row["quarantined_at"] is None else float(row["quarantined_at"])
            ),
            quarantine_reason=row["quarantine_reason"],
            settled_at=(None if row["settled_at"] is None else float(row["settled_at"])),
            result_drawer_id=row["result_drawer_id"],
            claim_token=row["claim_token"],
            claim_expires_at=(
                None
                if row["claim_expires_at"] is None
                else float(row["claim_expires_at"])
            ),
        )

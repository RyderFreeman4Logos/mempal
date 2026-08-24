"""Crash-safe local write spool for Hermes mempal authoritative writes."""

from __future__ import annotations

import json
import os
import secrets
import sqlite3
import time
from contextlib import closing
from typing import Callable, Optional

from ._write_spool_claims import WriteSpoolClaims
from ._write_spool_replay import (
    ClaimLostError,
    GetCallback,
    JsonObject,
    OperationKeyConflictError,
    PostCallback,
    ReplayOutcome,
    SpoolOperation,
    WriteSpoolReplay,
    _MAX_REPLAY_ATTEMPTS,
    _RETRY_BACKOFF_SECS,
    classify_replay_error,
    classify_write_error,
    legacy_track_key,
)

_DB_RELATIVE_PATH = os.path.join("state", "mempal", "write-spool.sqlite3")
_CONNECT_TIMEOUT_SECS = 0.5
_MAX_FINITE_BOUND = 1e300
_CONTROL_TOKEN_MAX_BYTES = 128


def valid_control_token(value: object, *, allow_none: bool = True) -> bool:
    if value is None:
        return allow_none
    return (
        isinstance(value, str)
        and bool(value)
        and len(value) <= _CONTROL_TOKEN_MAX_BYTES
        and all(char.isascii() and char.isprintable() and not char.isspace() for char in value)
    )


def valid_target(value: object) -> bool:
    return valid_control_token(value, allow_none=False)


def make_track_key(target: str, wing: str, project_id: Optional[str]) -> str:
    """Encode tracking scope as a typed, injective JSON tuple."""
    return json.dumps(
        ["track", target, wing, project_id],
        ensure_ascii=False,
        separators=(",", ":"),
    )


class WriteSpool(WriteSpoolClaims, WriteSpoolReplay):
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
                try:
                    existing = self.get(key)
                except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
                    # Existing row body is not decodable (malformed).  Read
                    # terminal metadata without decoding it; a metadata match
                    # keeps the retry on the same operation so the keyed
                    # replay can project the terminal malformed row, instead
                    # of masking it as an operation-key conflict.
                    row = self._select_row(key)
                    if row is None:
                        raise
                    if (
                        str(row["kind"]) != kind
                        or row["action"] != action
                        or row["track_key"] != track_key
                    ):
                        raise OperationKeyConflictError("operation_key_conflict")
                    return self._metadata_operation(row)
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
                  AND (
                    candidate.next_attempt_at <= ?1
                    OR candidate.next_attempt_at IS NULL
                    OR typeof(candidate.next_attempt_at) NOT IN ('real', 'integer')
                    OR abs(candidate.next_attempt_at) > ?2
                  )
                  AND (
                    candidate.claim_token IS NULL
                    OR candidate.claim_expires_at <= ?1
                    OR candidate.claim_expires_at IS NULL
                    OR typeof(candidate.claim_expires_at) NOT IN ('real', 'integer')
                    OR abs(candidate.claim_expires_at) > ?2
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
                (now, _MAX_FINITE_BOUND),
            ).fetchone()
        return self._row_to_operation(row) if row is not None else None

    def get(self, operation_key: str) -> Optional[SpoolOperation]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (operation_key,),
            ).fetchone()
        return self._row_to_operation(row) if row is not None else None

    def _select_row(self, operation_key: str) -> Optional[sqlite3.Row]:
        with closing(self._connect()) as connection:
            return connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (operation_key,),
            ).fetchone()

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
            if track_key and delete_track:
                connection.execute(
                    "DELETE FROM track_drawers WHERE track_key = ?1", (track_key,)
                )
                legacy = legacy_track_key(track_key)
                if legacy is not None:
                    connection.execute(
                        "DELETE FROM track_drawers WHERE track_key = ?1", (legacy,)
                    )
            elif track_key and drawer_id:
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
            connection.execute("COMMIT")

    def drawer_for_track(self, track_key: str) -> Optional[str]:
        drawer = self._drawer_id_for_key(track_key)
        if drawer is not None:
            return drawer
        legacy = legacy_track_key(track_key)
        if legacy is not None:
            return self._drawer_id_for_key(legacy)
        return None

    def _drawer_id_for_key(self, key: str) -> Optional[str]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT drawer_id FROM track_drawers WHERE track_key = ?1",
                (key,),
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

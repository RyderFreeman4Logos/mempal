"""Crash-safe local write spool for the Hermes mempal provider.

The spool owns evidence until mempal's durable receipt reaches a completed
terminal state. Network delivery is deliberately outside this module so
callbacks only wait for a bounded local SQLite transaction.
"""

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


_DB_RELATIVE_PATH = os.path.join("state", "mempal", "write-spool.sqlite3")
_CONNECT_TIMEOUT_SECS = 0.5


@dataclass(frozen=True)
class SpoolOperation:
    sequence: int
    operation_key: str
    kind: str
    body: Dict[str, Any]
    track_key: Optional[str]
    action: Optional[str]
    receipt_operation_id: Optional[str]
    attempt_count: int
    last_error_class: Optional[str]


@dataclass(frozen=True)
class ReplayOutcome:
    operation: SpoolOperation
    completed: bool
    error_class: Optional[str] = None


class WriteSpool:
    """Durable FIFO of provider writes with persistent target lineage."""

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

    def _initialize_schema(self) -> None:
        with closing(self._connect()) as connection:
            connection.executescript(
                """
                BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS write_operations (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    operation_key TEXT NOT NULL UNIQUE,
                    kind TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    track_key TEXT,
                    action TEXT,
                    receipt_operation_id TEXT,
                    attempt_count INTEGER NOT NULL DEFAULT 0,
                    last_error_class TEXT,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_write_operations_fifo
                    ON write_operations(sequence);
                CREATE TABLE IF NOT EXISTS track_drawers (
                    track_key TEXT PRIMARY KEY,
                    drawer_id TEXT NOT NULL,
                    updated_at REAL NOT NULL
                );
                COMMIT;
                """
            )
        self._chmod_sqlite_files()

    def _chmod_sqlite_files(self) -> None:
        for path in (self.path, f"{self.path}-wal", f"{self.path}-shm"):
            if os.path.exists(path):
                os.chmod(path, 0o600)

    def admit(
        self,
        kind: str,
        body: Dict[str, Any],
        *,
        track_key: Optional[str] = None,
        action: Optional[str] = None,
    ) -> SpoolOperation:
        operation_key = secrets.token_urlsafe(32)
        encoded = json.dumps(body, ensure_ascii=False, separators=(",", ":"))
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            cursor = connection.execute(
                """
                INSERT INTO write_operations (
                    operation_key, kind, body_json, track_key, action,
                    created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                """,
                (operation_key, kind, encoded, track_key, action, now),
            )
            sequence = int(cursor.lastrowid)
            connection.execute("COMMIT")
        self._chmod_sqlite_files()
        return SpoolOperation(
            sequence=sequence,
            operation_key=operation_key,
            kind=kind,
            body=dict(body),
            track_key=track_key,
            action=action,
            receipt_operation_id=None,
            attempt_count=0,
            last_error_class=None,
        )

    def next_operation(self) -> Optional[SpoolOperation]:
        with closing(self._connect()) as connection:
            row = connection.execute(
                "SELECT * FROM write_operations ORDER BY sequence LIMIT 1"
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
            row = connection.execute("SELECT COUNT(*) FROM write_operations").fetchone()
        return int(row[0]) if row is not None else 0

    def record_receipt(self, operation_key: str, operation_id: str) -> None:
        self._update_operation(
            operation_key,
            "receipt_operation_id = ?2, updated_at = ?3",
            (operation_id, time.time()),
        )

    def record_attempt(self, operation_key: str, error_class: Optional[str]) -> None:
        self._update_operation(
            operation_key,
            "attempt_count = attempt_count + 1, last_error_class = ?2, updated_at = ?3",
            (error_class, time.time()),
        )

    def complete(
        self,
        operation_key: str,
        *,
        track_key: Optional[str] = None,
        drawer_id: Optional[str] = None,
        delete_track: bool = False,
    ) -> None:
        now = time.time()
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
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
            deleted = connection.execute(
                "DELETE FROM write_operations WHERE operation_key = ?1",
                (operation_key,),
            ).rowcount
            if deleted != 1:
                connection.execute("ROLLBACK")
                raise KeyError("spool operation is no longer present")
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
        post: Callable[[str, Dict[str, Any]], Any],
        get: Callable[[str], Any],
    ) -> Optional[ReplayOutcome]:
        """Attempt the oldest operation without surrendering it on ambiguity."""
        operation = self.next_operation()
        if operation is None:
            return None
        try:
            operation_id = operation.receipt_operation_id
            if operation_id:
                status = get(f"/api/operations/{operation_id}")
            else:
                receipt = post(
                    "/api/ingest/durable",
                    {
                        "idempotency_key": operation.operation_key,
                        "request": operation.body,
                    },
                )
                if not isinstance(receipt, dict):
                    raise RuntimeError("durable admission returned an invalid receipt")
                operation_id = str(receipt.get("operation_id") or "")
                if not operation_id:
                    raise RuntimeError("durable admission omitted operation_id")
                self.record_receipt(operation.operation_key, operation_id)
                status = get(f"/api/operations/{operation_id}")
            if not isinstance(status, dict):
                raise RuntimeError("durable status returned an invalid response")
            state = str(status.get("state") or "")
            drawer_id = str(status.get("drawer_id") or "")
            if state == "completed" and drawer_id:
                self.complete(operation.operation_key)
                return ReplayOutcome(operation, completed=True)
            error_class = f"terminal_{state}" if state in {"failed", "rejected"} else None
            if error_class:
                self.record_attempt(operation.operation_key, error_class)
            return ReplayOutcome(operation, completed=False, error_class=error_class)
        except Exception as exc:
            error_class = self._error_class(exc)
            self.record_attempt(operation.operation_key, error_class)
            return ReplayOutcome(operation, completed=False, error_class=error_class)

    @staticmethod
    def _error_class(exc: Exception) -> str:
        if isinstance(exc, urllib.error.HTTPError):
            return f"http_{exc.code}"
        if isinstance(exc, (TimeoutError, urllib.error.URLError)):
            return "network_timeout"
        if isinstance(exc, OSError):
            return f"os_error_{exc.errno or 'unknown'}"
        return "network_or_protocol_error"

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
        )

"""SQLite schema and single-owner claim lease mechanics for the write spool."""

from __future__ import annotations

import json
import math
import secrets
import sqlite3
import time
from contextlib import closing
from typing import TYPE_CHECKING, Any, Optional, Protocol

if TYPE_CHECKING:
    from ._write_spool import ReplayOutcome, SpoolOperation

_CLAIM_LEASE_SECS = 30.0
_MAX_FINITE_BOUND = 1e300

__all__ = ["WriteSpoolClaims"]


class _SpoolOwner(Protocol):
    path: str

    def _connect(self) -> sqlite3.Connection: ...

    def _chmod_sqlite_files(self) -> None: ...

    def _migrate_schema(self, connection: sqlite3.Connection) -> None: ...

    def _claim_candidate(
        self,
        operation_key: Optional[str],
        *,
        ignore_retry_delay: bool,
    ) -> Optional[SpoolOperation]: ...

class WriteSpoolClaims:
    """Durable schema migration and claim operations kept outside the spool core."""

    @staticmethod
    def _row_to_operation(row: sqlite3.Row) -> SpoolOperation:
        from ._write_spool import SpoolOperation

        body = json.loads(str(row["body_json"]))
        if not isinstance(body, dict):
            raise sqlite3.DatabaseError("spool operation body is not a JSON object")

        def finite(value: Any, column: str) -> float:
            parsed = float(value)
            if not math.isfinite(parsed):
                raise ValueError(f"spool {column} is not finite")
            return parsed

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
            next_attempt_at=finite(row["next_attempt_at"], "next_attempt_at"),
            quarantined_at=(
                None
                if row["quarantined_at"] is None
                else finite(row["quarantined_at"], "quarantined_at")
            ),
            quarantine_reason=row["quarantine_reason"],
            settled_at=(
                None
                if row["settled_at"] is None
                else finite(row["settled_at"], "settled_at")
            ),
            result_drawer_id=row["result_drawer_id"],
            claim_token=row["claim_token"],
            claim_expires_at=(
                None
                if row["claim_expires_at"] is None
                else finite(row["claim_expires_at"], "claim_expires_at")
            ),
        )

    @staticmethod
    def _quarantined_row_to_operation(
        row: sqlite3.Row, now: float, reason: str
    ) -> SpoolOperation:
        from ._write_spool import SpoolOperation

        def safe_int(value: Any, default: int = 0) -> int:
            try:
                return int(value)
            except (TypeError, ValueError, OverflowError):
                return default

        def safe_float(value: Any, default: float = 0.0) -> float:
            try:
                return float(value)
            except (TypeError, ValueError, OverflowError):
                return default

        return SpoolOperation(
            sequence=safe_int(row["sequence"]),
            operation_key=str(row["operation_key"]),
            kind=str(row["kind"]),
            body={},
            track_key=None,
            action=None,
            receipt_operation_id=None,
            attempt_count=safe_int(row["attempt_count"]),
            last_error_class=None,
            next_attempt_at=safe_float(row["next_attempt_at"], now),
            quarantined_at=now,
            quarantine_reason=reason,
            settled_at=None,
            result_drawer_id=None,
            claim_token=None,
            claim_expires_at=None,
        )

    @staticmethod
    def _settled_outcome(operation: SpoolOperation) -> ReplayOutcome:
        from ._write_spool import ReplayOutcome

        return ReplayOutcome(
            operation,
            completed=operation.settled_at is not None,
            drawer_id=operation.result_drawer_id,
            operation_id=operation.receipt_operation_id,
            quarantined=False,
        )

    def _update_operation(
        self: _SpoolOwner,
        operation_key: str,
        assignment: str,
        values: tuple[Any, ...],
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

    def _initialize_schema(self: _SpoolOwner) -> None:
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
                    next_attempt_at REAL NOT NULL DEFAULT 0,
                    quarantined_at REAL,
                    quarantine_reason TEXT,
                    settled_at REAL,
                    result_drawer_id TEXT,
                    claim_token TEXT,
                    claim_expires_at REAL,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS track_drawers (
                    track_key TEXT PRIMARY KEY,
                    drawer_id TEXT NOT NULL,
                    updated_at REAL NOT NULL
                );
                COMMIT;
                """
            )
            self._migrate_schema(connection)
        self._chmod_sqlite_files()

    def _migrate_schema(
        self: _SpoolOwner, connection: sqlite3.Connection
    ) -> None:
        columns = {
            str(row["name"])
            for row in connection.execute("PRAGMA table_info(write_operations)")
        }
        required = {
            "sequence",
            "operation_key",
            "kind",
            "body_json",
            "track_key",
            "action",
            "receipt_operation_id",
            "attempt_count",
            "last_error_class",
            "created_at",
            "updated_at",
        }
        if required - columns:
            raise sqlite3.DatabaseError("unsupported durable spool schema")
        additions = (
            ("next_attempt_at", "REAL NOT NULL DEFAULT 0"),
            ("quarantined_at", "REAL"),
            ("quarantine_reason", "TEXT"),
            ("settled_at", "REAL"),
            ("result_drawer_id", "TEXT"),
            ("claim_token", "TEXT"),
            ("claim_expires_at", "REAL"),
        )
        connection.execute("BEGIN IMMEDIATE")
        try:
            for name, definition in additions:
                if name not in columns:
                    connection.execute(
                        f"ALTER TABLE write_operations ADD COLUMN {name} {definition}"
                    )
            for index_name in (
                "idx_write_operations_fifo",
                "idx_write_operations_replay",
                "idx_write_operations_track",
                "idx_write_operations_claim",
            ):
                connection.execute(f"DROP INDEX IF EXISTS {index_name}")
            for index_sql in (
                """
                CREATE INDEX IF NOT EXISTS idx_write_operations_fifo
                    ON write_operations(settled_at, quarantined_at, sequence)
                """,
                """
                CREATE INDEX IF NOT EXISTS idx_write_operations_replay
                    ON write_operations(quarantined_at, next_attempt_at, sequence)
                """,
                """
                CREATE INDEX IF NOT EXISTS idx_write_operations_track
                    ON write_operations(track_key, sequence)
                """,
                """
                CREATE INDEX IF NOT EXISTS idx_write_operations_claim
                    ON write_operations(claim_expires_at, sequence)
                """,
            ):
                connection.execute(index_sql)
            connection.execute("COMMIT")
        except Exception:
            connection.execute("ROLLBACK")
            raise

    def release_claim(
        self: _SpoolOwner, operation_key: str, claim_token: str
    ) -> bool:
        """Release a pre-network claim without changing durable retry state."""
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            updated = connection.execute(
                """
                UPDATE write_operations
                SET claim_token = NULL,
                    claim_expires_at = NULL,
                    updated_at = ?3
                WHERE operation_key = ?1
                  AND claim_token = ?2
                  AND settled_at IS NULL
                """,
                (operation_key, claim_token, time.time()),
            ).rowcount
            connection.execute("COMMIT")
        return updated == 1

    def _claim_next(self: _SpoolOwner) -> Optional[SpoolOperation]:
        return self._claim_candidate(None, ignore_retry_delay=False)

    def _claim_operation(
        self: _SpoolOwner, operation_key: str, *, ignore_retry_delay: bool
    ) -> Optional[SpoolOperation]:
        return self._claim_candidate(operation_key, ignore_retry_delay=ignore_retry_delay)

    def _claim_candidate(
        self: _SpoolOwner,
        operation_key: Optional[str],
        *,
        ignore_retry_delay: bool,
    ) -> Optional[SpoolOperation]:
        now = time.time()
        claim_token = secrets.token_urlsafe(24)
        claim_expires_at = now + _CLAIM_LEASE_SECS
        where = [
            "candidate.settled_at IS NULL",
            "candidate.quarantined_at IS NULL",
            "("
            "candidate.claim_token IS NULL "
            "OR candidate.claim_expires_at <= ? "
            "OR candidate.claim_expires_at IS NULL "
            "OR typeof(candidate.claim_expires_at) NOT IN ('real', 'integer') "
            "OR abs(candidate.claim_expires_at) > ?"
            ")",
            "NOT EXISTS (SELECT 1 FROM write_operations AS earlier "
            "WHERE earlier.sequence < candidate.sequence "
            "AND earlier.settled_at IS NULL AND earlier.quarantined_at IS NULL)",
        ]
        params: list[object] = [now, _MAX_FINITE_BOUND]
        if not ignore_retry_delay:
            where.append(
                "("
                "candidate.next_attempt_at <= ? "
                "OR candidate.next_attempt_at IS NULL "
                "OR typeof(candidate.next_attempt_at) NOT IN ('real', 'integer') "
                "OR abs(candidate.next_attempt_at) > ?"
                ")"
            )
            params.append(now)
            params.append(_MAX_FINITE_BOUND)
        if operation_key is not None:
            where.append("candidate.operation_key = ?")
            params.append(operation_key)
        query = (
            "SELECT candidate.* FROM write_operations AS candidate WHERE "
            + " AND ".join(where)
            + " ORDER BY candidate.sequence LIMIT 1"
        )
        with closing(self._connect()) as connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute(query, tuple(params)).fetchone()
            if row is None:
                connection.execute("COMMIT")
                return None
            try:
                operation = WriteSpoolClaims._row_to_operation(row)
            except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
                # Decode before the claim UPDATE: claiming overwrites
                # claim_expires_at with a fresh finite lease, which would
                # otherwise mask a corrupt persisted value. Validate the
                # untouched candidate row first and quarantine in place.
                reason = "malformed_spool_row"
                connection.execute(
                    """
                    UPDATE write_operations
                    SET quarantined_at = ?2,
                        quarantine_reason = ?3,
                        claim_token = NULL,
                        claim_expires_at = NULL,
                        updated_at = ?2
                    WHERE operation_key = ?1 AND settled_at IS NULL
                    """,
                    (str(row["operation_key"]), now, reason),
                )
                connection.execute("COMMIT")
                return WriteSpoolClaims._quarantined_row_to_operation(
                    row, now, reason
                )
            updated = connection.execute(
                """
                UPDATE write_operations
                SET claim_token = ?2,
                    claim_expires_at = ?3,
                    updated_at = ?3
                WHERE operation_key = ?1
                  AND settled_at IS NULL
                  AND quarantined_at IS NULL
                  AND (
                    claim_token IS NULL
                    OR claim_expires_at <= ?3
                    OR claim_expires_at IS NULL
                    OR typeof(claim_expires_at) NOT IN ('real', 'integer')
                    OR abs(claim_expires_at) > ?5
                  )
                """,
                (
                    str(row["operation_key"]),
                    claim_token,
                    claim_expires_at,
                    _MAX_FINITE_BOUND,
                    _MAX_FINITE_BOUND,
                ),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                return None
            claimed = connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (str(row["operation_key"]),),
            ).fetchone()
            if claimed is None:
                raise sqlite3.DatabaseError("claimed spool operation disappeared")
            try:
                operation = WriteSpoolClaims._row_to_operation(claimed)
            except (ValueError, TypeError, OverflowError, sqlite3.DatabaseError):
                reason = "malformed_spool_row"
                connection.execute(
                    """
                    UPDATE write_operations
                    SET quarantined_at = ?2,
                        quarantine_reason = ?3,
                        claim_token = NULL,
                        claim_expires_at = NULL,
                        updated_at = ?2
                    WHERE operation_key = ?1 AND claim_token = ?4 AND settled_at IS NULL
                    """,
                    (str(row["operation_key"]), now, reason, claim_token),
                )
                operation = WriteSpoolClaims._quarantined_row_to_operation(
                    claimed, now, reason
                )
            connection.execute("COMMIT")
        return operation

    def _fifo_available(self: _SpoolOwner, operation_key: str) -> bool:
        with closing(self._connect()) as connection:
            row = connection.execute(
                """
                SELECT 1
                FROM write_operations AS candidate
                WHERE candidate.operation_key = ?1
                  AND candidate.settled_at IS NULL
                  AND candidate.quarantined_at IS NULL
                  AND NOT EXISTS (
                    SELECT 1
                    FROM write_operations AS earlier
                    WHERE earlier.sequence < candidate.sequence
                      AND earlier.settled_at IS NULL
                      AND earlier.quarantined_at IS NULL
                  )
                """,
                (operation_key,),
            ).fetchone()
        return row is not None

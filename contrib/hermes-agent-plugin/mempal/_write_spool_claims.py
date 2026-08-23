"""SQLite schema and single-owner claim lease mechanics for the write spool."""

from __future__ import annotations

import secrets
import sqlite3
import time
from contextlib import closing
from typing import TYPE_CHECKING, Optional, Protocol

if TYPE_CHECKING:
    from ._write_spool import SpoolOperation

_CLAIM_LEASE_SECS = 30.0


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

    @staticmethod
    def _row_to_operation(row: sqlite3.Row) -> SpoolOperation: ...


class WriteSpoolClaims:
    """Durable schema migration and claim operations kept outside the spool core."""

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
                CREATE INDEX IF NOT EXISTS idx_write_operations_fifo
                    ON write_operations(settled_at, quarantined_at, sequence);
                CREATE INDEX IF NOT EXISTS idx_write_operations_replay
                    ON write_operations(quarantined_at, next_attempt_at, sequence);
                CREATE INDEX IF NOT EXISTS idx_write_operations_track
                    ON write_operations(track_key, sequence);
                CREATE INDEX IF NOT EXISTS idx_write_operations_claim
                    ON write_operations(claim_expires_at, sequence);
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
            "next_attempt_at",
            "quarantined_at",
            "quarantine_reason",
            "created_at",
            "updated_at",
        }
        if required - columns:
            raise sqlite3.DatabaseError("unsupported durable spool schema")
        additions = (
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
            "(candidate.claim_token IS NULL OR candidate.claim_expires_at <= ?)",
            "NOT EXISTS (SELECT 1 FROM write_operations AS earlier "
            "WHERE earlier.sequence < candidate.sequence "
            "AND earlier.settled_at IS NULL AND earlier.quarantined_at IS NULL)",
        ]
        params: list[object] = [now]
        if not ignore_retry_delay:
            where.append("candidate.next_attempt_at <= ?")
            params.append(now)
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
            updated = connection.execute(
                """
                UPDATE write_operations
                SET claim_token = ?2,
                    claim_expires_at = ?3,
                    updated_at = ?3
                WHERE operation_key = ?1
                  AND settled_at IS NULL
                  AND quarantined_at IS NULL
                  AND (claim_token IS NULL OR claim_expires_at <= ?3)
                """,
                (str(row["operation_key"]), claim_token, claim_expires_at),
            ).rowcount
            if updated != 1:
                connection.execute("ROLLBACK")
                return None
            claimed = connection.execute(
                "SELECT * FROM write_operations WHERE operation_key = ?1",
                (str(row["operation_key"]),),
            ).fetchone()
            connection.execute("COMMIT")
        if claimed is None:
            raise sqlite3.DatabaseError("claimed spool operation disappeared")
        return self._row_to_operation(claimed)

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

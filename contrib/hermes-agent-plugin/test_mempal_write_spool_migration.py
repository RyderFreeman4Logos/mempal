import os
import sqlite3
import tempfile
import unittest
from typing import Any, List

from mempal._write_spool import WriteSpool


class DeleteLineageTests(unittest.TestCase):
    def test_delete_completion_removes_mapping_even_with_drawer_id(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            track_key = "track-key"
            added = spool.admit(
                "ingest", {"content": "original"}, track_key=track_key, action="add"
            )
            spool.complete(added.operation_key, track_key=track_key, drawer_id="drawer-A")
            removed = spool.admit("delete", {}, track_key=track_key, action="delete")
            spool.complete(
                removed.operation_key,
                track_key=track_key,
                drawer_id="drawer-A",
                delete_track=True,
            )

            self.assertIsNone(spool.drawer_for_track(track_key))
            later_remove = spool.admit("delete", {}, track_key=track_key, action="delete")
            outcome = spool.replay_operation_key(
                later_remove.operation_key,
                lambda _path, _body: self.fail("deleted mapping was reused"),
                lambda _path: {"state": "completed", "drawer_id": "drawer-B"},
                ignore_retry_delay=True,
            )

            self.assertIsNotNone(outcome)
            assert outcome is not None
            self.assertEqual(outcome.error_class, "target_unresolved")
            self.assertIsNone(spool.drawer_for_track(track_key))


class DurableSpoolReplayMigrationTests(unittest.TestCase):
    def test_malformed_fifo_head_is_quarantined_and_tail_replays(self) -> None:
        corruptions = [
            ("body_json", "{"),
            ("body_json", "[]"),
            ("attempt_count", "not-a-number"),
        ]
        for column, value in corruptions:
            with self.subTest(column=column, value_type=type(value).__name__):
                with tempfile.TemporaryDirectory() as hermes_home:
                    spool = WriteSpool(hermes_home)
                    head = spool.admit(
                        "ingest", {"content": "head", "wing": "wing", "room": "facts"}
                    )
                    tail = spool.admit(
                        "ingest", {"content": "tail", "wing": "wing", "room": "facts"}
                    )
                    connection = sqlite3.connect(spool.path)
                    try:
                        connection.execute(
                            f"UPDATE write_operations SET {column} = ? WHERE operation_key = ?",
                            (value, head.operation_key),
                        )
                        connection.commit()
                    finally:
                        connection.close()
                    restarted = WriteSpool(hermes_home)

                    calls: List[Any] = []
                    post = lambda _path, body: calls.append(body) or {
                        "operation_id": "remote-tail",
                        "state": "completed",
                        "drawer_id": "drawer-tail",
                    }
                    get = lambda _path: {
                        "state": "completed",
                        "drawer_id": "drawer-tail",
                    }

                    quarantined = restarted.replay_one(post, get)
                    self.assertIsNotNone(quarantined)
                    assert quarantined is not None
                    self.assertTrue(quarantined.quarantined)
                    self.assertEqual(quarantined.error_class, "malformed_spool_row")
                    self.assertEqual(calls, [])

                    delivered = restarted.replay_one(post, get)
                    self.assertIsNotNone(delivered)
                    assert delivered is not None
                    self.assertTrue(delivered.completed)
                    self.assertEqual(delivered.operation.operation_key, tail.operation_key)
                    self.assertEqual(len(calls), 1)
                    self.assertNotIn("head", calls[0].get("request", {}))
                    self.assertIsNone(restarted.replay_one(post, get))
                    connection = sqlite3.connect(restarted.path)
                    try:
                        quarantine = connection.execute(
                            "SELECT quarantine_reason, COUNT(*) FROM write_operations WHERE operation_key = ?",
                            (head.operation_key,),
                        ).fetchone()
                    finally:
                        connection.close()
                    self.assertEqual(quarantine, ("malformed_spool_row", 1))

    def test_direct_replay_respects_same_track_fifo(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            seed = spool.admit(
                "ingest",
                {"content": "old", "wing": "wing", "room": "facts"},
                track_key="profile:wing",
                action="add",
            )
            spool.complete(seed.operation_key, track_key="profile:wing", drawer_id="drawer-old")
            replace = spool.admit(
                "ingest",
                {
                    "content": "new",
                    "replace_text": "old",
                    "wing": "wing",
                    "room": "facts",
                },
                track_key="profile:wing",
                action="replace",
            )
            remove = spool.admit(
                "delete",
                {},
                track_key="profile:wing",
                action="delete",
            )
            calls = []

            outcome = spool.replay_operation_key(
                remove.operation_key,
                lambda path, body: calls.append((path, body)),
                lambda _path: {"state": "completed"},
                ignore_retry_delay=True,
            )

            self.assertIsNotNone(outcome)
            self.assertEqual(outcome.error_class, "fifo_blocked")
            self.assertEqual(outcome.operation.operation_key, remove.operation_key)
            self.assertEqual(calls, [])
            self.assertEqual(
                spool.next_replayable_operation().operation_key,
                replace.operation_key,
            )


class DurableSchemaMigrationTests(unittest.TestCase):
    @staticmethod
    def _create_historical_spool(hermes_home: str, *, include_scheduling: bool) -> None:
        database_dir = os.path.join(hermes_home, "state", "mempal")
        os.makedirs(database_dir, mode=0o700)
        database_path = os.path.join(database_dir, "write-spool.sqlite3")
        connection = sqlite3.connect(database_path)
        try:
            columns = [
                "sequence INTEGER PRIMARY KEY AUTOINCREMENT",
                "operation_key TEXT NOT NULL UNIQUE",
                "kind TEXT NOT NULL",
                "body_json TEXT NOT NULL",
                "track_key TEXT",
                "action TEXT",
                "receipt_operation_id TEXT",
                "attempt_count INTEGER NOT NULL DEFAULT 0",
                "last_error_class TEXT",
            ]
            if include_scheduling:
                columns.extend([
                    "next_attempt_at REAL NOT NULL DEFAULT 0",
                    "quarantined_at REAL",
                    "quarantine_reason TEXT",
                ])
            columns.extend([
                "created_at REAL NOT NULL",
                "updated_at REAL NOT NULL",
            ])
            connection.execute(
                "CREATE TABLE write_operations (" + ", ".join(columns) + ")"
            )
            connection.execute(
                "CREATE TABLE track_drawers ("
                "track_key TEXT PRIMARY KEY, drawer_id TEXT NOT NULL, updated_at REAL NOT NULL)"
            )
            values = (
                "legacy-key",
                "ingest",
                '{"content":"legacy pending"}',
                "legacy-track",
                "add",
                0,
                None,
                100.0,
                100.0,
            )
            if include_scheduling:
                connection.execute(
                    "INSERT INTO write_operations ("
                    "operation_key, kind, body_json, track_key, action, attempt_count, "
                    "last_error_class, next_attempt_at, quarantined_at, quarantine_reason, "
                    "created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (*values[:7], 0.0, None, None, *values[7:]),
                )
            else:
                connection.execute(
                    "INSERT INTO write_operations ("
                    "operation_key, kind, body_json, track_key, action, attempt_count, "
                    "last_error_class, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    values,
                )
            connection.commit()
        finally:
            connection.close()

    def _assert_migrated(self, hermes_home: str) -> None:
        spool = WriteSpool(hermes_home)
        operation = spool.get("legacy-key")
        self.assertIsNotNone(operation)
        assert operation is not None
        self.assertEqual(operation.body["content"], "legacy pending")
        self.assertEqual(operation.operation_key, "legacy-key")
        replayable = spool.next_replayable_operation()
        self.assertIsNotNone(replayable)
        assert replayable is not None
        self.assertEqual(replayable.operation_key, "legacy-key")
        connection = sqlite3.connect(spool.path)
        try:
            columns = {
                row[1] for row in connection.execute("PRAGMA table_info(write_operations)")
            }
            indexes = {
                row[1] for row in connection.execute("PRAGMA index_list(write_operations)")
            }
        finally:
            connection.close()
        self.assertTrue({
            "next_attempt_at", "quarantined_at", "quarantine_reason",
            "settled_at", "result_drawer_id", "claim_token", "claim_expires_at",
        } <= columns)
        self.assertTrue({
            "idx_write_operations_fifo", "idx_write_operations_replay",
            "idx_write_operations_track", "idx_write_operations_claim",
        } <= indexes)

    def test_main_schema_migrates_and_retains_pending_row(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            self._create_historical_spool(hermes_home, include_scheduling=True)
            self._assert_migrated(hermes_home)

    def test_pre_scheduling_schema_migrates_without_data_loss(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            self._create_historical_spool(hermes_home, include_scheduling=False)
            self._assert_migrated(hermes_home)

    def test_fresh_schema_has_full_columns_and_indexes(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            connection = sqlite3.connect(spool.path)
            try:
                columns = {
                    row[1] for row in connection.execute("PRAGMA table_info(write_operations)")
                }
                indexes = {
                    row[1] for row in connection.execute("PRAGMA index_list(write_operations)")
                }
            finally:
                connection.close()
            self.assertTrue({
                "next_attempt_at", "quarantined_at", "quarantine_reason",
                "settled_at", "result_drawer_id", "claim_token", "claim_expires_at",
            } <= columns)
            self.assertTrue({
                "idx_write_operations_fifo", "idx_write_operations_replay",
                "idx_write_operations_track", "idx_write_operations_claim",
            } <= indexes)


if __name__ == "__main__":
    unittest.main()

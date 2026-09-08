import sqlite3
import tempfile
import unittest
from typing import Any, Dict, List

from mempal._write_spool import WriteSpool


class KeyedReplaySettlementTests(unittest.TestCase):
    def test_keyed_independent_conclude_replays_behind_unrelated_fifo_head(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            head = spool.admit(
                "ingest",
                {"content": "raw turn ahead", "wing": "wing", "room": "turns"},
                action="raw_turn",
            )
            conclude = spool.admit(
                "ingest",
                {"content": "independent conclude", "wing": "wing", "room": "facts"},
                action="conclude",
            )
            posts: List[Dict[str, Any]] = []

            def post(_path: str, body: Dict[str, Any]) -> Dict[str, Any]:
                posts.append(dict(body))
                return {"operation_id": "op-conclude", "state": "queued"}

            def get(_path: str) -> Dict[str, Any]:
                return {
                    "operation_id": "op-conclude",
                    "state": "completed",
                    "drawer_id": "drawer-conclude",
                }

            outcome = spool.replay_operation_key(
                conclude.operation_key,
                post,
                get,
                ignore_retry_delay=True,
            )

            self.assertIsNotNone(outcome)
            assert outcome is not None
            self.assertTrue(outcome.completed)
            self.assertEqual(outcome.drawer_id, "drawer-conclude")
            self.assertEqual(
                [str(body["idempotency_key"]) for body in posts],
                [conclude.operation_key],
            )
            remaining = spool.get(head.operation_key)
            self.assertIsNotNone(remaining)
            assert remaining is not None
            self.assertIsNone(remaining.settled_at)
            self.assertEqual(spool.count(), 1)
            next_row = spool.next_replayable_operation()
            self.assertIsNotNone(next_row)
            assert next_row is not None
            self.assertEqual(next_row.operation_key, head.operation_key)

    def test_keyed_null_track_replace_waits_on_earlier_null_track_create(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            spool.admit(
                "ingest",
                {"content": "create first", "wing": "wing", "room": "facts"},
                action="add",
            )
            replace = spool.admit(
                "ingest",
                {
                    "content": "replace later",
                    "replace_text": "create first",
                    "wing": "wing",
                    "room": "facts",
                },
                action="replace",
            )
            posts: List[Dict[str, Any]] = []

            outcome = spool.replay_operation_key(
                replace.operation_key,
                lambda _path, body: posts.append(dict(body)),
                lambda _path: {
                    "state": "completed",
                    "drawer_id": "drawer-replace",
                },
                ignore_retry_delay=True,
            )

            self.assertIsNotNone(outcome)
            assert outcome is not None
            self.assertEqual(outcome.error_class, "fifo_blocked")
            self.assertEqual(posts, [])
            remaining = spool.get(replace.operation_key)
            self.assertIsNotNone(remaining)
            assert remaining is not None
            self.assertIsNone(remaining.settled_at)

    def test_keyed_replay_settles_existing_receipt_behind_fifo_while_breaker_open(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            spool.admit(
                "ingest",
                {"content": "raw turn ahead", "wing": "wing", "room": "turns"},
                action="raw_turn",
            )
            conclude = spool.admit(
                "ingest",
                {"content": "already completed elsewhere", "wing": "wing", "room": "facts"},
                action="conclude",
            )
            connection = sqlite3.connect(spool.path)
            try:
                connection.execute(
                    "UPDATE write_operations SET receipt_operation_id = ? "
                    "WHERE operation_key = ?",
                    ("op-existing", conclude.operation_key),
                )
                connection.commit()
            finally:
                connection.close()
            posts: List[Dict[str, Any]] = []

            def post(_path: str, body: Dict[str, Any]) -> Dict[str, Any]:
                posts.append(dict(body))
                raise AssertionError("must not re-ingest an exact existing receipt")

            def get(path: str) -> Dict[str, Any]:
                self.assertEqual(path, "/api/operations/op-existing")
                return {
                    "operation_id": "op-existing",
                    "state": "completed",
                    "drawer_id": "drawer-existing",
                }

            outcome = spool.replay_operation_key(
                conclude.operation_key,
                post,
                get,
                ignore_retry_delay=True,
                replay_allowed=lambda: False,
            )

            self.assertIsNotNone(outcome)
            assert outcome is not None
            self.assertTrue(outcome.completed)
            self.assertEqual(outcome.drawer_id, "drawer-existing")
            self.assertEqual(posts, [])
            settled = spool.get(conclude.operation_key)
            self.assertIsNotNone(settled)
            assert settled is not None
            self.assertIsNotNone(settled.settled_at)
            self.assertEqual(settled.result_drawer_id, "drawer-existing")

    def test_replay_one_settles_fifo_head_receipt_while_breaker_open(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            head = spool.admit(
                "ingest",
                {"content": "completed elsewhere", "wing": "wing", "room": "turns"},
                action="raw_turn",
            )
            connection = sqlite3.connect(spool.path)
            try:
                connection.execute(
                    "UPDATE write_operations SET receipt_operation_id = ? "
                    "WHERE operation_key = ?",
                    ("op-head", head.operation_key),
                )
                connection.commit()
            finally:
                connection.close()
            posts: List[Dict[str, Any]] = []

            outcome = spool.replay_one(
                lambda _path, body: posts.append(dict(body)),
                lambda path: {
                    "operation_id": "op-head",
                    "state": "completed",
                    "drawer_id": "drawer-head",
                }
                if path == "/api/operations/op-head"
                else (_ for _ in ()).throw(AssertionError(path)),
                replay_allowed=lambda: False,
            )

            self.assertIsNotNone(outcome)
            assert outcome is not None
            self.assertTrue(outcome.completed)
            self.assertEqual(outcome.drawer_id, "drawer-head")
            self.assertEqual(posts, [])
            self.assertEqual(spool.count(), 0)


if __name__ == "__main__":
    unittest.main()

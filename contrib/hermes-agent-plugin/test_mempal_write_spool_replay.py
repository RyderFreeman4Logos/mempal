import sqlite3
import tempfile
import unittest
from typing import Any, Dict, List

from mempal._write_spool import WriteSpool, classify_replay_error
from mempal._write_spool_replay import _MAX_REPLAY_ATTEMPTS


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

    def _force_all_spool_rows_due(self, spool: WriteSpool) -> None:
        connection = sqlite3.connect(spool.path)
        try:
            connection.execute("UPDATE write_operations SET next_attempt_at = 0")
            connection.commit()
        finally:
            connection.close()

    def test_classify_replay_progress_distinguishes_inflight_stalled_fifo(self) -> None:
        import mempal._write_spool_replay as replay

        classify_replay_progress = getattr(replay, "classify_replay_progress", None)
        self.assertIsNotNone(
            classify_replay_progress,
            "classify_replay_progress must exist for in-flight vs stalled vs fifo_blocked",
        )
        self.assertEqual(
            classify_replay_progress("status_running", attempt_count=0),
            "in_flight",
        )
        self.assertEqual(
            classify_replay_progress("status_queued", attempt_count=6),
            "in_flight",
        )
        self.assertEqual(
            classify_replay_progress(
                "status_running", attempt_count=_MAX_REPLAY_ATTEMPTS
            ),
            "stalled",
        )
        self.assertEqual(classify_replay_progress("status_stalled"), "stalled")
        self.assertEqual(classify_replay_progress("fifo_blocked"), "fifo_blocked")
        running = classify_replay_error("status_running")
        self.assertTrue(running.retryable)
        self.assertFalse(running.count_failure)
        stalled = classify_replay_error("status_stalled")
        self.assertFalse(stalled.retryable)
        self.assertTrue(stalled.count_failure)
        blocked = classify_replay_error("fifo_blocked")
        self.assertTrue(blocked.retryable)
        self.assertFalse(blocked.count_failure)

    def test_status_running_fifo_head_stalls_and_unblocks_later_conclude(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            head = spool.admit(
                "ingest",
                {"content": "stuck running turn", "wing": "wing", "room": "turns"},
                action="raw_turn",
            )
            conclude = spool.admit(
                "ingest",
                {"content": "later conclude", "wing": "wing", "room": "facts"},
                action="conclude",
            )
            head_gets = 0
            conclude_posts: List[Dict[str, Any]] = []

            def post(_path: str, body: Dict[str, Any]) -> Dict[str, Any]:
                key = str(body["idempotency_key"])
                if key == head.operation_key:
                    raise AssertionError("running receipt must not re-POST")
                conclude_posts.append(dict(body))
                return {"operation_id": "op-conclude-after-stall", "state": "queued"}

            def get(path: str) -> Dict[str, Any]:
                nonlocal head_gets
                if path == "/api/operations/op-running-head":
                    head_gets += 1
                    return {
                        "operation_id": "op-running-head",
                        "state": "running",
                    }
                if path == "/api/operations/op-conclude-after-stall":
                    return {
                        "operation_id": "op-conclude-after-stall",
                        "state": "completed",
                        "drawer_id": "drawer-conclude-after-stall",
                    }
                raise AssertionError(path)

            connection = sqlite3.connect(spool.path)
            try:
                connection.execute(
                    "UPDATE write_operations SET receipt_operation_id = ? "
                    "WHERE operation_key = ?",
                    ("op-running-head", head.operation_key),
                )
                connection.commit()
            finally:
                connection.close()

            first = spool.replay_one(post, get)
            self.assertIsNotNone(first)
            assert first is not None
            self.assertEqual(first.error_class, "status_running")
            self.assertFalse(first.quarantined)
            self.assertEqual(conclude_posts, [])

            for _ in range(_MAX_REPLAY_ATTEMPTS):
                self._force_all_spool_rows_due(spool)
                outcome = spool.replay_one(post, get)
                self.assertIsNotNone(outcome)
                assert outcome is not None
                if outcome.quarantined:
                    break
            else:
                self.fail("running FIFO head never fail-closed as stalled")

            self.assertEqual(outcome.error_class, "status_stalled")
            self.assertTrue(outcome.quarantined)
            stalled_head = spool.get(head.operation_key)
            self.assertIsNotNone(stalled_head)
            assert stalled_head is not None
            self.assertIsNotNone(stalled_head.quarantined_at)
            self.assertEqual(stalled_head.quarantine_reason, "status_stalled")
            self.assertGreaterEqual(head_gets, _MAX_REPLAY_ATTEMPTS)

            completed = spool.replay_one(post, get)
            self.assertIsNotNone(completed)
            assert completed is not None
            self.assertTrue(completed.completed)
            self.assertEqual(completed.drawer_id, "drawer-conclude-after-stall")
            self.assertEqual(
                [str(body["idempotency_key"]) for body in conclude_posts],
                [conclude.operation_key],
            )
            remaining = spool.get(conclude.operation_key)
            self.assertIsNotNone(remaining)
            assert remaining is not None
            self.assertIsNotNone(remaining.settled_at)


if __name__ == "__main__":
    unittest.main()

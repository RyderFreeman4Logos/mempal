import json
import sqlite3
import tempfile
import threading
import time
import unittest

from mempal._conclude import conclusion_request
from mempal._write_spool import WriteSpool
from test_mempal_provider import RecordingProvider


def _set_receipt(spool: WriteSpool, operation_key: str, operation_id: str) -> None:
    with sqlite3.connect(spool.path) as connection:
        connection.execute(
            "UPDATE write_operations SET receipt_operation_id = ? WHERE operation_key = ?",
            (operation_id, operation_key),
        )


def _open_breaker(provider: RecordingProvider) -> None:
    for _ in range(5):
        provider._record_failure()


def _admit_conclusion(
    provider: RecordingProvider,
    operation_key: str,
    operation_id: str,
    conclusion: str = "pending conclusion",
) -> None:
    assert provider._write_spool is not None
    provider._write_spool.admit(
        "ingest",
        conclusion_request(
            conclusion,
            provider._wing,
            provider._facts_room,
            provider._safe_min_importance,
            provider._project_id,
        ),
        action="conclude",
        operation_key=operation_key,
    )
    _set_receipt(provider._write_spool, operation_key, operation_id)


def _call_write(provider: RecordingProvider, caller: str, operation_key=None) -> str:
    assert provider._write_spool is not None
    if caller == "background":
        if operation_key is None:
            operation_key = provider._write_spool.admit(
                "ingest",
                {"content": "pending value"},
                action="raw_turn",
            ).operation_key
        else:
            with sqlite3.connect(provider._write_spool.path) as connection:
                connection.execute(
                    "UPDATE write_operations SET next_attempt_at = 0 "
                    "WHERE operation_key = ?",
                    (operation_key,),
                )
        provider._replay_spooled_write()
        return operation_key
    if caller == "conclude":
        args = {"conclusion": "pending conclusion"}
        if operation_key is not None:
            args["operation_key"] = operation_key
        result = json.loads(provider.handle_tool_call("mempal_conclude", args))
        return str(
            result.get("operation_key") or result["error_details"]["operation_key"]
        )
    args = {"action": "add", "target": "user", "content": "pending value"}
    if operation_key is not None:
        args["operation_key"] = operation_key
    result = json.loads(provider.authoritative_memory_write(args))
    return str(result["operation_key"])


class ReplayReviewRegressionTests(unittest.TestCase):
    def test_breaker_open_corrupt_head_keeps_worker_alive_and_replays_next_row(
        self,
    ) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        assert provider._write_spool is not None
        spool = provider._write_spool
        provider._drain_writes()
        head = spool.admit("ingest", {"content": "bad"}, action="raw_turn")
        later = spool.admit("ingest", {"content": "good"}, action="raw_turn")
        with sqlite3.connect(spool.path) as connection:
            connection.execute(
                "UPDATE write_operations SET body_json = ?, receipt_operation_id = ? "
                "WHERE operation_key = ?",
                ("{", "operation-bad", head.operation_key),
            )
            connection.execute(
                "UPDATE write_operations SET receipt_operation_id = ? WHERE operation_key = ?",
                ("operation-good", later.operation_key),
            )
        provider.durable_status["operation-good"] = {
            "operation_id": "operation-good",
            "state": "completed",
            "drawer_id": "drawer-good",
        }
        _open_breaker(provider)

        try:
            provider._wake_spool_worker()
            deadline = time.monotonic() + 2.0
            later_settled = False
            head_row = None
            while time.monotonic() < deadline:
                with sqlite3.connect(spool.path) as connection:
                    head_row = connection.execute(
                        "SELECT quarantine_reason FROM write_operations WHERE operation_key = ?",
                        (head.operation_key,),
                    ).fetchone()
                    later_row = connection.execute(
                        "SELECT settled_at FROM write_operations WHERE operation_key = ?",
                        (later.operation_key,),
                    ).fetchone()
                later_settled = later_row is not None and later_row[0] is not None
                if later_settled:
                    break
                time.sleep(0.02)

            self.assertTrue(
                provider._write_worker and provider._write_worker.is_alive()
            )
            self.assertIsNotNone(head_row)
            assert head_row is not None
            self.assertEqual(head_row[0], "malformed_spool_row")
            self.assertTrue(later_settled)
            self.assertEqual(provider.posts, [])
        finally:
            provider.shutdown()

    def test_completed_status_requires_exact_bound_operation_identity_for_all_actions(
        self,
    ) -> None:
        cases = (
            ("conclude", "ingest", None, {"content": "fact"}),
            ("add", "ingest", "track-add", {"content": "add"}),
            (
                "replace",
                "ingest",
                "track-replace",
                {"content": "new", "replace_text": "old"},
            ),
            ("delete", "delete", "track-delete", {}),
        )
        for action, kind, track_key, body in cases:
            for returned_id in (None, "operation-other"):
                with self.subTest(action=action, returned_id=returned_id):
                    with tempfile.TemporaryDirectory() as hermes_home:
                        spool = WriteSpool(hermes_home)
                        operation = spool.admit(
                            kind,
                            body,
                            track_key=track_key,
                            action=action,
                            operation_key=f"key-{action}-{returned_id}",
                        )
                        expected_id = f"operation-{action}"
                        _set_receipt(spool, operation.operation_key, expected_id)
                        if action == "delete":
                            with sqlite3.connect(spool.path) as connection:
                                connection.execute(
                                    "INSERT INTO track_drawers(track_key, drawer_id, updated_at) "
                                    "VALUES (?, ?, ?)",
                                    (track_key, "drawer-original", time.time()),
                                )
                        status = {
                            "state": "completed",
                            "drawer_id": "drawer-unbound",
                        }
                        if returned_id is not None:
                            status["operation_id"] = returned_id

                        outcome = spool.replay_operation_key(
                            operation.operation_key,
                            lambda *_args: self.fail("existing receipt must not POST"),
                            lambda _path: status,
                            ignore_retry_delay=True,
                            replay_allowed=lambda: False,
                        )

                        self.assertIsNotNone(outcome)
                        assert outcome is not None
                        self.assertFalse(outcome.completed)
                        remaining = spool.get(operation.operation_key)
                        self.assertIsNotNone(remaining)
                        assert remaining is not None
                        self.assertIsNone(remaining.settled_at)
                        expected_drawer = (
                            "drawer-original" if action == "delete" else None
                        )
                        self.assertEqual(
                            spool.drawer_for_track(track_key) if track_key else None,
                            expected_drawer,
                        )

    def test_cross_process_claim_cannot_settle_mismatched_status(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            first = WriteSpool(hermes_home)
            second = WriteSpool(hermes_home)
            operation = first.admit(
                "ingest",
                {"content": "value"},
                track_key="track",
                action="add",
            )
            _set_receipt(first, operation.operation_key, "operation-expected")
            entered = threading.Event()
            release = threading.Event()
            outcomes = []

            def mismatched_get(_path: str):
                entered.set()
                release.wait(timeout=1.0)
                return {
                    "operation_id": "operation-other",
                    "state": "completed",
                    "drawer_id": "drawer-other",
                }

            worker = threading.Thread(
                target=lambda: outcomes.append(
                    first.replay_operation_key(
                        operation.operation_key,
                        lambda *_args: self.fail("must not POST"),
                        mismatched_get,
                        ignore_retry_delay=True,
                    )
                )
            )
            worker.start()
            self.assertTrue(entered.wait(timeout=1.0))
            duplicate = second.replay_operation_key(
                operation.operation_key,
                lambda *_args: self.fail("must not POST"),
                lambda _path: self.fail("losing claim must not GET"),
                ignore_retry_delay=True,
            )
            release.set()
            worker.join(timeout=1.0)

            self.assertIsNotNone(duplicate)
            assert duplicate is not None
            self.assertEqual(duplicate.error_class, "claim_busy")
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(outcomes), 1)
            self.assertFalse(outcomes[0].completed)
            remaining = first.get(operation.operation_key)
            self.assertIsNotNone(remaining)
            assert remaining is not None
            self.assertIsNone(remaining.settled_at)
            self.assertIsNone(first.drawer_for_track("track"))

    def test_get_only_completion_never_resets_write_breaker_at_any_caller(self) -> None:
        for caller in ("background", "conclude", "authoritative"):
            with self.subTest(caller=caller):
                provider = RecordingProvider()
                provider.initialize("session-a", user_id="alice", profile="work")
                provider._start_write_worker = lambda: None
                assert provider._write_spool is not None
                key = f"key-{caller}"
                operation_id = f"operation-{caller}"
                if caller == "conclude":
                    _admit_conclusion(provider, key, operation_id)
                else:
                    track_key = provider._track_key("user")
                    provider._write_spool.admit(
                        "ingest",
                        {
                            "content": "pending value",
                            "wing": provider._wing,
                            "room": provider._facts_room,
                            "memory_kind": "profile_fact",
                            "importance": provider._safe_min_importance,
                            "source_type": "user_explicit",
                        },
                        track_key=track_key,
                        action="raw_turn" if caller == "background" else "add",
                        operation_key=key,
                    )
                    _set_receipt(provider._write_spool, key, operation_id)
                provider.durable_status[operation_id] = {
                    "operation_id": operation_id,
                    "state": "completed",
                    "drawer_id": f"drawer-{caller}",
                }
                _open_breaker(provider)
                before = provider._backoff._read_state()

                if caller == "background":
                    provider._replay_spooled_write()
                elif caller == "conclude":
                    result = json.loads(
                        provider.handle_tool_call(
                            "mempal_conclude",
                            {"conclusion": "pending conclusion", "operation_key": key},
                        )
                    )
                    self.assertEqual(result.get("result"), "Fact stored.")
                else:
                    result = json.loads(
                        provider.authoritative_memory_write(
                            {
                                "action": "add",
                                "target": "user",
                                "content": "pending value",
                                "operation_key": key,
                            }
                        )
                    )
                    self.assertTrue(result["success"])

                after = provider._backoff._read_state()
                self.assertEqual(after.failure_count, before.failure_count)
                self.assertEqual(after.open_until_epoch, before.open_until_epoch)
                provider.posts.clear()
                fresh = json.loads(
                    provider.handle_tool_call(
                        "mempal_conclude", {"conclusion": "fresh conclusion"}
                    )
                )
                self.assertNotIn("result", fresh)
                self.assertEqual(provider.posts, [])
                provider.shutdown()

    def test_post_admission_resets_breaker_before_later_settlement_at_all_callers(
        self,
    ) -> None:
        for caller in ("background", "conclude", "authoritative"):
            for first_status in ("queued", "running", "get_error"):
                with self.subTest(caller=caller, first_status=first_status):
                    provider = RecordingProvider()
                    provider.initialize("session-a", user_id="alice", profile="work")
                    provider._start_write_worker = lambda: None
                    provider._conclude_wait_timeout = 0.0
                    assert provider._write_spool is not None
                    original_post = provider._post
                    original_get = provider._get
                    allow_completion = [False]

                    def post(
                        path,
                        body,
                        original_post=original_post,
                        provider=provider,
                        first_status=first_status,
                    ):
                        receipt = original_post(path, body)
                        operation_id = receipt["operation_id"]
                        provider.durable_status[operation_id] = {
                            "operation_id": operation_id,
                            "state": (
                                "queued"
                                if first_status == "get_error"
                                else first_status
                            ),
                        }
                        return receipt

                    def get(
                        path,
                        params=None,
                        first_status=first_status,
                        allow_completion=allow_completion,
                        original_get=original_get,
                    ):
                        if (
                            first_status == "get_error"
                            and not allow_completion[0]
                            and path.startswith("/api/operations/")
                        ):
                            raise TimeoutError("status unavailable")
                        return original_get(path, params)

                    provider._post = post
                    provider._get = get
                    for _ in range(4):
                        provider._record_failure()

                    operation_key = _call_write(provider, caller)
                    expected_failures = 1 if first_status == "get_error" else 0
                    self.assertEqual(
                        provider._backoff._read_state().failure_count,
                        expected_failures,
                    )
                    operation_id = f"operation_{operation_key}"
                    provider.durable_status[operation_id] = {
                        "operation_id": operation_id,
                        "state": "completed",
                        "drawer_id": f"drawer-{caller}-{first_status}",
                    }
                    allow_completion[0] = True
                    _call_write(provider, caller, operation_key)
                    settled = provider._write_spool.get(operation_key)
                    self.assertIsNotNone(settled)
                    assert settled is not None
                    self.assertIsNotNone(settled.settled_at)
                    self.assertEqual(len(provider.posts), 1)
                    self.assertEqual(
                        provider._backoff._read_state().failure_count,
                        expected_failures,
                    )
                    provider._record_failure()
                    self.assertEqual(
                        provider._backoff._read_state().failure_count,
                        expected_failures + 1,
                    )
                    self.assertFalse(provider._is_breaker_open())
                    provider.shutdown()

    def test_invalid_post_receipt_never_resets_breaker_at_any_caller(self) -> None:
        for caller in ("background", "conclude", "authoritative"):
            with self.subTest(caller=caller):
                provider = RecordingProvider()
                provider.initialize("session-a", user_id="alice", profile="work")
                provider._start_write_worker = lambda: None
                provider._conclude_wait_timeout = 0.0
                assert provider._write_spool is not None

                def invalid_post(path, body, provider=provider):
                    provider.posts.append((path, dict(body)))
                    return {}

                provider._post = invalid_post
                for _ in range(4):
                    provider._record_failure()

                _call_write(provider, caller)
                self.assertEqual(provider._backoff._read_state().failure_count, 5)
                self.assertTrue(provider._is_breaker_open())
                self.assertEqual(len(provider.posts), 1)
                provider.shutdown()

    def test_open_breaker_queued_and_running_receipts_poll_once_and_honor_backoff(
        self,
    ) -> None:
        for state in ("queued", "running"):
            with self.subTest(state=state):
                provider = RecordingProvider()
                provider.initialize("session-a", user_id="alice", profile="work")
                provider._start_write_worker = lambda: None
                provider._conclude_wait_timeout = 0.21
                key = f"key-{state}"
                operation_id = f"operation-{state}"
                _admit_conclusion(provider, key, operation_id)
                provider.durable_status[operation_id] = {
                    "operation_id": operation_id,
                    "state": state,
                }
                _open_breaker(provider)

                result = json.loads(
                    provider.handle_tool_call(
                        "mempal_conclude",
                        {"conclusion": "pending conclusion", "operation_key": key},
                    )
                )

                self.assertEqual(
                    result["error_details"]["kind"], "durable_operation_pending"
                )
                operation_gets = [
                    path
                    for path, _ in provider.gets
                    if path.startswith("/api/operations/")
                ]
                self.assertEqual(operation_gets, [f"/api/operations/{operation_id}"])
                assert provider._write_spool is not None
                operation = provider._write_spool.get(key)
                self.assertIsNotNone(operation)
                assert operation is not None
                self.assertEqual(operation.attempt_count, 1)
                self.assertGreater(operation.next_attempt_at, time.time())
                provider.shutdown()

    def test_concurrent_open_breaker_retry_has_one_total_status_read(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None
        provider._conclude_wait_timeout = 0.5
        key = "key-concurrent"
        operation_id = "operation-concurrent"
        _admit_conclusion(provider, key, operation_id)
        _open_breaker(provider)
        entered = threading.Event()
        release = threading.Event()
        original_get = provider._get
        status_gets = []

        def blocking_get(path, params=None):
            if path.startswith("/api/operations/"):
                status_gets.append(path)
                entered.set()
                release.wait(timeout=1.0)
                return {"operation_id": operation_id, "state": "queued"}
            return original_get(path, params)

        provider._get = blocking_get
        results = []
        first = threading.Thread(
            target=lambda: results.append(
                json.loads(
                    provider.handle_tool_call(
                        "mempal_conclude",
                        {"conclusion": "pending conclusion", "operation_key": key},
                    )
                )
            )
        )
        second_done = threading.Event()

        def retry_again() -> None:
            results.append(
                json.loads(
                    provider.handle_tool_call(
                        "mempal_conclude",
                        {"conclusion": "pending conclusion", "operation_key": key},
                    )
                )
            )
            second_done.set()

        second = threading.Thread(target=retry_again)
        first.start()
        self.assertTrue(entered.wait(timeout=1.0))
        second.start()
        finished_before_release = second_done.wait(timeout=0.15)
        release.set()
        first.join(timeout=1.0)
        second.join(timeout=1.0)

        self.assertTrue(finished_before_release)
        self.assertFalse(first.is_alive())
        self.assertFalse(second.is_alive())
        self.assertEqual(len(results), 2)
        self.assertEqual(status_gets, [f"/api/operations/{operation_id}"])
        assert provider._write_spool is not None
        operation = provider._write_spool.get(key)
        self.assertIsNotNone(operation)
        assert operation is not None
        self.assertEqual(operation.attempt_count, 1)
        provider.shutdown()


if __name__ == "__main__":
    unittest.main()

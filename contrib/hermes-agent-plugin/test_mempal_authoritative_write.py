import json
import sqlite3
import tempfile
import threading
import time
import unittest
import urllib.error

from mempal._write_spool import WriteSpool
from test_mempal_provider import RecordingProvider


class SequencedStatusProvider(RecordingProvider):
    def __init__(self, states):
        super().__init__()
        self.states = list(states)
        self.wake_calls = 0

    def _get(self, path, params=None):
        status = super()._get(path, params)
        if path.startswith("/api/operations/") and self.states:
            status = dict(status)
            status["state"] = self.states.pop(0)
        return status

    def _wake_spool_worker(self):
        self.wake_calls += 1


class FailingStatusProvider(RecordingProvider):
    def __init__(self, status: int) -> None:
        super().__init__()
        self.status = status
        self.wake_calls = 0

    def _post(self, path, body):
        self.posts.append((path, dict(body)))
        error = urllib.error.HTTPError(
            path, self.status, "synthetic unavailable", None, None
        )
        error.close()
        raise error

    def _wake_spool_worker(self):
        self.wake_calls += 1


class SharedDurableBackend:
    def __init__(self) -> None:
        self.calls = []
        self.operations = {}
        self.entered = threading.Event()
        self.release = threading.Event()
        self._lock = threading.Lock()
        self.block_first = False

    def post(self, path, body):
        key = body["idempotency_key"]
        with self._lock:
            self.calls.append(key)
            first = key not in self.operations
            operation_id = f"operation_{key}"
            self.operations.setdefault(operation_id, {
                "operation_id": operation_id,
                "state": "completed",
                "drawer_id": f"drawer_{len(self.operations) + 1}",
            })
        if first and self.block_first:
            self.entered.set()
            self.release.wait(timeout=2.0)
        return {"operation_id": operation_id, "state": "completed"}


class SharedProvider(RecordingProvider):
    def __init__(self, backend: SharedDurableBackend) -> None:
        super().__init__()
        self.backend = backend

    def _post(self, path, body):
        self.posts.append((path, dict(body)))
        if path in {"/api/ingest/durable", "/api/delete/durable"}:
            return self.backend.post(path, body)
        return {"ok": True}

    def _get(self, path, params=None):
        if path.startswith("/api/operations/"):
            return self.backend.operations.get(path.rsplit("/", 1)[-1], {})
        return {}


class AuthoritativeMemoryWriteTests(unittest.TestCase):
    def test_queued_admission_is_successful_and_wakes_spool(self) -> None:
        provider = SequencedStatusProvider(["queued"])
        provider.initialize("session-a", user_id="alice", profile="work")
        before = provider._backoff._read_state()

        receipt = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "queued preference",
        }))

        after = provider._backoff._read_state()
        self.assertTrue(receipt["success"])
        self.assertEqual(receipt["state"], "local_admitted")
        self.assertEqual(receipt["durability"]["kind"], "durable_replay_deferred")
        self.assertEqual(receipt["operation_id"], "operation_" + receipt["operation_key"])
        self.assertEqual(provider.wake_calls, 1)
        self.assertEqual(after.failure_count, before.failure_count)
        provider.shutdown()

    def test_five_queued_admissions_do_not_open_breaker(self) -> None:
        provider = SequencedStatusProvider(["queued"] * 5)
        provider.initialize("session-a", user_id="alice", profile="work")

        receipts = [json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": f"queued preference {index}",
        })) for index in range(5)]

        self.assertTrue(all(receipt["success"] for receipt in receipts))
        self.assertFalse(provider._is_breaker_open())
        self.assertEqual(provider._backoff._read_state().failure_count, 0)
        provider.shutdown()

    def test_batch_continues_after_queued_item(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.authoritative_memory_write({
            "target": "user",
            "operations": [
                {"action": "add", "content": "queued preference"},
                {"action": "add", "content": "completed preference"},
            ],
        }))

        self.assertTrue(result["success"])
        self.assertEqual(len(result["operation_ids"]), 1)
        self.assertEqual(len(result["operation_keys"]), 2)
        self.assertEqual(provider.wake_calls, 2)
        self.assertEqual(provider._write_spool.count(), 2)
        provider.shutdown()

    def test_remove_waits_for_pending_same_track_replace(self) -> None:
        provider = SequencedStatusProvider(["completed", "queued"])
        provider.initialize("session-a", user_id="alice", profile="work")
        added = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "old preference",
        }))
        replaced = json.loads(provider.authoritative_memory_write({
            "action": "replace",
            "target": "user",
            "old_text": "old preference",
            "content": "new preference",
        }))
        before = provider._backoff._read_state()

        removed = json.loads(provider.authoritative_memory_write({
            "action": "remove",
            "target": "user",
            "old_text": "new preference",
        }))

        after = provider._backoff._read_state()
        self.assertTrue(added["success"])
        self.assertTrue(replaced["success"])
        self.assertTrue(removed["success"])
        self.assertEqual(removed["state"], "local_admitted")
        self.assertEqual(provider._write_spool.count(), 2)
        self.assertEqual(
            [path for path, _ in provider.posts],
            ["/api/ingest/durable", "/api/ingest/durable"],
        )
        self.assertEqual(after.failure_count, before.failure_count)
        provider.shutdown()

    def test_retry_with_returned_operation_key_reuses_operation(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")

        first = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "retryable preference",
        }))
        second = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "retryable preference",
            "operation_key": first["operation_key"],
        }))

        self.assertTrue(second["success"])
        self.assertEqual(second["operation_key"], first["operation_key"])
        self.assertEqual(provider._write_spool.count(), 0)
        self.assertEqual(
            {body["idempotency_key"] for _, body in provider.posts},
            {first["operation_key"]},
        )
        provider.shutdown()

    def test_retry_with_operation_key_kwarg_reuses_operation(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")

        first = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "retryable kwarg preference",
        }))
        second = json.loads(provider.authoritative_memory_write(
            {
                "action": "add",
                "target": "user",
                "content": "retryable kwarg preference",
            },
            operation_key=first["operation_key"],
        ))

        self.assertTrue(second["success"])
        self.assertEqual(second["operation_key"], first["operation_key"])
        self.assertEqual(provider._write_spool.count(), 0)
        self.assertEqual(
            {body["idempotency_key"] for _, body in provider.posts},
            {first["operation_key"]},
        )
        provider.shutdown()

    def test_partial_batch_retry_reuses_returned_operation_keys(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")

        first = json.loads(provider.authoritative_memory_write({
            "target": "user",
            "operations": [
                {"action": "add", "content": "first batch preference"},
                {"action": "add", "content": "second batch preference"},
            ],
        }))
        second = json.loads(provider.authoritative_memory_write({
            "target": "user",
            "operations": [
                {
                    "action": "add",
                    "content": "first batch preference",
                    "operation_key": first["operation_keys"][0],
                },
                {
                    "action": "add",
                    "content": "second batch preference",
                    "operation_key": first["operation_keys"][1],
                },
            ],
        }))

        self.assertTrue(second["success"])
        self.assertEqual(second["operation_keys"], first["operation_keys"])
        self.assertEqual(provider._write_spool.count(), 0)
        self.assertEqual(
            [body["idempotency_key"] for _, body in provider.posts],
            first["operation_keys"],
        )
        provider.shutdown()

    def test_project_scopes_drawer_tracking(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            project_a = RecordingProvider()
            project_a.initialize(
                "session-a",
                hermes_home=hermes_home,
                user_id="alice",
                profile="work",
                project_id="project-a",
            )
            added = json.loads(project_a.authoritative_memory_write({
                "action": "add",
                "target": "user",
                "content": "project A preference",
            }))
            project_a.shutdown()

            project_b = RecordingProvider()
            project_b.initialize(
                "session-b",
                hermes_home=hermes_home,
                user_id="alice",
                profile="work",
                project_id="project-b",
            )
            removed = json.loads(project_b.authoritative_memory_write({
                "action": "remove",
                "target": "user",
                "old_text": "project A preference",
            }))

            self.assertTrue(added["success"])
            self.assertFalse(removed["success"])
            self.assertEqual(removed["error_class"], "target_unresolved")
            self.assertEqual(project_b.posts, [])
            self.assertEqual(project_b._write_spool.count(), 1)
            project_b.shutdown()

    def test_unscoped_tracking_cannot_resolve_scoped_drawer(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            scoped = RecordingProvider()
            scoped.initialize(
                "session-a",
                hermes_home=hermes_home,
                user_id="alice",
                profile="work",
                project_id="project-a",
            )
            scoped.authoritative_memory_write({
                "action": "add",
                "target": "user",
                "content": "scoped preference",
            })
            scoped.shutdown()

            unscoped = RecordingProvider()
            unscoped.initialize(
                "session-b",
                hermes_home=hermes_home,
                user_id="alice",
                profile="work",
            )
            removed = json.loads(unscoped.authoritative_memory_write({
                "action": "remove",
                "target": "user",
                "old_text": "scoped preference",
            }))

            self.assertFalse(removed["success"])
            self.assertEqual(removed["error_class"], "target_unresolved")
            self.assertEqual(unscoped.posts, [])
            unscoped.shutdown()

    def test_authoritative_memory_write_returns_durable_crud_receipts(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        added = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "old preference",
        }))
        replaced = json.loads(provider.authoritative_memory_write({
            "action": "replace",
            "target": "user",
            "old_text": "old preference",
            "content": "new preference",
        }))
        removed = json.loads(provider.authoritative_memory_write({
            "action": "remove",
            "target": "user",
            "old_text": "new preference",
        }))

        for receipt in (added, replaced, removed):
            self.assertTrue(receipt["success"])
            self.assertTrue(receipt["operation_key"])
            self.assertTrue(receipt["operation_id"])
        self.assertTrue(added["drawer_id"])
        self.assertTrue(replaced["drawer_id"])
        self.assertTrue(removed["drawer_id"])
        durable_requests = [
            body["request"]
            for path, body in provider.posts
            if path in {"/api/ingest/durable", "/api/delete/durable"}
        ]
        self.assertEqual(durable_requests[1]["replace_text"], "old preference")
        self.assertNotIn("supersedes", durable_requests[1])
        delete_requests = [
            body["request"]
            for path, body in provider.posts
            if path == "/api/delete/durable"
        ]
        self.assertEqual(delete_requests[0]["drawer_id"], replaced["drawer_id"])

    def test_authoritative_memory_write_accepts_remove_add_batch(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        added = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "old preference",
        }))
        result = json.loads(provider.authoritative_memory_write({
            "target": "user",
            "operations": [
                {"action": "remove", "old_text": "old preference"},
                {"action": "add", "content": "new preference"},
            ],
        }))

        self.assertTrue(result["success"])
        self.assertEqual(len(result["operation_ids"]), 2)
        durable = [
            (path, body["request"])
            for path, body in provider.posts
            if path in {"/api/ingest/durable", "/api/delete/durable"}
        ][1:]
        self.assertEqual([path for path, _ in durable], [
            "/api/delete/durable",
            "/api/ingest/durable",
        ])
        self.assertEqual(durable[0][1]["drawer_id"], added["drawer_id"])
        self.assertEqual(durable[1][1]["content"], "new preference")

    def test_503_with_wake_false_is_pending_and_wakes_replay(self) -> None:
        provider = FailingStatusProvider(503)
        provider.initialize("session-a", user_id="alice", profile="work")

        receipt = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "retryable evidence",
        }))

        operation = provider._write_spool.next_operation()
        self.assertTrue(receipt["success"])
        self.assertEqual(receipt["state"], "local_admitted")
        self.assertTrue(receipt["retryable"])
        self.assertEqual(provider.wake_calls, 1)
        self.assertEqual(operation.last_error_class, "http_503")
        self.assertIsNone(operation.quarantined_at)
        self.assertGreater(operation.next_attempt_at, time.time())
        provider.shutdown()

    def test_non_allowlisted_4xx_is_quarantined_not_forgiven(self) -> None:
        provider = FailingStatusProvider(409)
        provider.initialize("session-a", user_id="alice", profile="work")

        receipt = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "conflict evidence",
        }))

        operation = provider._write_spool.next_operation()
        self.assertFalse(receipt["success"])
        self.assertEqual(receipt["error_class"], "http_409")
        self.assertFalse(receipt["retryable"])
        self.assertIsNotNone(operation.quarantined_at)
        self.assertEqual(operation.quarantine_reason, "http_409")
        provider.shutdown()

    def test_operation_key_conflict_fails_closed_without_payload(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")
        first = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "first private value",
            "operation_key": "stable-operation",
        }))

        retry = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "first private value",
            "operation_key": first["operation_key"],
        }))
        conflict = json.loads(provider.authoritative_memory_write({
            "action": "replace",
            "target": "user",
            "old_text": "first private value",
            "content": "second private value",
            "operation_key": first["operation_key"],
        }))

        self.assertTrue(first["success"])
        self.assertTrue(retry["success"])
        self.assertFalse(conflict["success"])
        self.assertEqual(conflict["error_class"], "operation_key_conflict")
        self.assertFalse(conflict["retryable"])
        rendered = json.dumps(conflict)
        self.assertNotIn("first private value", rendered)
        self.assertNotIn("second private value", rendered)
        self.assertEqual(provider._write_spool.count(), 0)
        self.assertEqual(len(provider.posts), 1)
        provider.shutdown()

    def test_remove_rejects_ambiguous_content_before_admission(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        receipt = json.loads(provider.authoritative_memory_write({
            "action": "remove",
            "target": "user",
            "old_text": "the tracked value",
            "content": "another value",
        }))

        self.assertFalse(receipt["success"])
        self.assertEqual(receipt["error_class"], "ambiguous_remove_payload")
        self.assertEqual(provider.posts, [])
        self.assertEqual(provider._write_spool.count(), 0)
        provider.shutdown()

    def test_track_identity_is_injective_for_adversarial_scope_ids(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        cases = [
            ("a", "b:c", None),
            ("a:b", "c", None),
            ("a|b", "c|d", "project|x"),
            ("a", "b|c", "project|x"),
            ("", "", "project"),
            ("prefix", "suffix", "project"),
            ("prefix|suffix", "", "project"),
            ("é", "统一", "项目"),
        ]
        keys = []
        for target, wing, project_id in cases:
            provider._wing = wing
            provider._project_id = project_id
            keys.append(provider._track_key(target))
        self.assertEqual(len(keys), len(set(keys)))
        provider.shutdown()

    def test_shared_spool_claim_prevents_duplicate_cross_instance_send(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            backend = SharedDurableBackend()
            backend.block_first = True
            first = SharedProvider(backend)
            second = SharedProvider(backend)
            first.initialize("session-a", hermes_home=hermes_home)
            second.initialize("session-b", hermes_home=hermes_home)
            operation = first._spool_write(
                "ingest", {"content": "one"}, action="add", wake=False
            )
            results = []
            worker = threading.Thread(
                target=lambda: results.append(first._write_spool.replay_operation_key(
                    operation.operation_key, first._post, first._get,
                    ignore_retry_delay=True,
                ))
            )
            worker.start()
            self.assertTrue(backend.entered.wait(timeout=1.0))

            second._spool_write(
                "ingest", {"content": "two"}, action="add", wake=False
            )
            duplicate = second._write_spool.replay_operation_key(
                operation.operation_key, second._post, second._get,
                ignore_retry_delay=True,
            )

            self.assertIsNotNone(duplicate)
            self.assertEqual(duplicate.error_class, "claim_busy")
            self.assertEqual(backend.calls, [operation.operation_key])
            backend.release.set()
            worker.join(timeout=1.0)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(results), 1)
            self.assertTrue(results[0].completed)

            next_result = second._write_spool.replay_one(
                second._post, second._get
            )
            self.assertTrue(next_result.completed)
            self.assertEqual(len(backend.calls), 2)
            self.assertEqual(len(set(backend.calls)), 2)
            first.shutdown()
            second.shutdown()

    def test_global_fifo_blocks_later_append_while_predecessor_retries(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            spool = WriteSpool(hermes_home)
            other = WriteSpool(hermes_home)
            first = spool.admit("ingest", {"content": "one"}, track_key="one", action="add")
            second = spool.admit("ingest", {"content": "two"}, track_key="two", action="add")
            entered = threading.Event()
            release = threading.Event()
            delivered = []
            first_attempt = True

            def post(_path, body):
                nonlocal first_attempt
                key = body["idempotency_key"]
                delivered.append(body["request"]["content"])
                if key == first.operation_key and first_attempt:
                    first_attempt = False
                    entered.set()
                    release.wait(timeout=2.0)
                    error = urllib.error.HTTPError(
                        "/api/ingest/durable", 503, "synthetic unavailable", None, None
                    )
                    error.close()
                    raise error
                return {"operation_id": f"operation_{key}", "state": "completed"}

            def get(_path):
                return {"state": "completed", "drawer_id": "drawer"}

            results = []
            worker = threading.Thread(
                target=lambda: results.append(spool.replay_one(post, get))
            )
            worker.start()
            self.assertTrue(entered.wait(timeout=1.0))
            other.admit("ingest", {"content": "three"}, track_key="three", action="add")
            blocked = other.replay_one(post, get)
            self.assertIsNone(blocked)
            self.assertEqual(delivered, ["one"])

            release.set()
            worker.join(timeout=1.0)
            self.assertFalse(worker.is_alive())
            connection = sqlite3.connect(spool.path)
            try:
                connection.execute("UPDATE write_operations SET next_attempt_at = 0")
                connection.commit()
            finally:
                connection.close()
            for _ in range(3):
                outcome = other.replay_one(post, get)
                self.assertIsNotNone(outcome)
                self.assertTrue(outcome.completed)
            self.assertEqual(delivered, ["one", "one", "two", "three"])


if __name__ == "__main__":
    unittest.main()

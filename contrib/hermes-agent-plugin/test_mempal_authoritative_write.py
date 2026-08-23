import json
import tempfile
import unittest

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


if __name__ == "__main__":
    unittest.main()

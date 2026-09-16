"""Real worker abandonment with isolated durable admission and caller-owned keys."""

import concurrent.futures
import json
import threading
import unittest

from mempal import CONCLUDE_SCHEMA
from test_mempal_conclude import SharedConcludeBackend, SharedConcludeProvider


class DelayedBackend(SharedConcludeBackend):
    def __init__(self) -> None:
        super().__init__()
        self.admitted = threading.Event()
        self.release = threading.Event()
        self.lock = threading.Lock()

    def admit(self, body):
        key = body["idempotency_key"]
        operation_id = f"operation_{key}"
        with self.lock:
            self.keys.append(key)
            self.operations.setdefault(operation_id, {
                "operation_id": operation_id, "state": "queued",
            })
        self.admitted.set()
        if not self.release.wait(3):
            raise TimeoutError("fixture release missing")
        with self.lock:
            if self.operations[operation_id]["state"] != "completed":
                self.drawer_count += 1
                self.operations[operation_id].update(
                    state="completed", drawer_id=f"drawer_{self.drawer_count}",
                )
        return {"operation_id": operation_id, "state": "completed"}


class ConcludeAbandonmentTests(unittest.TestCase):
    def provider(self, backend):
        provider = SharedConcludeProvider(backend)
        provider._start_write_worker = lambda: None
        provider.initialize("fixture-session", user_id="fixture-user", profile="work")
        self.addCleanup(provider.shutdown)
        return provider

    def test_schema_requires_identity_before_admission(self):
        self.assertIn("operation_key", CONCLUDE_SCHEMA["parameters"]["required"])

    def test_missing_identity_is_rejected_without_spool_transport_or_breaker_effects(self):
        backend = SharedConcludeBackend()
        provider = self.provider(backend)
        wake = []
        provider._wake_spool_worker = lambda: wake.append(True)
        before = provider._backoff._read_state().failure_count
        result = json.loads(provider.handle_tool_call(
            "mempal_conclude", {"conclusion": "fixture private fact"},
        ))
        self.assertNotIn("result", result)
        self.assertEqual(result["error_details"]["kind"], "operation_key_required")
        self.assertEqual(provider._write_spool.count(), 0)
        self.assertEqual(backend.drawer_count, 0)
        self.assertEqual(provider.posts, [])
        self.assertEqual(wake, [])
        self.assertEqual(provider._backoff._read_state().failure_count, before)
        self.assertNotIn("fixture private fact", json.dumps(result))

    def test_outer_timeout_and_cancel_recover_identity_from_original_arguments(self):
        for abandoned in ("timeout", "cancel"):
            with self.subTest(abandoned=abandoned):
                backend = DelayedBackend()
                provider = self.provider(backend)
                # The caller records these args BEFORE dispatch; it never consumes
                # the abandoned worker's return to discover the operation identity.
                args = {"conclusion": "fixture private fact", "operation_key": abandoned}
                executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)
                future = executor.submit(provider.handle_tool_call, "mempal_conclude", args)
                try:
                    self.assertTrue(backend.admitted.wait(2))
                    self.assertIsNotNone(provider._write_spool.get(args["operation_key"]))
                    self.assertEqual(backend.drawer_count, 0)
                    if abandoned == "timeout":
                        with self.assertRaises(concurrent.futures.TimeoutError):
                            future.result(timeout=0.02)
                    else:
                        # Like Hermes: cancelling a running future cannot stop IO.
                        self.assertFalse(future.cancel())
                    self.assertFalse(future.done())
                    retry = executor.submit(provider.handle_tool_call, "mempal_conclude", args)
                    pending = json.loads(retry.result(timeout=1))
                    self.assertNotIn("result", pending)
                    self.assertEqual(pending["error_details"]["operation_key"], abandoned)
                finally:
                    backend.release.set()
                    executor.shutdown(wait=True)
                self.assertIsNone(future.exception())
                # Discard the original response just as Hermes abandonment does.
                missing = json.loads(provider.handle_tool_call(
                    "mempal_conclude", {"conclusion": args["conclusion"]},
                ))
                self.assertNotIn("result", missing)
                restarted = SharedConcludeProvider(backend)
                restarted.initialize("fixture-session", hermes_home=provider._hermes_home,
                                     user_id="fixture-user", profile="work")
                self.addCleanup(restarted.shutdown)
                stored = json.loads(restarted.handle_tool_call("mempal_conclude", args))
                self.assertEqual(stored["result"], "Fact stored.")
                self.assertEqual(stored["operation_key"], args["operation_key"])
                self.assertEqual(backend.drawer_count, 1)
                self.assertEqual(backend.keys, [args["operation_key"]])
                self.assertEqual(provider._write_spool.count(), 0)

    def test_key_reuse_cannot_cross_user_profile_or_project(self):
        backend = SharedConcludeBackend()
        provider = self.provider(backend)
        scope = {"user_id": "fixture-user", "profile": "work", "project_id": "project-a"}
        provider.initialize("fixture-session", **scope)
        args = {"conclusion": "identical scoped fact", "operation_key": "scoped-intent"}
        first = json.loads(provider.handle_tool_call("mempal_conclude", args))
        self.assertEqual(first["result"], "Fact stored.")
        for field, value in (("user_id", "other-user"), ("profile", "personal"),
                             ("project_id", "project-b")):
            with self.subTest(field=field):
                provider.initialize("fixture-session", **{**scope, field: value})
                conflict = json.loads(provider.handle_tool_call("mempal_conclude", args))
                self.assertEqual(conflict["error_details"]["kind"], "operation_key_conflict")
                self.assertNotIn("drawer_id", conflict)
                self.assertNotIn("identical scoped fact", json.dumps(conflict))
        self.assertEqual(backend.drawer_count, 1)

    def test_concurrent_identical_intentional_submissions_have_distinct_keys(self):
        backend = DelayedBackend()
        provider = self.provider(backend)
        executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)
        futures = [executor.submit(provider.handle_tool_call, "mempal_conclude", {
            "conclusion": "identical fixture fact", "operation_key": key,
        }) for key in ("intent-a", "intent-b")]
        try:
            self.assertTrue(backend.admitted.wait(2))
        finally:
            backend.release.set()
            executor.shutdown(wait=True)
        receipts = [json.loads(future.result()) for future in futures]
        self.assertEqual({item["operation_key"] for item in receipts}, {"intent-a", "intent-b"})
        self.assertEqual(len({item["drawer_id"] for item in receipts}), 2)
        self.assertEqual(backend.drawer_count, 2)


if __name__ == "__main__":
    unittest.main()

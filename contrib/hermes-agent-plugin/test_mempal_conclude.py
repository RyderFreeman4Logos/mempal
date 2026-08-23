import io
import json
import os
import sys
import tempfile
import time
import unittest
import urllib.error
from typing import Any, Dict, Optional

PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

import mempal._conclude as conclude_module  # noqa: E402
from test_mempal_provider import RecordingProvider  # noqa: E402


class ControlledConcludeProvider(RecordingProvider):
    def __init__(
        self,
        state: str,
        *,
        drawer_id: Optional[str] = None,
        status_error: Optional[Exception] = None,
    ) -> None:
        super().__init__()
        self.state = state
        self.drawer_id = drawer_id
        self.status_error = status_error

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        receipt = super()._post(path, body)
        if path == "/api/ingest/durable":
            status = {
                "operation_id": receipt["operation_id"],
                "state": self.state,
            }
            if self.drawer_id is not None:
                status["drawer_id"] = self.drawer_id
            self.durable_status[receipt["operation_id"]] = status
            receipt["state"] = self.state
        return receipt

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        if path.startswith("/api/operations/") and self.status_error is not None:
            self.gets.append((path, dict(params or {})))
            raise self.status_error
        return super()._get(path, params)


class AdmissionFailureProvider(RecordingProvider):
    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        raise urllib.error.HTTPError(
            "http://127.0.0.1:3080/api/ingest/durable?auth=SECRET_ENDPOINT",
            503,
            "Service Unavailable",
            {},
            io.BytesIO(b'{"response":"SECRET_RESPONSE_BODY"}'),
        )


class SharedConcludeBackend:
    def __init__(self, *, lose_receipt_once: bool = False) -> None:
        self.lose_receipt_once = lose_receipt_once
        self.keys = []
        self.operations: Dict[str, Dict[str, Any]] = {}
        self.drawer_count = 0

    def admit(self, body: Dict[str, Any]) -> Dict[str, Any]:
        key = str(body["idempotency_key"])
        self.keys.append(key)
        operation_id = f"operation_{key}"
        if operation_id not in self.operations:
            self.drawer_count += 1
            self.operations[operation_id] = {
                "operation_id": operation_id,
                "state": "completed",
                "drawer_id": f"drawer_{self.drawer_count}",
            }
        if self.lose_receipt_once:
            self.lose_receipt_once = False
            raise urllib.error.URLError("SECRET_LOST_RECEIPT_BODY")
        return {
            "operation_id": operation_id,
            "accepted_at": "2026-07-10T00:00:00Z",
            "state": "completed",
        }


class SharedConcludeProvider(RecordingProvider):
    def __init__(self, backend: SharedConcludeBackend) -> None:
        super().__init__()
        self.backend = backend

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        if path == "/api/ingest/durable":
            return self.backend.admit(body)
        return {"ok": True}

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        if path.startswith("/api/operations/"):
            return self.backend.operations.get(path.rsplit("/", 1)[-1], {})
        return self.responses.get(path, [])


class BrokenSpool:
    def admit(self, *_args: Any, **_kwargs: Any) -> None:
        raise OSError("SECRET_LOCAL_SPOOL_BODY")


class TransitioningBreakerProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.breaker_checks = 0

    def _is_breaker_open(self) -> bool:
        self.breaker_checks += 1
        return self.breaker_checks >= 3


class DurableConcludeTests(unittest.TestCase):
    def test_completed_receipt_with_drawer_reports_success(self) -> None:
        provider = ControlledConcludeProvider("completed", drawer_id="drawer-terminal")
        provider.initialize("session-a", user_id="alice", profile="work")

        result = self._conclude(provider, "terminal fact", operation_key="event-terminal")

        self.assertEqual(result["result"], "Fact stored.")
        self.assertEqual(result["drawer_id"], "drawer-terminal")
        self.assertEqual(result["operation_key"], "event-terminal")

    def test_pending_receipt_times_out_with_retry_safe_identity(self) -> None:
        provider = ControlledConcludeProvider("queued")
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._conclude_wait_timeout = 0.0

        result = self._conclude(provider, "pending fact", operation_key="event-pending")

        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_operation_pending")
        self.assertEqual(details["operation_id"], "operation_event-pending")
        self.assertEqual(details["operation_key"], "event-pending")
        self.assertTrue(details["retry_safe"])
        self.assertNotIn("result", result)

    def test_pending_receipt_does_not_record_breaker_failure(self) -> None:
        provider = ControlledConcludeProvider("queued")
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._conclude_wait_timeout = 0.0
        before = provider._backoff._read_state()

        self._conclude(provider, "pending fact")

        after = provider._backoff._read_state()
        self.assertEqual(after.failure_count, before.failure_count)

    def test_admission_503_never_claims_storage_and_redacts_payload(self) -> None:
        provider = AdmissionFailureProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None
        before = provider._backoff._read_state()

        result = self._conclude(provider, "SECRET_CONCLUSION")

        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_admission_deferred")
        self.assertEqual(details["error_class"], "http_503")
        self.assertEqual(details["state"], "local_admitted")
        self.assertTrue(details["retry_safe"])
        self.assertTrue(details["operation_key"])
        self.assertEqual(details["retry_operation_id"], details["operation_key"])
        self.assertEqual(provider._write_spool.count(), 1)
        self.assertEqual(provider.posts[0][1]["idempotency_key"], details["operation_key"])
        self.assertEqual(
            provider._write_spool.next_operation().operation_key,
            details["operation_key"],
        )
        after = provider._backoff._read_state()
        self.assertEqual(after.failure_count - before.failure_count, 1)
        self.assertNotIn("result", result)
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_CONCLUSION", serialized)
        self.assertNotIn("SECRET_ENDPOINT", serialized)
        self.assertNotIn("SECRET_RESPONSE_BODY", serialized)

    def test_unhealthy_provider_gate_still_returns_durable_conclude_receipt(self) -> None:
        provider = AdmissionFailureProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None
        provider._update_health(False)

        result = self._hermes_style_tool_dispatch(
            provider,
            "mempal_conclude",
            {"conclusion": "SECRET_GATE_CONCLUSION"},
        )

        self.assertEqual(result["error"], "Memory is not yet confirmed stored.")
        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_admission_deferred")
        self.assertEqual(details["error_class"], "http_503")
        self.assertTrue(details["operation_key"])
        self.assertEqual(details["retry_operation_id"], details["operation_key"])
        self.assertEqual(provider._write_spool.count(), 1)
        self.assertEqual(
            provider._write_spool.next_operation().operation_key,
            details["operation_key"],
        )
        serialized = json.dumps(result)
        self.assertNotIn("mempal temporarily unavailable", serialized)
        self.assertNotIn("SECRET_GATE_CONCLUSION", serialized)

    def test_status_lookup_failure_is_retry_safe_and_content_free(self) -> None:
        provider = ControlledConcludeProvider(
            "queued",
            status_error=RuntimeError("SECRET_STATUS_RESPONSE"),
        )
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None
        before = provider._backoff._read_state()

        result = self._conclude(provider, "SECRET_CONCLUSION", operation_key="event-status")

        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_status_unavailable")
        self.assertEqual(details["operation_id"], "operation_event-status")
        self.assertTrue(details["retry_safe"])
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_CONCLUSION", serialized)
        self.assertNotIn("SECRET_STATUS_RESPONSE", serialized)
        after = provider._backoff._read_state()
        self.assertEqual(after.failure_count - before.failure_count, 1)

    def test_terminal_failed_or_rejected_never_reports_success(self) -> None:
        for state in ("failed", "rejected"):
            with self.subTest(state=state):
                provider = ControlledConcludeProvider(state)
                provider.initialize("session-a", user_id="alice", profile="work")

                result = self._conclude(
                    provider,
                    "terminal negative",
                    operation_key=f"event-{state}",
                )

                self.assertEqual(
                    result["error_details"]["kind"],
                    f"durable_operation_{state}",
                )
                self.assertNotIn("result", result)

    def test_completed_without_drawer_never_reports_success(self) -> None:
        provider = ControlledConcludeProvider("completed")
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._conclude_wait_timeout = 0.0
        provider._start_write_worker = lambda: None
        before = provider._backoff._read_state()

        result = self._conclude(provider, "missing drawer", operation_key="event-no-drawer")

        self.assertEqual(result["error_details"]["kind"], "durable_status_invalid")
        after = provider._backoff._read_state()
        self.assertEqual(after.failure_count - before.failure_count, 1)
        self.assertNotIn("result", result)

    def test_replay_status_matrix_distinguishes_pending_and_invalid_terminal(self) -> None:
        expected = {
            "queued": ("durable_operation_pending", 0),
            "running": ("durable_operation_pending", 0),
            "unknown": ("durable_status_invalid", 1),
            "completed": ("durable_status_invalid", 1),
            "failed": ("durable_operation_failed", 1),
            "rejected": ("durable_operation_rejected", 1),
        }
        for state, (kind, failure_delta) in expected.items():
            with self.subTest(state=state):
                provider = ControlledConcludeProvider(state)
                provider.initialize("session-a", user_id="alice", profile="work")
                provider._conclude_wait_timeout = 0.0
                provider._start_write_worker = lambda: None
                before = provider._backoff._read_state()

                result = self._conclude(
                    provider, "status matrix", operation_key=f"event-{state}"
                )

                self.assertEqual(result["error_details"]["kind"], kind)
                after = provider._backoff._read_state()
                self.assertEqual(after.failure_count - before.failure_count, failure_delta)

    def test_retry_with_same_operation_key_reuses_operation_and_drawer(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        first = self._conclude(provider, "same event", operation_key="stable-event")
        second = self._conclude(provider, "same event", operation_key="stable-event")

        self.assertEqual(first["operation_id"], second["operation_id"])
        self.assertEqual(first["drawer_id"], second["drawer_id"])
        self.assertEqual(len(provider.durable_status), 1)
        self.assertEqual(
            sum(path == "/api/ingest/durable" for path, _ in provider.posts),
            1,
        )

    def test_lost_receipt_retry_reuses_generated_key_and_single_effect(self) -> None:
        backend = SharedConcludeBackend(lose_receipt_once=True)
        provider = SharedConcludeProvider(backend)
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None

        first = self._conclude(provider, "SECRET_LOST_RECEIPT_CONCLUSION")
        key = first["error_details"]["operation_key"]
        second = self._conclude(
            provider,
            "SECRET_LOST_RECEIPT_CONCLUSION",
            operation_key=key,
        )

        self.assertEqual(first["error_details"]["kind"], "durable_admission_deferred")
        self.assertEqual(second["result"], "Fact stored.")
        self.assertEqual(second["operation_key"], key)
        self.assertEqual(second["operation_id"], f"operation_{key}")
        self.assertEqual(backend.drawer_count, 1)
        self.assertEqual(set(backend.keys), {key})
        self.assertEqual(provider._write_spool.count(), 0)
        serialized = json.dumps(first)
        self.assertNotIn("SECRET_LOST_RECEIPT_CONCLUSION", serialized)
        self.assertNotIn("SECRET_LOST_RECEIPT_BODY", serialized)

    def test_lost_receipt_replays_after_provider_restart_exactly_once(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            backend = SharedConcludeBackend(lose_receipt_once=True)
            first = SharedConcludeProvider(backend)
            first.initialize("session-a", hermes_home=hermes_home)
            first._start_write_worker = lambda: None

            result = self._conclude(first, "restart conclusion")
            key = result["error_details"]["operation_key"]
            self.assertEqual(first._write_spool.count(), 1)
            first.shutdown()

            restarted = SharedConcludeProvider(backend)
            restarted.initialize("session-a", hermes_home=hermes_home)
            deadline = time.monotonic() + 3.0
            while restarted._write_spool.count() and time.monotonic() < deadline:
                time.sleep(0.05)
            restarted.shutdown()

            self.assertEqual(restarted._write_spool.count(), 0)
            self.assertEqual(backend.drawer_count, 1)
            self.assertEqual(set(backend.keys), {key})

    def test_distinct_identical_conclusions_get_distinct_operation_identity(self) -> None:
        backend = SharedConcludeBackend()
        provider = SharedConcludeProvider(backend)
        provider.initialize("session-a", user_id="alice", profile="work")
        generated = iter(("explicit-a", "explicit-b"))
        original = conclude_module.secrets.token_urlsafe
        conclude_module.secrets.token_urlsafe = lambda size: (
            next(generated) if size == 32 else original(size)
        )
        try:
            first = self._conclude(provider, "identical explicit conclusion")
            second = self._conclude(provider, "identical explicit conclusion")
        finally:
            conclude_module.secrets.token_urlsafe = original

        self.assertEqual(first["operation_key"], "explicit-a")
        self.assertEqual(second["operation_key"], "explicit-b")
        self.assertNotEqual(first["operation_id"], second["operation_id"])
        self.assertNotEqual(first["drawer_id"], second["drawer_id"])
        self.assertEqual(backend.drawer_count, 2)

    def test_local_admission_failure_returns_stable_content_free_retry_handle(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._write_spool = BrokenSpool()

        result = self._conclude(provider, "SECRET_LOCAL_CONCLUSION")

        details = result["error_details"]
        self.assertEqual(details["kind"], "local_durable_admission_failed")
        self.assertEqual(details["state"], "local_admission_failed")
        self.assertTrue(details["operation_key"])
        self.assertEqual(provider.posts, [])
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_LOCAL_CONCLUSION", serialized)
        self.assertNotIn("SECRET_LOCAL_SPOOL_BODY", serialized)

    def test_replay_breaker_deferral_returns_pending_success(self) -> None:
        provider = TransitioningBreakerProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._start_write_worker = lambda: None

        first = self._conclude(provider, "SECRET_REPLAY_CONCLUSION")
        retry_key = first["operation_key"]
        second = self._conclude(
            provider,
            "SECRET_REPLAY_CONCLUSION",
            operation_key=retry_key,
        )

        for result in (first, second):
            self.assertEqual(result["result"], "Fact admitted locally; durable storage pending.")
            self.assertEqual(result["state"], "local_admitted")
            self.assertEqual(result["operation_key"], retry_key)
            self.assertEqual(result["retry_operation_id"], retry_key)
            self.assertTrue(result["retry_safe"])
            self.assertEqual(
                result["durability"],
                {
                    "state": "pending",
                    "kind": "durable_replay_deferred",
                    "deferred_reason": "breaker_open",
                },
            )
            self.assertNotIn("error", result)
        self.assertEqual(provider._write_spool.count(), 1)
        self.assertEqual(provider.posts, [])
        self.assertNotIn("SECRET_REPLAY_CONCLUSION", json.dumps(second))

    def test_open_breaker_returns_local_admission_success_without_transport(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        for _ in range(5):
            provider._backoff.record_failure()
        before = provider._backoff._read_state()

        result = self._conclude(provider, "SECRET_BREAKER_CONCLUSION")
        after = provider._backoff._read_state()

        self.assertEqual(result["result"], "Fact admitted locally; durable storage pending.")
        self.assertEqual(result["state"], "local_admitted")
        self.assertTrue(result["operation_key"])
        self.assertEqual(result["retry_operation_id"], result["operation_key"])
        self.assertTrue(result["retry_safe"])
        self.assertEqual(
            result["durability"],
            {
                "state": "pending",
                "kind": "durable_replay_deferred",
                "deferred_reason": "breaker_open",
            },
        )
        self.assertNotIn("error", result)
        self.assertEqual(provider.posts, [])
        self.assertEqual(provider._write_spool.count(), 1)
        self.assertEqual(after.failure_count, before.failure_count)
        self.assertEqual(after.open_until_epoch, before.open_until_epoch)
        self.assertNotIn("SECRET_BREAKER_CONCLUSION", json.dumps(result))

    @staticmethod
    def _hermes_style_tool_dispatch(
        provider: RecordingProvider,
        tool_name: str,
        args: Dict[str, Any],
    ) -> Dict[str, Any]:
        if not provider.is_available():
            return {"error": "mempal temporarily unavailable. Will retry automatically."}
        return json.loads(provider.handle_tool_call(tool_name, args))

    @staticmethod
    def _conclude(
        provider: RecordingProvider,
        conclusion: str,
        *,
        operation_key: Optional[str] = None,
    ) -> Dict[str, Any]:
        args = {"conclusion": conclusion}
        if operation_key is not None:
            args["operation_key"] = operation_key
        return json.loads(provider.handle_tool_call(
            "mempal_conclude",
            args,
        ))


if __name__ == "__main__":
    unittest.main()

import io
import json
import os
import sys
import unittest
import urllib.error
from typing import Any, Dict, Optional


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

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

    def test_admission_503_never_claims_storage_and_redacts_payload(self) -> None:
        provider = AdmissionFailureProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = self._conclude(provider, "SECRET_CONCLUSION", operation_key="event-503")

        self.assertEqual(result["error_details"]["http_status"], 503)
        self.assertNotIn("result", result)
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_CONCLUSION", serialized)
        self.assertNotIn("SECRET_ENDPOINT", serialized)
        self.assertNotIn("SECRET_RESPONSE_BODY", serialized)

    def test_status_lookup_failure_is_retry_safe_and_content_free(self) -> None:
        provider = ControlledConcludeProvider(
            "queued",
            status_error=RuntimeError("SECRET_STATUS_RESPONSE"),
        )
        provider.initialize("session-a", user_id="alice", profile="work")

        result = self._conclude(provider, "SECRET_CONCLUSION", operation_key="event-status")

        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_status_unavailable")
        self.assertEqual(details["operation_id"], "operation_event-status")
        self.assertTrue(details["retry_safe"])
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_CONCLUSION", serialized)
        self.assertNotIn("SECRET_STATUS_RESPONSE", serialized)

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

        result = self._conclude(provider, "missing drawer", operation_key="event-no-drawer")

        self.assertEqual(result["error_details"]["kind"], "durable_operation_pending")
        self.assertNotIn("result", result)

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
            2,
        )

    @staticmethod
    def _conclude(
        provider: RecordingProvider,
        conclusion: str,
        *,
        operation_key: str,
    ) -> Dict[str, Any]:
        return json.loads(provider.handle_tool_call(
            "mempal_conclude",
            {"conclusion": conclusion, "operation_key": operation_key},
        ))


if __name__ == "__main__":
    unittest.main()

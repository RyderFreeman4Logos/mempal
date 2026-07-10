import http.client
import io
import json
import os
import sys
import tempfile
import unittest
import urllib.error
from typing import Any, Dict, List, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import MempalMemoryProvider, SharedPluginBackoff  # noqa: E402


class FailingPostProvider(MempalMemoryProvider):
    def __init__(self, exc: Exception) -> None:
        super().__init__()
        self._backoff_dir = tempfile.TemporaryDirectory()
        self._backoff = SharedPluginBackoff(
            path=os.path.join(self._backoff_dir.name, ".plugin_backoff")
        )
        self.exc = exc
        self.posts: List[Tuple[str, Dict[str, Any]]] = []

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        raise self.exc

    def __del__(self) -> None:
        cleanup = getattr(self, "_backoff_dir", None)
        if cleanup is not None:
            cleanup.cleanup()


class IncompleteReader:
    def read(self, _amount: int) -> bytes:
        raise http.client.IncompleteRead(b"partial stale-daemon response")


class StaleDaemonErrorTests(unittest.TestCase):
    def _conclude_error_details(self, response: Any) -> Dict[str, Any]:
        provider = FailingPostProvider(urllib.error.HTTPError(
            "http://127.0.0.1:3080/api/ingest?debug=true",
            503,
            "Service Unavailable",
            {},
            response,
        ))
        provider.initialize("session-a", user_id="alice", profile="work")
        result = json.loads(provider.handle_tool_call(
            "mempal_conclude",
            {"conclusion": "synthetic harmless durable fact"},
        ))
        return result["error_details"]

    def test_conclude_stale_daemon_503_preserves_restart_contract(self) -> None:
        response_body = json.dumps({
            "error": {
                "kind": "stale_daemon",
                "stale_daemon": True,
                "daemon_pid": 706141,
                "exe_deleted": True,
                "retryable": False,
                "retry_safe_after_restart": True,
                "message": "SECRET_RESPONSE_TEXT",
                "exe_path": "/usr/local/bin/mempal (deleted)",
            },
        }).encode("utf-8")
        provider = FailingPostProvider(urllib.error.HTTPError(
            "http://127.0.0.1:3080/api/ingest?debug=true",
            503,
            "Service Unavailable",
            {},
            io.BytesIO(response_body),
        ))
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_conclude",
            {"conclusion": "synthetic harmless durable fact"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "stale_daemon")
        self.assertEqual(details["boundary"], "daemon_executable")
        self.assertEqual(details["action"], "restart_daemon_then_retry")
        self.assertTrue(details["stale_daemon"])
        self.assertEqual(details["daemon_pid"], 706141)
        self.assertTrue(details["exe_deleted"])
        self.assertFalse(details["retryable"])
        self.assertTrue(details["retry_safe_after_restart"])
        self.assertIn("mempal daemon restart", details["recovery_hint"])
        serialized = json.dumps(result)
        self.assertNotIn("synthetic harmless durable fact", serialized)
        self.assertNotIn("SECRET_RESPONSE_TEXT", serialized)
        self.assertNotIn("/usr/local/bin/mempal", serialized)
        self.assertNotIn("127.0.0.1", serialized)
        self.assertNotIn("debug=true", serialized)

    def test_truncated_stale_daemon_body_falls_back_to_generic_503(self) -> None:
        details = self._conclude_error_details(IncompleteReader())

        self.assertEqual(details["kind"], "rest_http_error")
        self.assertTrue(details["retryable"])
        self.assertNotIn("stale_daemon", details)

    def test_extreme_json_integer_falls_back_to_generic_503(self) -> None:
        body = (
            b'{"error":{"kind":"stale_daemon","daemon_pid":'
            + (b"9" * 5000)
            + b'}}'
        )
        details = self._conclude_error_details(io.BytesIO(body))

        self.assertEqual(details["kind"], "rest_http_error")
        self.assertTrue(details["retryable"])
        self.assertNotIn("stale_daemon", details)

    def test_oversized_error_body_falls_back_to_generic_503(self) -> None:
        details = self._conclude_error_details(io.BytesIO(b"x" * (64 * 1024 + 1)))

        self.assertEqual(details["kind"], "rest_http_error")
        self.assertTrue(details["retryable"])
        self.assertNotIn("stale_daemon", details)


if __name__ == "__main__":
    unittest.main()

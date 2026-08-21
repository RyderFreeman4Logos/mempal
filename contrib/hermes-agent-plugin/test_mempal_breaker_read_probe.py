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

from test_mempal_provider import RecordingProvider  # noqa: E402
from mempal import SharedPluginBackoff  # noqa: E402


class BreakerReadProbeTests(unittest.TestCase):
    def test_open_breaker_allows_read_probe_to_return_typed_result(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = os.path.join(tmpdir, ".plugin_backoff")
            backoff = SharedPluginBackoff(path=path)
            for _ in range(5):
                backoff.record_failure()

            provider = RecordingProvider()
            provider._backoff = SharedPluginBackoff(path=path)
            provider.initialize("session-a", user_id="alice", profile="work")
            provider.responses["/api/search"] = [{
                "content": "read probe recovered",
                "importance": 4,
            }]

            result = json.loads(provider.handle_tool_call(
                "mempal_search", {"query": "test"},
            ))

            self.assertIn("results", result)
            self.assertEqual(result["results"][0]["memory"], "read probe recovered")
            self.assertEqual(provider._consecutive_failures, 0)

    def test_open_breaker_profile_failure_is_typed_and_redacted(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = os.path.join(tmpdir, ".plugin_backoff")
            backoff = SharedPluginBackoff(path=path)
            for _ in range(5):
                backoff.record_failure()

            class FailingProfileProvider(RecordingProvider):
                def _get(
                    self,
                    path: str,
                    params: Optional[Dict[str, Any]] = None,
                ) -> Any:
                    del path, params
                    raise urllib.error.URLError("SECRET_PROFILE_ENDPOINT")

            provider = FailingProfileProvider()
            provider._backoff = SharedPluginBackoff(path=path)
            provider.initialize("session-a", user_id="alice", profile="work")

            result = json.loads(provider.handle_tool_call(
                "mempal_profile", {"limit": 0},
            ))

            self.assertIn("error_details", result)
            self.assertEqual(result["error_details"]["kind"], "rest_transport_error")
            self.assertNotIn("SECRET_PROFILE_ENDPOINT", json.dumps(result))

    def test_breaker_open_allows_read_probe(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._consecutive_failures = 10
        provider._breaker_open_until = time.monotonic() + 999

        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))
        self.assertEqual(result["result"], "No relevant memories found.")
        self.assertEqual(provider._consecutive_failures, 0)


if __name__ == "__main__":
    unittest.main()

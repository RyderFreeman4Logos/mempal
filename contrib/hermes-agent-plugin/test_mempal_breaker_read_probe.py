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


class RaisingBackoff(SharedPluginBackoff):
    def __init__(self, path: str, method: str) -> None:
        super().__init__(path=path)
        self.method = method

    def record_success(self):
        if self.method == "success":
            raise OSError("backoff persistence unavailable")
        return super().record_success()

    def record_failure(self):
        if self.method == "failure":
            raise OSError("backoff persistence unavailable")
        return super().record_failure()


class BreakerReadProbeTests(unittest.TestCase):
    def test_profile_success_survives_success_bookkeeping_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            provider = RecordingProvider()
            provider._backoff = RaisingBackoff(
                os.path.join(tmpdir, ".plugin_backoff"), "success"
            )
            provider.initialize("session-a", user_id="alice", profile="work")
            provider._consecutive_failures = 5
            provider._breaker_open_until = time.monotonic() + 999
            provider.responses["/api/timeline"] = [{
                "content": "profile memory",
                "importance": 4,
            }]

            result = json.loads(provider.handle_tool_call("mempal_profile", {"limit": 5}))

            self.assertEqual(result["results"][0]["content"], "profile memory")

    def test_profile_failure_survives_failure_bookkeeping_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            class FailingProfileProvider(RecordingProvider):
                def _get(
                    self,
                    path: str,
                    params: Optional[Dict[str, Any]] = None,
                ) -> Any:
                    del path, params
                    raise urllib.error.URLError("SECRET_PROFILE_ENDPOINT")

            provider = FailingProfileProvider()
            provider._backoff = RaisingBackoff(
                os.path.join(tmpdir, ".plugin_backoff"), "failure"
            )
            provider.initialize("session-a", user_id="alice", profile="work")
            provider._consecutive_failures = 5
            provider._breaker_open_until = time.monotonic() + 999

            result = json.loads(provider.handle_tool_call("mempal_profile", {"limit": 5}))

            self.assertEqual(result["error_details"]["kind"], "rest_transport_error")
            self.assertNotIn("SECRET_PROFILE_ENDPOINT", json.dumps(result))

    def test_search_success_survives_success_bookkeeping_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            provider = RecordingProvider()
            provider._backoff = RaisingBackoff(
                os.path.join(tmpdir, ".plugin_backoff"), "success"
            )
            provider.initialize("session-a", user_id="alice", profile="work")
            provider._consecutive_failures = 5
            provider._breaker_open_until = time.monotonic() + 999
            provider.responses["/api/search"] = [{
                "content": "search memory",
                "importance": 4,
            }]

            result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))

            self.assertEqual(result["results"][0]["memory"], "search memory")

    def test_search_failure_survives_failure_bookkeeping_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            provider = RecordingProvider()
            provider._backoff = RaisingBackoff(
                os.path.join(tmpdir, ".plugin_backoff"), "failure"
            )
            provider.initialize("session-a", user_id="alice", profile="work")
            provider._consecutive_failures = 5
            provider._breaker_open_until = time.monotonic() + 999

            def failing_search(params: Dict[str, Any]) -> Any:
                del params
                raise urllib.error.URLError("SECRET_SEARCH_ENDPOINT")

            provider._search_request = failing_search

            result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))

            self.assertEqual(result["error_details"]["kind"], "search_transport_failure")
            self.assertNotIn("SECRET_SEARCH_ENDPOINT", json.dumps(result))

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

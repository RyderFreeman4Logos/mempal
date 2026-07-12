import json
import os
import sys
import unittest
from typing import Any, Dict, List, Optional


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from test_mempal_provider import RecordingProvider  # noqa: E402


class TimeoutSearchProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.search_timeouts: List[float] = []

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None, timeout: float = 10.0) -> Any:
        self.gets.append((path, dict(params or {})))
        if path == "/api/search":
            self.search_timeouts.append(timeout)
            raise TimeoutError("SECRET_QUERY_OR_ENDPOINT timed out")
        if path == "/api/status":
            return {
                "search_policy": {
                    "query_deadline_secs": 240,
                    "db_deadline_secs": 240,
                    "embed_deadline_secs": 240,
                    "reranker_timeout_secs": 240,
                }
            }
        raise TimeoutError("SECRET_QUERY_OR_ENDPOINT timed out")


class FailedSearchProvider(RecordingProvider):
    def _get(self, path: str, params: Optional[Dict[str, Any]] = None, timeout: float = 10.0) -> Any:
        self.gets.append((path, dict(params or {})))
        if path == "/api/status":
            return {
                "search_policy": {
                    "query_deadline_secs": 240,
                }
            }
        raise RuntimeError("SECRET_ENDPOINT_RESPONSE_BODY")


class MetadataSearchProvider(RecordingProvider):
    def _get(self, path: str, params: Optional[Dict[str, Any]] = None, timeout: float = 10.0) -> Any:
        self.gets.append((path, dict(params or {})))
        if path == "/api/status":
            return {
                "search_policy": {
                    "query_deadline_secs": 240,
                }
            }
        correlation_id = str((params or {})["correlation_id"])
        self._last_response_headers = {
            "degraded": "true",
            "mempal-warnings": "bounded fallback",
            "mempal-search-metadata": json.dumps({
                "correlation_id": correlation_id,
                "elapsed_ms": 7421,
                "deadline_ms": 240_000,
                "partial": True,
                "retry_safe": True,
                "fallback_used": ["bm25", "original_ranking"],
                "timeouts": [
                    {"stage": "hybrid_db", "boundary": "daemon.search_db"},
                    {"stage": "rerank", "boundary": "daemon.reranker"},
                ],
            }),
        }
        return [{
            "content": "bounded BM25 result",
            "drawer_id": "drawer-bounded",
            "importance": 4,
        }]


class LongDeadlineSearchProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.search_timeouts: List[float] = []

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None, timeout: float = 10.0) -> Any:
        self.gets.append((path, dict(params or {})))
        if path == "/api/status":
            return {
                "search_policy": {
                    "query_deadline_secs": 600,
                    "db_deadline_secs": 600,
                    "embed_deadline_secs": 600,
                    "reranker_timeout_secs": 600,
                }
            }
        if path == "/api/search":
            self.search_timeouts.append(timeout)
            return []
        return []


class SearchTimeoutContractTests(unittest.TestCase):
    def test_transport_timeout_is_structured_correlated_and_redacted(self) -> None:
        provider = TimeoutSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "SECRET_QUERY_TEXT"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "search_timeout")
        self.assertEqual(details["deadline_ms"], 240_000)
        self.assertTrue(details["retry_safe"])
        self.assertEqual(details["timeouts"][0]["stage"], "transport")
        self.assertEqual(
            details["timeouts"][0]["boundary"], "plugin.rest_transport",
        )
        self.assertTrue(details["correlation_id"].startswith("search-"))
        search_calls = [item for item in provider.gets if item[0] == "/api/search"]
        self.assertEqual(len(search_calls), 1)
        params = search_calls[0][1] or {}
        self.assertNotIn("deadline_ms", params)
        self.assertEqual(params["correlation_id"], details["correlation_id"])
        self.assertEqual(len(provider.search_timeouts), 1)
        self.assertGreaterEqual(provider.search_timeouts[0], 240.0)
        self.assertLessEqual(provider.search_timeouts[0], 250.0)
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_QUERY_TEXT", serialized)
        self.assertNotIn("SECRET_QUERY_OR_ENDPOINT", serialized)
        self.assertNotIn("127.0.0.1", serialized)

    def test_non_timeout_transport_failure_is_structured_and_redacted(self) -> None:
        provider = FailedSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "SECRET_QUERY_TEXT"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "search_transport_failure")
        self.assertTrue(details["retry_safe"])
        self.assertEqual(details["failures"], [{
            "stage": "transport",
            "boundary": "plugin.rest_transport",
            "error_class": "RuntimeError",
        }])
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_QUERY_TEXT", serialized)
        self.assertNotIn("SECRET_ENDPOINT_RESPONSE_BODY", serialized)

    def test_partial_result_reports_all_timeout_boundaries(self) -> None:
        provider = MetadataSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "bounded memory"},
        ))

        self.assertEqual(result["results"][0]["drawer_id"], "drawer-bounded")
        metadata = result["search_metadata"]
        self.assertEqual(metadata["deadline_ms"], 240_000)
        self.assertTrue(metadata["partial"])
        self.assertTrue(metadata["retry_safe"])
        self.assertEqual(metadata["fallback_used"], ["bm25", "original_ranking"])
        self.assertEqual(
            metadata["timeouts"],
            [
                {"stage": "hybrid_db", "boundary": "daemon.search_db"},
                {"stage": "rerank", "boundary": "daemon.reranker"},
            ],
        )

    def test_transport_timeout_honors_daemon_deadline_above_default(self) -> None:
        provider = LongDeadlineSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "slow local models"},
        ))

        self.assertIn("result", result)
        self.assertEqual(result["result"], "No relevant memories found.")
        search_calls = [item for item in provider.gets if item[0] == "/api/search"]
        self.assertEqual(len(search_calls), 1)
        params = search_calls[0][1] or {}
        self.assertNotIn("deadline_ms", params)
        self.assertEqual(len(provider.search_timeouts), 1)
        self.assertGreaterEqual(provider.search_timeouts[0], 600.0)
        self.assertLessEqual(provider.search_timeouts[0], 610.0)
        self.assertEqual(provider._effective_search_deadline_ms(), 600_000)


if __name__ == "__main__":
    unittest.main()

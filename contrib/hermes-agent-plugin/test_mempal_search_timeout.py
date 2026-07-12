import json
import os
import sys
import unittest
from typing import Any, Dict, Optional


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from test_mempal_provider import RecordingProvider  # noqa: E402


class TimeoutSearchProvider(RecordingProvider):
    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        raise TimeoutError("SECRET_QUERY_OR_ENDPOINT timed out")


class FailedSearchProvider(RecordingProvider):
    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        raise RuntimeError("SECRET_ENDPOINT_RESPONSE_BODY")


class MetadataSearchProvider(RecordingProvider):
    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        correlation_id = str((params or {})["correlation_id"])
        self._last_response_headers = {
            "degraded": "true",
            "mempal-warnings": "bounded fallback",
            "mempal-search-metadata": json.dumps({
                "correlation_id": correlation_id,
                "elapsed_ms": 7421,
                "deadline_ms": 8000,
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


class SearchTimeoutContractTests(unittest.TestCase):
    def test_transport_timeout_is_structured_correlated_and_redacted(self) -> None:
        provider = TimeoutSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "SECRET_QUERY_TEXT"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "search_timeout")
        self.assertEqual(details["deadline_ms"], 8000)
        self.assertTrue(details["retry_safe"])
        self.assertEqual(details["timeouts"][0]["stage"], "transport")
        self.assertEqual(
            details["timeouts"][0]["boundary"], "plugin.rest_transport",
        )
        self.assertTrue(details["correlation_id"].startswith("search-"))
        params = provider.gets[-1][1]
        self.assertEqual(params["deadline_ms"], 8000)
        self.assertEqual(params["correlation_id"], details["correlation_id"])
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
        self.assertEqual(metadata["deadline_ms"], 8000)
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


if __name__ == "__main__":
    unittest.main()

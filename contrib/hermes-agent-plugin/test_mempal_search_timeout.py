import io
import json
import os
import sys
import unittest
import urllib.error
from typing import Any, Dict, List, Optional


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from test_mempal_provider import RecordingProvider  # noqa: E402
from mempal import SearchTransportResponse  # noqa: E402


class TimeoutSearchProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.search_timeouts: List[Optional[float]] = []

    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        self.search_timeouts.append(None)
        raise TimeoutError("SECRET_QUERY_OR_ENDPOINT timed out")


class FailedSearchProvider(RecordingProvider):
    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        raise RuntimeError("SECRET_ENDPOINT_RESPONSE_BODY")


class MetadataSearchProvider(RecordingProvider):
    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        correlation_id = str(params["correlation_id"])
        headers = {
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
        return SearchTransportResponse([{
                "content": "bounded BM25 result",
                "drawer_id": "drawer-bounded",
                "importance": 4,
            }], headers)


class LongDeadlineSearchProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.search_timeouts: List[Optional[float]] = []

    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        self.search_timeouts.append(None)
        return SearchTransportResponse([], {})


class StaleLowPolicySearchProvider(RecordingProvider):
    def __init__(self) -> None:
        super().__init__()
        self.status_deadline = 1
        self.search_timeouts: List[Optional[float]] = []

    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        self.search_timeouts.append(None)
        return SearchTransportResponse([], {})


class GatewayTimeoutSearchProvider(RecordingProvider):
    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        correlation_id = str(params["correlation_id"])
        metadata = {
            "correlation_id": correlation_id,
            "elapsed_ms": 1999,
            "deadline_ms": 2000,
            "partial": True,
            "retry_safe": True,
            "fallback_used": [],
            "timeouts": [{
                "stage": "embedding",
                "boundary": "daemon.embedding",
            }],
        }
        body = json.dumps({
            "error": {
                "kind": "search_timeout",
                "status": 504,
                "message": "SECRET_DAEMON_DETAIL",
                "search_metadata": metadata,
            }
        }).encode("utf-8")
        raise urllib.error.HTTPError(
            "http://127.0.0.1:3080/api/search?SECRET_QUERY",
            504,
            "Gateway Timeout",
            {"mempal-search-metadata": json.dumps(metadata)},
            io.BytesIO(body),
        )


class SearchTimeoutContractTests(unittest.TestCase):
    def test_transport_timeout_is_structured_correlated_and_redacted(self) -> None:
        provider = TimeoutSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "SECRET_QUERY_TEXT"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "search_timeout")
        self.assertNotIn("deadline_ms", details)
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
        self.assertIsNone(provider.search_timeouts[0])
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
        self.assertNotIn("deadline_ms", details)
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

    def test_transport_has_no_finite_read_ceiling_for_deadlines_above_default(self) -> None:
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
        self.assertIsNone(provider.search_timeouts[0])

    def test_stale_low_policy_cache_never_sets_a_finite_search_read_deadline(self) -> None:
        provider = StaleLowPolicySearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.handle_tool_call("mempal_search", {"query": "first"})
        provider.status_deadline = 600

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "after upward reload"},
        ))

        self.assertEqual(result["result"], "No relevant memories found.")
        self.assertEqual(provider.search_timeouts, [None, None])

    def test_gateway_timeout_parses_allowlisted_body_and_header_metadata(self) -> None:
        provider = GatewayTimeoutSearchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_search", {"query": "SECRET_QUERY_TEXT"},
        ))

        details = result["error_details"]
        self.assertEqual(details["kind"], "search_timeout")
        self.assertEqual(details["deadline_ms"], 2000)
        self.assertEqual(details["timeouts"], [{
            "stage": "embedding",
            "boundary": "daemon.embedding",
        }])
        serialized = json.dumps(result)
        self.assertNotIn("SECRET_QUERY_TEXT", serialized)
        self.assertNotIn("SECRET_QUERY", serialized)
        self.assertNotIn("SECRET_DAEMON_DETAIL", serialized)


if __name__ == "__main__":
    unittest.main()

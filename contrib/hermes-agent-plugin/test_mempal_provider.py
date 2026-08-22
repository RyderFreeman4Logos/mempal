import json
import importlib.util
import os
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
from typing import Any, Dict, List, Optional, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import (  # noqa: E402
    MempalMemoryProvider,
    _LLMClient,
    _IntelligenceEnhancer,
    _encode_query_params,
    SearchTransportResponse,
    SharedPluginBackoff,
)


class DiscoveryTests(unittest.TestCase):
    def test_user_plugin_marker_is_visible_to_hermes_scan_window(self) -> None:
        init_path = os.path.join(PLUGIN_DIR, "mempal", "__init__.py")
        with open(init_path, encoding="utf-8") as handle:
            scan_window = handle.read(8192)

        self.assertIn("register_memory_provider", scan_window)
        self.assertIn("MemoryProvider", scan_window)

    def test_rest_query_encoding_uses_serde_compatible_booleans(self) -> None:
        encoded = _encode_query_params({"include_raw_turns": False, "active": True})

        self.assertIn("include_raw_turns=false", encoded)
        self.assertIn("active=true", encoded)

    def test_hooks_rest_query_encoding_uses_serde_compatible_booleans(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        spec = importlib.util.spec_from_file_location("mempal_hooks_for_test", hooks_path)
        if spec is None or spec.loader is None:
            self.fail("failed to load mempal-hooks plugin module")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        encoded = module._encode_query_params({"include_raw_turns": False, "active": True})

        self.assertIn("include_raw_turns=false", encoded)
        self.assertIn("active=true", encoded)

    def test_hooks_plugin_loads_without_mempal_package_import(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        original_path = list(sys.path)
        original_mempal = sys.modules.pop("mempal", None)
        try:
            sys.path = [path for path in sys.path if path != PLUGIN_DIR]
            spec = importlib.util.spec_from_file_location("mempal_hooks_standalone_test", hooks_path)
            if spec is None or spec.loader is None:
                self.fail("failed to load mempal-hooks plugin module")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)

            self.assertTrue(hasattr(module, "SharedPluginBackoff"))
        finally:
            sys.path = original_path
            if original_mempal is not None:
                sys.modules["mempal"] = original_mempal

class RecordingProvider(MempalMemoryProvider):
    def __init__(self) -> None:
        super().__init__()
        self._backoff_dir = tempfile.TemporaryDirectory()
        self._backoff = SharedPluginBackoff(
            path=os.path.join(self._backoff_dir.name, ".plugin_backoff")
        )
        self.gets: List[Tuple[str, Optional[Dict[str, Any]]]] = []
        self.posts: List[Tuple[str, Dict[str, Any]]] = []
        self.responses: Dict[str, Any] = {}
        self.durable_status = {}

    def initialize(self, session_id: str, **kwargs) -> None:
        kwargs.setdefault("hermes_home", self._backoff_dir.name)
        super().initialize(session_id, **kwargs)

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        if path.startswith("/api/operations/"):
            return self.durable_status.get(path.rsplit("/", 1)[-1], {})
        return self.responses.get(path, [])

    def _search_request(self, params: Dict[str, Any]) -> SearchTransportResponse:
        self.gets.append(("/api/search", dict(params)))
        return SearchTransportResponse(self.responses.get("/api/search", []), {})

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        if path in {"/api/ingest/durable", "/api/delete/durable"}:
            operation_id = f"operation_{body['idempotency_key']}"
            request = body["request"]
            drawer_id = request.get("drawer_id") or f"drawer_{len(self.durable_status) + 1}"
            self.durable_status.setdefault(operation_id, {
                "operation_id": operation_id,
                "state": "completed",
                "drawer_id": drawer_id,
            })
            return {
                "operation_id": operation_id,
                "state": "completed",
            }
        return {"ok": True, "drawer_id": f"drawer_{len(self.posts)}"}

    def _turn_storage_mode(self) -> str:
        return "raw_evidence"

    def _drain_writes(self) -> None:
        self._write_queue.join()
        self._write_stop.set()
        if self._write_worker:
            self._write_worker.join(timeout=2.0)
        self._write_worker = None
        self._write_stop.clear()

    def __del__(self) -> None:
        cleanup = getattr(self, "_backoff_dir", None)
        if cleanup is not None:
            cleanup.cleanup()


class StatusRecordingProvider(MempalMemoryProvider):
    def __init__(self) -> None:
        super().__init__()
        self._backoff_dir = tempfile.TemporaryDirectory()
        self._backoff = SharedPluginBackoff(
            path=os.path.join(self._backoff_dir.name, ".plugin_backoff")
        )
        self.gets: List[Tuple[str, Optional[Dict[str, Any]]]] = []
        self.responses: Dict[str, Any] = {}

    def initialize(self, session_id: str, **kwargs) -> None:
        kwargs.setdefault("hermes_home", self._backoff_dir.name)
        super().initialize(session_id, **kwargs)

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        return self.responses.get(path, {})

    def __del__(self) -> None:
        cleanup = getattr(self, "_backoff_dir", None)
        if cleanup is not None:
            cleanup.cleanup()


class FailingPostProvider(RecordingProvider):
    def __init__(self, exc: Exception) -> None:
        super().__init__()
        self.exc = exc

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        raise self.exc


class MempalProviderScopeTests(unittest.TestCase):
    def test_two_profiles_use_distinct_wings(self) -> None:
        work = RecordingProvider()
        personal = RecordingProvider()
        work.initialize("session-a", user_id="alice", profile="work")
        personal.initialize("session-b", user_id="alice", profile="personal")

        work.handle_tool_call("mempal_conclude", {"conclusion": "likes vim"})
        personal.handle_tool_call("mempal_conclude", {"conclusion": "likes emacs"})

        self.assertEqual(work.posts[-1][1]["request"]["wing"], "hermes-user/alice/work")
        self.assertEqual(personal.posts[-1][1]["request"]["wing"], "hermes-user/alice/personal")

    def test_same_profile_facts_are_shared_across_chats(self) -> None:
        chat_a = RecordingProvider()
        chat_b = RecordingProvider()
        chat_a.initialize("session-a", user_id="alice", profile="work", chat_id="chat-a")
        chat_b.initialize("session-b", user_id="alice", profile="work", chat_id="chat-b")

        chat_a.handle_tool_call("mempal_conclude", {"conclusion": "prefers concise answers"})
        chat_b.handle_tool_call("mempal_conclude", {"conclusion": "prefers citations"})

        self.assertEqual(
            chat_a.posts[-1][1]["request"]["wing"],
            chat_b.posts[-1][1]["request"]["wing"],
        )
        self.assertEqual(chat_a.posts[-1][1]["request"]["room"], "facts")
        self.assertEqual(chat_b.posts[-1][1]["request"]["room"], "facts")

    def test_chat_and_thread_ids_scope_turn_rooms(self) -> None:
        chat_only = RecordingProvider()
        threaded = RecordingProvider()
        chat_only.initialize("session-a", user_id="alice", profile="work", platform="slack", chat_id="chat-a")
        threaded.initialize(
            "session-b",
            user_id="alice",
            profile="work",
            platform="slack",
            chat_id="chat-a",
            thread_id="thread-1",
        )

        chat_only.sync_turn("hello", "hi")
        threaded.sync_turn("hello", "hi")
        chat_only._drain_writes()
        threaded._drain_writes()

        self.assertEqual(
            chat_only.posts[-1][1]["request"]["room"], "turns/slack/chat-a"
        )
        self.assertEqual(
            threaded.posts[-1][1]["request"]["room"],
            "turns/slack/chat-a/thread-1",
        )

    def test_project_id_passes_through_rest_calls(self) -> None:
        provider = RecordingProvider()
        provider.initialize(
            "session-a",
            user_id="alice",
            profile="work",
            project_id="project-alpha",
            chat_id="chat-a",
        )

        provider.responses["/api/timeline"] = [{"content": "fact"}]
        provider.handle_tool_call("mempal_profile", {"limit": 5})
        provider.handle_tool_call("mempal_search", {"query": "fact", "top_k": 3})
        provider.handle_tool_call("mempal_conclude", {"conclusion": "durable fact"})
        provider.sync_turn("hello", "hi")
        provider._drain_writes()

        timeline_params = provider.gets[0][1]
        search_params = provider.gets[1][1]
        ingest_bodies = [
            body["request"]
            for path, body in provider.posts
            if path == "/api/ingest/durable"
        ]

        self.assertEqual(timeline_params["project_id"], "project-alpha")
        self.assertEqual(search_params["project_id"], "project-alpha")
        self.assertTrue(ingest_bodies)
        self.assertTrue(all(body["project_id"] == "project-alpha" for body in ingest_bodies))

    def test_cwd_fallback_uses_directory_basename_as_project_id(self) -> None:
        provider = RecordingProvider()
        provider.initialize(
            "session-a",
            user_id="alice",
            profile="work",
            cwd="/home/alice/my-project/",
            chat_id="chat-a",
        )

        provider.responses["/api/timeline"] = [{"content": "fact"}]
        provider.handle_tool_call("mempal_profile", {"limit": 5})
        provider.handle_tool_call("mempal_search", {"query": "fact", "top_k": 3})
        provider.handle_tool_call("mempal_conclude", {"conclusion": "durable fact"})

        timeline_params = provider.gets[0][1]
        search_params = provider.gets[1][1]
        ingest_bodies = [
            body["request"]
            for path, body in provider.posts
            if path == "/api/ingest/durable"
        ]

        self.assertEqual(timeline_params["project_id"], "my-project")
        self.assertEqual(search_params["project_id"], "my-project")
        self.assertTrue(ingest_bodies)
        self.assertTrue(all(body["project_id"] == "my-project" for body in ingest_bodies))

    def test_session_switch_clears_prefetch_results(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        with provider._prefetch_lock:
            provider._prefetch_result = "stale"
            provider._prefetch_results["session-a"] = "stale"

        provider.on_session_switch("session-b", reason="reset")

        self.assertEqual(provider.prefetch("anything", session_id="session-a"), "")
        self.assertEqual(provider.prefetch("anything", session_id="session-b"), "")

    def test_prefetch_is_keyed_by_session_and_project(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work", project_id="project-alpha")
        provider.responses["/api/search"] = [{"content": "session scoped memory"}]

        provider.queue_prefetch("memory", session_id="session-a")
        result = provider.prefetch("memory", session_id="session-a")

        self.assertIn("session scoped memory", result)
        self.assertEqual(provider.gets[-1][1]["project_id"], "project-alpha")
        self.assertEqual(provider.prefetch("memory", session_id="session-b"), "")

    def test_memory_write_routes_by_target(self) -> None:
        provider = RecordingProvider()
        provider.initialize(
            "session-a",
            user_id="alice",
            profile="work",
            project_id="project-alpha",
            chat_id="chat-a",
        )

        provider.on_memory_write("add", "profile", "profile fact")
        provider._drain_writes()
        provider.on_memory_write("add", "turns", "turn content")
        provider._drain_writes()

        self.assertEqual(provider.posts[-2][1]["request"]["room"], "facts")
        self.assertEqual(provider.posts[-1][1]["request"]["room"], "turns/cli/chat-a")
        self.assertEqual(provider.posts[-1][1]["request"]["project_id"], "project-alpha")


class SearchResultTests(unittest.TestCase):
    def test_search_returns_typed_fields(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "fact", "drawer_id": "d1", "memory_kind": "preference", "importance": 3},
        ]

        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))
        self.assertEqual(result["results"][0]["drawer_id"], "d1")
        self.assertEqual(result["results"][0]["memory_kind"], "preference")

    def test_search_strips_none_fields(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "fact", "drawer_id": "d1", "domain": None},
        ]

        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))
        self.assertNotIn("domain", result["results"][0])

    def test_conclude_returns_drawer_id(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call("mempal_conclude", {"conclusion": "test fact"}))
        self.assertIn("drawer_id", result)

    def test_conclude_http_500_returns_retry_safe_pending_handle(self) -> None:
        provider = FailingPostProvider(urllib.error.HTTPError(
            "http://127.0.0.1:3080/api/ingest?debug=true",
            500,
            "Internal Server Error",
            {},
            None,
        ))
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(provider.handle_tool_call(
            "mempal_conclude",
            {"conclusion": "synthetic harmless durable fact"},
        ))

        self.assertEqual(result["error"], "Memory is not yet confirmed stored.")
        details = result["error_details"]
        self.assertEqual(details["kind"], "durable_admission_deferred")
        self.assertEqual(details["error_class"], "http_500")
        self.assertTrue(details["retry_safe"])
        self.assertTrue(details["operation_key"])
        self.assertEqual(provider._write_spool.count(), 1)
        serialized = json.dumps(result)
        self.assertNotIn("synthetic harmless durable fact", serialized)
        self.assertNotIn("127.0.0.1", serialized)
        self.assertNotIn("debug=true", serialized)
        self.assertNotIn("HTTP Error 500", serialized)

    def test_profile_returns_drawer_id(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/timeline"] = [
            {"content": "fact", "drawer_id": "d1", "importance": 3, "added_at": "2026-01-01"},
        ]

        result = json.loads(provider.handle_tool_call("mempal_profile", {"limit": 5}))
        self.assertEqual(result["results"][0]["drawer_id"], "d1")
        self.assertEqual(result["results"][0]["importance"], 3)


class SafeModeTests(unittest.TestCase):
    def test_search_filters_low_importance_raw_evidence_and_labels_background(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "low raw turn", "drawer_id": "raw1", "memory_kind": "evidence", "importance": 1},
            {"content": "durable fact", "drawer_id": "fact1", "memory_kind": "profile_fact", "importance": 1},
            {"content": "important evidence", "drawer_id": "ev1", "memory_kind": "evidence", "importance": 4},
        ]

        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "memory"}))

        drawer_ids = [item["drawer_id"] for item in result["results"]]
        self.assertNotIn("raw1", drawer_ids)
        self.assertEqual(drawer_ids, ["ev1", "fact1"])
        self.assertEqual(result["results"][0]["authority"], "evidence/background")
        self.assertFalse(provider.gets[-1][1]["include_raw_turns"])

    def test_search_labels_pinned_and_canonical_as_authoritative(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "canonical rule", "drawer_id": "c1", "status": "canonical", "memory_kind": "evidence", "importance": 0},
            {"content": "pinned rule", "drawer_id": "p1", "is_pinned": True, "memory_kind": "evidence", "importance": 0},
        ]

        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "rule"}))

        self.assertEqual([item["authority"] for item in result["results"]], ["authoritative", "authoritative"])

    def test_prefetch_preserves_citations_and_budget(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._safe_context_budget_chars = 140
        provider.responses["/api/search"] = [
            {"content": "important cited memory", "drawer_id": "d1", "source": "source-a", "memory_kind": "evidence", "importance": 4},
            {"content": "second important memory that should exceed the tiny budget", "drawer_id": "d2", "source": "source-b", "memory_kind": "evidence", "importance": 4},
        ]

        provider.queue_prefetch("memory", session_id="session-a")
        block = provider.prefetch("memory", session_id="session-a")

        self.assertIn("drawer_id: d1", block)
        self.assertIn("source: source-a", block)
        self.assertNotIn("drawer_id: d2", block)
        self.assertIn("evidence/background", block)

    def test_safe_mode_can_be_disabled_by_config(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            with open(os.path.join(hermes_home, "mempal.json"), "w", encoding="utf-8") as handle:
                json.dump({"safe_mode": {"enabled": False, "include_raw_turns": True}}, handle)
            provider = RecordingProvider()
            provider.initialize("session-a", hermes_home=hermes_home, user_id="alice", profile="work")
            provider.responses["/api/search"] = [
                {"content": "low raw turn", "drawer_id": "raw1", "memory_kind": "evidence", "importance": 0},
            ]

            result = json.loads(provider.handle_tool_call("mempal_search", {"query": "raw"}))

            self.assertEqual(result["results"][0]["drawer_id"], "raw1")
            self.assertTrue(provider.gets[-1][1]["include_raw_turns"])


class PluginBackoffTests(unittest.TestCase):
    def test_degraded_prefetch_response_records_failure(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {
                "content": "partial memory",
                "drawer_id": "d1",
                "source": "source-a",
                "importance": 4,
                "warnings": ["deadline_hit"],
            },
        ]

        provider.queue_prefetch("memory", session_id="session-a")
        block = provider.prefetch("memory", session_id="session-a")

        self.assertIn("partial memory", block)
        self.assertEqual(provider._consecutive_failures, 1)

    def test_degraded_prefetch_responses_open_breaker(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "partial memory", "importance": 4, "warnings": ["deadline_hit"]},
        ]

        for _ in range(5):
            provider.queue_prefetch("memory", session_id="session-a")
            provider.prefetch("memory", session_id="session-a")

        self.assertTrue(provider._is_breaker_open())

    def test_prefetch_uses_supported_retrieval_params(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = [
            {"content": "cheap memory", "drawer_id": "d1", "source": "source-a", "importance": 4},
        ]

        provider.queue_prefetch("memory", session_id="session-a")
        provider.prefetch("memory", session_id="session-a")

        params = provider.gets[-1][1]
        self.assertEqual(params["top_k"], 5)
        self.assertNotIn("mode", params)
        self.assertNotIn("rerank", params)

    def test_search_warning_header_records_failure(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        def search_with_warning_header(params: Dict[str, Any]) -> SearchTransportResponse:
            provider.gets.append(("/api/search", dict(params)))
            headers = {
                "degraded": "true",
                "mempal-warnings": "embedding deadline exceeded after 5s",
            }
            return SearchTransportResponse(
                [{"content": "partial memory", "importance": 4}],
                headers,
            )

        provider._search_request = search_with_warning_header

        provider.queue_prefetch("memory", session_id="session-a")
        block = provider.prefetch("memory", session_id="session-a")

        self.assertIn("partial memory", block)
        self.assertEqual(provider._consecutive_failures, 1)

    def test_shared_breaker_suppresses_hooks_ingest(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        spec = importlib.util.spec_from_file_location("mempal_hooks_backoff_test", hooks_path)
        if spec is None or spec.loader is None:
            self.fail("failed to load mempal-hooks plugin module")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        with tempfile.TemporaryDirectory() as tmpdir:
            backoff = SharedPluginBackoff(path=os.path.join(tmpdir, ".plugin_backoff"))
            for _ in range(5):
                backoff.record_failure()

            hooks = module._MempalHooks()
            hooks._initialized = True
            hooks._backoff = SharedPluginBackoff(path=backoff.path)
            posts: List[Tuple[str, Dict[str, Any]]] = []
            hooks._post = lambda path, body: posts.append((path, dict(body)))

            hooks.post_tool_call("bash", {}, "x" * 80)

            self.assertEqual(posts, [])

    def test_shared_breaker_concurrent_writes_use_unique_temp_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = os.path.join(tmpdir, ".plugin_backoff")
            errors: List[BaseException] = []

            def write_backoff() -> None:
                try:
                    SharedPluginBackoff(path=path).record_failure()
                except BaseException as exc:
                    errors.append(exc)

            threads = [threading.Thread(target=write_backoff) for _ in range(20)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()

            self.assertEqual(errors, [])
            with open(path, encoding="utf-8") as handle:
                payload = json.load(handle)
            self.assertIn("failure_count", payload)
            leftovers = [name for name in os.listdir(tmpdir) if name.startswith(".plugin_backoff.tmp.")]
            self.assertEqual(leftovers, [])

    def test_hooks_degraded_search_response_records_failure(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        spec = importlib.util.spec_from_file_location("mempal_hooks_degraded_test", hooks_path)
        if spec is None or spec.loader is None:
            self.fail("failed to load mempal-hooks plugin module")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        with tempfile.TemporaryDirectory() as tmpdir:
            hooks = module._MempalHooks()
            hooks._initialized = True
            hooks._wing = "hermes-user/alice/work"
            hooks._backoff = SharedPluginBackoff(path=os.path.join(tmpdir, ".plugin_backoff"))
            class DegradedTransport:
                def get_json(self, path: str, params: Dict[str, Any]) -> Any:
                    del path, params
                    return module.SearchTransportResponse([{
                        "content": "partial hooks memory",
                        "importance": 4,
                        "warnings": ["deadline_hit"],
                    }], {})

            hooks._search_transport = DegradedTransport()

            result = hooks.pre_llm_call("session-a", "memory")

            self.assertIsNotNone(result)
            self.assertEqual(hooks._consecutive_failures, 1)

    def test_hooks_session_start_warmup_and_tiered_context(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        spec = importlib.util.spec_from_file_location("mempal_hooks_warmup_test", hooks_path)
        if spec is None or spec.loader is None:
            self.fail("failed to load mempal-hooks plugin module")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        with tempfile.TemporaryDirectory() as tmpdir:
            hooks = module._MempalHooks()
            hooks._initialized = True
            hooks._wing = "hermes-user/alice/work"
            hooks._backoff = SharedPluginBackoff(path=os.path.join(tmpdir, ".plugin_backoff"))
            hooks._get = lambda path, params=None: {
                "markdown": "brief: project prefers atomic commits",
            }
            class SearchTransport:
                def get_json(self, path: str, params: Dict[str, Any]) -> Any:
                    del path, params
                    return module.SearchTransportResponse(
                        [
                            {
                                "content": "decision: always write RED first",
                                "memory_kind": "decision",
                                "importance": 4,
                            },
                            {
                                "content": "note: raw log",
                                "memory_kind": "observation",
                                "importance": 1,
                            },
                        ],
                        {},
                    )

            hooks._search_transport = SearchTransport()

            hooks.on_session_start("session-warmup")
            result = hooks.pre_llm_call("session-warmup", "how do we ship?")
            self.assertIsInstance(result, dict)
            assert result is not None
            context = result["context"]
            self.assertIn("Session warmup", context)
            self.assertIn("brief: project prefers atomic commits", context)
            self.assertIn("High-signal", context)
            self.assertIn("decision: always write RED first", context)
            # Warmup is one-shot.
            self.assertNotIn("session-warmup", hooks._session_warmup)

            # Broader tool allowlist accepts terminal observations.
            posts: List[Tuple[str, Dict[str, Any]]] = []
            hooks._post = lambda path, body: posts.append((path, dict(body)))
            hooks.post_tool_call("terminal", {"command": "ls"}, "x" * 80)
            self.assertEqual(len(posts), 1)

    def test_shared_breaker_allows_operation_after_cooldown(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = os.path.join(tmpdir, ".plugin_backoff")
            backoff = SharedPluginBackoff(path=path)
            for _ in range(5):
                backoff.record_failure()
            with open(path, "w", encoding="utf-8") as handle:
                json.dump({"failure_count": 5, "open_until_epoch": time.time() - 1}, handle)

            provider = RecordingProvider()
            provider._backoff = SharedPluginBackoff(path=path)
            provider.initialize("session-a", user_id="alice", profile="work")
            provider.responses["/api/search"] = [{"content": "found after cooldown", "importance": 4}]

            result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))

            self.assertIn("results", result)
            self.assertEqual(result["results"][0]["memory"], "found after cooldown")

    def test_turn_storage_mode_is_cached(self) -> None:
        provider = StatusRecordingProvider()
        provider.responses["/api/status"] = {
            "turn_storage": {"storage_mode": "raw_evidence"},
        }

        self.assertEqual(provider._turn_storage_mode(), "raw_evidence")
        self.assertEqual(provider._turn_storage_mode(), "raw_evidence")

        self.assertEqual(provider.gets, [("/api/status", {})])


class PinnedFactsTests(unittest.TestCase):
    def test_pinned_facts_in_system_prompt(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/pinned_facts"] = [
            {"content": "always remember this", "memory_kind": "rule", "importance": 5},
        ]

        block = provider.system_prompt_block()
        self.assertIn("Pinned Facts", block)
        self.assertIn("always remember this", block)

    def test_pinned_facts_cache_ttl(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/pinned_facts"] = [
            {"content": "cached fact", "memory_kind": "fact", "importance": 3},
        ]

        provider.system_prompt_block()
        first_call_count = len(provider.gets)
        provider.system_prompt_block()
        self.assertEqual(len(provider.gets), first_call_count)

    def test_pinned_facts_empty_no_section(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/pinned_facts"] = []

        block = provider.system_prompt_block()
        self.assertNotIn("Pinned Facts", block)

    def test_session_switch_invalidates_pinned_cache(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/pinned_facts"] = [
            {"content": "fact", "memory_kind": "fact", "importance": 1},
        ]

        provider.system_prompt_block()
        call_count_before = len(provider.gets)
        provider.on_session_switch("session-b", reason="reset")
        provider.system_prompt_block()

        self.assertGreater(len(provider.gets), call_count_before)


class IntelligenceModeTests(unittest.TestCase):
    def test_default_mode_is_deterministic(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        self.assertEqual(provider._intelligence_mode, "deterministic")
        self.assertFalse(provider._should_enhance())

    def test_deterministic_mode_no_llm_calls(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.sync_turn("hello there friend", "hi back to you friend")
        provider._drain_writes()
        self.assertEqual(len(provider.posts), 1)
        self.assertNotIn("source_type", provider.posts[0][1]["request"])

    def test_system_prompt_shows_mode(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/pinned_facts"] = []
        block = provider.system_prompt_block()
        self.assertIn("Mode: deterministic", block)

    def test_invalid_mode_falls_back_to_deterministic(self) -> None:
        provider = RecordingProvider()
        provider._hermes_home = ""
        provider._configure_intelligence({"memory_intelligence": {"mode": "invalid_mode"}})
        self.assertEqual(provider._intelligence_mode, "deterministic")

    def test_auto_without_llm_config_is_deterministic(self) -> None:
        provider = RecordingProvider()
        provider._configure_intelligence({"memory_intelligence": {"mode": "auto"}})
        self.assertEqual(provider._intelligence_mode, "deterministic")
        self.assertFalse(provider._should_enhance())

    def test_auto_with_llm_config_enables_enhancement(self) -> None:
        provider = RecordingProvider()
        provider._configure_intelligence({
            "memory_intelligence": {
                "mode": "auto",
                "llm": {"base_url": "http://localhost:18009/v1", "model": "test-model"},
            },
        })
        self.assertEqual(provider._intelligence_mode, "auto")
        self.assertTrue(provider._should_enhance())
        self.assertIsNotNone(provider._enhancer)

    def test_local_llm_mode_configures_enhancer(self) -> None:
        provider = RecordingProvider()
        provider._configure_intelligence({
            "memory_intelligence": {
                "mode": "local_llm",
                "llm": {"base_url": "http://localhost:18009/v1", "model": "qwen3.6-27b"},
            },
        })
        self.assertEqual(provider._intelligence_mode, "local_llm")
        self.assertIsNotNone(provider._enhancer)
        self.assertEqual(provider._llm._model, "qwen3.6-27b")

    def test_llm_breaker_disables_enhancement(self) -> None:
        provider = RecordingProvider()
        provider._configure_intelligence({
            "memory_intelligence": {
                "mode": "local_llm",
                "llm": {"base_url": "http://localhost:18009/v1", "model": "test"},
            },
        })
        self.assertTrue(provider._should_enhance())
        provider._llm._consecutive_failures = 10
        provider._llm._breaker_open_until = time.monotonic() + 999
        self.assertFalse(provider._should_enhance())

    def test_config_schema_includes_intelligence_fields(self) -> None:
        provider = RecordingProvider()
        schema = provider.get_config_schema()
        keys = [s["key"] for s in schema]
        self.assertIn("memory_intelligence.mode", keys)
        self.assertIn("memory_intelligence.llm.base_url", keys)
        self.assertIn("memory_intelligence.llm.model", keys)


class LLMClientTests(unittest.TestCase):
    def test_unconfigured_returns_none(self) -> None:
        client = _LLMClient({})
        self.assertFalse(client.is_configured)
        self.assertIsNone(client.chat("system", "user"))
        self.assertEqual(client.status, "not_configured")

    def test_configured_status(self) -> None:
        client = _LLMClient({"base_url": "http://localhost:18009/v1", "model": "test"})
        self.assertTrue(client.is_configured)
        self.assertEqual(client.status, "available")

    def test_breaker_open_status(self) -> None:
        client = _LLMClient({"base_url": "http://localhost:18009/v1", "model": "test"})
        client._consecutive_failures = 5
        client._breaker_open_until = time.monotonic() + 999
        self.assertEqual(client.status, "breaker_open")
        self.assertIsNone(client.chat("system", "user"))

    def test_extra_body_preserved(self) -> None:
        client = _LLMClient({
            "base_url": "http://localhost:18009/v1",
            "model": "qwen3",
            "extra_body": {"chat_template_kwargs": {"enable_thinking": False}},
        })
        self.assertEqual(client._extra_body, {"chat_template_kwargs": {"enable_thinking": False}})


class MetadataValidationTests(unittest.TestCase):
    def test_valid_metadata_json(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata(
            '{"memory_kind": "preference", "domain": "coding", "importance": 4, "tags": ["vim", "editor"]}'
        )
        self.assertIsNotNone(result)
        self.assertEqual(result["memory_kind"], "preference")
        self.assertEqual(result["domain"], "coding")
        self.assertEqual(result["importance"], 4)
        self.assertEqual(result["tags"], ["vim", "editor"])

    def test_metadata_strips_code_fence(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata(
            '```json\n{"memory_kind": "fact", "importance": 3}\n```'
        )
        self.assertIsNotNone(result)
        self.assertEqual(result["memory_kind"], "fact")

    def test_invalid_json_returns_none(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata("not json at all")
        self.assertIsNone(result)

    def test_invalid_memory_kind_excluded(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata(
            '{"memory_kind": "hallucination", "importance": 3}'
        )
        self.assertIsNotNone(result)
        self.assertNotIn("memory_kind", result)
        self.assertEqual(result["importance"], 3)

    def test_importance_clamped(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata('{"importance": 99}')
        self.assertIsNone(result)

    def test_empty_result_returns_none(self) -> None:
        result = _IntelligenceEnhancer._validate_metadata('{"memory_kind": "invalid"}')
        self.assertIsNone(result)


class FactExtractionValidationTests(unittest.TestCase):
    def test_valid_facts_extracted(self) -> None:
        source = "User: I always prefer dark mode in my editor and terminal applications"
        raw = json.dumps([
            {"fact": "User prefers dark mode in editor and terminal", "memory_kind": "preference", "importance": 3},
        ])
        result = _IntelligenceEnhancer._validate_facts(raw, source)
        self.assertIsNotNone(result)
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["memory_kind"], "preference")

    def test_hallucinated_facts_rejected(self) -> None:
        source = "User: I like Python"
        raw = json.dumps([
            {"fact": "User has extensive experience with quantum computing frameworks", "importance": 4},
        ])
        result = _IntelligenceEnhancer._validate_facts(raw, source)
        self.assertIsNone(result)

    def test_empty_array_returns_none(self) -> None:
        result = _IntelligenceEnhancer._validate_facts("[]", "some source")
        self.assertIsNone(result)

    def test_code_fence_stripped(self) -> None:
        source = "User: I prefer using vim for all code editing tasks"
        raw = '```json\n[{"fact": "prefers vim for code editing", "importance": 2}]\n```'
        result = _IntelligenceEnhancer._validate_facts(raw, source)
        self.assertIsNotNone(result)

    def test_max_facts_capped(self) -> None:
        source = "word " * 100
        facts = [{"fact": f"word word word word fact {i}", "importance": 1} for i in range(15)]
        result = _IntelligenceEnhancer._validate_facts(json.dumps(facts), source)
        if result:
            self.assertLessEqual(len(result), 10)


class ReadinessActivationTests(unittest.TestCase):
    """Provider activation: config loading, availability, REST-down behavior."""

    def test_no_base_url_makes_unavailable(self) -> None:
        provider = RecordingProvider()
        provider._hermes_home = ""
        old = os.environ.get("MEMPAL_BASE_URL")
        try:
            os.environ["MEMPAL_BASE_URL"] = ""
            self.assertFalse(provider.is_available())
        finally:
            if old is not None:
                os.environ["MEMPAL_BASE_URL"] = old
            else:
                os.environ.pop("MEMPAL_BASE_URL", None)

    def test_provider_name_is_mempal(self) -> None:
        provider = RecordingProvider()
        self.assertEqual(provider.name, "mempal")

    def test_tool_schemas_registered(self) -> None:
        provider = RecordingProvider()
        schemas = provider.get_tool_schemas()
        names = {s["name"] for s in schemas}
        self.assertEqual(names, {"mempal_profile", "mempal_search", "mempal_conclude"})

    def test_breaker_resets_after_cooldown(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider._consecutive_failures = 10
        provider._breaker_open_until = time.monotonic() - 1

        provider.responses["/api/search"] = [{"content": "found"}]
        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "test"}))
        self.assertIn("results", result)

    def test_recent_unhealthy_transport_does_not_disable_local_durable_provider(self) -> None:
        provider = RecordingProvider()
        provider._is_healthy = False
        provider._last_health_at = time.monotonic()
        self.assertTrue(provider.is_available())


class ReadinessDurableMemoryTests(unittest.TestCase):
    """Durable memory semantics: add/replace/remove correctness."""

    def test_add_creates_one_ingest(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.on_memory_write("add", "profile", "single fact")
        provider._drain_writes()
        ingests = [b["request"] for p, b in provider.posts if p == "/api/ingest/durable"]
        self.assertEqual(len(ingests), 1)
        self.assertEqual(ingests[0]["content"], "single fact")

    def test_replace_supersedes_and_creates_new(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.on_memory_write("add", "profile", "v1")
        provider._drain_writes()
        provider.on_memory_write("replace", "profile", "v2")
        provider._drain_writes()
        ingests = [b["request"] for p, b in provider.posts if p == "/api/ingest/durable"]
        self.assertEqual(len(ingests), 2)
        self.assertIn("supersedes", ingests[1])
        self.assertEqual(ingests[1]["content"], "v2")

    def test_remove_deletes_tracked(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.on_memory_write("add", "profile", "ephemeral")
        provider._drain_writes()
        provider.on_memory_write("remove", "profile", "")
        provider._drain_writes()
        deletes = [b for p, b in provider.posts if p == "/api/delete/durable"]
        self.assertEqual(len(deletes), 1)

    def test_remove_without_prior_remains_recoverable(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.on_memory_write("remove", "profile", "")
        self.assertEqual(len(provider.posts), 0)
        self.assertEqual(provider._write_spool.count(), 1)
        provider.shutdown()

    def test_conclude_stores_verbatim(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        result = json.loads(provider.handle_tool_call("mempal_conclude", {"conclusion": "exact text"}))
        self.assertEqual(provider.posts[-1][1]["request"]["content"], "exact text")
        self.assertIn("drawer_id", result)

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


class ReadinessReliabilityTests(unittest.TestCase):
    """Reliability: degradation, rapid writes, session switch draining."""

    def test_rapid_writes_then_shutdown_drains_all(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        for i in range(20):
            provider.on_memory_write("add", "profile", f"fact {i}")
        provider.shutdown()
        delivered = sum(1 for path, _ in provider.posts if path == "/api/ingest/durable")
        self.assertEqual(delivered + provider._write_spool.count(), 20)

    def test_session_switch_clears_stale_state(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        with provider._prefetch_lock:
            provider._prefetch_result = "old data"
        with provider._pinned_facts_lock:
            provider._pinned_facts_cache = [{"content": "stale"}]
            provider._pinned_facts_fetched_at = time.monotonic()

        provider.on_session_switch("session-b", reason="reset")

        self.assertEqual(provider.prefetch("x", session_id="session-a"), "")
        self.assertEqual(provider._pinned_facts_fetched_at, 0.0)

    def test_consecutive_failures_trip_breaker(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        for _ in range(5):
            provider._record_failure()
        self.assertTrue(provider._is_breaker_open())

    def test_success_resets_failure_count(self) -> None:
        provider = RecordingProvider()
        provider._consecutive_failures = 4
        provider._record_success()
        self.assertEqual(provider._consecutive_failures, 0)

    def test_empty_search_returns_no_results_message(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/search"] = []
        result = json.loads(provider.handle_tool_call("mempal_search", {"query": "nothing"}))
        self.assertIn("result", result)
        self.assertIn("No relevant", result["result"])

    def test_empty_profile_returns_no_memories_message(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/timeline"] = []
        result = json.loads(provider.handle_tool_call("mempal_profile", {}))
        self.assertIn("result", result)
        self.assertIn("No memories", result["result"])


if __name__ == "__main__":
    unittest.main()

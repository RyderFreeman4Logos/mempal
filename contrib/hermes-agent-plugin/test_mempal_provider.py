import json
import os
import sys
import time
import unittest
from typing import Any, Dict, List, Optional, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import MempalMemoryProvider, _LLMClient, _IntelligenceEnhancer  # noqa: E402


class RecordingProvider(MempalMemoryProvider):
    def __init__(self) -> None:
        super().__init__()
        self.gets: List[Tuple[str, Optional[Dict[str, Any]]]] = []
        self.posts: List[Tuple[str, Dict[str, Any]]] = []
        self.responses: Dict[str, Any] = {}

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        return self.responses.get(path, [])

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        return {"ok": True, "drawer_id": f"drawer_{len(self.posts)}"}

    def _turn_storage_mode(self) -> str:
        return "raw_evidence"

    def _drain_writes(self) -> None:
        self._write_queue.join()


class MempalProviderScopeTests(unittest.TestCase):
    def test_two_profiles_use_distinct_wings(self) -> None:
        work = RecordingProvider()
        personal = RecordingProvider()
        work.initialize("session-a", user_id="alice", profile="work")
        personal.initialize("session-b", user_id="alice", profile="personal")

        work.handle_tool_call("mempal_conclude", {"conclusion": "likes vim"})
        personal.handle_tool_call("mempal_conclude", {"conclusion": "likes emacs"})

        self.assertEqual(work.posts[-1][1]["wing"], "hermes-user/alice/work")
        self.assertEqual(personal.posts[-1][1]["wing"], "hermes-user/alice/personal")

    def test_same_profile_facts_are_shared_across_chats(self) -> None:
        chat_a = RecordingProvider()
        chat_b = RecordingProvider()
        chat_a.initialize("session-a", user_id="alice", profile="work", chat_id="chat-a")
        chat_b.initialize("session-b", user_id="alice", profile="work", chat_id="chat-b")

        chat_a.handle_tool_call("mempal_conclude", {"conclusion": "prefers concise answers"})
        chat_b.handle_tool_call("mempal_conclude", {"conclusion": "prefers citations"})

        self.assertEqual(chat_a.posts[-1][1]["wing"], chat_b.posts[-1][1]["wing"])
        self.assertEqual(chat_a.posts[-1][1]["room"], "facts")
        self.assertEqual(chat_b.posts[-1][1]["room"], "facts")

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

        self.assertEqual(chat_only.posts[-1][1]["room"], "turns/slack/chat-a")
        self.assertEqual(threaded.posts[-1][1]["room"], "turns/slack/chat-a/thread-1")

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
        ingest_bodies = [body for path, body in provider.posts if path == "/api/ingest"]

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
        ingest_bodies = [body for path, body in provider.posts if path == "/api/ingest"]

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

    def test_session_end_uses_session_scoped_room(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work", project_id="project-alpha")

        provider.on_session_end([{"role": "assistant", "content": "summary text"}])
        provider._drain_writes()

        self.assertEqual(provider.posts[-1][1]["room"], "sessions/session-a")
        self.assertEqual(provider.posts[-1][1]["project_id"], "project-alpha")

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

        self.assertEqual(provider.posts[-2][1]["room"], "facts")
        self.assertEqual(provider.posts[-1][1]["room"], "turns/cli/chat-a")
        self.assertEqual(provider.posts[-1][1]["project_id"], "project-alpha")


class WriteQueueTests(unittest.TestCase):
    def test_write_queue_drains_on_shutdown(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("add", "profile", "fact 1")
        provider.on_memory_write("add", "profile", "fact 2")
        provider.shutdown()

        ingest_bodies = [body for path, body in provider.posts if path == "/api/ingest"]
        self.assertEqual(len(ingest_bodies), 2)

    def test_is_available_is_config_based(self) -> None:
        provider = RecordingProvider()
        self.assertTrue(provider.is_available())

    def test_sync_turn_enqueues_not_threads(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.sync_turn("hello", "world")
        provider._drain_writes()

        self.assertEqual(len(provider.posts), 1)
        self.assertEqual(provider.posts[0][0], "/api/ingest")
        self.assertIn("User: hello", provider.posts[0][1]["content"])


class WriteSemanticTests(unittest.TestCase):
    def test_add_tracks_drawer_id(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("add", "profile", "important fact")
        provider._drain_writes()

        track_key = "profile:hermes-user/alice/work"
        with provider._drawer_map_lock:
            self.assertIn(track_key, provider._drawer_map)

    def test_replace_sends_supersedes(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("add", "profile", "original fact")
        provider._drain_writes()
        provider.on_memory_write("replace", "profile", "updated fact")
        provider._drain_writes()

        replace_body = provider.posts[-1][1]
        self.assertIn("supersedes", replace_body)

    def test_replace_without_prior_has_no_supersedes(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("replace", "profile", "new fact")
        provider._drain_writes()

        body = provider.posts[-1][1]
        self.assertNotIn("supersedes", body)

    def test_remove_sends_delete(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("add", "profile", "to be removed")
        provider._drain_writes()
        provider.on_memory_write("remove", "profile", "")
        provider._drain_writes()

        delete_posts = [(p, b) for p, b in provider.posts if p == "/api/delete"]
        self.assertEqual(len(delete_posts), 1)
        self.assertIn("drawer_id", delete_posts[0][1])

    def test_remove_noop_without_tracked_drawer(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        initial_count = len(provider.posts)
        provider.on_memory_write("remove", "profile", "")

        self.assertEqual(len(provider.posts), initial_count)

    def test_metadata_passes_typed_fields(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        provider.on_memory_write("add", "profile", "typed fact", metadata={
            "memory_kind": "preference",
            "domain": "coding",
            "importance": 4,
            "is_pinned": True,
        })
        provider._drain_writes()

        body = provider.posts[-1][1]
        self.assertEqual(body["memory_kind"], "preference")
        self.assertEqual(body["domain"], "coding")
        self.assertEqual(body["importance"], 4)
        self.assertTrue(body["is_pinned"])


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

    def test_profile_returns_drawer_id(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.responses["/api/timeline"] = [
            {"content": "fact", "drawer_id": "d1", "importance": 3, "added_at": "2026-01-01"},
        ]

        result = json.loads(provider.handle_tool_call("mempal_profile", {"limit": 5}))
        self.assertEqual(result["results"][0]["drawer_id"], "d1")
        self.assertEqual(result["results"][0]["importance"], 3)


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
        self.assertNotIn("source_type", provider.posts[0][1])

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


if __name__ == "__main__":
    unittest.main()

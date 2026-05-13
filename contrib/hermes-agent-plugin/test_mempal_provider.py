import os
import sys
import unittest
from typing import Any, Dict, List, Optional, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import MempalMemoryProvider  # noqa: E402


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
        return {"ok": True}

    def _turn_storage_mode(self) -> str:
        return "raw_evidence"


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
        chat_only._sync_thread.join(timeout=1.0)
        threaded._sync_thread.join(timeout=1.0)

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
        provider._sync_thread.join(timeout=1.0)

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
        provider._session_end_thread.join(timeout=1.0)

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

        provider.on_memory_write("set", "profile", "profile fact")
        provider._mirror_thread.join(timeout=1.0)
        provider.on_memory_write("set", "turns", "turn content")
        provider._mirror_thread.join(timeout=1.0)

        self.assertEqual(provider.posts[-2][1]["room"], "facts")
        self.assertEqual(provider.posts[-1][1]["room"], "turns/cli/chat-a")
        self.assertEqual(provider.posts[-1][1]["project_id"], "project-alpha")


if __name__ == "__main__":
    unittest.main()

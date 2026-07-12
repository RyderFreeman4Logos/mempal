import importlib.util
import os
import sys
import threading
import time
import unittest
from typing import Any, Dict, List, Optional, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from test_mempal_provider import RecordingProvider  # noqa: E402


class SearchConcurrencyContractTests(unittest.TestCase):
    def test_hooks_search_uses_shared_transport_without_finite_read_deadline(self) -> None:
        hooks_path = os.path.join(PLUGIN_DIR, "mempal-hooks", "__init__.py")
        spec = importlib.util.spec_from_file_location("mempal_hooks_transport_test", hooks_path)
        if spec is None or spec.loader is None:
            self.fail("failed to load mempal-hooks plugin module")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        class RecordingSearchTransport:
            def __init__(self) -> None:
                self.calls: List[Tuple[str, Dict[str, Any]]] = []

            def get_json(self, path: str, params: Dict[str, Any]) -> Any:
                self.calls.append((path, dict(params)))
                return module.SearchTransportResponse([], {})

        hooks = module._MempalHooks()
        hooks._initialized = True
        transport = RecordingSearchTransport()
        hooks._search_transport = transport
        hooks._get = lambda path, params=None: self.fail(
            "search must not use the legacy fixed-timeout _get path"
        )

        results, reason = hooks._get_search({"q": "after upward reload"})

        self.assertEqual(results, [])
        self.assertEqual(reason, "")
        self.assertEqual(transport.calls, [("/api/search", {"q": "after upward reload"})])

    def test_prefetch_latest_wins_queue_has_strict_capacity_one(self) -> None:
        class BlockingPrefetchProvider(RecordingProvider):
            def __init__(self) -> None:
                super().__init__()
                self.entered = threading.Event()
                self.release = threading.Event()
                self.completed = threading.Event()
                self.calls: List[str] = []
                self.active = 0
                self.peak_active = 0
                self.counter_lock = threading.Lock()

            def _get_search(
                self, params: Dict[str, Any], correlation_id: Optional[str] = None,
            ) -> Tuple[List[Dict[str, Any]], str, Dict[str, Any]]:
                del correlation_id
                query = str(params["q"])
                with self.counter_lock:
                    self.calls.append(query)
                    self.active += 1
                    self.peak_active = max(self.peak_active, self.active)
                    call_number = len(self.calls)
                try:
                    if call_number == 1:
                        self.entered.set()
                        self.release.wait(timeout=5.0)
                    else:
                        time.sleep(0.02)
                    return ([{
                        "content": query,
                        "drawer_id": f"drawer-{query}",
                        "source": "tests://prefetch",
                        "importance": 4,
                    }], "", {})
                finally:
                    with self.counter_lock:
                        self.active -= 1
                        if query == "latest":
                            self.completed.set()

        provider = BlockingPrefetchProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.queue_prefetch("first", session_id="session-a")
        self.assertTrue(provider.entered.wait(timeout=1.0))
        for index in range(50):
            provider.queue_prefetch(f"superseded-{index}", session_id="session-a")
        provider.queue_prefetch("latest", session_id="session-a")

        with provider.counter_lock:
            self.assertEqual(provider.peak_active, 1)
            self.assertEqual(provider.calls, ["first"])
        provider.release.set()
        self.assertTrue(provider.completed.wait(timeout=2.0))
        provider.shutdown()

        with provider.counter_lock:
            self.assertEqual(provider.peak_active, 1)
            self.assertEqual(provider.calls, ["first", "latest"])
            self.assertEqual(provider.active, 0)
        self.assertFalse(
            provider._prefetch_thread and provider._prefetch_thread.is_alive()
        )


if __name__ == "__main__":
    unittest.main()

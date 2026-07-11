import os
import sqlite3
import tempfile
import time
import unittest
import urllib.error
from typing import Any, Dict, List, Optional

from test_mempal_provider import RecordingProvider


class _RestartBackend:
    def __init__(self, failures: int) -> None:
        self.failures = failures
        self.keys: List[str] = []
        self.operations: Dict[str, Dict[str, Any]] = {}
        self.drawer_count = 0

    def admit(self, body: Dict[str, Any]) -> Dict[str, Any]:
        key = str(body["idempotency_key"])
        self.keys.append(key)
        operation_id = f"operation_{key}"
        if operation_id not in self.operations:
            self.drawer_count += 1
            self.operations[operation_id] = {
                "operation_id": operation_id,
                "state": "completed",
                "drawer_id": f"drawer_{self.drawer_count}",
            }
        if self.failures > 0:
            self.failures -= 1
            error = urllib.error.HTTPError(
                "/api/ingest/durable", 503, "synthetic unavailable", None, None
            )
            error.close()
            raise error
        return {
            "operation_id": operation_id,
            "accepted_at": "2026-07-10T00:00:00Z",
            "state": "completed",
        }


class _RestartProvider(RecordingProvider):
    def __init__(self, backend: _RestartBackend) -> None:
        super().__init__()
        self.backend = backend

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        self.posts.append((path, dict(body)))
        if path == "/api/ingest/durable":
            return self.backend.admit(body)
        return {"ok": True, "drawer_id": f"drawer_{len(self.posts)}"}

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self.gets.append((path, dict(params or {})))
        if path.startswith("/api/operations/"):
            return self.backend.operations.get(path.rsplit("/", 1)[-1], {})
        return self.responses.get(path, [])


class DurableRawTurnTests(unittest.TestCase):
    def test_sync_turn_spools_before_callback_returns(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        for _ in range(5):
            provider._backoff.record_failure()

        provider.sync_turn("synthetic user", "synthetic assistant")

        spool_path = provider._write_spool.path
        connection = sqlite3.connect(spool_path)
        try:
            row = connection.execute(
                "SELECT action, body_json FROM write_operations"
            ).fetchone()
        finally:
            connection.close()
        self.assertIsNotNone(row)
        self.assertEqual(row[0], "raw_turn")
        self.assertIn("synthetic user", row[1])
        self.assertEqual(os.stat(os.path.dirname(spool_path)).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(spool_path).st_mode & 0o777, 0o600)
        provider.shutdown()

    def test_sync_turn_survives_503_and_provider_restart_exactly_once(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            backend = _RestartBackend(failures=2)
            first = _RestartProvider(backend)
            first.initialize("session-a", hermes_home=hermes_home)
            first.sync_turn("synthetic restart user", "synthetic restart assistant")
            first._drain_writes()
            self.assertEqual(first._write_spool.count(), 1)
            first.shutdown()

            restarted = _RestartProvider(backend)
            restarted.initialize("session-a", hermes_home=hermes_home)
            deadline = time.monotonic() + 4.0
            while restarted._write_spool.count() and time.monotonic() < deadline:
                time.sleep(0.05)
            restarted.shutdown()

            self.assertEqual(restarted._write_spool.count(), 0)
            self.assertEqual(backend.drawer_count, 1)
            self.assertGreaterEqual(len(backend.keys), 3)
            self.assertEqual(len(set(backend.keys)), 1)


class _FailingEnhancer:
    def enhance_summary(self, _text: str) -> str:
        raise RuntimeError("synthetic enhancement failure")


class DurableSessionSummaryTests(unittest.TestCase):
    def test_session_summary_spools_while_breaker_open(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", project_id="project-alpha")
        for _ in range(5):
            provider._backoff.record_failure()

        provider.on_session_end([{"role": "assistant", "content": "summary text"}])

        operation = provider._write_spool.next_operation()
        self.assertEqual(operation.action, "session_summary")
        self.assertEqual(operation.body["room"], "sessions/session-a")
        self.assertEqual(operation.body["project_id"], "project-alpha")
        provider.shutdown()

    def test_enhancement_failure_retains_deterministic_summary(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a")
        provider._should_enhance = lambda: True
        provider._enhancer = _FailingEnhancer()
        for _ in range(5):
            provider._backoff.record_failure()

        provider.on_session_end([{"role": "assistant", "content": "deterministic"}])

        operation = provider._write_spool.next_operation()
        self.assertIn("deterministic", operation.body["content"])
        provider.shutdown()

    def test_session_summary_survives_503_and_provider_restart_exactly_once(self) -> None:
        with tempfile.TemporaryDirectory() as hermes_home:
            backend = _RestartBackend(failures=2)
            first = _RestartProvider(backend)
            first.initialize("session-a", hermes_home=hermes_home)
            first.on_session_end([{"role": "assistant", "content": "summary restart"}])
            first._drain_writes()
            self.assertEqual(first._write_spool.count(), 1)
            first.shutdown()

            restarted = _RestartProvider(backend)
            restarted.initialize("session-a", hermes_home=hermes_home)
            deadline = time.monotonic() + 4.0
            while restarted._write_spool.count() and time.monotonic() < deadline:
                time.sleep(0.05)
            restarted.shutdown()

            self.assertEqual(restarted._write_spool.count(), 0)
            self.assertEqual(backend.drawer_count, 1)
            self.assertEqual(len(set(backend.keys)), 1)


class WriteQueueCompatibilityTests(unittest.TestCase):
    def test_legacy_write_queue_drains_on_shutdown(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.on_memory_write("add", "profile", "fact 1")
        provider.on_memory_write("add", "profile", "fact 2")
        provider.shutdown()

        ingests = [body for path, body in provider.posts if path == "/api/ingest"]
        self.assertEqual(len(ingests), 2)

    def test_is_available_is_config_based(self) -> None:
        provider = RecordingProvider()
        self.assertTrue(provider.is_available())

    def test_sync_turn_uses_durable_route(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")
        provider.sync_turn("hello", "world")
        provider._drain_writes()

        self.assertEqual(len(provider.posts), 1)
        self.assertEqual(provider.posts[0][0], "/api/ingest/durable")
        self.assertIn("User: hello", provider.posts[0][1]["request"]["content"])
        provider.shutdown()


if __name__ == "__main__":
    unittest.main()

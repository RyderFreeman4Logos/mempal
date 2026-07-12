#!/usr/bin/env python3
"""Regression tests for smoke MCP subprocess lifecycle primitives."""
from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
from pathlib import Path
from types import ModuleType
from typing import Any
import unittest
from unittest import mock


def load_full_smoke() -> ModuleType:
    script = Path(__file__).resolve().parents[1] / "scripts" / "full_smoke.py"
    spec = importlib.util.spec_from_file_location("full_smoke", script)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class McpLifecycleTests(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = load_full_smoke()
        self.smoke.OWNED_MCP_CHILDREN.clear()

    def tearDown(self) -> None:
        self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.2)
        manifest = self.smoke.CLEANUP_MANIFEST
        if manifest is not None:
            manifest.discard()

    def _sleeping_client(self) -> tuple[Any, subprocess.Popen[str]]:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        return client, proc

    def test_mcp_error_reaps_child_and_checkpoints_before_fallback(self) -> None:
        child = """
import json
import sys
import time

request = json.loads(sys.stdin.readline())
print(json.dumps({
    'jsonrpc': '2.0',
    'id': request['id'],
    'error': {'code': -32603, 'message': 'reproduced create failure'},
}), flush=True)
time.sleep(60)
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        structured, info = client.tool("mempal_ingest", {}, timeout=2)
        self.assertIsNone(structured)
        self.assertEqual(info.get("error_code"), -32603)

        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-safe"])
            self.smoke.CLEANUP_MANIFEST = manifest

            def fallback() -> list[str]:
                self.assertIsNotNone(proc.poll())
                self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})
                self.assertTrue(manifest.path.exists())
                return ["fallback-id"]

            result = self.smoke.run_fallback_after_mcp_reaped(
                client,
                "create",
                fallback,
            )

        self.assertEqual(result, ["fallback-id"])

    def test_fallback_uses_final_sweep_state_when_initial_close_reports_false(self) -> None:
        client, proc = self._sleeping_client()
        client.close = mock.Mock(return_value=False)

        result = self.smoke.run_fallback_after_mcp_reaped(
            client,
            "create",
            lambda: ["fallback-after-sweep"],
        )

        self.assertEqual(result, ["fallback-after-sweep"])
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_exact_cli_cleanup_does_not_trust_empty_marker_search(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-a", "drawer-b"])
            self.smoke.CLEANUP_MANIFEST = manifest

            def child_result(command: list[str], **kwargs: Any) -> dict[str, Any]:
                del kwargs
                if command[:2] == ["mempal", "delete"]:
                    return {
                        "returncode": 0 if command[2] == "drawer-a" else 1,
                        "stdout": b"",
                        "stderr": b"",
                    }
                if command[2] == "drawer-a":
                    return {
                        "returncode": 1,
                        "stdout": b"",
                        "stderr": b"drawer drawer-a not found",
                    }
                return {"returncode": 0, "stdout": b"still present", "stderr": b""}

            with mock.patch.object(
                self.smoke,
                "run_child_process",
                side_effect=child_result,
            ):
                with mock.patch.object(
                    self.smoke,
                    "run_cli",
                    return_value=(0, b"", b"", {"results": []}, {}),
                ):
                    result = self.smoke.delete_exact_ids_cli(
                        ["drawer-a", "drawer-b"],
                        "unit_cleanup",
                        room="mcp",
                    )

            self.assertEqual(result["verified_absent_count"], 1)
            self.assertEqual(result["failed_count"], 1)
            self.assertEqual(result["active_matches_after_deletes"], 0)
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-b"]},
            )

    def test_exact_mcp_cleanup_checkpoints_each_verified_absence(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-a", "drawer-b"])
            self.smoke.CLEANUP_MANIFEST = manifest
            client = mock.Mock()
            client.tool.side_effect = [
                ({"deleted": True}, {"ok": True}),
                (None, {"ok": False, "error_code": -32002}),
                (None, {"ok": False, "error_code": -32603}),
                ({"drawer_id": "drawer-b"}, {"ok": True}),
            ]

            result = self.smoke.delete_exact_ids_mcp(
                client,
                ["drawer-a", "drawer-b"],
            )

            self.assertEqual(result["verified_absent_count"], 1)
            self.assertEqual(result["failed_count"], 1)
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-b"]},
            )

    def test_exact_cleanup_exception_retains_only_unverified_ids(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-a", "drawer-b"])

            def delete(drawer_id: str) -> bool:
                if drawer_id == "drawer-b":
                    raise RuntimeError("injected second delete failure")
                return True

            with self.assertRaisesRegex(RuntimeError, "second delete failure"):
                self.smoke._SMOKE_RUNTIME.cleanup_exact_ids(
                    ["drawer-a", "drawer-b"],
                    checkpoint=manifest.checkpoint,
                    delete=delete,
                    verify_absent=lambda drawer_id: drawer_id == "drawer-a",
                    mark_cleaned=manifest.mark_cleaned,
                )

            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-b"]},
            )

    def test_unreaped_child_blocks_exact_cli_cleanup(self) -> None:
        process = mock.Mock()
        process.pid = 424243
        process.poll.return_value = None
        process.wait.side_effect = OSError("wait proof unavailable")
        process.stdin = None
        process.stdout = None
        process.stderr = None
        self.smoke.OWNED_MCP_REGISTRY.register(process, "mcp_stdio")
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-a"])
            self.smoke.CLEANUP_MANIFEST = manifest
            with mock.patch.object(self.smoke, "delete_exact_ids_cli") as cleanup:
                result = self.smoke.run_exact_cli_cleanup_after_mcp(
                    ["drawer-a"],
                    "blocked_cleanup",
                )

            self.assertEqual(result, [])
            cleanup.assert_not_called()
            self.assertEqual(manifest.pending_count, 1)
            self.assertTrue(manifest.path.exists())
            self.assertIn(process.pid, self.smoke.OWNED_MCP_CHILDREN)
        process.wait.side_effect = None
        process.wait.return_value = 0
        self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.01)

    def test_checkpoint_exception_emits_safe_manifest_recovery_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-recoverable"])
            self.smoke.CLEANUP_MANIFEST = manifest
            output = io.StringIO()

            def fail_after_creation() -> int:
                manifest.checkpoint()
                return 0

            with mock.patch.object(
                manifest,
                "checkpoint",
                side_effect=OSError("private raw checkpoint detail"),
            ):
                with contextlib.redirect_stdout(output):
                    return_code = self.smoke.run_with_owned_mcp_cleanup(
                        fail_after_creation,
                    )

            receipt = json.loads(output.getvalue())
            self.assertEqual(return_code, 1)
            self.assertFalse(receipt["overall_ok"])
            self.assertEqual(receipt["cleanup_manifest_path"], str(manifest.path))
            self.assertEqual(receipt["cleanup_pending_count"], 1)
            self.assertEqual(
                receipt["groups"]["runner_exception"]["error_type"],
                "OSError",
            )
            self.assertNotIn("private raw checkpoint detail", output.getvalue())
            self.assertTrue(manifest.path.exists())

    def test_initialize_failure_closes_spawned_client(self) -> None:
        fake_client = mock.Mock()
        fake_client.call.side_effect = TimeoutError("initialize timeout")
        with mock.patch.object(self.smoke, "McpClient", return_value=fake_client):
            with self.assertRaises(TimeoutError):
                self.smoke.mcp_start_initialized()

        fake_client.close.assert_called_once_with()

    def test_initialize_failure_reaps_real_child_and_preserves_original_error(self) -> None:
        client, proc = self._sleeping_client()
        with mock.patch.object(self.smoke, "new_mcp_client", return_value=client):
            with mock.patch.object(
                client,
                "call",
                side_effect=RuntimeError("original initialize failure"),
            ):
                with mock.patch.object(client, "close", return_value=False):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "original initialize failure",
                    ):
                        self.smoke.mcp_start_initialized()

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_initialized_notification_failure_reaps_child_and_preserves_error(self) -> None:
        client, proc = self._sleeping_client()
        with mock.patch.object(self.smoke, "new_mcp_client", return_value=client):
            with mock.patch.object(
                client,
                "call",
                return_value={"result": {"serverInfo": {}}},
            ):
                with mock.patch.object(
                    client,
                    "notify",
                    side_effect=RuntimeError("initialized notification failure"),
                ):
                    with self.assertRaisesRegex(
                        RuntimeError,
                        "initialized notification failure",
                    ):
                        self.smoke.mcp_start_initialized()

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_partial_json_line_cannot_bypass_response_timeout(self) -> None:
        child = """
import json
import sys
import time

request = json.loads(sys.stdin.readline())
sys.stdout.write('{"jsonrpc":"2.0","id":' + str(request['id']))
sys.stdout.flush()
time.sleep(0.4)
sys.stdout.write(',"result":{}}\\n')
sys.stdout.flush()
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        started = self.smoke.time.monotonic()
        elapsed = 0.0
        try:
            with self.assertRaises(TimeoutError):
                client.call("partial-response", timeout=0.1)
            elapsed = self.smoke.time.monotonic() - started
        finally:
            client.close()

        self.assertLess(elapsed, 0.35)
        self.assertIsNotNone(proc.poll())

    def test_buffered_json_lines_are_consumed_without_waiting_for_fd_readiness(self) -> None:
        child = """
import json
import sys

request = json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc': '2.0', 'id': 999, 'result': {}}))
print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': {'ok': True}}))
sys.stdout.flush()
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        try:
            response = client.call("buffered-responses", timeout=0.5)
        finally:
            client.close()

        self.assertEqual(response["result"], {"ok": True})
        self.assertIsNotNone(proc.poll())

    def test_oversized_mcp_response_is_rejected(self) -> None:
        child = f"""
import json
import sys
import time

json.loads(sys.stdin.readline())
sys.stdout.write('x' * {self.smoke._SMOKE_RUNTIME.MAX_MCP_RESPONSE_BYTES + 1})
sys.stdout.flush()
time.sleep(60)
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        try:
            with self.assertRaisesRegex(ValueError, "mcp response exceeds"):
                client.call("oversized-response", timeout=2)
        finally:
            client.close()

        self.assertIsNotNone(proc.poll())

    def test_term_resistant_child_is_killed_and_reaped(self) -> None:
        child = """
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print('ready', flush=True)
time.sleep(60)
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.assertEqual(proc.stdout.readline(), "ready\n")
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )

        self.assertTrue(client.close())

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})
        self.assertEqual(self.smoke.SUMMARY["mcp_stdio_lifecycle"]["killed_count"], 1)

    def test_wait_error_keeps_unproven_child_registered(self) -> None:
        process = mock.Mock()
        process.pid = 424242
        process.poll.return_value = None
        process.wait.side_effect = OSError("wait proof unavailable")
        process.stdin = None
        process.stdout = None
        process.stderr = None
        registry = {process.pid: process}

        receipt = self.smoke._SMOKE_RUNTIME.terminate_and_reap_owned_processes(
            registry,
            timeout=0.01,
        )

        self.assertEqual(receipt["reaped_count"], 0)
        self.assertEqual(receipt["remaining_count"], 1)
        self.assertEqual(registry, {process.pid: process})

    def test_client_close_wait_error_keeps_child_registered(self) -> None:
        client, proc = self._sleeping_client()
        with mock.patch.object(
            proc,
            "wait",
            side_effect=OSError("wait proof unavailable"),
        ):
            self.assertFalse(client.close())

        self.assertIn(proc.pid, self.smoke.OWNED_MCP_CHILDREN)
        sweep = self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.2)
        self.assertEqual(sweep["remaining_count"], 0)
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_hard_timeout_uses_client_close_instead_of_direct_kill(self) -> None:
        client = mock.Mock()
        client.tool.side_effect = lambda *args, **kwargs: self.smoke.time.sleep(2)

        structured, info = self.smoke._mcp_tool_with_hard_timeout(
            client,
            "mempal_ingest",
            {},
            timeout=1,
        )

        self.assertIsNone(structured)
        self.assertEqual(info["error_type"], "TimeoutError")
        client.close.assert_called_once_with()
        client.proc.kill.assert_not_called()

    def test_hard_timeout_restores_existing_alarm_handler_and_timer(self) -> None:
        original_handler = self.smoke.signal.getsignal(self.smoke.signal.SIGALRM)
        original_timer = self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0)

        def prior_handler(signum: int, frame: Any) -> None:
            del signum, frame

        client = mock.Mock()
        client.tool.return_value = (None, {"ok": False})
        try:
            self.smoke.signal.signal(self.smoke.signal.SIGALRM, prior_handler)
            self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 30.0)

            self.smoke._mcp_tool_with_hard_timeout(
                client,
                "mempal_status",
                {},
                timeout=1,
            )

            restored_timer = self.smoke.signal.getitimer(self.smoke.signal.ITIMER_REAL)
            self.assertIs(
                self.smoke.signal.getsignal(self.smoke.signal.SIGALRM),
                prior_handler,
            )
            self.assertGreater(restored_timer[0], 29.0)
            self.assertLessEqual(restored_timer[0], 30.0)
        finally:
            self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0)
            self.smoke.signal.signal(self.smoke.signal.SIGALRM, original_handler)
            self.smoke.signal.setitimer(
                self.smoke.signal.ITIMER_REAL,
                *original_timer,
            )

    def test_hard_timeout_reaps_before_restoring_raising_periodic_alarm(self) -> None:
        original_handler = self.smoke.signal.getsignal(self.smoke.signal.SIGALRM)
        original_timer = self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0)
        proc = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.read()"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        observations: list[tuple[bool, bool]] = []

        class PriorTimerRaised(Exception):
            pass

        def raising_prior_handler(signum: int, frame: Any) -> None:
            del signum, frame
            self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0)
            observations.append(
                (
                    proc.poll() is not None,
                    proc.pid not in self.smoke.OWNED_MCP_CHILDREN,
                )
            )
            raise PriorTimerRaised

        client.tool = mock.Mock(
            side_effect=lambda *args, **kwargs: self.smoke.time.sleep(1)
        )
        try:
            self.smoke.signal.signal(self.smoke.signal.SIGALRM, raising_prior_handler)
            self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0.01, 0.01)

            with self.assertRaises(PriorTimerRaised):
                structured, info = self.smoke._mcp_tool_with_hard_timeout(
                    client,
                    "mempal_ingest",
                    {},
                    timeout=0.1,
                )
                self.assertIsNone(structured)
                self.assertEqual(info["error_type"], "TimeoutError")
                self.smoke.time.sleep(0.2)

            self.assertEqual(observations, [(True, True)])
        finally:
            self.smoke.signal.setitimer(self.smoke.signal.ITIMER_REAL, 0)
            self.smoke.signal.signal(self.smoke.signal.SIGALRM, original_handler)
            self.smoke.signal.setitimer(
                self.smoke.signal.ITIMER_REAL,
                *original_timer,
            )
            self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.2)

    def test_owned_children_are_reaped_when_runner_is_cancelled(self) -> None:
        _client, proc = self._sleeping_client()

        def cancelled() -> int:
            raise KeyboardInterrupt

        with contextlib.redirect_stdout(io.StringIO()):
            return_code = self.smoke.run_with_owned_mcp_cleanup(cancelled)

        self.assertEqual(return_code, 1)
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_is_idempotent(self) -> None:
        client, proc = self._sleeping_client()

        self.assertTrue(client.close())
        first_lifecycle = dict(self.smoke.SUMMARY["mcp_stdio_lifecycle"])
        self.assertTrue(client.close())

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.SUMMARY["mcp_stdio_lifecycle"], first_lifecycle)

    def test_close_can_retry_after_interruption(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.read()"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        client._read_proc_io = mock.Mock(
            side_effect=[KeyboardInterrupt, None, None],
        )

        with self.assertRaises(KeyboardInterrupt):
            client.close()

        self.assertIn(proc.pid, self.smoke.OWNED_MCP_CHILDREN)
        self.assertTrue(client.close())
        first_lifecycle = dict(self.smoke.SUMMARY["mcp_stdio_lifecycle"])
        self.assertTrue(client.close())

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})
        self.assertEqual(self.smoke.SUMMARY["mcp_stdio_lifecycle"], first_lifecycle)

    def test_close_can_retry_when_stream_cleanup_is_interrupted(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.read()"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        client.notify = mock.Mock(wraps=client.notify)
        close_streams = self.smoke._SMOKE_RUNTIME._close_process_streams
        close_attempts = 0

        def interrupt_stream_cleanup_once(process: subprocess.Popen[Any]) -> None:
            nonlocal close_attempts
            close_attempts += 1
            if close_attempts == 1:
                raise KeyboardInterrupt
            close_streams(process)

        with mock.patch.object(
            self.smoke._SMOKE_RUNTIME,
            "_close_process_streams",
            side_effect=interrupt_stream_cleanup_once,
        ):
            with self.assertRaises(KeyboardInterrupt):
                client.close()

            first_lifecycle = dict(self.smoke.SUMMARY["mcp_stdio_lifecycle"])
            self.assertTrue(client.close())

        self.assertEqual(close_attempts, 2)
        self.assertEqual(client.notify.call_count, 1)
        self.assertEqual(self.smoke.SUMMARY["mcp_stdio_lifecycle"], first_lifecycle)
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_can_retry_when_terminate_is_interrupted(self) -> None:
        client, proc = self._sleeping_client()
        terminate = proc.terminate
        proc.kill = mock.Mock(wraps=proc.kill)
        proc.terminate = mock.Mock(side_effect=KeyboardInterrupt)

        with self.assertRaises(KeyboardInterrupt):
            client.close()

        proc.terminate.side_effect = terminate
        self.assertTrue(client.close())
        self.assertEqual(proc.terminate.call_count, 1)
        self.assertGreaterEqual(proc.kill.call_count, 1)
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_can_retry_when_kill_is_interrupted(self) -> None:
        child = """
import signal
import time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
print('ready', flush=True)
time.sleep(60)
"""
        proc = subprocess.Popen(
            [sys.executable, "-u", "-c", child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.assertEqual(proc.stdout.readline(), "ready\n")
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        proc.terminate = mock.Mock(wraps=proc.terminate)
        kill = proc.kill
        proc.kill = mock.Mock(side_effect=KeyboardInterrupt)

        with self.assertRaises(KeyboardInterrupt):
            client.close()

        proc.kill.side_effect = kill
        self.assertTrue(client.close())
        self.assertEqual(proc.terminate.call_count, 1)
        self.assertEqual(proc.kill.call_count, 2)
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_can_retry_when_io_receipt_is_interrupted(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.read()"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        record_io = client._record_proc_io_delta
        record_calls = 0

        def record_then_interrupt(*args: Any) -> None:
            nonlocal record_calls
            record_calls += 1
            record_io(*args)
            if record_calls == 1:
                raise KeyboardInterrupt

        client._record_proc_io_delta = record_then_interrupt

        with self.assertRaises(KeyboardInterrupt):
            client.close()

        self.assertTrue(client.close())
        self.assertEqual(record_calls, 2)
        self.assertEqual(
            self.smoke.SUMMARY["io"]["mcp_stdio_child_processes"]["process_count"],
            1,
        )
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_can_retry_when_lifecycle_receipt_is_interrupted(self) -> None:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import sys; sys.stdin.read()"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        record_lifecycle = client._record_lifecycle
        record_calls = 0

        def record_then_interrupt(killed: bool, reaped: bool) -> None:
            nonlocal record_calls
            record_calls += 1
            record_lifecycle(killed, reaped)
            if record_calls == 1:
                raise KeyboardInterrupt

        client._record_lifecycle = record_then_interrupt

        with self.assertRaises(KeyboardInterrupt):
            client.close()

        self.assertTrue(client.close())
        self.assertEqual(record_calls, 2)
        self.assertEqual(self.smoke.SUMMARY["mcp_stdio_lifecycle"]["process_count"], 1)
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})


if __name__ == "__main__":
    unittest.main()

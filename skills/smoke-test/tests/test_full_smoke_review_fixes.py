#!/usr/bin/env python3
"""Focused regressions for full_smoke review fixes."""
from __future__ import annotations

import importlib.util
import io
import unittest
from email.message import Message
from pathlib import Path
from types import ModuleType
from unittest import mock
from urllib.error import HTTPError


def load_full_smoke() -> ModuleType:
    script = Path(__file__).resolve().parents[1] / "scripts" / "full_smoke.py"
    spec = importlib.util.spec_from_file_location("full_smoke", script)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ReviewFixTests(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = load_full_smoke()
        self.smoke.OWNED_MCP_CHILDREN.clear()

    def tearDown(self) -> None:
        self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.2)
        manifest = self.smoke.CLEANUP_MANIFEST
        if manifest is not None:
            manifest.discard()

    def test_rest_ingest_caps_http_error_body_read(self) -> None:
        error = HTTPError(
            "http://127.0.0.1:3080/api/ingest",
            503,
            "service unavailable",
            Message(),
            io.BytesIO(b'{"outcome":"admission_blocked"}'),
        )
        error.read = mock.Mock(wraps=error.read)

        with mock.patch("urllib.request.urlopen", side_effect=error):
            self.smoke._rest_ingest_fallback("content", "unit_rest_http_error")

        error.read.assert_called_once_with(8192)
        error.close()

    def test_unreaped_child_returns_requested_rest_fallback_shape(self) -> None:
        process = mock.Mock()
        process.pid = 424244
        process.poll.return_value = None
        process.wait.side_effect = OSError("wait proof unavailable")
        process.stdin = None
        process.stdout = None
        process.stderr = None
        self.smoke.OWNED_MCP_REGISTRY.register(process, "mcp_stdio")
        fallback = mock.Mock()

        result = self.smoke.run_fallback_after_mcp_reaped(
            None,
            "blocked_rest",
            fallback,
            failure_result=([], None),
        )

        self.assertEqual(result, ([], None))
        fallback.assert_not_called()
        process.wait.side_effect = None
        process.wait.return_value = 0
        self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.01)


if __name__ == "__main__":
    unittest.main()

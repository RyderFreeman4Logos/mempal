"""Regression coverage for full-smoke CLI CRUD receipt classification."""
from __future__ import annotations

import importlib.util
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


class CliCrudReceiptTests(unittest.TestCase):
    def test_no_write_admission_stays_terminal_when_rest_fallback_times_out(self) -> None:
        smoke = load_full_smoke()
        original_summary = smoke.SUMMARY
        original_manifest = smoke.CLEANUP_MANIFEST
        smoke.SUMMARY = {"groups": {}, "failures": []}
        smoke.CLEANUP_MANIFEST = None

        def rest_timeout(*_args: Any, **_kwargs: Any) -> tuple[list[str], None]:
            smoke.note("cli_create_rest_fallback", False, error_type="TimeoutError")
            return [], None

        blocked = {
            "outcome": "admission_blocked",
            "action": "write_refused",
            "reason": "holder_budget_exceeded",
            "created_drawer_ids": [],
            "cleanup_drawer_ids": [],
        }
        try:
            with (
                mock.patch.object(smoke, "run_cli", return_value=(1, b"", b"", blocked, {})),
                mock.patch.object(smoke, "_rest_ingest_fallback", side_effect=rest_timeout),
            ):
                self.assertEqual(smoke.cli_crud(), [])
            self.assertEqual(
                smoke.SUMMARY["groups"]["cli_crud"],
                {
                    "ok": True,
                    "skipped": "admission_blocked_no_write",
                    "outcome": "admission_blocked",
                    "reason": "holder_budget_exceeded",
                    "cleanup_required": False,
                },
            )
            self.assertIn("cli_create_rest_fallback", smoke.SUMMARY["failures"])
            self.assertNotIn("cli_crud", smoke.SUMMARY["failures"])
        finally:
            smoke.SUMMARY = original_summary
            smoke.CLEANUP_MANIFEST = original_manifest

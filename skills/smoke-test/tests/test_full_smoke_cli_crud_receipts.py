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
    @staticmethod
    def holder_budget_receipt() -> dict[str, Any]:
        return {
            "outcome": "admission_blocked",
            "action": "write_refused",
            "reason": "holder_budget_exceeded",
            "created_drawer_ids": [],
            "cleanup_drawer_ids": [],
            "capacity": {"holders": 16, "cache_bytes": 64},
            "headroom": {"holders": 2, "cache_bytes": 50},
            "profile_admission": {
                "active_holders": 14,
                "configured_holder_limit": 16,
                "active_cache_bytes": 14,
                "configured_cache_bytes": 64,
                "reaped_stale_holders_this_snapshot": 0,
                "reserved_service_holders": 2,
                "service_holders": 14,
                "requested_cache_bytes": 1,
                "budget_reason": "reserved_service_slots",
            },
        }

    def test_holder_budget_no_write_receipt_requires_exact_schema(self) -> None:
        smoke = load_full_smoke()
        expected = {
            "outcome": "admission_blocked",
            "reason": "holder_budget_exceeded",
            "cleanup_required": False,
        }
        self.assertEqual(
            smoke.holder_budget_no_write_receipt({"error": self.holder_budget_receipt()}),
            expected,
        )

        cases: dict[str, Any] = {
            "not_a_json_object": [],
            "missing_capacity": {key: value for key, value in self.holder_budget_receipt().items() if key != "capacity"},
            "unknown_reason": {**self.holder_budget_receipt(), "reason": "database_locked"},
            "unknown_action": {**self.holder_budget_receipt(), "action": "retry_later"},
            "wrong_capacity_type": {**self.holder_budget_receipt(), "capacity": {"holders": True, "cache_bytes": 64}},
            "missing_headroom": {key: value for key, value in self.holder_budget_receipt().items() if key != "headroom"},
            "inconsistent_headroom": {**self.holder_budget_receipt(), "headroom": {"holders": 3, "cache_bytes": 50}},
            "inconsistent_budget_reason": {
                **self.holder_budget_receipt(),
                "profile_admission": {
                    **self.holder_budget_receipt()["profile_admission"],
                    "budget_reason": "holder_limit",
                },
            },
            "created_id": {**self.holder_budget_receipt(), "created_drawer_ids": ["drawer-1"]},
            "cleanup_id": {**self.holder_budget_receipt(), "cleanup_drawer_ids": ["drawer-1"]},
        }
        for name, receipt in cases.items():
            with self.subTest(name=name):
                self.assertIsNone(smoke.holder_budget_no_write_receipt(receipt))

    def test_mcp_terminal_receipt_keeps_holder_budget_metadata_for_the_create_guard(self) -> None:
        smoke = load_full_smoke()
        terminal = smoke._SMOKE_RUNTIME.terminal_no_write_receipt(
            {"data": self.holder_budget_receipt()}
        )
        self.assertEqual(
            smoke.holder_budget_no_write_receipt({"terminal_receipt": terminal}),
            {
                "outcome": "admission_blocked",
                "reason": "holder_budget_exceeded",
                "cleanup_required": False,
            },
        )

    def test_no_write_admission_stays_terminal_when_rest_fallback_times_out(self) -> None:
        smoke = load_full_smoke()
        original_summary = smoke.SUMMARY
        original_manifest = smoke.CLEANUP_MANIFEST
        smoke.SUMMARY = {"groups": {}, "failures": []}
        smoke.CLEANUP_MANIFEST = None

        def rest_timeout(*_args: Any, **_kwargs: Any) -> tuple[list[str], None]:
            smoke.note("cli_create_rest_fallback", False, error_type="TimeoutError")
            return [], None

        blocked = self.holder_budget_receipt()
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

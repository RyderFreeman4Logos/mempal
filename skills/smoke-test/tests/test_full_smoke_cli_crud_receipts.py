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
    def mcp_holder_budget_receipt() -> dict[str, Any]:
        """Mirror the real 14-of-16 MCP async-pool admission receipt."""
        return {
            "outcome": "admission_blocked",
            "action": "write_refused",
            "reason": "holder_budget_exceeded",
            "created_drawer_ids": [],
            "cleanup_drawer_ids": [],
            "capacity": {"holders": 16, "cache_bytes": 256 * 1024 * 1024},
            "headroom": {"holders": 2, "cache_bytes": 32 * 1024 * 1024},
            "profile_admission": {
                "active_holders": 14,
                "configured_holder_limit": 16,
                "active_cache_bytes": 14 * 16 * 1024 * 1024,
                "configured_cache_bytes": 256 * 1024 * 1024,
                "reaped_stale_holders_this_snapshot": 0,
                "reserved_service_holders": 2,
                "service_holders": 14,
                "requested_cache_bytes": 3 * 16 * 1024 * 1024,
                "budget_reason": "cache_budget",
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
            smoke.holder_budget_no_write_receipt({"error": self.mcp_holder_budget_receipt()}),
            expected,
        )

        cases: dict[str, Any] = {
            "not_a_json_object": [],
            "missing_capacity": {key: value for key, value in self.mcp_holder_budget_receipt().items() if key != "capacity"},
            "unknown_reason": {**self.mcp_holder_budget_receipt(), "reason": "database_locked"},
            "unknown_action": {**self.mcp_holder_budget_receipt(), "action": "retry_later"},
            "wrong_capacity_type": {**self.mcp_holder_budget_receipt(), "capacity": {"holders": True, "cache_bytes": 256 * 1024 * 1024}},
            "missing_headroom": {key: value for key, value in self.mcp_holder_budget_receipt().items() if key != "headroom"},
            "inconsistent_headroom": {**self.mcp_holder_budget_receipt(), "headroom": {"holders": 3, "cache_bytes": 32 * 1024 * 1024}},
            "inconsistent_budget_reason": {
                **self.mcp_holder_budget_receipt(),
                "profile_admission": {
                    **self.mcp_holder_budget_receipt()["profile_admission"],
                    "budget_reason": "holder_limit",
                },
            },
            "created_id": {**self.mcp_holder_budget_receipt(), "created_drawer_ids": ["drawer-1"]},
            "cleanup_id": {**self.mcp_holder_budget_receipt(), "cleanup_drawer_ids": ["drawer-1"]},
        }
        for name, receipt in cases.items():
            with self.subTest(name=name):
                self.assertIsNone(smoke.holder_budget_no_write_receipt(receipt))

    def test_mcp_terminal_receipt_keeps_holder_budget_metadata_for_the_create_guard(self) -> None:
        smoke = load_full_smoke()
        terminal = smoke._SMOKE_RUNTIME.terminal_no_write_receipt(
            {"data": self.mcp_holder_budget_receipt()}
        )
        self.assertEqual(
            smoke.holder_budget_no_write_receipt({"terminal_receipt": terminal}),
            {
                "outcome": "admission_blocked",
                "reason": "holder_budget_exceeded",
                "cleanup_required": False,
            },
        )

    def test_mcp_exact_no_write_receipt_stays_terminal_after_rest_refusal_or_timeout(self) -> None:
        for name, direct_receipt, rest_receipt, expected_terminal in (
            (
                "rest_admission_refusal",
                self.mcp_holder_budget_receipt(),
                self.mcp_holder_budget_receipt(),
                True,
            ),
            ("rest_timeout", self.mcp_holder_budget_receipt(), None, True),
            (
                "rest_admission_refusal_missing_requested_cache_bytes",
                {
                    **self.mcp_holder_budget_receipt(),
                    "profile_admission": {
                        key: value
                        for key, value in self.mcp_holder_budget_receipt()["profile_admission"].items()
                        if key != "requested_cache_bytes"
                    },
                },
                self.mcp_holder_budget_receipt(),
                False,
            ),
            (
                "rest_timeout_write_bearing_mcp_receipt",
                {**self.mcp_holder_budget_receipt(), "created_drawer_ids": ["drawer-1"]},
                None,
                False,
            ),
        ):
            with self.subTest(name=name):
                smoke = load_full_smoke()
                original_summary = smoke.SUMMARY
                original_manifest = smoke.CLEANUP_MANIFEST
                smoke.SUMMARY = {"groups": {}, "failures": []}
                smoke.CLEANUP_MANIFEST = None
                discover = mock.Mock()
                discover.call.return_value = {
                    "result": {
                        "tools": [
                            {"name": tool}
                            for tool in (
                                "mempal_ingest",
                                "mempal_operation_status",
                                "mempal_search",
                                "mempal_read_drawer",
                                "mempal_delete",
                            )
                        ]
                    }
                }
                create_client = mock.Mock()
                terminal = smoke._SMOKE_RUNTIME.terminal_no_write_receipt(
                    {"data": direct_receipt}
                )
                info: dict[str, Any] = {"ok": False}
                if terminal is not None:
                    info["terminal_receipt"] = terminal

                try:
                    with (
                        mock.patch.object(
                            smoke,
                            "mcp_start_initialized",
                            side_effect=[discover, create_client],
                        ),
                        mock.patch.object(smoke, "mcp_call_isolated"),
                        mock.patch.object(
                            smoke,
                            "_mcp_tool_with_hard_timeout",
                            return_value=(None, info),
                        ),
                        mock.patch.object(
                            smoke,
                            "run_fallback_after_mcp_reaped",
                            side_effect=lambda _client, _label, fallback, **_kwargs: fallback(),
                        ),
                        mock.patch.object(
                            smoke,
                            "_rest_ingest_fallback",
                            return_value=([], rest_receipt),
                        ) as rest_fallback,
                    ):
                        self.assertEqual(smoke.mcp_crud(), [])
                    rest_fallback.assert_called_once()
                    if expected_terminal:
                        self.assertEqual(
                            smoke.SUMMARY["groups"]["mcp_inconclusive_no_cleanup_id"],
                            {
                                "ok": True,
                                "skipped": "admission_blocked_no_write",
                                "outcome": "admission_blocked",
                                "reason": "holder_budget_exceeded",
                                "cleanup_required": False,
                            },
                        )
                        self.assertNotIn("mcp_inconclusive_no_cleanup_id", smoke.SUMMARY["failures"])
                    else:
                        self.assertFalse(
                            smoke.SUMMARY["groups"]["mcp_inconclusive_no_cleanup_id"]["ok"]
                        )
                        self.assertIn("mcp_inconclusive_no_cleanup_id", smoke.SUMMARY["failures"])
                finally:
                    smoke.SUMMARY = original_summary
                    smoke.CLEANUP_MANIFEST = original_manifest

    def test_no_write_admission_stays_terminal_when_rest_fallback_times_out(self) -> None:
        smoke = load_full_smoke()
        original_summary = smoke.SUMMARY
        original_manifest = smoke.CLEANUP_MANIFEST
        smoke.SUMMARY = {"groups": {}, "failures": []}
        smoke.CLEANUP_MANIFEST = None

        def rest_timeout(*_args: Any, **_kwargs: Any) -> tuple[list[str], None]:
            smoke.note("cli_create_rest_fallback", False, error_type="TimeoutError")
            return [], None

        blocked = self.mcp_holder_budget_receipt()
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

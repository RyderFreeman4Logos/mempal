"""Regression coverage for full-smoke CLI CRUD receipt classification."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import tempfile
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

    def test_mcp_exact_no_write_receipt_skips_rest_fallback(self) -> None:
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
            {"data": self.mcp_holder_budget_receipt()}
        )

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
                    return_value=(None, {"ok": False, "terminal_receipt": terminal}),
                ),
                mock.patch.object(
                    smoke,
                    "_rest_ingest_fallback",
                    side_effect=AssertionError("REST must not follow exact no-write proof"),
                ) as rest_fallback,
            ):
                self.assertEqual(smoke.mcp_crud(), [])
            rest_fallback.assert_not_called()
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
        finally:
            smoke.SUMMARY = original_summary
            smoke.CLEANUP_MANIFEST = original_manifest

    def test_outer_ids_override_nested_no_write_receipts(self) -> None:
        for nested_key in ("error", "terminal_receipt"):
            with self.subTest(nested_key=nested_key):
                smoke = load_full_smoke()
                envelope = {
                    "created_drawer_ids": ["drawer-direct"],
                    nested_key: self.mcp_holder_budget_receipt(),
                }

                ids, info = smoke.recover_created_ids(envelope, "unused_wait")

                self.assertEqual(ids, ["drawer-direct"])
                self.assertNotIn("outcome", info)
                self.assertIsNone(smoke.holder_budget_no_write_receipt(envelope))
                self.assertEqual(
                    smoke.create_terminal_receipt(envelope),
                    {
                        "outcome": "write_accepted",
                        "created_drawer_ids": ["drawer-direct"],
                        "cleanup_required": True,
                    },
                )

    def test_cli_outer_ids_override_nested_no_write_and_reach_manifest(self) -> None:
        for nested_key in ("error", "terminal_receipt"):
            with self.subTest(nested_key=nested_key), tempfile.TemporaryDirectory() as tmp:
                smoke = load_full_smoke()
                original_manifest = smoke.CLEANUP_MANIFEST
                manifest = smoke.CleanupManifest(Path(tmp) / "cleanup.json")
                smoke.CLEANUP_MANIFEST = manifest
                direct = {
                    "created_drawer_ids": ["drawer-direct"],
                    nested_key: self.mcp_holder_budget_receipt(),
                }

                def run_cli(label: str, *_args: Any, **_kwargs: Any) -> tuple[int, bytes, bytes, dict[str, Any], dict[str, Any]]:
                    if label == "cli_create":
                        return 0, b"", b"", direct, {}
                    if label == "cli_update":
                        return 0, b"", b"", {"created_drawer_ids": ["drawer-update"]}, {}
                    return 0, b"", b"", {"results": []}, {}

                try:
                    with (
                        mock.patch.object(smoke, "run_cli", side_effect=run_cli),
                        mock.patch.object(
                            smoke,
                            "delete_exact_ids_cli",
                            return_value={"deleted_count": 2, "failed_count": 0},
                        ),
                        mock.patch.object(
                            smoke,
                            "_rest_ingest_fallback",
                            side_effect=AssertionError("REST must not follow explicit cleanup IDs"),
                        ) as rest_fallback,
                    ):
                        self.assertEqual(smoke.cli_crud(), ["drawer-direct", "drawer-update"])
                    rest_fallback.assert_not_called()
                    self.assertEqual(manifest.pending_count, 2)
                    self.assertTrue(smoke.SUMMARY["groups"]["cli_crud"]["ok"])
                    self.assertNotIn("admission_blocked_no_write", smoke.SUMMARY["groups"]["cli_crud"].values())
                finally:
                    manifest.discard()
                    smoke.CLEANUP_MANIFEST = original_manifest

    def test_mcp_outer_ids_override_nested_no_write_and_reach_manifest(self) -> None:
        for nested_key in ("error", "terminal_receipt"):
            with self.subTest(nested_key=nested_key), tempfile.TemporaryDirectory() as tmp:
                smoke = load_full_smoke()
                original_manifest = smoke.CLEANUP_MANIFEST
                manifest = smoke.CleanupManifest(Path(tmp) / "cleanup.json")
                smoke.CLEANUP_MANIFEST = manifest
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
                update_client = mock.Mock()
                update_client.tool.return_value = ({"results": []}, {"ok": True})
                info = {
                    "ok": False,
                    "created_drawer_ids": ["drawer-direct"],
                    nested_key: self.mcp_holder_budget_receipt(),
                }

                try:
                    with (
                        mock.patch.object(
                            smoke,
                            "mcp_start_initialized",
                            side_effect=[discover, create_client, update_client],
                        ),
                        mock.patch.object(smoke, "mcp_call_isolated"),
                        mock.patch.object(
                            smoke,
                            "mcp_call_isolated_labeled",
                            return_value=(None, {"ok": True}),
                        ),
                        mock.patch.object(
                            smoke,
                            "_mcp_tool_with_hard_timeout",
                            side_effect=[
                                (None, info),
                                ({"created_drawer_ids": ["drawer-update"]}, {"ok": True}),
                            ],
                        ),
                        mock.patch.object(
                            smoke,
                            "delete_exact_ids_mcp",
                            return_value={
                                "deleted_count": 2,
                                "failed_count": 0,
                                "delete_failed_attempt_count": 0,
                            },
                        ),
                        mock.patch.object(
                            smoke,
                            "_rest_ingest_fallback",
                            side_effect=AssertionError("REST must not follow explicit cleanup IDs"),
                        ) as rest_fallback,
                    ):
                        self.assertEqual(smoke.mcp_crud(), ["drawer-direct", "drawer-update"])
                    rest_fallback.assert_not_called()
                    self.assertEqual(manifest.pending_count, 2)
                    self.assertTrue(smoke.SUMMARY["groups"]["mcp_crud"]["ok"])
                    self.assertNotIn("mcp_inconclusive_no_cleanup_id", smoke.SUMMARY["groups"])
                finally:
                    manifest.discard()
                    smoke.CLEANUP_MANIFEST = original_manifest

    def test_cli_exact_no_write_receipt_skips_rest_fallback(self) -> None:
        smoke = load_full_smoke()
        original_summary = smoke.SUMMARY
        original_manifest = smoke.CLEANUP_MANIFEST
        smoke.SUMMARY = {"groups": {}, "failures": []}
        smoke.CLEANUP_MANIFEST = None

        try:
            with (
                mock.patch.object(
                    smoke,
                    "run_cli",
                    return_value=(1, b"", b"", self.mcp_holder_budget_receipt(), {}),
                ),
                mock.patch.object(
                    smoke,
                    "_rest_ingest_fallback",
                    side_effect=AssertionError("REST must not follow exact no-write proof"),
                ) as rest_fallback,
            ):
                self.assertEqual(smoke.cli_crud(), [])
            rest_fallback.assert_not_called()
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
            self.assertNotIn("cli_crud", smoke.SUMMARY["failures"])
        finally:
            smoke.SUMMARY = original_summary
            smoke.CLEANUP_MANIFEST = original_manifest

    def test_cli_rest_timeout_without_exact_direct_proof_is_inconclusive(self) -> None:
        smoke = load_full_smoke()
        original_summary = smoke.SUMMARY
        original_manifest = smoke.CLEANUP_MANIFEST
        smoke.SUMMARY = {"groups": {}, "failures": []}
        smoke.CLEANUP_MANIFEST = None
        incomplete = self.mcp_holder_budget_receipt()
        del incomplete["profile_admission"]["requested_cache_bytes"]

        try:
            with (
                mock.patch.object(
                    smoke,
                    "run_cli",
                    return_value=(1, b"", b"", incomplete, {}),
                ),
                mock.patch.object(
                    smoke,
                    "_rest_ingest_fallback",
                    return_value=([], None),
                ) as rest_fallback,
            ):
                self.assertEqual(smoke.cli_crud(), [])
            rest_fallback.assert_called_once()
            self.assertFalse(smoke.SUMMARY["groups"]["cli_crud"]["ok"])
            self.assertNotIn("mcp_inconclusive_no_cleanup_id", smoke.SUMMARY["groups"])
        finally:
            smoke.SUMMARY = original_summary
            smoke.CLEANUP_MANIFEST = original_manifest

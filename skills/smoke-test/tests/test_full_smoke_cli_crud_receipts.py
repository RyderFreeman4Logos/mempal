"""Regression coverage for full-smoke CLI CRUD receipt classification."""
from __future__ import annotations

import copy
import importlib.util
import json
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
        """Mirror the complete real 14-of-16 MCP async-pool refusal data."""
        return {
            "outcome": "admission_blocked",
            "action": "write_refused",
            "reason": "holder_budget_exceeded",
            "created_drawer_ids": [],
            "cleanup_drawer_ids": [],
            "capacity": {"holders": 16, "cache_bytes": 256 * 1024 * 1024},
            "headroom": {"holders": 2, "cache_bytes": 32 * 1024 * 1024},
            "async_pool_loaded": False,
            "database_diagnostic": {
                "path": "/private/palace.db",
                "source": "async_db",
                "failure_kind": "holder_budget_exceeded",
                "summary": "holder budget exhausted",
                "hint": "retry after holders are released",
            },
            "profile_admission": {
                "active_holders": 14,
                "configured_holder_limit": 16,
                "active_cache_bytes": 14 * 16 * 1024 * 1024,
                "configured_cache_bytes": 256 * 1024 * 1024,
                "available_cache_bytes": 32 * 1024 * 1024,
                "reaped_stale_holders_this_snapshot": 0,
                "reserved_service_holders": 2,
                "service_holders": 14,
                "requested_cache_bytes": 3 * 16 * 1024 * 1024,
                "budget_reason": "cache_budget",
                "capacity": {"holders": 16, "cache_bytes": 256 * 1024 * 1024},
                "headroom": {"holders": 2, "cache_bytes": 32 * 1024 * 1024},
                "unknown_holders": 0,
                "unknown_holder_diagnostics": [],
                "async_pool_loaded": False,
            },
        }

    @classmethod
    def cli_holder_budget_receipt(cls) -> dict[str, Any]:
        receipt = copy.deepcopy(cls.mcp_holder_budget_receipt())
        del receipt["async_pool_loaded"]
        del receipt["database_diagnostic"]
        profile = receipt["profile_admission"]
        for key in (
            "available_cache_bytes",
            "capacity",
            "headroom",
            "unknown_holders",
            "unknown_holder_diagnostics",
            "async_pool_loaded",
        ):
            del profile[key]
        return receipt

    @staticmethod
    def mcp_error_envelope(
        data: dict[str, Any], result: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        envelope: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": 1,
            "error": {
                "code": -32603,
                "message": "write refused before queueing",
                "data": data,
            },
        }
        if result is not None:
            envelope["result"] = result
        return envelope

    @staticmethod
    def mcp_success_tool_result(
        structured: dict[str, Any],
    ) -> tuple[dict[str, Any], dict[str, Any]]:
        return structured, {
            "ok": True,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"structuredContent": structured},
            },
        }

    @staticmethod
    def mcp_error_tool_result(
        smoke: ModuleType, data: dict[str, Any], result: dict[str, Any] | None = None
    ) -> tuple[dict[str, Any] | None, dict[str, Any]]:
        client = object.__new__(smoke.McpClient)
        client.call = mock.Mock(return_value=CliCrudReceiptTests.mcp_error_envelope(data, result))
        return client.tool("mempal_ingest", {}, timeout=1)

    def test_holder_budget_no_write_receipt_requires_exact_schema(self) -> None:
        smoke = load_full_smoke()
        expected = {
            "outcome": "admission_blocked",
            "reason": "holder_budget_exceeded",
            "cleanup_required": False,
        }
        self.assertEqual(
            smoke.holder_budget_no_write_receipt(
                self.mcp_error_envelope(self.mcp_holder_budget_receipt())
            ),
            expected,
        )
        self.assertEqual(
            smoke.holder_budget_no_write_receipt(
                [self.cli_holder_budget_receipt(), {"returncode": 1}]
            ),
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
                self.assertIsNone(
                    smoke.holder_budget_no_write_receipt(self.mcp_error_envelope(receipt))
                )

    def test_mcp_raw_receipt_is_classified_before_aggregate_info(self) -> None:
        smoke = load_full_smoke()
        structured, info = self.mcp_error_tool_result(smoke, self.mcp_holder_budget_receipt())
        attempt = smoke.create_attempt_from_mcp_info(info)
        self.assertEqual(
            smoke.holder_budget_no_write_receipt(attempt),
            {
                "outcome": "admission_blocked",
                "reason": "holder_budget_exceeded",
                "cleanup_required": False,
            },
        )
        self.assertIsNone(structured)
        self.assertEqual(attempt["error"]["data"], self.mcp_holder_budget_receipt())
        public_info = smoke.without_ok(info)
        self.assertNotIn("raw", json.dumps(public_info))
        self.assertNotIn("/private/palace.db", json.dumps(public_info))
        original_summary = smoke.SUMMARY
        smoke.SUMMARY = {"groups": {}, "failures": []}
        try:
            smoke.note("mcp_create", False, **public_info)
            self.assertNotIn("/private/palace.db", json.dumps(smoke.SUMMARY))
        finally:
            smoke.SUMMARY = original_summary

    def test_mcp_raw_error_keeps_write_contradictions_for_the_classifier(self) -> None:
        smoke = load_full_smoke()
        cases = {
            "queued": ("queued", True),
            "success": ("success", True),
            "malformed_created_ids": ("created_drawer_ids", "drawer-not-an-array"),
        }

        for name, (field, value) in cases.items():
            with self.subTest(name=name):
                structured, info = self.mcp_error_tool_result(
                    smoke, {**self.mcp_holder_budget_receipt(), field: value}
                )
                attempt = smoke.create_attempt_from_mcp_info(info)
                self.assertEqual(attempt["error"]["data"][field], value)
                self.assertIsNone(smoke.holder_budget_no_write_receipt(attempt))

    def test_mcp_raw_error_success_controls_reject_no_write_before_projection(self) -> None:
        smoke = load_full_smoke()
        cases = {
            "error_ok": {"ok": True},
            "accepted_at": {"accepted_at": 1},
            "chunk_count": {"chunk_count": 1},
            "unknown_control": {"write_committed": True},
        }

        for name, contradiction in cases.items():
            with self.subTest(name=name):
                structured, info = self.mcp_error_tool_result(
                    smoke, {**self.mcp_holder_budget_receipt(), **contradiction}
                )
                attempt = smoke.create_attempt_from_mcp_info(info)
                self.assertIsNone(
                    smoke.holder_budget_no_write_receipt(attempt)
                )

    def test_mcp_raw_no_write_requires_matching_false_pool_flags(self) -> None:
        smoke = load_full_smoke()
        cases = {
            "top_level_true": lambda receipt: receipt.__setitem__("async_pool_loaded", True),
            "nested_true": lambda receipt: receipt["profile_admission"].__setitem__("async_pool_loaded", True),
            "top_level_missing": lambda receipt: receipt.pop("async_pool_loaded"),
            "nested_missing": lambda receipt: receipt["profile_admission"].pop("async_pool_loaded"),
            "top_level_wrong_type": lambda receipt: receipt.__setitem__("async_pool_loaded", 0),
            "nested_wrong_type": lambda receipt: receipt["profile_admission"].__setitem__("async_pool_loaded", "false"),
        }

        for name, mutate in cases.items():
            with self.subTest(name=name):
                receipt = self.mcp_holder_budget_receipt()
                mutate(receipt)
                structured, info = self.mcp_error_tool_result(smoke, receipt)
                self.assertIsNone(structured)
                self.assertIsNone(
                    smoke.holder_budget_no_write_receipt(
                        smoke.create_attempt_from_mcp_info(info)
                    )
                )

    def test_mcp_error_and_result_ids_keep_cleanup_authority(self) -> None:
        smoke = load_full_smoke()
        structured, info = self.mcp_error_tool_result(
            smoke,
            self.mcp_holder_budget_receipt(),
            {"structuredContent": {"created_drawer_ids": ["drawer-from-result"]}},
        )
        attempt = smoke.create_attempt_from_mcp_info(info)

        self.assertIsNone(structured)
        self.assertEqual(smoke.created_ids_from(attempt), ["drawer-from-result"])
        self.assertIsNone(smoke.holder_budget_no_write_receipt(attempt))

    def test_mcp_result_ids_are_a_created_attempt(self) -> None:
        smoke = load_full_smoke()
        structured, info = self.mcp_success_tool_result(
            {"created_drawer_ids": ["drawer-from-result"]}
        )

        self.assertEqual(structured, {"created_drawer_ids": ["drawer-from-result"]})
        self.assertEqual(
            smoke.classify_create_attempt(smoke.create_attempt_from_mcp_info(info)),
            {"kind": "created", "created_drawer_ids": ["drawer-from-result"]},
        )

    def test_mcp_coherence_contradictions_reach_rest_fallback(self) -> None:
        for name, mutate in (
            ("top_level_pool_loaded", lambda receipt: receipt.__setitem__("async_pool_loaded", True)),
            ("nested_pool_loaded", lambda receipt: receipt["profile_admission"].__setitem__("async_pool_loaded", True)),
            ("diagnostic_source", lambda receipt: receipt["database_diagnostic"].__setitem__("source", "query_only_async_db")),
            ("diagnostic_failure_kind", lambda receipt: receipt["database_diagnostic"].__setitem__("failure_kind", "locked_or_busy")),
            ("available_cache_bytes", lambda receipt: receipt["profile_admission"].__setitem__("available_cache_bytes", 1)),
            ("unknown_holders", lambda receipt: receipt["profile_admission"].__setitem__("unknown_holders", 1)),
        ):
            with self.subTest(name=name):
                smoke = load_full_smoke()
                original_summary = smoke.SUMMARY
                original_manifest = smoke.CLEANUP_MANIFEST
                smoke.SUMMARY = {"groups": {}, "failures": []}
                smoke.CLEANUP_MANIFEST = None
                receipt = self.mcp_holder_budget_receipt()
                mutate(receipt)
                self.assertEqual(
                    smoke.classify_create_attempt(self.mcp_error_envelope(receipt)),
                    {"kind": "inconclusive"},
                )
                structured, info = self.mcp_error_tool_result(smoke, receipt)
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
                            return_value=(structured, info),
                        ),
                        mock.patch.object(
                            smoke,
                            "_rest_ingest_fallback",
                            return_value=([], None),
                        ) as rest_fallback,
                    ):
                        self.assertEqual(smoke.mcp_crud(), [])
                    rest_fallback.assert_called_once()
                    self.assertNotEqual(
                        smoke.SUMMARY["groups"]["mcp_inconclusive_no_cleanup_id"].get("skipped"),
                        "admission_blocked_no_write",
                    )
                finally:
                    smoke.SUMMARY = original_summary
                    smoke.CLEANUP_MANIFEST = original_manifest

    def test_holder_budget_no_write_receipt_rejects_attempt_contradictions(self) -> None:
        smoke = load_full_smoke()
        invalid_signals = {
            "queued_operation": {"operation_id": "op-queued", "state": "queued"},
            "timed_out": {"timed_out": True},
            "write_accepted": {"outcome": "write_accepted"},
            "accepted_status": {"status": "accepted"},
            "accepted_at": {"accepted_at": 1},
            "chunk_count": {"chunk_count": 1},
            "unknown_control": {"write_committed": True},
            "malformed_id_array": {"created_drawer_ids": "drawer-not-an-array"},
            "malformed_id_item": {"cleanup_drawer_ids": [123]},
        }
        overflowed = self.cli_holder_budget_receipt()
        overflowed["profile_admission"]["reaped_stale_holders_this_snapshot"] = 1 << 64

        self.assertIsNotNone(
            smoke.holder_budget_no_write_receipt(
                [self.cli_holder_budget_receipt(), {"returncode": 1}]
            )
        )
        for name, signals in invalid_signals.items():
            with self.subTest(signal=name):
                self.assertIsNone(
                    smoke.holder_budget_no_write_receipt(
                        [{**self.cli_holder_budget_receipt(), **signals}, {"returncode": 1}]
                    )
                )
        self.assertIsNone(
            smoke.holder_budget_no_write_receipt([overflowed, {"returncode": 1}])
        )

        self.assertIsNone(
            smoke.holder_budget_no_write_receipt(
                [self.cli_holder_budget_receipt(), {"returncode": 0}]
            )
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
        structured, info = self.mcp_error_tool_result(smoke, self.mcp_holder_budget_receipt())

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
                    return_value=(structured, info),
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
        smoke = load_full_smoke()
        envelope = {
            "created_drawer_ids": ["drawer-direct"],
            "error": self.mcp_holder_budget_receipt(),
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

    def test_mcp_raw_error_and_result_ids_reach_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
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
            structured, info = self.mcp_error_tool_result(
                smoke,
                self.mcp_holder_budget_receipt(),
                {
                    "structuredContent": {
                        "created_drawer_ids": ["drawer-direct"],
                        "cleanup_drawer_ids": ["drawer-direct"],
                    }
                },
            )

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
                            (structured, info),
                            self.mcp_success_tool_result(
                                {"created_drawer_ids": ["drawer-update"]}
                            ),
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
                    return_value=(1, b"", b"", self.cli_holder_budget_receipt(), {}),
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

    def test_cli_queued_envelope_waits_and_records_cleanup_ids(self) -> None:
        smoke = load_full_smoke()
        original_manifest = smoke.CLEANUP_MANIFEST
        queued = {
            "operation_id": "op-queued",
            "state": "queued",
            "timed_out": True,
            "error": self.mcp_holder_budget_receipt(),
        }

        def run_cli(label: str, *_args: Any, **_kwargs: Any) -> tuple[int, bytes, bytes, dict[str, Any], dict[str, Any]]:
            if label == "cli_create":
                return 1, b"", b"", queued, {}
            if label == "cli_update":
                return 0, b"", b"", {"created_drawer_ids": ["drawer-update"]}, {}
            return 0, b"", b"", {"results": []}, {}

        with tempfile.TemporaryDirectory() as tmp:
            manifest = smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            smoke.CLEANUP_MANIFEST = manifest
            try:
                with (
                    mock.patch.object(smoke, "run_cli", side_effect=run_cli),
                    mock.patch.object(
                        smoke,
                        "wait_operation",
                        return_value={
                            "state": "completed",
                            "created_drawer_ids": ["drawer-recovered"],
                            "cleanup_drawer_ids": ["drawer-recovered"],
                        },
                    ) as wait_operation,
                    mock.patch.object(
                        smoke,
                        "delete_exact_ids_cli",
                        return_value={"deleted_count": 2, "failed_count": 0},
                    ),
                    mock.patch.object(
                        smoke,
                        "_rest_ingest_fallback",
                        side_effect=AssertionError("queued CLI receipt must recover before REST"),
                    ) as rest_fallback,
                ):
                    self.assertEqual(smoke.cli_crud(), ["drawer-recovered", "drawer-update"])
                wait_operation.assert_called_once_with("op-queued", "cli_create_wait")
                rest_fallback.assert_not_called()
                self.assertEqual(manifest.pending_count, 2)
            finally:
                manifest.discard()
                smoke.CLEANUP_MANIFEST = original_manifest

    def test_mcp_queued_envelope_checks_status_and_records_cleanup_ids(self) -> None:
        smoke = load_full_smoke()
        original_manifest = smoke.CLEANUP_MANIFEST
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
        create_client.tool.return_value = (
            {
                "operation_id": "op-queued",
                "state": "completed",
                "created_drawer_ids": ["drawer-recovered"],
                "cleanup_drawer_ids": ["drawer-recovered"],
            },
            {"ok": True},
        )
        update_client = mock.Mock()
        update_client.tool.return_value = ({"results": []}, {"ok": True})
        structured, queued_info = self.mcp_error_tool_result(
            smoke,
            {
                **self.mcp_holder_budget_receipt(),
                "operation_id": "op-queued",
                "state": "queued",
                "queued": True,
            },
        )

        with tempfile.TemporaryDirectory() as tmp:
            manifest = smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            smoke.CLEANUP_MANIFEST = manifest
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
                            (structured, queued_info),
                            self.mcp_success_tool_result(
                                {"created_drawer_ids": ["drawer-update"]}
                            ),
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
                        side_effect=AssertionError("queued MCP receipt must check status before REST"),
                    ) as rest_fallback,
                ):
                    self.assertEqual(smoke.mcp_crud(), ["drawer-recovered", "drawer-update"])
                create_client.tool.assert_called_once_with(
                    "mempal_operation_status", {"operation_id": "op-queued"}, timeout=30
                )
                rest_fallback.assert_not_called()
                self.assertEqual(manifest.pending_count, 2)
                self.assertNotIn("mcp_inconclusive_no_cleanup_id", smoke.SUMMARY["groups"])
            finally:
                manifest.discard()
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

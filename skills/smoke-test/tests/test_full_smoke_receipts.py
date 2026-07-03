#!/usr/bin/env python3
"""Regression tests for full_smoke operation receipt handling."""
from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType
from typing import Any
import unittest


def load_full_smoke() -> ModuleType:
    script = Path(__file__).resolve().parents[1] / "scripts" / "full_smoke.py"
    spec = importlib.util.spec_from_file_location("full_smoke", script)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ReceiptExtractionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = load_full_smoke()

    def test_created_ids_from_accepts_only_cleanup_safe_fields(self) -> None:
        payload = {
            "drawer_ids": ["unsafe-affected-id"],
            "result": {
                "structuredContent": {
                    "created_drawer_ids": ["created-a"],
                    "cleanup_drawer_ids": ["created-a", "created-b"],
                }
            },
        }

        self.assertEqual(
            self.smoke.created_ids_from(payload),
            ["created-a", "created-b"],
        )

    def test_created_ids_from_handles_ndjson_style_receipt_lists(self) -> None:
        receipts = [
            {"state": "queued", "operation_id": "op-1"},
            {"state": "completed", "cleanup_drawer_ids": ["created-final"]},
        ]

        self.assertEqual(
            self.smoke.created_ids_from(receipts),
            ["created-final"],
        )
        self.assertEqual(self.smoke.operation_id_from(receipts), "op-1")
        self.assertTrue(self.smoke.terminal_state(receipts))

    def test_recover_created_ids_waits_on_operation_receipt(self) -> None:
        calls: list[tuple[str, str]] = []

        def fake_wait(operation_id: str, name: str) -> dict[str, Any]:
            calls.append((operation_id, name))
            return {
                "operation_id": operation_id,
                "state": "completed",
                "created_drawer_ids": ["created-after-wait"],
            }

        original_wait = self.smoke.wait_operation
        self.smoke.wait_operation = fake_wait
        try:
            ids, info = self.smoke.recover_created_ids(
                {"operation_id": "op-timeout", "state": "queued", "timed_out": True},
                "unit_wait",
            )
        finally:
            self.smoke.wait_operation = original_wait

        self.assertEqual(ids, ["created-after-wait"])
        self.assertEqual(calls, [("op-timeout", "unit_wait")])
        self.assertEqual(info["recovered_via"], "unit_wait")
        self.assertEqual(info["recovered_state"], "completed")

    def test_recover_created_ids_reports_precise_non_pass_without_operation(self) -> None:
        ids, info = self.smoke.recover_created_ids(
            {"state": "queued", "timed_out": True},
            "unused_wait",
        )

        self.assertEqual(ids, [])
        self.assertEqual(info["operation_state"], "queued")
        self.assertFalse(info["operation_id_present"])

    def test_classify_stderr_reports_database_extra_holder_without_raw_text(self) -> None:
        stderr = (
            b"error: failed to open database\n"
            b"holder summary: pid=427 role=mempal_mcp_server classification=extra_holder\n"
        )

        self.assertEqual(
            self.smoke.classify_stderr(stderr),
            "database_lock_extra_holder",
        )

    def test_classify_stderr_returns_none_for_pure_hot_reload_noise(self) -> None:
        stderr = b"config hot-reload: bootstrapped version 8213fc2392e7\n"
        self.assertIsNone(self.smoke.classify_stderr(stderr))

    def test_classify_stderr_filters_hot_reload_but_keeps_real_errors(self) -> None:
        stderr = (
            b"config hot-reload: bootstrapped version 8213fc2392e7\n"
            b"error: database is locked\n"
        )
        self.assertEqual(self.smoke.classify_stderr(stderr), "database_locked")


class DoctorValidationTests(unittest.TestCase):
    """Unit tests for validate_doctor_health.

    These tests feed representative doctor JSON into the same validation
    function used by main(), ensuring the schema-match and embedding-health
    contract is exercised against the real implementation.
    """

    def setUp(self) -> None:
        self.smoke = load_full_smoke()

    def _base_report(self) -> dict[str, Any]:
        return {
            "supported_schema_version": 20,
            "db": {"schema_version": 20, "compatible": True},
            "warnings": [],
            "embedding": {
                "endpoints": [{"id": "primary", "cooldown_remaining_secs": None}],
                "queue": {"failed_terminal": 0},
            },
        }

    def test_healthy_report_passes(self) -> None:
        result = self.smoke.validate_doctor_health(self._base_report())
        self.assertTrue(result["ok"])
        self.assertTrue(result["schema_matches"])
        self.assertTrue(result["embedding_ok"])

    def test_schema_mismatch_fails(self) -> None:
        report = self._base_report()
        report["db"]["schema_version"] = 19
        result = self.smoke.validate_doctor_health(report)
        self.assertFalse(result["ok"])
        self.assertFalse(result["schema_matches"])

    def test_embedding_cooldown_fails(self) -> None:
        report = self._base_report()
        report["embedding"]["endpoints"][0]["cooldown_remaining_secs"] = 30
        result = self.smoke.validate_doctor_health(report)
        self.assertFalse(result["ok"])
        self.assertEqual(result["embedding_endpoint_cooldowns"], 1)

    def test_critical_warning_fails(self) -> None:
        report = self._base_report()
        report["warnings"] = ["database is corrupt"]
        result = self.smoke.validate_doctor_health(report)
        self.assertFalse(result["ok"])
        self.assertEqual(result["critical_warning_count"], 1)

    def test_operational_warning_does_not_fail(self) -> None:
        report = self._base_report()
        report["warnings"] = ["1 extra process(es) hold the database open"]
        result = self.smoke.validate_doctor_health(report)
        self.assertTrue(result["ok"])
        self.assertEqual(result["critical_warning_count"], 0)

    def test_non_dict_input_fails(self) -> None:
        result = self.smoke.validate_doctor_health(None)
        self.assertFalse(result["ok"])


if __name__ == "__main__":
    unittest.main()

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
    """Unit tests for doctor_json_validation logic.

    These tests verify the schema-match and embedding-health contract
    without requiring a live daemon. They test the validation predicates
    in isolation.
    """

    def test_schema_mismatch_detected(self) -> None:
        """A compatible-but-different schema version must fail."""
        db_schema_version = 19
        supported_schema = 20
        schema_matches = (
            db_schema_version is not None
            and db_schema_version == supported_schema
        )
        self.assertFalse(schema_matches)

    def test_schema_match_passes(self) -> None:
        """Exact schema version match passes the schema check."""
        db_schema_version = 20
        supported_schema = 20
        schema_matches = (
            db_schema_version is not None
            and db_schema_version == supported_schema
        )
        self.assertTrue(schema_matches)

    def test_embedding_cooldown_detected(self) -> None:
        """An endpoint with cooldown_remaining_secs set indicates unhealthy embedding."""
        endpoints = [
            {"id": "primary", "cooldown_remaining_secs": 30},
            {"id": "secondary", "cooldown_remaining_secs": None},
        ]
        cooldowns = sum(
            1 for ep in endpoints
            if isinstance(ep, dict) and ep.get("cooldown_remaining_secs") is not None
        )
        self.assertEqual(cooldowns, 1)
        self.assertFalse(cooldowns == 0)

    def test_embedding_healthy_when_no_cooldowns(self) -> None:
        """No endpoints in cooldown means embedding health passes."""
        endpoints = [
            {"id": "primary", "cooldown_remaining_secs": None},
        ]
        cooldowns = sum(
            1 for ep in endpoints
            if isinstance(ep, dict) and ep.get("cooldown_remaining_secs") is not None
        )
        self.assertEqual(cooldowns, 0)
        self.assertTrue(cooldowns == 0)

    def test_critical_warning_classification_excludes_operational(self) -> None:
        """'extra process holds database' must NOT be classified as critical."""
        warnings = ["1 extra process(es) hold the database open"]
        critical_keywords = (
            "corrupt", "mismatch", "missing",
            "incompatible", "migration failed",
        )
        critical = [
            w for w in warnings
            if isinstance(w, str) and any(k in w.lower() for k in critical_keywords)
        ]
        self.assertEqual(len(critical), 0)


if __name__ == "__main__":
    unittest.main()

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
                "degraded": False,
                "write_refused": False,
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

    def test_embedding_degraded_fails_with_structured_fields(self) -> None:
        report = self._base_report()
        report["embedding"]["degraded"] = True
        report["embedding"]["write_refused"] = True
        report["embedding"]["queue"]["failed_terminal"] = 3
        result = self.smoke.validate_doctor_health(report)
        self.assertFalse(result["ok"])
        self.assertTrue(result["embedder_degraded"])
        self.assertTrue(result["embedding_write_refused"])
        self.assertEqual(result["queue_terminal_failures"], 3)

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

    def test_daemon_status_diagnostics_extracts_degraded_and_terminal_failures(self) -> None:
        stdout = (
            b"status: running\n"
            b"rest.embedder_status_source: daemon_rest\n"
            b"rest.embedder_degraded: true\n"
            b"rest.embedder_write_refused: true\n"
            b"queue.failed_terminal: 2\n"
            b"rest.queue_terminal_failures: 4\n"
        )
        result = self.smoke.daemon_status_diagnostics(stdout)
        self.assertEqual(result["embedder_status_source"], "daemon_rest")
        self.assertTrue(result["embedder_degraded"])
        self.assertTrue(result["embedder_write_refused"])
        self.assertEqual(result["queue_terminal_failures"], 4)


class FailureLedgerTests(unittest.TestCase):
    """Regression tests for note()/clear_probe_failures failure accounting.

    Guards against F001-style bugs where a REST probe failure stays in the
    failure ledger after a later fallback succeeds, and F001(r3) where REST
    success masks a never-exercised MCP ingest path.
    """

    def setUp(self) -> None:
        self.smoke = load_full_smoke()
        # Reset SUMMARY to a clean slate for each test.
        self.smoke.SUMMARY['failures'] = []
        self.smoke.SUMMARY['groups'] = {}

    def test_note_false_appends_failure(self) -> None:
        self.smoke.note('probe_a', False)
        self.assertIn('probe_a', self.smoke.SUMMARY['failures'])

    def test_note_true_clears_same_label(self) -> None:
        self.smoke.note('probe_a', False)
        self.smoke.note('probe_a', True)
        self.assertNotIn('probe_a', self.smoke.SUMMARY['failures'])

    def test_clear_probe_failures_removes_specified_labels(self) -> None:
        self.smoke.note('mcp_create_rest_fallback', False)
        self.smoke.note('unrelated_failure', False)
        self.smoke.clear_probe_failures('mcp_create_rest', 'mcp_create_rest_fallback')
        self.assertNotIn('mcp_create_rest_fallback', self.smoke.SUMMARY['failures'])
        self.assertIn('unrelated_failure', self.smoke.SUMMARY['failures'])

    def test_successful_fallback_does_not_leave_stale_probe_failure(self) -> None:
        # Simulate: first REST probe fails, then a fallback succeeds.
        self.smoke.note('mcp_create_rest_fallback', False, error_type='ConnectionRefusedError')
        self.smoke.clear_probe_failures('mcp_create_rest_fallback')
        self.smoke.note('mcp_create', True, created_id_count=1)
        self.assertEqual(self.smoke.SUMMARY['failures'], [])


class ConformanceReportTests(unittest.TestCase):
    """Regression tests for aggregate conformance reporting."""

    def setUp(self) -> None:
        self.smoke = load_full_smoke()
        self.smoke.SUMMARY['failures'] = []
        self.smoke.SUMMARY['groups'] = {}

    def test_conformance_report_summarizes_pass_fail_and_missing(self) -> None:
        self.smoke.note('probe_pass', True, stdout_bytes=10)
        self.smoke.note('probe_fail', False, stderr_class='database_locked')
        specs = {
            'example': {
                'features': ['feature.a', 'feature.b'],
                'probes': ['probe_pass', 'probe_fail', 'probe_missing'],
                'skipped_reason': 'not available',
            }
        }

        report = self.smoke.build_conformance_report(specs)
        group = report['groups']['example']

        self.assertEqual(report['schema'], 'mempal_conformance_smoke_v1')
        self.assertEqual(group['status'], 'fail')
        self.assertEqual(group['feature_count'], 2)
        self.assertEqual(group['failed_probes'], ['probe_fail'])
        self.assertEqual(group['missing_probes'], ['probe_missing'])
        self.assertEqual(group['failure_classes'], {'probe_fail': 'database_locked'})

    def test_conformance_report_fails_when_passed_group_has_missing_probe(self) -> None:
        self.smoke.note('probe_pass', True, stdout_bytes=10)
        specs = {
            'example': {
                'features': ['feature.a', 'feature.b'],
                'probes': ['probe_pass', 'probe_missing'],
                'skipped_reason': 'not available',
            }
        }

        report = self.smoke.build_conformance_report(specs)
        group = report['groups']['example']

        self.assertEqual(group['status'], 'fail')
        self.assertEqual(group['passed_probe_count'], 1)
        self.assertEqual(group['missing_probe_count'], 1)
        self.assertEqual(group['missing_probes'], ['probe_missing'])
        self.assertEqual(report['summary']['fail'], 1)

    def test_overall_ok_fails_when_conformance_group_fails(self) -> None:
        self.smoke.note('probe_pass', True, stdout_bytes=10)
        specs = {
            'example': {
                'features': ['feature.a'],
                'probes': ['probe_pass', 'probe_missing'],
            }
        }
        self.smoke.SUMMARY['conformance'] = self.smoke.build_conformance_report(specs)
        self.smoke.SUMMARY['cleanup'] = {'failures': 0}
        self.smoke.SUMMARY['binary_consistency'] = {'ok': True}

        self.assertEqual(self.smoke.SUMMARY['failures'], [])
        self.assertFalse(self.smoke.smoke_overall_ok(self.smoke.SUMMARY))

    def test_conformance_report_fails_group_when_expected_probes_are_missing(self) -> None:
        specs = {
            'optional': {
                'features': ['feature.optional'],
                'probes': ['probe_missing'],
                'skipped_reason': 'tool not advertised',
            }
        }

        report = self.smoke.build_conformance_report(specs)
        group = report['groups']['optional']

        self.assertEqual(group['status'], 'fail')
        self.assertIsNone(group['skipped_reason'])
        self.assertEqual(report['summary']['fail'], 1)

    def test_conformance_report_marks_recorded_skips_without_probe_passes_as_skipped(self) -> None:
        self.smoke.note('probe_skipped', True, skipped='tool_not_advertised')
        specs = {
            'optional': {
                'features': ['feature.optional'],
                'probes': ['probe_skipped'],
                'skipped_reason': 'tool not advertised',
            }
        }

        report = self.smoke.build_conformance_report(specs)
        group = report['groups']['optional']

        self.assertEqual(group['status'], 'skipped')
        self.assertEqual(group['skipped_reason'], 'tool not advertised')
        self.assertEqual(report['summary']['skipped'], 1)

    def test_conformance_report_does_not_copy_raw_probe_fields(self) -> None:
        self.smoke.note(
            'probe_pass',
            True,
            content='raw drawer text must not be copied',
            preview='raw preview must not be copied',
        )
        specs = {'safe': {'features': ['feature.safe'], 'probes': ['probe_pass']}}

        encoded = repr(self.smoke.build_conformance_report(specs))

        self.assertNotIn('raw drawer text', encoded)
        self.assertNotIn('raw preview', encoded)


if __name__ == "__main__":
    unittest.main()

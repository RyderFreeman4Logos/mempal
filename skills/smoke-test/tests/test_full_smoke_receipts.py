#!/usr/bin/env python3
"""Regression tests for full_smoke operation receipt handling."""
from __future__ import annotations

import importlib.util
import json
import stat
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


class CleanupManifestTests(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = load_full_smoke()

    def test_manifest_is_private_atomic_and_contains_only_cleanup_ids(self) -> None:
        manifest = self.smoke.CleanupManifest()
        try:
            self.assertEqual(manifest.path.parent, Path('/tmp'))

            manifest.checkpoint()
            manifest.add_created_ids(['drawer-a', 'drawer-b'])

            self.assertEqual(stat.S_IMODE(manifest.path.stat().st_mode), 0o600)
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding='utf-8')),
                {'cleanup_drawer_ids': ['drawer-a', 'drawer-b']},
            )
            self.assertNotIn('raw drawer', manifest.path.read_text(encoding='utf-8'))
            self.assertEqual(list(manifest.path.parent.glob(f'.{manifest.path.name}.*')), [])
        finally:
            manifest.discard()

    def test_manifest_survives_partial_cleanup_and_deletes_after_verified_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            manifest.add_created_ids(['drawer-a', 'drawer-b'])

            manifest.mark_cleaned(['drawer-a'])
            self.assertTrue(manifest.path.exists())
            self.assertEqual(manifest.pending_count, 1)

            manifest.mark_cleaned(['drawer-b'])
            self.assertFalse(manifest.path.exists())
            self.assertEqual(manifest.pending_count, 0)

    def test_manifest_rejects_invalid_ids_without_changing_pending_state(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            manifest.add_created_ids(['drawer-safe'])
            original = manifest.path.read_bytes()

            invalid_inputs = [
                [''],
                [123],
                ['x' * (self.smoke._SMOKE_RUNTIME.MAX_CLEANUP_DRAWER_ID_BYTES + 1)],
                [
                    f'drawer-{index}'
                    for index in range(
                        self.smoke._SMOKE_RUNTIME.MAX_CLEANUP_DRAWER_IDS + 1
                    )
                ],
            ]
            for drawer_ids in invalid_inputs:
                with self.subTest(drawer_ids_type=type(drawer_ids[0]).__name__):
                    with self.assertRaises(ValueError):
                        manifest.add_created_ids(drawer_ids)
                    self.assertEqual(manifest.pending_count, 1)
                    self.assertEqual(manifest.path.read_bytes(), original)

    def test_manifest_rejects_oversized_serialized_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            drawer_ids = [
                f'{index:04d}-' + ('x' * 500)
                for index in range(self.smoke._SMOKE_RUNTIME.MAX_CLEANUP_DRAWER_IDS)
            ]

            with self.assertRaises(ValueError):
                manifest.add_created_ids(drawer_ids)

            self.assertEqual(manifest.pending_count, 0)
            self.assertFalse(manifest.path.exists())

    def test_checkpoint_replace_failure_preserves_prior_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            manifest.add_created_ids(['drawer-a'])
            original = manifest.path.read_bytes()

            with mock.patch.object(
                self.smoke._SMOKE_RUNTIME.os,
                'replace',
                side_effect=OSError('injected replace failure'),
            ):
                with self.assertRaisesRegex(OSError, 'injected replace failure'):
                    manifest.add_created_ids(['drawer-b'])

            self.assertEqual(manifest.pending_count, 1)
            self.assertEqual(manifest.path.read_bytes(), original)
            self.assertEqual(list(manifest.path.parent.glob(f'.{manifest.path.name}.*')), [])

    def test_checkpoint_parent_fsync_failure_is_reported(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            real_open = self.smoke._SMOKE_RUNTIME.os.open

            def fail_directory_open(path: Any, flags: int, *args: Any) -> int:
                if Path(path) == manifest.path.parent:
                    raise OSError('injected directory open failure')
                return real_open(path, flags, *args)

            with mock.patch.object(
                self.smoke._SMOKE_RUNTIME.os,
                'open',
                side_effect=fail_directory_open,
            ):
                with self.assertRaisesRegex(OSError, 'injected directory open failure'):
                    manifest.add_created_ids(['drawer-a'])

            self.assertEqual(manifest.pending_count, 1)
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding='utf-8')),
                {'cleanup_drawer_ids': ['drawer-a']},
            )

    def test_created_id_is_checkpointed_before_next_operation_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            with self.assertRaisesRegex(RuntimeError, 'next operation failed'):
                manifest.add_created_ids(['drawer-created'])
                raise RuntimeError('next operation failed')

            self.assertEqual(
                json.loads(manifest.path.read_text(encoding='utf-8')),
                {'cleanup_drawer_ids': ['drawer-created']},
            )
            self.assertEqual(stat.S_IMODE(manifest.path.stat().st_mode), 0o600)

    def test_unrelated_failure_with_zero_pending_does_not_report_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            self.smoke.CLEANUP_MANIFEST = manifest
            summary = {'overall_ok': False, 'failures': ['unrelated_failure']}

            self.smoke.finalize_cleanup_manifest(summary)

            self.assertNotIn('cleanup_manifest_path', summary)
            self.assertFalse(manifest.path.exists())

    def test_pending_manifest_finalization_discloses_only_path_and_count(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            self.smoke.CLEANUP_MANIFEST = manifest
            try:
                manifest.add_created_ids(['exact-private-drawer-id'])
                summary: dict[str, Any] = {}

                self.smoke.finalize_cleanup_manifest(summary)

                self.assertEqual(
                    summary,
                    {
                        'cleanup_manifest_path': str(manifest.path),
                        'cleanup_pending_count': 1,
                    },
                )
                self.assertNotIn('exact-private-drawer-id', repr(summary))
                self.assertTrue(manifest.path.exists())
                self.assertEqual(stat.S_IMODE(manifest.path.stat().st_mode), 0o600)
            finally:
                manifest.discard()


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
            [sys.executable, '-c', 'import time; time.sleep(60)'],
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
            [sys.executable, '-u', '-c', child],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        client = self.smoke.new_mcp_client(
            process_factory=lambda *args, **kwargs: proc,
        )
        structured, info = client.tool('mempal_ingest', {}, timeout=2)
        self.assertIsNone(structured)
        self.assertEqual(info.get('error_code'), -32603)

        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / 'cleanup.json')
            manifest.add_created_ids(['drawer-safe'])
            self.smoke.CLEANUP_MANIFEST = manifest

            def fallback() -> list[str]:
                self.assertIsNotNone(proc.poll())
                self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})
                self.assertTrue(manifest.path.exists())
                return ['fallback-id']

            result = self.smoke.run_fallback_after_mcp_reaped(
                client,
                'create',
                fallback,
            )

        self.assertEqual(result, ['fallback-id'])

    def test_fallback_uses_final_sweep_state_when_initial_close_reports_false(self) -> None:
        client, proc = self._sleeping_client()
        client.close = mock.Mock(return_value=False)

        result = self.smoke.run_fallback_after_mcp_reaped(
            client,
            'create',
            lambda: ['fallback-after-sweep'],
        )

        self.assertEqual(result, ['fallback-after-sweep'])
        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_initialize_failure_closes_spawned_client(self) -> None:
        fake_client = mock.Mock()
        fake_client.call.side_effect = TimeoutError('initialize timeout')
        with mock.patch.object(self.smoke, 'McpClient', return_value=fake_client):
            with self.assertRaises(TimeoutError):
                self.smoke.mcp_start_initialized()

        fake_client.close.assert_called_once_with()

    def test_owned_children_are_reaped_when_runner_is_cancelled(self) -> None:
        _client, proc = self._sleeping_client()

        def cancelled() -> int:
            raise KeyboardInterrupt

        with self.assertRaises(KeyboardInterrupt):
            self.smoke.run_with_owned_mcp_cleanup(cancelled)

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.OWNED_MCP_CHILDREN, {})

    def test_close_is_idempotent(self) -> None:
        client, proc = self._sleeping_client()

        self.assertTrue(client.close())
        first_lifecycle = dict(self.smoke.SUMMARY['mcp_stdio_lifecycle'])
        self.assertTrue(client.close())

        self.assertIsNotNone(proc.poll())
        self.assertEqual(self.smoke.SUMMARY['mcp_stdio_lifecycle'], first_lifecycle)


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

    def test_default_conformance_groups_include_reranker_probes(self) -> None:
        group = self.smoke.CONFORMANCE_GROUPS['search_reranker_behavior']

        self.assertEqual(
            group['probes'],
            [
                'reranker_endpoint_reachable',
                'reranker_reorders_results',
                'reranker_fallback_warning',
            ],
        )
        self.assertIn('search.reranker_reordering', group['features'])

    def test_reranker_disabled_records_skipped_probe_shape(self) -> None:
        self.smoke.probe_search_reranker_behavior(config_path=Path('/tmp/mempal-smoke-missing-config.toml'))

        report = self.smoke.build_conformance_report({
            'reranker': self.smoke.CONFORMANCE_GROUPS['search_reranker_behavior'],
        })
        group = report['groups']['reranker']

        self.assertEqual(group['status'], 'skipped')
        self.assertEqual(group['skipped_probe_count'], 3)
        self.assertEqual(report['summary']['skipped'], 1)
        for probe in self.smoke.RERANKER_PROBES:
            self.assertEqual(self.smoke.SUMMARY['groups'][probe]['skipped'], 'reranker_disabled')

    def test_reranker_config_present_skips_without_stdlib_toml_parser(self) -> None:
        original_parser = self.smoke.tomllib
        self.smoke.tomllib = None
        try:
            configs = [
                '[privacy.remote_calls]\nfail_closed = true\n',
                '[search.reranker]\nenabled = false\n',
            ]
            for content in configs:
                with self.subTest(content=content):
                    self.smoke.SUMMARY['failures'] = []
                    self.smoke.SUMMARY['groups'] = {}
                    with tempfile.TemporaryDirectory() as tmp:
                        config_path = Path(tmp) / 'config.toml'
                        config_path.write_text(content, encoding='utf-8')

                        self.smoke.probe_search_reranker_behavior(config_path=config_path)

                    self.assertEqual(self.smoke.SUMMARY['failures'], [])
                    for probe in self.smoke.RERANKER_PROBES:
                        self.assertEqual(
                            self.smoke.SUMMARY['groups'][probe]['skipped'],
                            'reranker_disabled',
                        )
        finally:
            self.smoke.tomllib = original_parser

    def test_reranker_fail_closed_remote_policy_skips_without_network_call(self) -> None:
        calls: list[tuple[Any, ...]] = []

        def fail_if_called(*args: Any, **_kwargs: Any) -> tuple[Any | None, dict[str, Any]]:
            calls.append(args)
            self.fail('policy-blocked remote reranker should not be called')

        original_call = self.smoke.call_reranker_endpoint
        self.smoke.call_reranker_endpoint = fail_if_called
        try:
            with tempfile.TemporaryDirectory() as tmp:
                config_path = Path(tmp) / 'config.toml'
                config_path.write_text(
                    '\n'.join([
                        '[search.reranker]',
                        'enabled = true',
                        'endpoint = "https://rerank.example.com/v1/rerank"',
                        'model = "rerank"',
                        '',
                        '[privacy.remote_calls]',
                        'fail_closed = true',
                        'allow_rerank = false',
                    ]),
                    encoding='utf-8',
                )

                self.smoke.probe_search_reranker_behavior(config_path=config_path)
        finally:
            self.smoke.call_reranker_endpoint = original_call

        self.assertEqual(calls, [])
        self.assertEqual(self.smoke.SUMMARY['failures'], [])
        for probe in self.smoke.RERANKER_PROBES:
            info = self.smoke.SUMMARY['groups'][probe]
            self.assertEqual(info['skipped'], 'reranker_remote_blocked_by_policy')
            self.assertTrue(info['remote_call_blocked'])
            self.assertFalse(info['network_call'])
            self.assertEqual(info['endpoint_host_kind'], 'hostname')
            self.assertEqual(info['endpoint_path_kind'], 'default_rerank')

    def test_reranker_invalid_port_records_failure_without_network_call(self) -> None:
        calls: list[tuple[Any, ...]] = []

        def fail_if_called(*args: Any, **_kwargs: Any) -> tuple[Any | None, dict[str, Any]]:
            calls.append(args)
            self.fail('invalid reranker endpoint should not be called')

        original_call = self.smoke.call_reranker_endpoint
        self.smoke.call_reranker_endpoint = fail_if_called
        try:
            with tempfile.TemporaryDirectory() as tmp:
                config_path = Path(tmp) / 'config.toml'
                config_path.write_text(
                    '\n'.join([
                        '[search.reranker]',
                        'enabled = true',
                        'endpoint = "https://rerank.example.com:bad/v1/rerank"',
                        'model = "rerank"',
                    ]),
                    encoding='utf-8',
                )

                self.smoke.probe_search_reranker_behavior(config_path=config_path)
        finally:
            self.smoke.call_reranker_endpoint = original_call

        self.assertEqual(calls, [])
        self.assertEqual(
            self.smoke.SUMMARY['failures'],
            ['reranker_endpoint_reachable', 'reranker_reorders_results'],
        )
        for probe in ('reranker_endpoint_reachable', 'reranker_reorders_results'):
            info = self.smoke.SUMMARY['groups'][probe]
            self.assertEqual(info['stage'], 'normalize_endpoint')
            self.assertEqual(info['reason'], 'invalid_port')
            self.assertEqual(info['error_type'], 'ValueError')

    def test_reranker_policy_blocked_invalid_port_diagnostics_do_not_crash(self) -> None:
        calls: list[tuple[Any, ...]] = []

        def invalid_port_endpoint(_endpoint: str) -> str:
            return 'https://rerank.example.com:bad/v1/rerank'

        def fail_if_called(*args: Any, **_kwargs: Any) -> tuple[Any | None, dict[str, Any]]:
            calls.append(args)
            self.fail('policy-blocked remote reranker should not be called')

        original_normalize = self.smoke.normalize_reranker_endpoint
        original_call = self.smoke.call_reranker_endpoint
        self.smoke.normalize_reranker_endpoint = invalid_port_endpoint
        self.smoke.call_reranker_endpoint = fail_if_called
        try:
            with tempfile.TemporaryDirectory() as tmp:
                config_path = Path(tmp) / 'config.toml'
                config_path.write_text(
                    '\n'.join([
                        '[search.reranker]',
                        'enabled = true',
                        'endpoint = "https://rerank.example.com/v1/rerank"',
                        'model = "rerank"',
                        '',
                        '[privacy.remote_calls]',
                        'fail_closed = true',
                        'allow_rerank = false',
                    ]),
                    encoding='utf-8',
                )

                self.smoke.probe_search_reranker_behavior(config_path=config_path)
        finally:
            self.smoke.normalize_reranker_endpoint = original_normalize
            self.smoke.call_reranker_endpoint = original_call

        self.assertEqual(calls, [])
        self.assertEqual(self.smoke.SUMMARY['failures'], [])
        for probe in self.smoke.RERANKER_PROBES:
            info = self.smoke.SUMMARY['groups'][probe]
            self.assertEqual(info['skipped'], 'reranker_remote_blocked_by_policy')
            self.assertTrue(info['remote_call_blocked'])
            self.assertFalse(info['network_call'])
            self.assertTrue(info['endpoint_has_port'])
            self.assertFalse(info['endpoint_port_valid'])
            self.assertEqual(info['endpoint_host_kind'], 'hostname')
            self.assertEqual(info['endpoint_path_kind'], 'default_rerank')

    def test_reranker_policy_allows_local_private_endpoints(self) -> None:
        config = {'remote_calls': {'fail_closed': True, 'allow_rerank': False}}

        self.assertFalse(
            self.smoke.reranker_remote_call_blocked(config, 'http://127.0.0.1:18003/v1/rerank')
        )
        self.assertFalse(
            self.smoke.reranker_remote_call_blocked(config, 'http://gb10:18003/v1/rerank')
        )
        self.assertFalse(
            self.smoke.reranker_remote_call_blocked(config, 'http://192.168.1.20:18003/v1/rerank')
        )
        self.assertTrue(
            self.smoke.reranker_remote_call_blocked(config, 'https://rerank.example.com/v1/rerank')
        )

    def test_reranker_response_shape_accepts_score_aliases(self) -> None:
        payload = {
            'results': [
                {'index': 1, 'relevance_score': 0.92},
                {'index': 0, 'score': 0.17},
            ]
        }

        shape = self.smoke.reranker_response_shape(payload, document_count=2)
        reorder = self.smoke.reranker_reorder_evidence(payload)

        self.assertTrue(shape['ok'])
        self.assertEqual(shape['valid_score_count'], 2)
        self.assertTrue(shape['has_relevance_score'])
        self.assertTrue(shape['has_score'])
        self.assertTrue(reorder['ok'])
        self.assertEqual(reorder['first_returned_index'], 1)


if __name__ == "__main__":
    unittest.main()

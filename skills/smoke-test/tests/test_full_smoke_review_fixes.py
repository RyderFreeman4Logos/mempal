#!/usr/bin/env python3
"""Focused regressions for full_smoke review fixes."""
from __future__ import annotations

import importlib.util
import io
import json
import time
import tempfile
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


def cli_holder_budget_receipt() -> dict[str, object]:
    return {
        "outcome": "admission_blocked",
        "reason": "holder_budget_exceeded",
        "action": "write_refused",
        "created_drawer_ids": [],
        "cleanup_drawer_ids": [],
        "capacity": {"holders": 2, "cache_bytes": 8},
        "headroom": {"holders": 0, "cache_bytes": 4},
        "profile_admission": {
            "active_holders": 2,
            "configured_holder_limit": 2,
            "active_cache_bytes": 4,
            "configured_cache_bytes": 8,
            "reaped_stale_holders_this_snapshot": 0,
            "reserved_service_holders": 1,
            "service_holders": 2,
            "requested_cache_bytes": 1,
            "budget_reason": "holder_limit",
        },
    }


def mcp_holder_budget_envelope() -> dict[str, object]:
    receipt = cli_holder_budget_receipt()
    profile = receipt["profile_admission"]
    assert isinstance(profile, dict)
    profile.update({
        "available_cache_bytes": 4,
        "capacity": {"holders": 2, "cache_bytes": 8},
        "headroom": {"holders": 0, "cache_bytes": 4},
        "unknown_holders": 0,
        "unknown_holder_diagnostics": [],
        "async_pool_loaded": False,
    })
    receipt.update({
        "async_pool_loaded": False,
        "database_diagnostic": {
            "path": "/private/palace.db",
            "source": "async_db",
            "failure_kind": "holder_budget_exceeded",
            "summary": "holder budget exhausted",
            "hint": "retry later",
        },
    })
    return {
        "jsonrpc": "2.0",
        "id": 1,
        "error": {"code": -32603, "message": "write refused", "data": receipt},
    }


def rest_holder_budget_envelope() -> dict[str, object]:
    error = cli_holder_budget_receipt()
    error.update({
        "message": "mempal profile holder budget is exhausted; write was refused before queueing",
        "status": 503,
        "kind": "admission_blocked",
        "retryable": False,
    })
    return {"error": error}


def advertised_tools() -> dict[str, object]:
    return {
        "result": {
            "tools": [
                {"name": name}
                for name in (
                    "mempal_ingest",
                    "mempal_operation_status",
                    "mempal_search",
                    "mempal_read_drawer",
                    "mempal_delete",
                )
            ]
        }
    }


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

        error.read.assert_called_once_with(8193)
        error.close()

    def test_rest_ingest_hard_deadline_interrupts_slow_drip_http_error_body(self) -> None:
        error = HTTPError(
            "http://127.0.0.1:3080/api/ingest",
            503,
            "service unavailable",
            Message(),
            io.BytesIO(),
        )

        def slow_drip_read(n: int = -1) -> bytes:
            del n
            received = bytearray()
            while True:
                received.extend(b"x")
                time.sleep(0.01)

        error.read = slow_drip_read
        real_setitimer = self.smoke.signal.setitimer

        def accelerated_timer(
            which: int,
            seconds: float,
            interval: float = 0.0,
        ) -> tuple[float, float]:
            if seconds == 35:
                seconds = 0.1
            return real_setitimer(which, seconds, interval)

        with (
            mock.patch("urllib.request.urlopen", side_effect=error),
            mock.patch.object(self.smoke.signal, "setitimer", side_effect=accelerated_timer),
        ):
            started = time.monotonic()
            result = self.smoke._rest_ingest_fallback(
                "content", "unit_rest_slow_drip_http_error"
            )

        self.assertEqual(result, ([], {"kind": "inconclusive"}))
        self.assertLess(time.monotonic() - started, 1.0)
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

    def test_duplicate_json_keys_are_rejected_at_every_write_boundary(self) -> None:
        private_key = "private_raw_key"
        private_value = "private-raw-value"
        mcp_line = (
            b'{"jsonrpc":"2.0","id":1,"result":{"private_raw_key":"first",'
            b'"private_raw_key":"private-raw-value"}}\n'
        )
        client = object.__new__(self.smoke.McpClient)
        client._response_buffer = bytearray(mcp_line)
        with self.subTest(boundary="mcp"):
            with self.assertRaises(ValueError) as raised:
                client._take_buffered_response(1)
            self.assertNotIn(private_key, str(raised.exception))
            self.assertNotIn(private_value, str(raised.exception))

        cli_payloads = (
            b'{"created_drawer_ids":["drawer-private"],"created_drawer_ids":[],"private_raw_key":"private-raw-value"}',
            b'{"state":"queued"}\n{"cleanup_drawer_ids":["drawer-private"],"cleanup_drawer_ids":[],"private_raw_key":"private-raw-value"}\n',
        )
        for payload in cli_payloads:
            with self.subTest(boundary="cli", ndjson=b"\n" in payload.strip()):
                parsed, shape = self.smoke.parse_json_bytes(payload)
                self.assertIsNone(parsed)
                self.assertFalse(shape["ok"])
                self.assertNotIn(private_key, repr(shape))
                self.assertNotIn(private_value, repr(shape))

        rest_payload = cli_payloads[0]
        for boundary in ("success", "error"):
            with self.subTest(boundary=f"rest_{boundary}"), tempfile.TemporaryDirectory() as tmp:
                manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
                setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
                setattr(self.smoke, "SUMMARY", {"groups": {}, "failures": []})
                if boundary == "success":
                    response = mock.Mock(status=201)
                    response.read.return_value = rest_payload
                    effect: object = response
                else:
                    effect = HTTPError(
                        "http://127.0.0.1:3080/api/ingest",
                        503,
                        "service unavailable",
                        Message(),
                        io.BytesIO(rest_payload),
                    )
                with mock.patch("urllib.request.urlopen", side_effect=effect if boundary == "error" else None, return_value=effect if boundary == "success" else None):
                    self.assertEqual(
                        self.smoke._rest_ingest_fallback("content", f"unit_rest_{boundary}"),
                        ([], {"kind": "inconclusive"}),
                    )
                self.assertEqual(manifest.pending_count, 0)
                public = json.dumps(self.smoke.SUMMARY)
                self.assertNotIn(private_key, public)
                self.assertNotIn(private_value, public)
                if isinstance(effect, HTTPError):
                    effect.close()

    def test_malformed_mcp_result_retains_raw_cleanup_authority_privately(self) -> None:
        envelope = {
            "jsonrpc": "2.0",
            "id": 1,
            "result": [{
                "cleanup_drawer_ids": ["drawer-safe"],
                "private_raw_key": "private-raw-value",
            }],
        }
        client = object.__new__(self.smoke.McpClient)
        client.call = mock.Mock(return_value=envelope)
        structured, info = client.tool("mempal_ingest", {}, timeout=1)

        self.assertIsNone(structured)
        self.assertIs(info["_raw_mcp_response"], envelope)

        scalar_envelope = {"jsonrpc": "2.0", "id": 2, "result": "private-raw-value"}
        client.call = mock.Mock(return_value=scalar_envelope)
        scalar_structured, scalar_info = client.tool("mempal_ingest", {}, timeout=1)
        self.assertIsNone(scalar_structured)
        self.assertIs(scalar_info["_raw_mcp_response"], scalar_envelope)
        self.assertNotIn("private-raw-value", json.dumps(self.smoke.without_ok(scalar_info)))

        classification = self.smoke.classify_create_attempt(
            self.smoke.create_attempt_from_mcp_info(info)
        )
        self.assertEqual(
            classification,
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-safe"]},
        )
        self.assertNotIn("private_raw_key", json.dumps(self.smoke.without_ok(info)))
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(classification["cleanup_drawer_ids"])
            saved = manifest.path.read_text(encoding="utf-8")
            self.assertEqual(json.loads(saved), {"cleanup_drawer_ids": ["drawer-safe"]})
            self.assertNotIn("private_raw_key", saved)
            self.assertNotIn("private-raw-value", saved)

    def test_cleanup_ids_never_authorize_creation(self) -> None:
        cases = {
            "direct_cleanup": {"cleanup_drawer_ids": ["drawer-cleanup"]},
            "nested_cleanup": {
                "result": {
                    "structuredContent": {
                        "cleanup_drawer_ids": ["drawer-cleanup"]
                    }
                }
            },
            "failed_cli": [
                {"created_drawer_ids": ["drawer-cleanup"]},
                {"returncode": 1},
            ],
        }
        for name, attempt in cases.items():
            with self.subTest(name=name):
                self.assertEqual(
                    self.smoke.classify_create_attempt(attempt),
                    {
                        "kind": "inconclusive",
                        "cleanup_drawer_ids": ["drawer-cleanup"],
                    },
                )
        self.assertEqual(
            self.smoke.classify_create_attempt(
                {"created_drawer_ids": ["drawer-created"]}
            ),
            {
                "kind": "created",
                "created_drawer_ids": ["drawer-created"],
                "cleanup_drawer_ids": ["drawer-created"],
            },
        )

    def test_mixed_malformed_ids_preserve_known_ids_without_overriding_state(self) -> None:
        malformed_queued = {
            "operation_id": "operation-queued",
            "state": "queued",
            "created_drawer_ids": ["drawer-known", 7],
            "cleanup_drawer_ids": "not-an-array",
        }
        self.assertEqual(
            self.smoke.classify_create_attempt(malformed_queued),
            {
                "kind": "inconclusive",
                "cleanup_drawer_ids": ["drawer-known"],
            },
        )
        malformed = {"created_drawer_ids": ["drawer-known", 7]}
        self.assertEqual(
            self.smoke.classify_create_attempt(malformed),
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-known"]},
        )
        ids, info = self.smoke.recover_created_ids(malformed, "unused_wait")
        self.assertEqual(ids, ["drawer-known"])
        self.assertEqual(info["kind"], "inconclusive")
        with mock.patch.object(
            self.smoke,
            "wait_operation",
        ) as wait_operation:
            ids, info = self.smoke.recover_created_ids(
                malformed_queued, "unit_wait"
            )
        self.assertEqual(ids, ["drawer-known"])
        self.assertEqual(info["kind"], "inconclusive")
        wait_operation.assert_not_called()

        with mock.patch.object(
            self.smoke,
            "wait_operation",
            return_value={
                "operation_id": "op-B",
                "state": "completed",
                "created_drawer_ids": ["drawer-other"],
            },
        ):
            ids, info = self.smoke.recover_created_ids(
                {"operation_id": "op-A", "state": "queued"}, "unit_wait"
            )
        self.assertEqual(ids, [])
        self.assertEqual(info["kind"], "inconclusive")

    def test_operation_receipts_require_one_coherent_id(self) -> None:
        conflict = {
            "result": {"structuredContent": {"operation_id": "op-A", "state": "queued"}},
            "error": {"data": {
                "operation_id": "op-B",
                "state": "queued",
                "timed_out": True,
                "cleanup_drawer_ids": ["drawer-known"],
                "private_raw_key": "private-raw-value",
            }},
        }
        malformed = {
            "operation_id": 7,
            "state": "queued",
            "cleanup_drawer_ids": ["drawer-known"],
        }
        malformed_state = {
            "operation_id": "op-A",
            "state": "private-raw-value",
            "cleanup_drawer_ids": ["drawer-known"],
        }
        split = {
            "result": {"operation_id": "op-A"},
            "error": {"data": {
                "state": "completed",
                "timed_out": True,
                "cleanup_drawer_ids": ["drawer-known"],
            }},
        }
        terminal_conflict = [
            {
                "operation_id": "op-A",
                "state": "completed",
                "cleanup_drawer_ids": ["drawer-known"],
            },
            {"operation_id": "op-A", "state": "failed"},
        ]
        cases = (
            ("conflict", conflict),
            ("malformed_id", malformed),
            ("malformed_state", malformed_state),
            ("split", split),
            ("terminal_conflict", terminal_conflict),
        )
        for name, attempt in cases:
            with self.subTest(name=name):
                classification = self.smoke.classify_create_attempt(attempt)
                self.assertEqual(
                    classification,
                    {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-known"]},
                )
                self.assertIsNone(self.smoke.operation_id_from(attempt))
                self.assertIsNone(self.smoke.operation_state_from(attempt))
                self.assertIsNone(self.smoke.followable_timeout_operation_id(attempt))
                public = json.dumps(classification)
                self.assertNotIn("private_raw_key", public)
                self.assertNotIn("private-raw-value", public)

        coherent = {"operation_id": "op-A", "state": "queued", "timed_out": True}
        self.assertEqual(
            self.smoke.classify_create_attempt(coherent),
            {
                "kind": "queued",
                "operation_id": "op-A",
                "state": "queued",
                "timed_out": True,
            },
        )
        self.assertEqual(self.smoke.operation_id_from(coherent), "op-A")
        self.assertEqual(self.smoke.operation_state_from(coherent), "queued")
        self.assertEqual(self.smoke.followable_timeout_operation_id(coherent), "op-A")

    def test_wait_operation_never_promotes_failed_cli_process(self) -> None:
        terminal = {
            "operation_id": "op-A",
            "state": "completed",
            "created_drawer_ids": ["drawer-cleanup"],
        }
        with mock.patch.object(
            self.smoke,
            "run_cli",
            side_effect=[
                (1, b"", b"", terminal, {}),
                (1, b"", b"", None, {}),
            ],
        ) as run_cli:
            waited = self.smoke.wait_operation("op-A", "unit_wait")

        self.assertEqual(
            self.smoke.classify_create_attempt(
                waited, expected_operation_id="op-A"
            ),
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-cleanup"]},
        )
        self.assertEqual(run_cli.call_count, 2)

        other_operation = {
            "operation_id": "op-B",
            "state": "completed",
            "created_drawer_ids": ["drawer-other"],
        }
        expected_operation = {
            "operation_id": "op-A",
            "state": "completed",
            "created_drawer_ids": ["drawer-expected"],
        }
        with mock.patch.object(
            self.smoke,
            "run_cli",
            side_effect=[
                (0, b"", b"", other_operation, {}),
                (0, b"", b"", expected_operation, {}),
            ],
        ) as run_cli:
            waited = self.smoke.wait_operation("op-A", "unit_wait")

        self.assertEqual(
            self.smoke.classify_create_attempt(
                waited, expected_operation_id="op-A"
            ),
            {"kind": "inconclusive"},
        )
        self.assertEqual(run_cli.call_count, 1)

    def test_cli_mixed_ids_are_cleanup_only_and_rest_stays_reachable(self) -> None:
        def run_cli(label: str, *_args: object, **_kwargs: object) -> tuple[int, bytes, bytes, object, dict[str, object]]:
            if label == "cli_create":
                return 0, b"", b"", {
                    "created_drawer_ids": ["drawer-known", 7],
                    "private_raw_key": "private-raw-value",
                }, {}
            raise AssertionError("CRUD must not continue without authoritative create evidence")

        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            cleanup = mock.Mock(return_value={"deleted_count": 1, "failed_count": 0})
            with (
                mock.patch.object(self.smoke, "run_cli", side_effect=run_cli),
                mock.patch.object(
                    self.smoke,
                    "_rest_ingest_fallback",
                    return_value=([], {"kind": "inconclusive"}),
                ) as rest_fallback,
                mock.patch.object(self.smoke, "delete_exact_ids_cli", cleanup),
            ):
                self.assertEqual(self.smoke.cli_crud(), ["drawer-known"])

            self.assertEqual(rest_fallback.call_args.args[1], "cli_create_rest_fallback")
            cleanup.assert_called_once_with(
                ["drawer-known"], "cli_cleanup_after_create_failure", room="cli"
            )
            self.assertFalse(self.smoke.SUMMARY["groups"]["cli_create"]["ok"])
            self.assertFalse(self.smoke.SUMMARY["groups"]["cli_crud"]["ok"])
            saved = manifest.path.read_text(encoding="utf-8")
            self.assertEqual(json.loads(saved), {"cleanup_drawer_ids": ["drawer-known"]})
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)
            self.assertNotIn("private_raw_key", saved)
            self.assertNotIn("private-raw-value", saved)

    def test_mcp_conflicting_operation_ids_never_follow(self) -> None:
        conflict = {
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"structuredContent": {"operation_id": "op-A", "state": "queued"}},
            "error": {"code": -32603, "message": "private-raw-value", "data": {
                "operation_id": "op-B", "state": "queued", "timed_out": True,
            }},
        }
        discover = mock.Mock()
        discover.call.return_value = advertised_tools()
        create_client = mock.Mock()
        create_client.tool.return_value = (None, {"ok": False})
        info = {"ok": False, "_raw_mcp_response": conflict}
        with (
            mock.patch.object(
                self.smoke, "mcp_start_initialized", side_effect=[discover, create_client]
            ),
            mock.patch.object(self.smoke, "mcp_call_isolated"),
            mock.patch.object(
                self.smoke, "_mcp_tool_with_hard_timeout", return_value=(None, info)
            ),
            mock.patch.object(self.smoke, "wait_operation", return_value=None) as wait_operation,
            mock.patch.object(
                self.smoke,
                "_rest_ingest_fallback",
                return_value=([], {"kind": "inconclusive"}),
            ) as rest_fallback,
            mock.patch.object(
                self.smoke, "run_exact_cli_cleanup_after_mcp"
            ) as exact_cleanup,
        ):
            self.assertEqual(self.smoke.mcp_crud(), [])

        create_client.tool.assert_not_called()
        wait_operation.assert_not_called()
        rest_fallback.assert_called_once()
        exact_cleanup.assert_not_called()
        public = json.dumps(self.smoke.SUMMARY)
        self.assertNotIn("op-A", public)
        self.assertNotIn("op-B", public)
        self.assertNotIn("private-raw-value", public)

    def test_mcp_create_status_rejects_other_operation_completion(self) -> None:
        discover = mock.Mock()
        discover.call.return_value = advertised_tools()
        create_client = mock.Mock()
        create_client.tool.return_value = (
            {
                "operation_id": "op-B",
                "state": "completed",
                "created_drawer_ids": ["drawer-status"],
                "cleanup_drawer_ids": ["drawer-status"],
                "private_raw_key": "private-raw-value",
            },
            {
                "ok": True,
            },
        )
        update_client = mock.Mock()
        update_client.tool.return_value = ({"results": []}, {"ok": True})
        queued_info = {
            "ok": False,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "error": {
                    "code": -32603,
                    "message": "queued",
                    "data": {"operation_id": "op-A", "state": "queued"},
                },
            },
        }
        update_info = {
            "ok": True,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 3,
                "result": {"structuredContent": {"created_drawer_ids": ["drawer-update"]}},
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            setattr(self.smoke, "SUMMARY", {"groups": {}, "failures": [], "created_counts": {"mcp": 0}, "cleanup": {"mcp_deleted_count": 0, "failures": 0}, "mcp_ingest_fallback_to_cli": 0})
            with (
                mock.patch.object(self.smoke, "mcp_start_initialized", side_effect=[discover, create_client, update_client]),
                mock.patch.object(self.smoke, "mcp_call_isolated"),
                mock.patch.object(self.smoke, "mcp_call_isolated_labeled", return_value=(None, {"ok": True})),
                mock.patch.object(self.smoke, "_mcp_tool_with_hard_timeout", side_effect=[(None, queued_info), ({"created_drawer_ids": ["drawer-update"]}, update_info)]),
                mock.patch.object(self.smoke, "delete_exact_ids_mcp", return_value={"deleted_count": 2, "failed_count": 0, "delete_failed_attempt_count": 0}) as delete_exact,
                mock.patch.object(
                    self.smoke,
                    "_rest_ingest_fallback",
                    return_value=(
                        ["drawer-rest"],
                        {"kind": "created", "created_drawer_ids": ["drawer-rest"]},
                    ),
                ) as rest_fallback,
            ):
                self.assertEqual(
                    self.smoke.mcp_crud(),
                    ["drawer-rest", "drawer-update"],
                )
            create_client.tool.assert_called_once_with(
                "mempal_operation_status", {"operation_id": "op-A"}, timeout=30
            )
            rest_fallback.assert_called_once()
            self.assertNotIn("drawer-status", delete_exact.call_args.args[1])
            saved = manifest.path.read_text(encoding="utf-8")
            self.assertEqual(
                json.loads(saved),
                {"cleanup_drawer_ids": ["drawer-rest", "drawer-update"]},
            )
            self.assertFalse(self.smoke.SUMMARY["groups"]["mcp_create"]["ok"])
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)

    def test_rest_exact_no_write_and_http_error_cleanup_ids(self) -> None:
        expected = {
            "kind": "proven_no_write",
            "receipt": {
                "outcome": "admission_blocked",
                "reason": "holder_budget_exceeded",
                "cleanup_required": False,
            },
        }
        valid = rest_holder_budget_envelope()
        self.assertEqual(self.smoke.classify_create_attempt(valid), expected)
        for name, mutate in (
            ("unknown_wrapper", lambda body: body["error"].__setitem__("private_control", True)),
            ("contradictory_retryable", lambda body: body["error"].__setitem__("retryable", True)),
            ("malformed_ids", lambda body: body["error"].__setitem__("created_drawer_ids", [7])),
        ):
            with self.subTest(name=name):
                body = json.loads(json.dumps(valid))
                mutate(body)
                self.assertEqual(
                    self.smoke.classify_create_attempt(body),
                    {"kind": "inconclusive"},
                )

        refusal_error = HTTPError(
            "http://127.0.0.1:3080/api/ingest",
            503,
            "service unavailable",
            Message(),
            io.BytesIO(json.dumps(valid).encode()),
        )
        with mock.patch("urllib.request.urlopen", side_effect=refusal_error):
            self.assertEqual(
                self.smoke._rest_ingest_fallback("content", "unit_rest_refusal"),
                ([], expected),
            )
        refusal_error.close()

        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            body = {
                "created_drawer_ids": ["drawer-rest-error"],
                "cleanup_drawer_ids": ["drawer-rest-error"],
                "private_raw_key": "private-raw-value",
            }
            created_error = HTTPError(
                "http://127.0.0.1:3080/api/ingest",
                503,
                "service unavailable",
                Message(),
                io.BytesIO(json.dumps(body).encode()),
            )
            with mock.patch("urllib.request.urlopen", side_effect=created_error):
                ids, disposition = self.smoke._rest_ingest_fallback(
                    "content", "unit_rest_created_error"
                )
            self.assertEqual(ids, ["drawer-rest-error"])
            self.assertEqual(
                disposition,
                {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-rest-error"]},
            )
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-rest-error"]},
            )
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)
            created_error.close()

    def test_cli_and_mcp_update_no_write_skip_rest_and_cleanup_original(self) -> None:
        def run_cli(label: str, *_args: object, **_kwargs: object) -> tuple[int, bytes, bytes, object, dict[str, object]]:
            if label == "cli_create":
                return 0, b"", b"", {"created_drawer_ids": ["drawer-original"]}, {}
            if label == "cli_update":
                return 1, b"", b"", cli_holder_budget_receipt(), {}
            return 0, b"", b"", {"results": []}, {}

        cli_cleanup = mock.Mock(return_value={"deleted_count": 1, "failed_count": 0})
        with (
            mock.patch.object(self.smoke, "run_cli", side_effect=run_cli),
            mock.patch.object(self.smoke, "delete_exact_ids_cli", cli_cleanup),
            mock.patch.object(self.smoke, "_rest_ingest_fallback", side_effect=AssertionError("REST must not follow exact update no-write")) as cli_rest,
        ):
            self.assertEqual(self.smoke.cli_crud(), ["drawer-original"])
        cli_rest.assert_not_called()
        cli_cleanup.assert_called_once_with(
            ["drawer-original"], "cli_cleanup_after_update_failure", room="cli"
        )

        discover = mock.Mock()
        discover.call.return_value = advertised_tools()
        create_client = mock.Mock()
        update_client = mock.Mock()
        create_info = {
            "ok": True,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"structuredContent": {"created_drawer_ids": ["drawer-original"]}},
            },
        }
        update_info = {"ok": False, "_raw_mcp_response": mcp_holder_budget_envelope()}
        mcp_cleanup = mock.Mock(return_value={"deleted_count": 1, "failed_count": 0})
        with (
            mock.patch.object(self.smoke, "mcp_start_initialized", side_effect=[discover, create_client, update_client]),
            mock.patch.object(self.smoke, "mcp_call_isolated"),
            mock.patch.object(self.smoke, "mcp_call_isolated_labeled", return_value=(None, {"ok": True})),
            mock.patch.object(self.smoke, "_mcp_tool_with_hard_timeout", side_effect=[({"created_drawer_ids": ["drawer-original"]}, create_info), (None, update_info)]),
            mock.patch.object(self.smoke, "run_exact_cli_cleanup_after_mcp", mcp_cleanup),
            mock.patch.object(self.smoke, "_rest_ingest_fallback", side_effect=AssertionError("REST must not follow exact update no-write")) as mcp_rest,
        ):
            self.assertEqual(self.smoke.mcp_crud(), ["drawer-original"])
        mcp_rest.assert_not_called()
        mcp_cleanup.assert_any_call(
            ["drawer-original"], "mcp_cleanup_after_update_failure"
        )


if __name__ == "__main__":
    unittest.main()

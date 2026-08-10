#!/usr/bin/env python3
"""Operation-bound cleanup authority regressions for the full smoke runner."""
from __future__ import annotations

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


class OperationCleanupAuthorityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.smoke = load_full_smoke()

    def tearDown(self) -> None:
        self.smoke.terminate_and_reap_owned_mcp_children(timeout=0.2)
        manifest = self.smoke.CLEANUP_MANIFEST
        if manifest is not None:
            manifest.discard()

    @staticmethod
    def foreign_receipt() -> dict[str, Any]:
        return {
            "operation_id": "op-B",
            "state": "completed",
            "created_drawer_ids": ["drawer-other"],
            "cleanup_drawer_ids": ["drawer-other"],
            "private_raw_key": "private-raw-value",
        }

    def test_expected_operation_id_binds_cleanup_authority(self) -> None:
        ids = {
            "created_drawer_ids": ["drawer-other"],
            "cleanup_drawer_ids": ["drawer-other"],
        }
        unauthorized = {
            "missing": {"state": "completed", **ids},
            "malformed": {"operation_id": 7, "state": "completed", **ids},
            "nonmatching": self.foreign_receipt(),
            "conflicting": [
                self.foreign_receipt(),
                {"operation_id": "op-C", "state": "failed"},
            ],
        }
        for name, attempt in unauthorized.items():
            with self.subTest(name=name):
                self.assertEqual(
                    self.smoke.classify_create_attempt(
                        attempt, expected_operation_id="op-A"
                    ),
                    {"kind": "inconclusive"},
                )

        owned = {
            "operation_id": "op-A",
            "state": "completed",
            "created_drawer_ids": ["drawer-owned"],
            "cleanup_drawer_ids": ["drawer-owned"],
        }
        self.assertEqual(
            self.smoke.classify_create_attempt(
                [owned, self.foreign_receipt()], expected_operation_id="op-A"
            ),
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-owned"]},
        )
        self.assertEqual(
            self.smoke.classify_create_attempt(
                owned, expected_operation_id="op-A"
            ),
            {
                "kind": "created",
                "created_drawer_ids": ["drawer-owned"],
                "cleanup_drawer_ids": ["drawer-owned"],
            },
        )
        self.assertEqual(
            self.smoke.classify_create_attempt(
                {
                    "operation_id": "op-A",
                    "state": "failed",
                    "cleanup_drawer_ids": ["drawer-owned"],
                },
                expected_operation_id="op-A",
            ),
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-owned"]},
        )
        self.assertEqual(
            self.smoke.classify_create_attempt(
                {"cleanup_drawer_ids": ["drawer-direct"]}
            ),
            {"kind": "inconclusive", "cleanup_drawer_ids": ["drawer-direct"]},
        )

    def test_wait_merge_does_not_cross_pair_missing_identity_ids(self) -> None:
        expected_status = {
            "operation_id": "op-A",
            "state": "completed",
            "created_drawer_ids": ["drawer-expected"],
            "cleanup_drawer_ids": ["drawer-expected"],
        }
        with mock.patch.object(
            self.smoke,
            "run_cli",
            side_effect=[
                (1, b"", b"", {
                    "created_drawer_ids": ["drawer-other"],
                    "cleanup_drawer_ids": ["drawer-other"],
                }, {}),
                (0, b"", b"", expected_status, {}),
            ],
        ) as run_cli:
            waited = self.smoke.wait_operation("op-A", "unit_wait")

        self.assertEqual(
            self.smoke.classify_create_attempt(
                waited, expected_operation_id="op-A"
            ),
            {
                "kind": "created",
                "created_drawer_ids": ["drawer-expected"],
                "cleanup_drawer_ids": ["drawer-expected"],
            },
        )
        self.assertEqual(run_cli.call_count, 2)

    def test_cli_create_and_update_foreign_ids_never_reach_cleanup(self) -> None:
        def run_cli(
            label: str, *_args: Any, **_kwargs: Any
        ) -> tuple[int, bytes, bytes, dict[str, Any], dict[str, Any]]:
            if label == "cli_create":
                return 0, b"", b"", {
                    "operation_id": "op-create",
                    "state": "queued",
                    "timed_out": True,
                }, {}
            if label == "cli_update":
                return 0, b"", b"", {
                    "operation_id": "op-update",
                    "state": "queued",
                    "timed_out": True,
                }, {}
            return 0, b"", b"", {"results": []}, {}

        rest_results = [
            (
                [drawer_id],
                {"kind": "created", "created_drawer_ids": [drawer_id]},
            )
            for drawer_id in ("drawer-create-rest", "drawer-update-rest")
        ]
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            with (
                mock.patch.object(self.smoke, "run_cli", side_effect=run_cli),
                mock.patch.object(
                    self.smoke,
                    "wait_operation",
                    return_value=self.foreign_receipt(),
                ) as wait_operation,
                mock.patch.object(
                    self.smoke,
                    "delete_exact_ids_cli",
                    return_value={"deleted_count": 2, "failed_count": 0},
                ) as delete_exact,
                mock.patch.object(
                    self.smoke, "_rest_ingest_fallback", side_effect=rest_results
                ),
            ):
                self.assertEqual(
                    self.smoke.cli_crud(),
                    ["drawer-create-rest", "drawer-update-rest"],
                )

            self.assertEqual(
                [call.args[0] for call in wait_operation.call_args_list],
                ["op-create", "op-update"],
            )
            self.assertNotIn("drawer-other", delete_exact.call_args.args[0])
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {
                    "cleanup_drawer_ids": [
                        "drawer-create-rest",
                        "drawer-update-rest",
                    ]
                },
            )
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)
            manifest.discard()

    def test_mcp_create_wait_foreign_ids_never_reach_cleanup(self) -> None:
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
            {"operation_id": "op-A", "state": "running"},
            {"ok": True},
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
                    "data": {
                        "operation_id": "op-A",
                        "state": "queued",
                        "timed_out": True,
                    },
                },
            },
        }
        update = {"created_drawer_ids": ["drawer-update"]}
        update_info = {
            "ok": True,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"structuredContent": update},
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            with (
                mock.patch.object(
                    self.smoke,
                    "mcp_start_initialized",
                    side_effect=[discover, create_client, update_client],
                ),
                mock.patch.object(self.smoke, "mcp_call_isolated"),
                mock.patch.object(
                    self.smoke,
                    "mcp_call_isolated_labeled",
                    return_value=(None, {"ok": True}),
                ),
                mock.patch.object(
                    self.smoke,
                    "_mcp_tool_with_hard_timeout",
                    side_effect=[(None, queued_info), (update, update_info)],
                ),
                mock.patch.object(
                    self.smoke,
                    "wait_operation",
                    return_value=self.foreign_receipt(),
                ) as wait_operation,
                mock.patch.object(
                    self.smoke,
                    "_rest_ingest_fallback",
                    return_value=(
                        ["drawer-rest"],
                        {"kind": "created", "created_drawer_ids": ["drawer-rest"]},
                    ),
                ) as rest_fallback,
                mock.patch.object(
                    self.smoke,
                    "delete_exact_ids_mcp",
                    return_value={
                        "deleted_count": 2,
                        "failed_count": 0,
                        "delete_failed_attempt_count": 0,
                    },
                ) as delete_exact,
            ):
                self.assertEqual(
                    self.smoke.mcp_crud(), ["drawer-rest", "drawer-update"]
                )

            create_client.tool.assert_called_once_with(
                "mempal_operation_status", {"operation_id": "op-A"}, timeout=30
            )
            wait_operation.assert_called_once_with("op-A", "mcp_create_cli_wait")
            rest_fallback.assert_called_once()
            self.assertNotIn("drawer-other", delete_exact.call_args.args[1])
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-rest", "drawer-update"]},
            )
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)
            manifest.discard()


if __name__ == "__main__":
    unittest.main()

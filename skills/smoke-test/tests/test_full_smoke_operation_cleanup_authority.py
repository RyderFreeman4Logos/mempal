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
                    ],
                    "pending_operations": [
                        {"operation_id": "op-create", "role": "cli_create_wait"},
                        {"operation_id": "op-update", "role": "cli_update_wait"},
                    ],
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
                {
                    "cleanup_drawer_ids": ["drawer-rest", "drawer-update"],
                    "pending_operations": [
                        {"operation_id": "op-A", "role": "mcp_create_cli_wait"}
                    ],
                },
            )
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("private_raw_key", public)
            self.assertNotIn("private-raw-value", public)
            manifest.discard()

    def test_mcp_update_terminal_resume_avoids_rest_retry(self) -> None:
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
        active_client = mock.Mock()
        active_client.tool.return_value = ({}, {"ok": True})
        create = {"created_drawer_ids": ["drawer-create"]}
        create_info = {
            "ok": True,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"structuredContent": create},
            },
        }
        queued_update = {
            "ok": False,
            "_raw_mcp_response": {
                "jsonrpc": "2.0",
                "id": 2,
                "error": {
                    "code": -32603,
                    "message": "queued",
                    "data": {
                        "operation_id": "op-update",
                        "state": "queued",
                        "timed_out": True,
                    },
                },
            },
        }
        terminal_update = {
            "operation_id": "op-update",
            "state": "completed",
            "created_drawer_ids": ["drawer-update"],
            "cleanup_drawer_ids": ["drawer-update"],
        }
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            with (
                mock.patch.object(
                    self.smoke,
                    "mcp_start_initialized",
                    side_effect=[discover, create_client, update_client, active_client],
                ),
                mock.patch.object(
                    self.smoke,
                    "mcp_call_isolated_labeled",
                    return_value=(None, {"ok": True}),
                ),
                mock.patch.object(
                    self.smoke,
                    "_mcp_tool_with_hard_timeout",
                    side_effect=[(create, create_info), (None, queued_update)],
                ),
                mock.patch.object(
                    self.smoke, "wait_operation", return_value=terminal_update
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
                ),
            ):
                self.assertEqual(
                    self.smoke.mcp_crud(), ["drawer-create", "drawer-update"]
                )

            wait_operation.assert_called_once_with("op-update", "mcp_update_cli_wait")
            rest_fallback.assert_not_called()
            self.assertEqual(manifest.pending_operation_count, 0)
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-create", "drawer-update"]},
            )
            public = json.dumps(self.smoke.SUMMARY)
            self.assertNotIn("pending_operations", public)
            manifest.discard()

    def test_cli_wait_persists_terminal_ids_before_resolving_custody(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            with mock.patch.object(
                self.smoke,
                "wait_operation",
                return_value={
                    "operation_id": "op-A",
                    "state": "completed",
                    "created_drawer_ids": ["drawer-A"],
                    "cleanup_drawer_ids": ["drawer-A"],
                },
            ):
                ids, info = self.smoke.recover_created_ids(
                    {
                        "operation_id": "op-A",
                        "state": "queued",
                        "timed_out": True,
                    },
                    "cli_create_wait",
                )

            self.assertEqual(ids, ["drawer-A"])
            self.assertEqual(info["kind"], "created")
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {"cleanup_drawer_ids": ["drawer-A"]},
            )
            manifest.discard()

    def test_pending_operation_ledger_survives_without_cleanup_ids(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_pending_operation("mcp_update_cli_wait", "op-A")

            self.assertEqual(manifest.pending_operation_count, 1)
            self.assertEqual(
                manifest.pending_operations,
                [{"operation_id": "op-A", "role": "mcp_update_cli_wait"}],
            )
            self.assertEqual(
                json.loads(manifest.path.read_text(encoding="utf-8")),
                {
                    "cleanup_drawer_ids": [],
                    "pending_operations": [
                        {"operation_id": "op-A", "role": "mcp_update_cli_wait"}
                    ],
                },
            )
            self.assertEqual(manifest.path.stat().st_mode & 0o777, 0o600)

            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            summary: dict[str, Any] = {}
            self.smoke.finalize_cleanup_manifest(summary)
            self.assertEqual(summary["cleanup_manifest_path"], str(manifest.path))
            self.assertEqual(summary["pending_operation_count"], 1)
            self.assertNotIn("pending_operations", json.dumps(summary))
        manifest.discard()

    def test_exact_cli_cleanup_delete_uses_all_projects_like_view(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            manifest = self.smoke.CleanupManifest(Path(tmp) / "cleanup.json")
            manifest.add_created_ids(["drawer-mcp"])
            setattr(self.smoke, "CLEANUP_MANIFEST", manifest)
            seen: list[list[str]] = []

            def child_result(command: list[str], **kwargs: Any) -> dict[str, Any]:
                del kwargs
                seen.append(list(command))
                if command[:2] == ["mempal", "delete"]:
                    # Live #1090: unscoped delete fails; scoped delete succeeds.
                    if "--all-projects" in command and "drawer-mcp" in command:
                        return {"returncode": 0, "stdout": b"", "stderr": b""}
                    return {
                        "returncode": 1,
                        "stdout": b"",
                        "stderr": b"not in current project",
                    }
                if command[:2] == ["mempal", "view"]:
                    return {
                        "returncode": 1,
                        "stdout": b"",
                        "stderr": b"drawer drawer-mcp not found",
                    }
                return {"returncode": 0, "stdout": b"", "stderr": b""}

            with mock.patch.object(
                self.smoke,
                "run_child_process",
                side_effect=child_result,
            ):
                with mock.patch.object(
                    self.smoke,
                    "run_cli",
                    return_value=(0, b"", b"", {"results": []}, {}),
                ):
                    result = self.smoke.delete_exact_ids_cli(
                        ["drawer-mcp"],
                        "unit_cleanup_scope",
                        room="mcp",
                    )

            delete_cmds = [cmd for cmd in seen if cmd[:2] == ["mempal", "delete"]]
            view_cmds = [cmd for cmd in seen if cmd[:2] == ["mempal", "view"]]
            self.assertEqual(len(delete_cmds), 1)
            self.assertIn("--all-projects", delete_cmds[0])
            self.assertIn("drawer-mcp", delete_cmds[0])
            self.assertTrue(view_cmds)
            self.assertTrue(all("--all-projects" in cmd for cmd in view_cmds))
            self.assertEqual(result["failed_count"], 0)
            self.assertEqual(result["verified_absent_count"], 1)


if __name__ == "__main__":
    unittest.main()

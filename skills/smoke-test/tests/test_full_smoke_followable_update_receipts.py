#!/usr/bin/env python3
"""Followable vs unfollowable update-receipt classification for #1096."""
from __future__ import annotations

import importlib.util
from pathlib import Path
from types import ModuleType
import unittest


def load_smoke_receipts() -> ModuleType:
    script = Path(__file__).resolve().parents[1] / "scripts" / "smoke_receipts.py"
    spec = importlib.util.spec_from_file_location("smoke_receipts", script)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FollowableUpdateReceiptTests(unittest.TestCase):
    def setUp(self) -> None:
        self.receipts = load_smoke_receipts()

    def test_followable_update_does_not_classify_as_missing_ids(self) -> None:
        running_follow = {
            "kind": "queued",
            "operation_id_present": True,
            "operation_state": "queued",
            "recovered_via": "mcp_update_cli_wait",
            "recovered_state": "running",
        }
        self.assertTrue(self.receipts.followable_update_without_terminal_ids(running_follow))
        self.assertEqual(
            self.receipts.update_missing_reason(running_follow),
            "update_followable_not_terminal",
        )

    def test_unfollowable_update_still_classifies_as_missing_ids(self) -> None:
        inconclusive = {"kind": "inconclusive", "operation_id_present": False}
        self.assertFalse(self.receipts.followable_update_without_terminal_ids(inconclusive))
        self.assertEqual(
            self.receipts.update_missing_reason(inconclusive),
            "update_missing_created_drawer_ids",
        )


if __name__ == "__main__":
    unittest.main()

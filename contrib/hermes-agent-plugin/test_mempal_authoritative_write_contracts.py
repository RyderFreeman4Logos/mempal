import inspect
import json
import os
import sys
import unittest
import urllib.error
from email.message import Message
from typing import Mapping, get_type_hints

PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import MempalMemoryProvider  # noqa: E402
import mempal._authoritative_write as authoritative_write_module  # noqa: E402
import mempal._write_spool_claims as write_spool_claims_module  # noqa: E402
from test_mempal_authoritative_write import (  # noqa: E402
    FailingReplayProvider,
    SequencedStatusProvider,
)


class AuthoritativeWriteContractTests(unittest.TestCase):
    def test_public_signature_and_exports_are_typed(self) -> None:
        wrapper = inspect.signature(MempalMemoryProvider.authoritative_memory_write)
        implementation = inspect.signature(
            authoritative_write_module.authoritative_memory_write
        )
        wrapper_hints = get_type_hints(MempalMemoryProvider.authoritative_memory_write)
        implementation_hints = get_type_hints(
            authoritative_write_module.authoritative_memory_write
        )

        self.assertEqual(
            wrapper.parameters["request"].kind,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        self.assertEqual(
            wrapper.parameters["kwargs"].kind,
            inspect.Parameter.VAR_KEYWORD,
        )
        self.assertEqual(
            implementation.parameters["request"].kind,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        )
        self.assertEqual(wrapper_hints.get("request"), Mapping[str, object])
        self.assertEqual(wrapper_hints.get("return"), str)
        self.assertEqual(implementation_hints.get("request"), Mapping[str, object])
        self.assertEqual(implementation_hints.get("return"), str)
        self.assertIn("_AuthoritativeWriteProvider", authoritative_write_module.__dict__)
        self.assertEqual(write_spool_claims_module.__all__, ["WriteSpoolClaims"])
        self.assertNotIn("_SpoolOwner", write_spool_claims_module.__all__)

    def test_malformed_control_fields_are_terminal_and_side_effect_free(self) -> None:
        cases = [
            ({"target": {"Authorization": "Bearer FIXTURE"}}, {}),
            ({"operation_key": ["private prompt"]}, {}),
            (
                {
                    "operations": [
                        {
                            "action": "add",
                            "content": "safe",
                            "operation_key": {"secret": "FIXTURE"},
                        }
                    ]
                },
                {},
            ),
            (
                {
                    "operations": [{"action": "add", "content": "safe"}],
                    "operation_keys": [{"secret": "FIXTURE"}],
                },
                {},
            ),
            (
                {"action": "add", "target": "user", "content": "safe"},
                {"operation_key": {"token": "FIXTURE"}},
            ),
        ]
        for request, kwargs in cases:
            provider = SequencedStatusProvider([])
            provider.initialize("session-a", user_id="alice", profile="work")
            before = provider._backoff._read_state()
            receipt = json.loads(provider.authoritative_memory_write(request, **kwargs))
            after = provider._backoff._read_state()
            rendered = json.dumps(receipt)
            self.assertFalse(receipt["success"])
            self.assertEqual(receipt["error_class"], "invalid_control_fields")
            self.assertFalse(receipt["retry_safe"])
            self.assertNotIn("FIXTURE", rendered)
            assert provider._write_spool is not None
            self.assertEqual(provider._write_spool.count(), 0)
            self.assertEqual(provider.wake_calls, 0)
            self.assertEqual(after.failure_count, before.failure_count)
            provider.shutdown()

    def test_batch_preserves_fifo_blocked_after_queued_item(self) -> None:
        provider = SequencedStatusProvider(["queued", "completed"])
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(
            provider.authoritative_memory_write(
                {
                    "target": "user",
                    "operations": [
                        {"action": "add", "content": "queued preference"},
                        {"action": "add", "content": "blocked preference"},
                    ],
                }
            )
        )

        self.assertFalse(result["success"])
        self.assertFalse(result["partial_write"])
        self.assertEqual(result["state"], "local_admitted")
        self.assertTrue(result["retryable"])
        self.assertTrue(result["retry_safe"])
        self.assertEqual(result["durability"]["state"], "pending")
        self.assertEqual(result["durability"]["kind"], "durable_replay_deferred")
        self.assertEqual(result["operation_ids"], [])
        self.assertEqual(len(result["operation_keys"]), 2)
        self.assertEqual(provider.wake_calls, 2)
        assert provider._write_spool is not None
        self.assertEqual(provider._write_spool.count(), 2)
        provider.shutdown()

    def test_mixed_completed_and_pending_batch_reports_partial_completion(self) -> None:
        provider = SequencedStatusProvider(["completed", "queued"])
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(
            provider.authoritative_memory_write(
                {
                    "target": "user",
                    "operations": [
                        {"action": "add", "content": "completed preference"},
                        {"action": "add", "content": "queued preference"},
                    ],
                }
            )
        )

        self.assertFalse(result["success"])
        self.assertTrue(result["partial_write"])
        self.assertEqual(result["state"], "local_admitted")
        self.assertEqual(
            result["operation_ids"], ["operation_" + result["operation_keys"][0]]
        )
        self.assertEqual(len(result["operation_keys"]), 2)
        self.assertEqual(result["durability"]["kind"], "durable_replay_deferred")
        provider.shutdown()

    def test_running_batch_is_pending_not_completed(self) -> None:
        provider = SequencedStatusProvider(["running"])
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(
            provider.authoritative_memory_write(
                {
                    "target": "user",
                    "operations": [
                        {"action": "add", "content": "running preference"}
                    ],
                }
            )
        )

        self.assertFalse(result["success"])
        self.assertFalse(result["partial_write"])
        self.assertEqual(result["durability"]["deferred_reason"], "running")
        self.assertEqual(result["operation_ids"], [])
        provider.shutdown()

    def test_all_deferred_batch_preserves_pending_handles_without_completed_ids(self) -> None:
        provider = SequencedStatusProvider(["queued", "running"])
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(
            provider.authoritative_memory_write(
                {
                    "target": "user",
                    "operations": [
                        {"action": "add", "content": "queued preference"},
                        {"action": "add", "content": "running preference"},
                    ],
                }
            )
        )

        self.assertFalse(result["success"])
        self.assertFalse(result["partial_write"])
        self.assertEqual(result["state"], "local_admitted")
        self.assertTrue(result["retryable"])
        self.assertTrue(result["retry_safe"])
        self.assertEqual(result["operation_ids"], [])
        self.assertEqual(len(result["operation_keys"]), 2)
        self.assertEqual(result["durability"]["kind"], "durable_replay_deferred")
        provider.shutdown()

    def test_retryable_transport_batch_is_pending_not_completed(self) -> None:
        provider = FailingReplayProvider(
            urllib.error.HTTPError(
                "/api/ingest/durable", 503, "synthetic unavailable", Message(), None
            )
        )
        provider.initialize("session-a", user_id="alice", profile="work")

        result = json.loads(
            provider.authoritative_memory_write(
                {
                    "target": "user",
                    "operations": [
                        {"action": "add", "content": "retryable preference"}
                    ],
                }
            )
        )

        self.assertFalse(result["success"])
        self.assertFalse(result["partial_write"])
        self.assertEqual(result["state"], "local_admitted")
        self.assertEqual(result["durability"]["deferred_reason"], "http_503")
        self.assertEqual(result["operation_ids"], [])
        self.assertEqual(len(result["operation_keys"]), 1)
        provider.shutdown()


if __name__ == "__main__":
    unittest.main()

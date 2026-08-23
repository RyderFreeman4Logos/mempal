import json
import unittest

from test_mempal_provider import RecordingProvider


class AuthoritativeMemoryWriteTests(unittest.TestCase):
    def test_authoritative_memory_write_returns_durable_crud_receipts(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        added = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "old preference",
        }))
        replaced = json.loads(provider.authoritative_memory_write({
            "action": "replace",
            "target": "user",
            "old_text": "old preference",
            "content": "new preference",
        }))
        removed = json.loads(provider.authoritative_memory_write({
            "action": "remove",
            "target": "user",
            "old_text": "new preference",
        }))

        for receipt in (added, replaced, removed):
            self.assertTrue(receipt["success"])
            self.assertTrue(receipt["operation_key"])
            self.assertTrue(receipt["operation_id"])
        self.assertTrue(added["drawer_id"])
        self.assertTrue(replaced["drawer_id"])
        self.assertTrue(removed["drawer_id"])
        durable_requests = [
            body["request"]
            for path, body in provider.posts
            if path in {"/api/ingest/durable", "/api/delete/durable"}
        ]
        self.assertEqual(durable_requests[1]["replace_text"], "old preference")
        self.assertNotIn("supersedes", durable_requests[1])
        delete_requests = [
            body["request"]
            for path, body in provider.posts
            if path == "/api/delete/durable"
        ]
        self.assertEqual(delete_requests[0]["drawer_id"], replaced["drawer_id"])

    def test_authoritative_memory_write_accepts_remove_add_batch(self) -> None:
        provider = RecordingProvider()
        provider.initialize("session-a", user_id="alice", profile="work")

        added = json.loads(provider.authoritative_memory_write({
            "action": "add",
            "target": "user",
            "content": "old preference",
        }))
        result = json.loads(provider.authoritative_memory_write({
            "target": "user",
            "operations": [
                {"action": "remove", "old_text": "old preference"},
                {"action": "add", "content": "new preference"},
            ],
        }))

        self.assertTrue(result["success"])
        self.assertEqual(len(result["operation_ids"]), 2)
        durable = [
            (path, body["request"])
            for path, body in provider.posts
            if path in {"/api/ingest/durable", "/api/delete/durable"}
        ][1:]
        self.assertEqual([path for path, _ in durable], [
            "/api/delete/durable",
            "/api/ingest/durable",
        ])
        self.assertEqual(durable[0][1]["drawer_id"], added["drawer_id"])
        self.assertEqual(durable[1][1]["content"], "new preference")


if __name__ == "__main__":
    unittest.main()

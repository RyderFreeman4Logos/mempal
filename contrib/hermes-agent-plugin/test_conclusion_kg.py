import unittest

from mempal._conclude import conclusion_request


class ConclusionKnowledgeGraphTests(unittest.TestCase):
    def test_conclusion_requests_kg_extraction(self) -> None:
        request = conclusion_request(
            "Project Mempal uses SQLite.",
            "hermes-user/test/default",
            "facts",
            4,
            None,
        )

        self.assertEqual(request["source"], "hermes-session-conclusion")


if __name__ == "__main__":
    unittest.main()

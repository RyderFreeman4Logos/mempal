import inspect
import os
import sys
import unittest
from typing import Mapping, get_type_hints

PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal import MempalMemoryProvider  # noqa: E402
import mempal._authoritative_write as authoritative_write_module  # noqa: E402
import mempal._write_spool_claims as write_spool_claims_module  # noqa: E402


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


if __name__ == "__main__":
    unittest.main()

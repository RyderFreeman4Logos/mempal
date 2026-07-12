import os
import sys
import unittest
from typing import Dict, List, Optional, Tuple


PLUGIN_DIR = os.path.dirname(__file__)
if PLUGIN_DIR not in sys.path:
    sys.path.insert(0, PLUGIN_DIR)

from mempal_search_transport import SearchTransport  # noqa: E402


class FakeSocket:
    def __init__(self) -> None:
        self.timeouts: List[Optional[float]] = []

    def settimeout(self, timeout: Optional[float]) -> None:
        self.timeouts.append(timeout)


class FakeResponse:
    status = 200
    reason = "OK"

    def getheaders(self) -> List[Tuple[str, str]]:
        return [("content-type", "application/json")]

    def read(self) -> bytes:
        return b'[{"drawer_id":"drawer-transport"}]'


class FakeConnection:
    def __init__(self) -> None:
        self.sock = FakeSocket()
        self.connected = False
        self.closed = False
        self.requests: List[Tuple[str, str, Dict[str, str]]] = []

    def connect(self) -> None:
        self.connected = True

    def request(self, method: str, target: str, headers: Dict[str, str]) -> None:
        self.requests.append((method, target, dict(headers)))

    def getresponse(self) -> FakeResponse:
        return FakeResponse()

    def close(self) -> None:
        self.closed = True


class SearchTransportTests(unittest.TestCase):
    def test_connect_is_short_but_response_read_has_no_stale_finite_timeout(self) -> None:
        connection = FakeConnection()
        factory_calls: List[Tuple[str, str, Optional[int], float]] = []

        def factory(
            scheme: str, host: str, port: Optional[int], timeout: float,
        ) -> FakeConnection:
            factory_calls.append((scheme, host, port, timeout))
            return connection

        transport = SearchTransport(
            "http://127.0.0.1:3080", connect_timeout_secs=2.0,
            connection_factory=factory,
        )

        response = transport.get_json("/api/search", {"q": "slow local model"})

        self.assertEqual(factory_calls, [("http", "127.0.0.1", 3080, 2.0)])
        self.assertTrue(connection.connected)
        self.assertEqual(connection.sock.timeouts, [None])
        self.assertEqual(connection.requests[0][0], "GET")
        self.assertIn("q=slow+local+model", connection.requests[0][1])
        self.assertEqual(response.payload[0]["drawer_id"], "drawer-transport")
        self.assertEqual(response.headers["content-type"], "application/json")
        self.assertTrue(connection.closed)


if __name__ == "__main__":
    unittest.main()

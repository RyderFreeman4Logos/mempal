import os
import sys
import unittest
import urllib.error
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

    def read(self, amount: Optional[int] = None) -> bytes:
        del amount
        return b'[{"drawer_id":"drawer-transport"}]'


class ErrorResponse:
    status = 503
    reason = "Service Unavailable"

    def __init__(self, body: bytes, content_length: Optional[int]) -> None:
        self.body = body
        self.content_length = content_length
        self.read_amounts: List[Optional[int]] = []
        self.closed = False

    def getheaders(self) -> List[Tuple[str, str]]:
        if self.content_length is None:
            return [("transfer-encoding", "chunked")]
        return [("content-length", str(self.content_length))]

    def read(self, amount: Optional[int] = None) -> bytes:
        self.read_amounts.append(amount)
        if amount is None:
            raise AssertionError("error response must never be read without a bound")
        return self.body[:amount]

    def close(self) -> None:
        self.closed = True


class FakeConnection:
    def __init__(self, response: Optional[object] = None) -> None:
        self.sock = FakeSocket()
        self.connected = False
        self.closed = False
        self.requests: List[Tuple[str, str, Dict[str, str]]] = []
        self.response = response or FakeResponse()

    def connect(self) -> None:
        self.connected = True

    def request(self, method: str, target: str, headers: Dict[str, str]) -> None:
        self.requests.append((method, target, dict(headers)))

    def getresponse(self) -> object:
        return self.response

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

    def test_oversized_content_length_error_body_is_bounded_and_discarded(self) -> None:
        response = ErrorResponse(b"x" * (64 * 1024 + 2), 10 * 1024 * 1024)

        error = self._request_error(response)
        self.addCleanup(error.close)

        self.assertEqual(response.read_amounts, [64 * 1024 + 1])
        self.assertEqual(error.read(), b"")
        self.assertTrue(response.closed)

    def test_unknown_length_error_body_is_bounded_and_discarded(self) -> None:
        response = ErrorResponse(b"x" * (64 * 1024 + 2), None)

        error = self._request_error(response)
        self.addCleanup(error.close)

        self.assertEqual(response.read_amounts, [64 * 1024 + 1])
        self.assertEqual(error.read(), b"")
        self.assertTrue(response.closed)

    def _request_error(self, response: ErrorResponse) -> urllib.error.HTTPError:
        connection = FakeConnection(response)
        transport = SearchTransport(
            "http://127.0.0.1:3080",
            connection_factory=lambda *_args: connection,
        )

        with self.assertRaises(urllib.error.HTTPError) as raised:
            transport.get_json("/api/search")
        self.assertTrue(connection.closed)
        return raised.exception


if __name__ == "__main__":
    unittest.main()

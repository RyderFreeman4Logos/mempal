"""Shared Hermes search transport with daemon-authoritative read deadlines."""

from __future__ import annotations

import http.client
import io
import json
import urllib.error
import urllib.parse
from dataclasses import dataclass
from typing import Any, Callable, Dict, Mapping, Optional

__all__ = ["SearchTransport", "SearchTransportResponse"]

_DEFAULT_CONNECT_TIMEOUT_SECS = 2.0
_REST_ERROR_BODY_MAX_BYTES = 64 * 1024


@dataclass(frozen=True)
class SearchTransportResponse:
    """Decoded response and headers from one daemon-authoritative search."""

    payload: Any
    headers: Dict[str, str]


ConnectionFactory = Callable[[str, str, Optional[int], float], Any]


def _default_connection_factory(
    scheme: str,
    host: str,
    port: Optional[int],
    timeout: float,
) -> Any:
    connection_type = (
        http.client.HTTPSConnection if scheme == "https" else http.client.HTTPConnection
    )
    return connection_type(host, port=port, timeout=timeout)


class SearchTransport:
    """GET JSON using a short connect timeout and no finite response-read ceiling.

    The daemon owns the checked end-to-end search budget. Once the local HTTP
    connection is established, a cached Hermes policy cannot terminate a query
    whose daemon deadline was hot-reloaded upward.
    """

    def __init__(
        self,
        base_url: str,
        *,
        connect_timeout_secs: float = _DEFAULT_CONNECT_TIMEOUT_SECS,
        connection_factory: Optional[ConnectionFactory] = None,
    ) -> None:
        parsed = urllib.parse.urlsplit(base_url.rstrip("/"))
        if parsed.scheme not in {"http", "https"} or not parsed.hostname:
            raise ValueError("mempal base_url must be an http(s) URL with a host")
        if parsed.username is not None or parsed.password is not None:
            raise ValueError("mempal base_url must not contain credentials")
        if connect_timeout_secs <= 0:
            raise ValueError("connect_timeout_secs must be positive")
        self._parsed_base = parsed
        self._connect_timeout_secs = float(connect_timeout_secs)
        self._connection_factory = connection_factory or _default_connection_factory

    def get_json(
        self,
        path: str,
        params: Optional[Mapping[str, Any]] = None,
    ) -> SearchTransportResponse:
        target, absolute_url = self._target(path, params)
        connection = self._connection_factory(
            self._parsed_base.scheme,
            self._parsed_base.hostname or "",
            self._parsed_base.port,
            self._connect_timeout_secs,
        )
        try:
            connection.connect()
            socket = getattr(connection, "sock", None)
            if socket is None:
                raise OSError("search transport connected without a socket")
            socket.settimeout(None)
            connection.request("GET", target, headers={"Accept": "application/json"})
            response = connection.getresponse()
            headers = {
                str(key): str(value)
                for key, value in response.getheaders()
                if value is not None
            }
            if not 200 <= int(response.status) <= 299:
                body = _read_bounded_error_body(response)
                raise urllib.error.HTTPError(
                    absolute_url,
                    int(response.status),
                    str(response.reason),
                    headers,
                    io.BytesIO(body),
                )
            body = response.read()
            payload = json.loads(body.decode("utf-8"))
            return SearchTransportResponse(payload, headers)
        finally:
            connection.close()

    def _target(
        self,
        path: str,
        params: Optional[Mapping[str, Any]],
    ) -> tuple[str, str]:
        request_path = "/" + path.lstrip("/")
        base_path = self._parsed_base.path.rstrip("/")
        target = f"{base_path}{request_path}" or "/"
        if params:
            query = urllib.parse.urlencode({
                key: _query_value(value)
                for key, value in params.items()
                if value is not None
            })
            if query:
                target = f"{target}?{query}"
        authority = self._parsed_base.netloc
        absolute_url = urllib.parse.urlunsplit((
            self._parsed_base.scheme,
            authority,
            target.split("?", 1)[0],
            target.partition("?")[2],
            "",
        ))
        return target, absolute_url


def _query_value(value: Any) -> Any:
    if isinstance(value, bool):
        return "true" if value else "false"
    return value


def _read_bounded_error_body(response: Any) -> bytes:
    """Read only the structured-error budget and discard oversized bodies."""
    try:
        body = response.read(_REST_ERROR_BODY_MAX_BYTES + 1)
    finally:
        response.close()
    if not isinstance(body, bytes) or len(body) > _REST_ERROR_BODY_MAX_BYTES:
        return b""
    return body

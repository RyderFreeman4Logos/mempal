"""Mempal hooks plugin for hermes-agent.

General plugin that registers lifecycle hooks to enrich hermes turns
with deep mempal context and capture tool observations as evidence.

Works standalone or alongside the mempal MemoryProvider plugin.
When both are active, MemoryProvider handles mirror/sync while this
plugin injects richer tiered context and captures observations.

Config via environment variables:
  MEMPAL_BASE_URL  -- mempal REST endpoint (default: http://127.0.0.1:3080)
  MEMPAL_USER_ID   -- user identifier (default: hermes-user)

Or via $HERMES_HOME/mempal.json (shared with MemoryProvider plugin).
"""

from __future__ import annotations

import json
import importlib.util
import logging
import os
import sys
import threading
import time
import uuid
from typing import Any, Dict, List, Optional

try:
    from mempal_search_transport import SearchTransport, SearchTransportResponse
except ImportError:
    _transport_candidates = (
        os.path.join(os.path.dirname(__file__), "_search_transport.py"),
        os.path.join(os.path.dirname(os.path.dirname(__file__)), "mempal_search_transport.py"),
    )
    _transport_path = next(
        (candidate for candidate in _transport_candidates if os.path.isfile(candidate)),
        "",
    )
    _transport_spec = importlib.util.spec_from_file_location(
        "mempal_hooks_search_transport", _transport_path,
    )
    if _transport_spec is None or _transport_spec.loader is None:
        raise ImportError("mempal search transport is unavailable")
    _transport_module = importlib.util.module_from_spec(_transport_spec)
    sys.modules[_transport_spec.name] = _transport_module
    _transport_spec.loader.exec_module(_transport_module)
    SearchTransport = _transport_module.SearchTransport
    SearchTransportResponse = _transport_module.SearchTransportResponse

logger = logging.getLogger(__name__)

_CONTEXT_TOP_K = 8
_OBSERVE_MIN_RESULT_LEN = 50
_OBSERVE_MAX_CONTENT_LEN = 2000
_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120
_DEGRADED_RESPONSE_SECS = 8.0
_OBSERVE_TOOL_ALLOWLIST = {
    # shell / command execution
    "bash", "shell", "run_command", "execute", "terminal", "process",
    # web / browse
    "web_search", "search", "browse", "web_extract", "open_page", "browser_navigate",
    # files
    "read_file", "write_file", "patch", "search_files",
    # code / analysis
    "python", "code_interpreter", "execute_code",
}
_OBSERVE_TOOL_DENYLIST = {
    "mempal_search", "mempal_conclude", "mempal_profile",
    "mempal_context", "mempal_ingest", "mempal_status",
    "mempal_timeline", "mempal_read_drawer", "mempal_read_drawers",
    "mempal_delete", "mempal_rollback", "mempal_kg",
    "mempal_knowledge_distill", "mempal_knowledge_promote",
    "mempal_knowledge_demote", "mempal_knowledge_gate",
    "mempal_knowledge_policy", "mempal_knowledge_cards",
    "mempal_knowledge_publish_anchor", "mempal_skill",
    "mempal_taxonomy", "mempal_field_taxonomy", "mempal_tunnels",
    "mempal_pinned_facts", "mempal_fact_check", "mempal_peek_partner",
    "mempal_cowork_push", "mempal_phase3", "mempal_search",
}


class BackoffState:
    def __init__(self, failure_count: int = 0, open_until_epoch: float = 0.0) -> None:
        self.failure_count = failure_count
        self.open_until_epoch = open_until_epoch

    @property
    def is_open(self) -> bool:
        return self.failure_count > 0 and time.time() < self.open_until_epoch


class SharedPluginBackoff:
    """Small file-backed breaker shared by mempal plugins via a common path."""

    def __init__(
        self,
        *,
        path: Optional[str] = None,
        threshold: int = 5,
        cooldown_secs: float = 120.0,
    ) -> None:
        self._path = path or self._default_path()
        self._threshold = threshold
        self._cooldown_secs = cooldown_secs
        self._write_lock = threading.Lock()

    @staticmethod
    def _default_path() -> str:
        configured = os.environ.get("MEMPAL_PLUGIN_BACKOFF_PATH", "").strip()
        if configured:
            return configured
        return os.path.join(os.path.expanduser("~"), ".mempal", ".plugin_backoff")

    @property
    def path(self) -> str:
        return self._path

    def is_open(self) -> bool:
        state = self._read_state()
        if state.failure_count < self._threshold:
            return False
        if time.time() >= state.open_until_epoch:
            self.record_success()
            return False
        return True

    def record_success(self) -> BackoffState:
        state = BackoffState()
        self._write_state(state)
        return state

    def record_failure(self) -> BackoffState:
        state = self._read_state()
        failure_count = state.failure_count + 1
        open_until_epoch = state.open_until_epoch
        if failure_count >= self._threshold:
            open_until_epoch = time.time() + self._cooldown_secs
        next_state = BackoffState(
            failure_count=failure_count,
            open_until_epoch=open_until_epoch,
        )
        self._write_state(next_state)
        return next_state

    def _read_state(self) -> BackoffState:
        try:
            with open(self._path, encoding="utf-8") as handle:
                raw = json.loads(handle.read() or "{}")
        except (OSError, json.JSONDecodeError, ValueError):
            return BackoffState()
        if not isinstance(raw, dict):
            return BackoffState()
        return BackoffState(
            failure_count=self._positive_int(raw.get("failure_count")),
            open_until_epoch=self._positive_float(raw.get("open_until_epoch")),
        )

    def _write_state(self, state: BackoffState) -> None:
        with self._write_lock:
            directory = os.path.dirname(self._path)
            if directory:
                os.makedirs(directory, exist_ok=True)
            tmp_path = f"{self._path}.tmp.{uuid.uuid4().hex}"
            payload: Dict[str, Any] = {
                "failure_count": state.failure_count,
                "open_until_epoch": state.open_until_epoch,
                "updated_at_epoch": time.time(),
            }
            with open(tmp_path, "w", encoding="utf-8") as handle:
                json.dump(payload, handle, separators=(",", ":"))
            os.replace(tmp_path, self._path)

    @staticmethod
    def _positive_int(value: Any) -> int:
        try:
            parsed = int(value)
        except (TypeError, ValueError):
            return 0
        return max(0, parsed)

    @staticmethod
    def _positive_float(value: Any) -> float:
        try:
            parsed = float(value)
        except (TypeError, ValueError):
            return 0.0
        return max(0.0, parsed)


def _load_config(hermes_home: str = "") -> dict:
    config = {
        "base_url": os.environ.get("MEMPAL_BASE_URL", "http://127.0.0.1:3080"),
        "user_id": os.environ.get("MEMPAL_USER_ID", "hermes-user"),
    }
    if hermes_home:
        config_path = os.path.join(hermes_home, "mempal.json")
        if os.path.exists(config_path):
            try:
                file_cfg = json.loads(open(config_path, encoding="utf-8").read())
                config.update({k: v for k, v in file_cfg.items() if v})
            except Exception:
                pass
    return config


def _rest_query_value(value: Any) -> Any:
    if isinstance(value, bool):
        return "true" if value else "false"
    return value


def _encode_query_params(params: Dict[str, Any]) -> str:
    import urllib.parse

    return urllib.parse.urlencode(
        {k: _rest_query_value(v) for k, v in params.items() if v is not None}
    )


class _MempalHooks:
    def __init__(self) -> None:
        self._base_url = "http://127.0.0.1:3080"
        self._user_id = "hermes-user"
        self._wing = "hermes-user/hermes-user/default"
        self._hermes_home = ""
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._backoff = SharedPluginBackoff(
            threshold=_BREAKER_THRESHOLD,
            cooldown_secs=_BREAKER_COOLDOWN_SECS,
        )
        self._last_response_headers: Dict[str, str] = {}
        self._search_transport = SearchTransport(self._base_url)
        self._initialized = False
        self._init_lock = threading.Lock()
        # Session-start warmup text keyed by session_id (consumed on first inject).
        self._session_warmup: Dict[str, str] = {}
        self._session_warmup_lock = threading.Lock()

    def _ensure_init(self, **kwargs) -> None:
        if self._initialized:
            return
        with self._init_lock:
            if self._initialized:
                return
            hermes_home = kwargs.get("hermes_home", "")
            if hermes_home:
                self._hermes_home = str(hermes_home)
            cfg = _load_config(self._hermes_home)
            self._base_url = cfg["base_url"].rstrip("/")
            self._search_transport = SearchTransport(self._base_url)
            self._user_id = cfg.get("user_id", "hermes-user")
            self._wing = f"hermes-user/{self._user_id}/default"
            self._initialized = True

    def _is_breaker_open(self) -> bool:
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            if time.monotonic() < self._breaker_open_until:
                return True
            self._consecutive_failures = 0
            self._breaker_open_until = 0.0
        return self._backoff.is_open()

    def _record_success(self) -> None:
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._backoff.record_success()

    def _record_failure(self) -> None:
        state = self._backoff.record_failure()
        self._consecutive_failures = state.failure_count
        if state.open_until_epoch:
            remaining = max(0.0, state.open_until_epoch - time.time())
            self._breaker_open_until = time.monotonic() + remaining
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            self._breaker_open_until = time.monotonic() + _BREAKER_COOLDOWN_SECS
            logger.warning(
                "mempal-hooks breaker tripped after %d failures, pausing %ds",
                self._consecutive_failures, _BREAKER_COOLDOWN_SECS,
            )

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        import urllib.request

        url = self._base_url + path
        if params:
            url += "?" + _encode_query_params(params)
        with urllib.request.urlopen(url, timeout=10) as resp:
            self._last_response_headers = dict(resp.headers.items())
            return json.loads(resp.read().decode())

    def _post(self, path: str, body: Dict[str, Any]) -> Any:
        import urllib.request

        data = json.dumps(body).encode()
        req = urllib.request.Request(
            self._base_url + path,
            data=data,
            headers={"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())

    @staticmethod
    def _search_results_payload(response: Any) -> List[Dict[str, Any]]:
        if isinstance(response, list):
            return response
        if isinstance(response, dict):
            results = response.get("results")
            if isinstance(results, list):
                return results
        return []

    @staticmethod
    def _search_degraded_reason(response: Any, elapsed_secs: float) -> str:
        if elapsed_secs > _DEGRADED_RESPONSE_SECS:
            return f"slow_response>{_DEGRADED_RESPONSE_SECS}s"
        if isinstance(response, list):
            for item in response:
                if not isinstance(item, dict):
                    continue
                warnings = item.get("warnings")
                if isinstance(warnings, list) and warnings:
                    return str(warnings[0] or "warnings")
            return ""
        if not isinstance(response, dict):
            return ""
        if response.get("deadline_hit"):
            return "deadline_hit"
        warning = response.get("warning")
        if warning:
            return str(warning)
        warnings = response.get("system_warnings")
        if warnings:
            return "system_warnings"
        return ""

    @staticmethod
    def _search_header_degraded_reason(headers: Any) -> str:
        if not headers:
            return ""
        normalized = {
            str(key).lower(): str(value)
            for key, value in dict(headers).items()
            if value is not None
        }
        degraded = normalized.get("degraded", "").strip().lower()
        if degraded in {"1", "true", "yes"}:
            return "degraded"
        warning = normalized.get("mempal-warnings", "").strip()
        if warning:
            return warning
        return ""

    def _get_search(self, params: Dict[str, Any]) -> tuple[List[Dict[str, Any]], str]:
        started = time.monotonic()
        self._last_response_headers = {}
        transport_response = self._search_transport.get_json("/api/search", params)
        response = transport_response.payload
        self._last_response_headers = dict(transport_response.headers)
        elapsed = time.monotonic() - started
        return (
            self._search_results_payload(response),
            self._search_degraded_reason(response, elapsed)
            or self._search_header_degraded_reason(self._last_response_headers),
        )

    # ── pre_llm_call: inject deep context ────────────────────────

    def pre_llm_call(
        self,
        session_id: str,
        user_message: str,
        is_first_turn: bool = False,
        **kwargs,
    ) -> Optional[Dict[str, str]]:
        self._ensure_init(**kwargs)
        if self._is_breaker_open():
            return None
        if not user_message or not user_message.strip():
            return None

        try:
            results, degraded_reason = self._get_search(
                {
                    "q": user_message,
                    "wing": self._wing,
                    "top_k": _CONTEXT_TOP_K,
                    "include_raw_turns": False,
                },
            )
            if degraded_reason:
                self._record_failure()
                logger.debug("mempal-hooks pre_llm_call search degraded: %s", degraded_reason)
            else:
                self._record_success()
        except Exception as exc:
            self._record_failure()
            logger.debug("mempal-hooks pre_llm_call search failed: %s", exc)
            return None

        if not results:
            return self._consume_session_warmup(session_id)

        # Tiered context: high-importance / decision-like kinds first, then the rest.
        high: List[str] = []
        mid: List[str] = []
        low: List[str] = []
        for r in results:
            content = (r.get("content") or "").strip()
            if not content:
                continue
            kind = str(r.get("memory_kind") or "")
            importance = r.get("importance")
            try:
                imp_val = float(importance) if importance is not None else 0.0
            except (TypeError, ValueError):
                imp_val = 0.0
            prefix = f"[{kind}]" if kind else ""
            suffix = f"(importance:{importance})" if importance is not None else ""
            line = f"- {prefix} {content} {suffix}".strip()
            kind_l = kind.lower()
            if imp_val >= 3 or kind_l in {"decision", "conclusion", "lesson", "pattern"}:
                high.append(line)
            elif imp_val >= 2 or kind_l in {"evidence", "observation"}:
                mid.append(line)
            else:
                low.append(line)

        sections: List[str] = []
        if high:
            sections.append("### High-signal\n" + "\n".join(high))
        if mid:
            sections.append("### Supporting\n" + "\n".join(mid))
        if low:
            sections.append("### Background\n" + "\n".join(low))
        if not sections:
            return self._consume_session_warmup(session_id)

        block = "## Relevant memories (mempal)\n" + "\n\n".join(sections)
        warmup = self._consume_session_warmup(session_id)
        if warmup and warmup.get("context"):
            block = warmup["context"] + "\n\n" + block
        return {"context": block}

    def _consume_session_warmup(self, session_id: str) -> Optional[Dict[str, str]]:
        with self._session_warmup_lock:
            text = self._session_warmup.pop(session_id, "")
        if not text:
            return None
        return {"context": text}

    # ── post_tool_call: capture observations ─────────────────────

    def post_tool_call(
        self,
        tool_name: str,
        args: Any,
        result: str,
        task_id: str = "",
        **kwargs,
    ) -> None:
        self._ensure_init(**kwargs)
        if self._is_breaker_open():
            logger.debug("mempal-hooks observation ingest suppressed while breaker is open")
            return

        if tool_name in _OBSERVE_TOOL_DENYLIST:
            return
        if tool_name not in _OBSERVE_TOOL_ALLOWLIST:
            return
        if not result or len(result) < _OBSERVE_MIN_RESULT_LEN:
            return

        try:
            parsed = json.loads(result) if isinstance(result, str) else result
        except (json.JSONDecodeError, TypeError):
            parsed = result

        if isinstance(parsed, dict) and parsed.get("error"):
            return

        summary = str(result)[:_OBSERVE_MAX_CONTENT_LEN]
        args_summary = ""
        if isinstance(args, dict):
            args_summary = " ".join(
                f"{k}={str(v)[:100]}" for k, v in args.items()
            )[:300]

        content = f"[tool:{tool_name}] {args_summary}\n{summary}"

        try:
            self._post(
                "/api/ingest",
                {
                    "content": content,
                    "wing": self._wing,
                    "room": "tool-observations",
                    "source_type": "tool_observation",
                    "memory_kind": "observation",
                    "importance": 1,
                },
            )
            self._record_success()
        except Exception as exc:
            self._record_failure()
            logger.debug("mempal-hooks observation ingest failed: %s", exc)

    # ── on_session_start: warm up context ────────────────────────

    def on_session_start(self, session_id: str, **kwargs) -> None:
        self._ensure_init(**kwargs)
        if self._is_breaker_open():
            return
        try:
            # Prefer brief for compact session wake-up; fall back to recent search.
            brief = self._get(
                "/api/brief",
                {
                    "wing": self._wing,
                    "limit": 8,
                },
            )
            text = self._format_brief_warmup(brief)
            if not text:
                results, degraded = self._get_search(
                    {
                        "q": "recent decisions lessons patterns",
                        "wing": self._wing,
                        "top_k": 6,
                        "include_raw_turns": False,
                    },
                )
                if degraded:
                    self._record_failure()
                    return
                text = self._format_search_warmup(results)
            if text:
                with self._session_warmup_lock:
                    self._session_warmup[session_id] = text
                self._record_success()
        except Exception as exc:
            self._record_failure()
            logger.debug("mempal-hooks session start warmup failed: %s", exc)

    @staticmethod
    def _format_brief_warmup(payload: Any) -> str:
        if payload is None:
            return ""
        if isinstance(payload, str) and payload.strip():
            return "## Session warmup (mempal brief)\n" + payload.strip()[:4000]
        if isinstance(payload, dict):
            # Common shapes: {text/markdown/content/brief: ...} or nested sections.
            for key in ("markdown", "text", "content", "brief", "summary"):
                value = payload.get(key)
                if isinstance(value, str) and value.strip():
                    return "## Session warmup (mempal brief)\n" + value.strip()[:4000]
            # Fall through to JSON snippet if structured.
            try:
                compact = json.dumps(payload, ensure_ascii=False)[:2000]
            except (TypeError, ValueError):
                return ""
            if compact and compact != "{}":
                return "## Session warmup (mempal brief)\n" + compact
        return ""

    @staticmethod
    def _format_search_warmup(results: List[Dict[str, Any]]) -> str:
        lines: List[str] = []
        for r in results:
            content = (r.get("content") or "").strip()
            if not content:
                continue
            kind = r.get("memory_kind") or ""
            prefix = f"[{kind}] " if kind else ""
            lines.append(f"- {prefix}{content[:400]}")
            if len(lines) >= 6:
                break
        if not lines:
            return ""
        return "## Session warmup (mempal recent)\n" + "\n".join(lines)


def register(ctx):
    hooks = _MempalHooks()
    ctx.register_hook("pre_llm_call", hooks.pre_llm_call)
    ctx.register_hook("post_tool_call", hooks.post_tool_call)
    ctx.register_hook("on_session_start", hooks.on_session_start)

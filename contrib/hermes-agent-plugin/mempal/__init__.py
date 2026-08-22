"""Mempal memory plugin for hermes-agent.

Local memory backend via mempal REST API.
BM25 + vector hybrid search, zero cloud dependency.

Hermes discovery marker: MemoryProvider register_memory_provider.

Config via environment variables:
  MEMPAL_BASE_URL  -- mempal REST endpoint (default: http://127.0.0.1:3080)
  MEMPAL_USER_ID   -- user identifier (default: hermes-user)

Or via $HERMES_HOME/mempal.json.
"""

from __future__ import annotations

import json
import logging
import os
import queue
import secrets
import threading
import time
from typing import Any, Dict, List, Optional

from ._backoff import SharedPluginBackoff
from ._conclude import conclusion_request, submit_conclusion
from ._intelligence import _IntelligenceEnhancer, _LLMClient
from ._rest_errors import (
    rest_error_payload as _rest_error_payload,
    search_metadata_from_headers,
    search_timeout_metadata_from_http_error,
)
from ._write_spool import SpoolOperation, WriteSpool, classify_write_error

try:
    from mempal_search_transport import SearchTransport, SearchTransportResponse
except ImportError:  # Installed plugin copy materialized by install_plugins.py.
    from ._search_transport import SearchTransport, SearchTransportResponse

logger = logging.getLogger(__name__)

_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120
_DEGRADED_RESPONSE_SECS = 8.0
_PREFETCH_TOP_K = 5
_TURN_STORAGE_MODE_TTL = 60.0
_WRITE_QUEUE_MAX = 1000
_WRITE_DRAIN_TIMEOUT = 10.0
_WRITE_RETRY_MAX = 3
_WRITE_RETRY_DELAY = 2.0
_PINNED_FACTS_TTL = 300.0
_DEFAULT_SAFE_MIN_IMPORTANCE = 3
_DEFAULT_SAFE_CONTEXT_BUDGET_CHARS = 4000
_SAFE_MEMORY_KINDS = {"knowledge", "profile_fact"}
_AUTHORITATIVE_STATUSES = {"canonical"}

_VALID_MODES = {"deterministic", "local_llm", "cloud_llm", "auto"}

PROFILE_SCHEMA = {
    "name": "mempal_profile",
    "description": (
        "Retrieve recent memory entries for the user via mempal timeline. "
        "Fast, ordered by recency. Use at conversation start for context."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "description": "Max entries to return (default: 20, max: 100).",
            },
        },
        "required": [],
    },
}

SEARCH_SCHEMA = {
    "name": "mempal_search",
    "description": (
        "Search memories by meaning via BM25 + vector hybrid search. "
        "Returns relevant facts ranked by similarity. "
        "Results include drawer_id, source, provenance, and typed metadata."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "What to search for."},
            "top_k": {"type": "integer", "description": "Max results (default: 10)."},
        },
        "required": ["query"],
    },
}

CONCLUDE_SCHEMA = {
    "name": "mempal_conclude",
    "description": "Store a durable fact verbatim in mempal for explicit preferences, corrections, or decisions.",
    "parameters": {
        "type": "object",
        "properties": {
            "conclusion": {"type": "string", "description": "The fact to store."},
            "operation_key": {"type": "string", "description": "Stable retry key from a pending response."},
        },
        "required": ["conclusion"],
    },
}


def _load_config(hermes_home: str = "") -> dict:
    config = {
        "base_url": os.environ.get("MEMPAL_BASE_URL", "http://127.0.0.1:3080"),
        "user_id": os.environ.get("MEMPAL_USER_ID", "hermes-user"),
        "turn_storage_mode": os.environ.get("MEMPAL_TURN_STORAGE_MODE", "raw_evidence"),
    }
    if hermes_home:
        config_path = os.path.join(hermes_home, "mempal.json")
        if os.path.exists(config_path):
            try:
                with open(config_path, encoding="utf-8") as handle:
                    file_cfg = json.loads(handle.read())
                config.update({
                    k: v for k, v in file_cfg.items()
                    if v is not None and not (isinstance(v, str) and not v)
                })
            except Exception:
                pass
    return config


def _strip_none(d: Dict[str, Any]) -> Dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


def _rest_query_value(value: Any) -> Any:
    if isinstance(value, bool):
        return "true" if value else "false"
    return value


def _encode_query_params(params: Dict[str, Any]) -> str:
    import urllib.parse

    return urllib.parse.urlencode(
        {k: _rest_query_value(v) for k, v in params.items() if v is not None}
    )


def _bounded_int(value: Any, default: int, minimum: int, maximum: int) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return max(minimum, min(maximum, parsed))


class MempalMemoryProvider:
    def __init__(self):
        self._base_url = "http://127.0.0.1:3080"
        self._session_id = ""
        self._hermes_home = ""
        self._user_id = "hermes-user"
        self._profile = "default"
        self._platform = "cli"
        self._chat_id = ""
        self._thread_id = ""
        self._wing = "hermes-user/hermes-user/default"
        self._turns_room = "turns"
        self._facts_room = "facts"
        self._project_id: Optional[str] = None
        self._prefetch_result = ""
        self._prefetch_results: Dict[str, str] = {}
        self._prefetch_generation = 0
        self._prefetch_lock = threading.Lock()
        self._prefetch_thread: Optional[threading.Thread] = None
        self._prefetch_pending = None
        self._prefetch_stop = threading.Event()
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._backoff = SharedPluginBackoff(
            threshold=_BREAKER_THRESHOLD,
            cooldown_secs=_BREAKER_COOLDOWN_SECS,
        )
        self._write_queue: queue.Queue = queue.Queue(maxsize=_WRITE_QUEUE_MAX)
        self._write_worker: Optional[threading.Thread] = None
        self._write_stop = threading.Event()
        self._write_drain_timeout = _WRITE_DRAIN_TIMEOUT
        self._conclude_wait_timeout = 5.0
        self._write_spool: Optional[WriteSpool] = None
        self._pinned_facts_cache: List[Dict[str, Any]] = []
        self._pinned_facts_fetched_at: float = 0.0
        self._pinned_facts_lock = threading.Lock()
        self._is_healthy = True
        self._last_health_at: float = 0.0
        self._last_response_headers: Dict[str, str] = {}
        self._search_transport = SearchTransport(self._base_url)
        self._intelligence_mode = "deterministic"
        self._llm = _LLMClient({})
        self._enhancer: Optional[_IntelligenceEnhancer] = None
        self._safe_mode = True
        self._safe_min_importance = _DEFAULT_SAFE_MIN_IMPORTANCE
        self._safe_context_budget_chars = _DEFAULT_SAFE_CONTEXT_BUDGET_CHARS
        self._safe_include_raw_turns = False
        self._safe_memory_kinds = set(_SAFE_MEMORY_KINDS)
        self._turn_storage_mode_cache = ""
        self._turn_storage_mode_fetched_at = 0.0

    def _configure_intelligence(self, cfg: dict) -> None:
        mi = cfg.get("memory_intelligence") or {}
        mode = str(mi.get("mode", "deterministic")).strip().lower()
        if mode not in _VALID_MODES:
            mode = "deterministic"
        self._intelligence_mode = mode
        if mode == "deterministic":
            self._llm = _LLMClient({})
            self._enhancer = None
            return
        llm_cfg = mi.get("llm") or {}
        self._llm = _LLMClient(llm_cfg)
        if mode == "auto" and not self._llm.is_configured:
            self._intelligence_mode = "deterministic"
            self._enhancer = None
            return
        if self._llm.is_configured:
            self._enhancer = _IntelligenceEnhancer(self._llm)
        else:
            self._enhancer = None

    def _configure_safe_mode(self, cfg: dict) -> None:
        safe = cfg.get("safe_mode") or {}
        if not isinstance(safe, dict):
            safe = {}
        enabled = safe.get("enabled", True)
        if isinstance(enabled, str):
            enabled = enabled.strip().lower() not in {"0", "false", "no", "off"}
        self._safe_mode = bool(enabled)
        self._safe_min_importance = _bounded_int(
            safe.get("min_importance"),
            _DEFAULT_SAFE_MIN_IMPORTANCE,
            0,
            5,
        )
        self._safe_context_budget_chars = _bounded_int(
            safe.get("context_budget_chars"),
            _DEFAULT_SAFE_CONTEXT_BUDGET_CHARS,
            500,
            20000,
        )
        include_raw = safe.get("include_raw_turns", False)
        if isinstance(include_raw, str):
            include_raw = include_raw.strip().lower() in {"1", "true", "yes", "on"}
        self._safe_include_raw_turns = bool(include_raw)
        kinds = safe.get("memory_kinds")
        if isinstance(kinds, list):
            clean = {str(kind).strip().lower() for kind in kinds if str(kind).strip()}
            self._safe_memory_kinds = clean or set(_SAFE_MEMORY_KINDS)
        else:
            self._safe_memory_kinds = set(_SAFE_MEMORY_KINDS)

    def _should_enhance(self) -> bool:
        if self._intelligence_mode == "deterministic":
            return False
        if self._enhancer is None:
            return False
        if self._llm._is_breaker_open():
            return False
        return True

    def _start_write_worker(self) -> None:
        if self._write_worker and self._write_worker.is_alive():
            return
        self._write_stop.clear()
        self._write_worker = threading.Thread(target=self._write_loop, daemon=True, name="mempal-write-worker")
        self._write_worker.start()

    def _write_loop(self) -> None:
        while not self._write_stop.is_set():
            try:
                item = self._write_queue.get(timeout=1.0)
            except queue.Empty:
                self._replay_spooled_write()
                continue
            if item.get("op") == "spool_wake":
                self._replay_spooled_write()
            else:
                self._process_write(item)
            self._write_queue.task_done()
            self._replay_spooled_write()
        while True:
            try:
                item = self._write_queue.get_nowait()
            except queue.Empty:
                break
            self._process_write(item)
            self._write_queue.task_done()

    def _spool_write(
        self,
        kind: str,
        body: Dict[str, Any],
        *,
        action: str,
        track_key: Optional[str] = None,
        wake: bool = True,
    ) -> SpoolOperation:
        if self._write_spool is None:
            raise RuntimeError("mempal durable write spool is not initialized")
        operation = self._write_spool.admit(
            kind, body, track_key=track_key, action=action
        )
        if wake:
            self._wake_spool_worker()
        return operation

    def _wake_spool_worker(self) -> None:
        self._start_write_worker()
        try:
            self._write_queue.put_nowait({"op": "spool_wake"})
        except queue.Full:
            pass

    def _replay_spooled_write(self) -> None:
        spool = self._write_spool
        if spool is None or self._is_breaker_open():
            return
        try:
            outcome = spool.replay_one(
                self._post,
                self._get,
                replay_allowed=lambda: not self._is_breaker_open(),
            )
        except Exception:
            logger.error("mempal durable spool metadata update failed")
            self._record_failure()
            self._update_health(False)
            return
        if outcome is None:
            return
        if outcome.completed:
            self._record_success()
            self._update_health(True)
        elif outcome.error_class:
            self._record_failure()
            self._update_health(False)
            logger.warning(
                "mempal durable replay deferred operation=%s kind=%s error_class=%s",
                outcome.operation.operation_key,
                outcome.operation.kind,
                outcome.error_class,
            )

    def _process_write(self, item: Dict[str, Any]) -> None:
        op = item.get("op")
        if op == "ingest" and self._is_breaker_open():
            logger.debug("mempal ingest suppressed while breaker is open")
            return
        if op == "ingest" and self._should_enhance() and self._enhancer:
            body = item["body"]
            content = body.get("content", "")
            if content and not item.get("skip_enhance"):
                metadata = self._enhancer.extract_metadata(content)
                if metadata:
                    for k, v in metadata.items():
                        if k not in body and v is not None:
                            body[k] = v
        for attempt in range(_WRITE_RETRY_MAX):
            try:
                if op == "ingest":
                    self._post("/api/ingest", item["body"])
                elif op == "delete":
                    self._post("/api/delete", item["body"])
                self._record_success()
                self._update_health(True)
                return
            except Exception as exc:
                error_class = classify_write_error(exc)
                if error_class.startswith("http_4"):
                    logger.warning("mempal write rejected error_class=%s", error_class)
                    self._record_failure()
                    return
                if attempt < _WRITE_RETRY_MAX - 1:
                    time.sleep(_WRITE_RETRY_DELAY)
                else:
                    logger.warning(
                        "mempal write deferred retries=%d error_class=%s",
                        _WRITE_RETRY_MAX,
                        error_class,
                    )
                    self._record_failure()
                    self._update_health(False)

    def _enqueue_write(self, item: Dict[str, Any]) -> bool:
        self._start_write_worker()
        try:
            self._write_queue.put_nowait(item)
            return True
        except queue.Full:
            logger.warning("mempal write wake queue full capacity=%d", _WRITE_QUEUE_MAX)
            return False

    def _update_health(self, healthy: bool) -> None:
        self._is_healthy = healthy
        self._last_health_at = time.monotonic()

    @property
    def name(self) -> str:
        return "mempal"

    def is_available(self) -> bool:
        cfg = _load_config(self._hermes_home)
        return bool(cfg.get("base_url"))

    def initialize(self, session_id: str, **kwargs) -> None:
        self._configure_scope(session_id, kwargs, preserve_existing=False)
        self._write_spool = WriteSpool(self._hermes_home)
        if self._write_spool.count():
            self._start_write_worker()

    def _configure_scope(self, session_id, context, *, preserve_existing):
        self._session_id = session_id
        if "hermes_home" in context or not preserve_existing:
            self._hermes_home = str(context.get("hermes_home", ""))
        cfg = _load_config(self._hermes_home)
        self._base_url = cfg["base_url"].rstrip("/")
        self._search_transport = SearchTransport(self._base_url)
        self._prefetch_stop.clear()
        user_id = context.get("user_id") or (self._user_id if preserve_existing else cfg.get("user_id", "hermes-user"))
        profile = context.get("agent_identity") or context.get("profile") or (self._profile if preserve_existing else "default")
        platform = context.get("platform") or (self._platform if preserve_existing else "cli")
        chat_id = str(context.get("chat_id") or "") if "chat_id" in context else (self._chat_id if preserve_existing else "")
        thread_id = str(context.get("thread_id") or "") if "thread_id" in context else (self._thread_id if preserve_existing else "")
        if "project_id" in context or "cwd" in context:
            project_id = context.get("project_id") or ""
            if not project_id:
                cwd = str(context.get("cwd") or "")
                project_id = os.path.basename(cwd.rstrip("/")) if cwd else ""
        else:
            project_id = self._project_id if preserve_existing else ""
        self._user_id = user_id
        self._profile = str(profile)
        self._platform = str(platform)
        self._chat_id = chat_id
        self._thread_id = thread_id
        self._wing = f"hermes-user/{user_id}/{self._profile}"
        self._turns_room = self._derive_turns_room(self._platform, chat_id, thread_id)
        self._facts_room = "facts"
        self._project_id = str(project_id) if project_id else None
        self._configure_intelligence(cfg)
        self._configure_safe_mode(cfg)

    @staticmethod
    def _derive_turns_room(platform, chat_id, thread_id):
        if not chat_id:
            return "turns"
        if thread_id:
            return f"turns/{platform}/{chat_id}/{thread_id}"
        return f"turns/{platform}/{chat_id}"

    def _session_key(self, session_id=""):
        return session_id or self._session_id

    def _with_project_id(self, payload):
        scoped = dict(payload)
        if self._project_id:
            scoped["project_id"] = self._project_id
        return scoped

    def _retrieval_params(self, payload):
        params = dict(payload)
        if self._safe_mode:
            params["include_raw_turns"] = self._safe_include_raw_turns
        else:
            params["include_raw_turns"] = bool(params.get("include_raw_turns", self._safe_include_raw_turns))
        return self._with_project_id(params)

    def _is_authoritative_memory(self, item):
        status = str(item.get("status", "")).strip().lower()
        return bool(item.get("is_pinned")) or status in _AUTHORITATIVE_STATUSES

    def _memory_authority_label(self, item):
        if self._is_authoritative_memory(item):
            return "authoritative"
        return "evidence/background"

    @staticmethod
    def _item_importance(item):
        try:
            return int(item.get("importance", 0) or 0)
        except (TypeError, ValueError):
            return 0

    def _safe_allows_item(self, item):
        if not self._safe_mode:
            return True
        if self._is_authoritative_memory(item):
            return True
        if "memory_kind" not in item and "importance" not in item:
            return True
        kind = str(item.get("memory_kind", "")).strip().lower()
        importance = self._item_importance(item)
        return kind in self._safe_memory_kinds or importance >= self._safe_min_importance

    def _rank_safe_items(self, items):
        indexed = list(enumerate(items))
        indexed.sort(
            key=lambda pair: (
                0 if self._is_authoritative_memory(pair[1]) else 1,
                -self._item_importance(pair[1]),
                pair[0],
            )
        )
        return [item for _, item in indexed]

    def _safe_filter_items(self, items):
        return self._rank_safe_items([item for item in items if self._safe_allows_item(item)])

    def _append_with_budget(self, lines, line, used):
        if used + len(line) > self._safe_context_budget_chars:
            return used, False
        lines.append(line)
        return used + len(line), True

    def _session_room(self):
        return f"sessions/{self._session_id}" if self._session_id else "sessions"

    def _memory_room_for_target(self, target):
        normalized = (target or "").strip().lower()
        if normalized in {"fact", "facts", "profile", "user", "global"}:
            return self._facts_room
        if normalized in {"turn", "turns", "conversation"}:
            return self._turns_room
        if normalized in {"session", "sessions"}:
            return self._session_room()
        if normalized:
            return f"memory-mirror/{normalized.replace('/', '_')}"
        return "memory-mirror"

    def _turn_storage_mode(self):
        now = time.monotonic()
        if (
            self._turn_storage_mode_cache
            and now - self._turn_storage_mode_fetched_at < _TURN_STORAGE_MODE_TTL
        ):
            return self._turn_storage_mode_cache
        try:
            status = self._get("/api/status")
            mode = status.get("turn_storage", {}).get("storage_mode", "").strip()
            if mode:
                self._turn_storage_mode_cache = mode
                self._turn_storage_mode_fetched_at = now
                return mode
        except Exception as exc:
            logger.debug("mempal status lookup for turn storage failed: %s", exc)
        cfg = _load_config(self._hermes_home)
        mode = str(cfg.get("turn_storage_mode", "raw_evidence")).strip() or "raw_evidence"
        self._turn_storage_mode_cache = mode
        self._turn_storage_mode_fetched_at = now
        return mode

    def _is_breaker_open(self):
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            if time.monotonic() < self._breaker_open_until:
                return True
            self._consecutive_failures = 0
            self._breaker_open_until = 0.0
        return self._backoff.is_open()

    def _record_success(self):
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._backoff.record_success()

    def _record_failure(self):
        state = self._backoff.record_failure()
        self._consecutive_failures = state.failure_count
        if state.open_until_epoch:
            remaining = max(0.0, state.open_until_epoch - time.time())
            self._breaker_open_until = time.monotonic() + remaining
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            self._breaker_open_until = time.monotonic() + _BREAKER_COOLDOWN_SECS
            logger.warning("mempal circuit breaker tripped after %d failures. Pausing %ds.", self._consecutive_failures, _BREAKER_COOLDOWN_SECS)

    @staticmethod
    def _search_results_payload(response):
        if isinstance(response, list):
            return response
        if isinstance(response, dict):
            results = response.get("results")
            if isinstance(results, list):
                return results
        return []

    @staticmethod
    def _search_degraded_reason(response, elapsed_secs):
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
    def _search_header_degraded_reason(headers):
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

    def _search_metadata_from_headers(self, headers, correlation_id):
        return search_metadata_from_headers(
            headers,
            correlation_id,
        )

    @staticmethod
    def _is_search_timeout(exc):
        if isinstance(exc, TimeoutError):
            return True
        reason = getattr(exc, "reason", None)
        return isinstance(reason, TimeoutError)

    def _search_request(self, params) -> SearchTransportResponse:
        return self._search_transport.get_json("/api/search", params)

    def _get_search(self, params, correlation_id=None):
        correlation_id = correlation_id or f"search-{secrets.token_hex(8)}"
        bounded_params = dict(params)
        # Omit deadline_ms so the daemon hot-reloaded E2E policy is authoritative.
        bounded_params.pop("deadline_ms", None)
        bounded_params["correlation_id"] = correlation_id
        started = time.monotonic()
        self._last_response_headers = {}
        transport_response = self._search_request(bounded_params)
        response = transport_response.payload
        self._last_response_headers = dict(transport_response.headers)
        elapsed = time.monotonic() - started
        reason = (
            self._search_degraded_reason(response, elapsed)
            or self._search_header_degraded_reason(self._last_response_headers)
        )
        metadata = self._search_metadata_from_headers(
            self._last_response_headers, correlation_id,
        )
        return self._search_results_payload(response), reason, metadata

    def _get(self, path, params=None, timeout=10.0):
        import urllib.request
        url = self._base_url + path
        if params:
            url += "?" + _encode_query_params(params)
        with urllib.request.urlopen(url, timeout=timeout) as resp:
            self._last_response_headers = dict(resp.headers.items())
            return json.loads(resp.read().decode())

    def _post(self, path, body):
        import urllib.request
        data = json.dumps(body).encode()
        req = urllib.request.Request(self._base_url + path, data=data, headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode())

    def _fetch_pinned_facts(self):
        now = time.monotonic()
        with self._pinned_facts_lock:
            if now - self._pinned_facts_fetched_at < _PINNED_FACTS_TTL:
                return list(self._pinned_facts_cache)
        try:
            facts = self._get("/api/pinned_facts", self._with_project_id({"wing": self._wing, "limit": 20}))
            self._record_success()
            with self._pinned_facts_lock:
                self._pinned_facts_cache = facts if isinstance(facts, list) else []
                self._pinned_facts_fetched_at = now
            return list(self._pinned_facts_cache)
        except Exception as exc:
            logger.debug("mempal pinned facts fetch failed: %s", exc)
            self._record_failure()
            with self._pinned_facts_lock:
                return list(self._pinned_facts_cache)

    def _format_pinned_block(self, facts):
        if not facts:
            return ""
        lines = []
        used = 0
        for f in facts:
            kind = f.get("memory_kind", "fact")
            imp = f.get("importance", 0)
            drawer_id = f.get("drawer_id") or "unknown"
            source = f.get("source_file") or "unknown"
            content = f.get("content", "").replace("\n", " ")[:200]
            line = (
                f"- [authoritative/pinned][{kind}] {content} "
                f"(drawer_id: {drawer_id}, source: {source}, importance: {imp})"
            )
            used, accepted = self._append_with_budget(lines, line, used)
            if not accepted:
                break
        return "## Pinned Facts (always active)\n" + "\n".join(lines)

    def system_prompt_block(self):
        mode_label = self._intelligence_mode
        if mode_label != "deterministic" and self._llm.is_configured:
            mode_label = f"{mode_label} (llm: {self._llm.status})"
        base = (
            "# Mempal Memory\n"
            f"Active. User: {self._user_id}. Profile: {self._profile}. "
            f"Mode: {mode_label}. "
            "Local BM25+vector backend, zero cloud.\n"
            "Use mempal_search to recall facts, mempal_conclude to store, "
            "mempal_profile for recent overview. Treat search/profile hits as "
            "evidence/background unless marked authoritative."
        )
        pinned = self._fetch_pinned_facts()
        pinned_block = self._format_pinned_block(pinned)
        if pinned_block:
            return f"{base}\n\n{pinned_block}"
        return base

    def prefetch(self, query, *, session_id=""):
        del query
        with self._prefetch_lock:
            thread = self._prefetch_thread
        if thread and thread.is_alive() and thread is not threading.current_thread():
            thread.join(timeout=3.0)
        key = self._session_key(session_id)
        with self._prefetch_lock:
            result = self._prefetch_results.pop(key, "")
            if not result and key == self._session_id:
                result = self._prefetch_result
            self._prefetch_result = ""
        if not result:
            return ""
        return f"## Mempal Memory\n{result}"

    def queue_prefetch(self, query, *, session_id=""):
        if self._is_breaker_open():
            return
        key = self._session_key(session_id)
        wing = self._wing
        params = self._retrieval_params({
            "q": query,
            "wing": wing,
            "top_k": _PREFETCH_TOP_K,
        })
        # REST currently supports cheaper prefetch only through top_k reduction.
        # Rerank bypass needs REST-side query support before the plugin can send it.
        with self._prefetch_lock:
            if self._prefetch_stop.is_set():
                return
            self._prefetch_pending = (
                params, key, self._prefetch_generation,
            )
            if self._prefetch_thread is not None:
                return
            worker = threading.Thread(
                target=self._prefetch_loop,
                daemon=True,
                name="mempal-prefetch",
            )
            self._prefetch_thread = worker
            worker.start()

    def _prefetch_loop(self):
        while not self._prefetch_stop.is_set():
            with self._prefetch_lock:
                job = self._prefetch_pending
                self._prefetch_pending = None
                if job is None:
                    self._prefetch_thread = None
                    return
            self._process_prefetch_job(*job)
        with self._prefetch_lock:
            self._prefetch_pending = None
            self._prefetch_thread = None

    def _process_prefetch_job(self, params, key, generation):
        try:
            results, degraded_reason, _metadata = self._get_search(params)
            if results:
                lines = []
                used = 0
                for result_item in self._safe_filter_items(results):
                    content = result_item.get("content", "").replace("\n", " ")
                    if not content:
                        continue
                    drawer_id = result_item.get("drawer_id") or "unknown"
                    source = (
                        result_item.get("source")
                        or result_item.get("source_file")
                        or "unknown"
                    )
                    label = self._memory_authority_label(result_item)
                    line = (
                        f"- [{label}] {content} "
                        f"(drawer_id: {drawer_id}, source: {source}, "
                        f"importance: {result_item.get('importance', 0)})"
                    )
                    used, accepted = self._append_with_budget(lines, line, used)
                    if not accepted:
                        break
                with self._prefetch_lock:
                    if generation == self._prefetch_generation:
                        rendered = "\n".join(lines)
                        self._prefetch_results[key] = rendered
                        if key == self._session_id:
                            self._prefetch_result = rendered
            if degraded_reason:
                self._record_failure()
                logger.debug("mempal prefetch degraded: %s", degraded_reason)
            else:
                self._record_success()
        except Exception as exc:
            self._record_failure()
            logger.debug("mempal prefetch failed: %s", exc)

    def sync_turn(self, user_content, assistant_content, *, session_id=""):
        if self._turn_storage_mode() != "raw_evidence":
            return
        content = f"User: {user_content}\nAssistant: {assistant_content}"
        body = self._with_project_id({"content": content, "wing": self._wing, "room": self._turns_room})
        self._spool_write("ingest", body, action="raw_turn")
        if self._should_enhance() and self._enhancer:
            facts = self._enhancer.extract_facts(content)
            if facts:
                for fact_entry in facts:
                    fact_body = self._with_project_id({
                        "content": fact_entry["fact"],
                        "wing": self._wing,
                        "room": self._facts_room,
                        "memory_kind": fact_entry.get("memory_kind", "fact"),
                        "importance": fact_entry.get("importance", 2),
                        "source_type": "llm_extracted",
                    })
                    self._enqueue_write({"op": "ingest", "body": fact_body, "skip_enhance": True})

    def get_tool_schemas(self):
        return [PROFILE_SCHEMA, SEARCH_SCHEMA, CONCLUDE_SCHEMA]

    def authoritative_memory_write(self, request, **kwargs):
        """Persist one Hermes core memory operation through the durable spool."""
        del kwargs
        if not isinstance(request, dict):
            return json.dumps({
                "success": False,
                "error_class": "invalid_request",
                "retryable": False,
            })
        action = request.get("action")
        target = str(request.get("target") or "memory")
        content = request.get("content")
        old_text = request.get("old_text")
        operations = request.get("operations")

        def validation_error(item_action, item_content, item_old_text):
            if item_action not in {"add", "replace", "remove"}:
                return "invalid_action"
            if item_action in {"add", "replace"} and (
                not isinstance(item_content, str) or not item_content
            ):
                return "missing_content"
            if item_action in {"replace", "remove"} and (
                not isinstance(item_old_text, str) or not item_old_text
            ):
                return "missing_old_text"
            return None

        if operations is not None:
            if not isinstance(operations, list) or not operations:
                return json.dumps({
                    "success": False,
                    "error_class": "invalid_operations",
                    "retryable": False,
                })
            normalized = []
            for item in operations:
                if not isinstance(item, dict):
                    return json.dumps({
                        "success": False,
                        "error_class": "invalid_request",
                        "retryable": False,
                    })
                item_action = item.get("action")
                item_content = item.get("content")
                item_old_text = item.get("old_text")
                error_class = validation_error(item_action, item_content, item_old_text)
                if error_class is not None:
                    return json.dumps({
                        "success": False,
                        "error_class": error_class,
                        "retryable": False,
                    })
                normalized.append({
                    "action": item_action,
                    "target": target,
                    "content": item_content,
                    "old_text": item_old_text,
                })

            completed_ids = []
            operation_keys = []
            for item in normalized:
                receipt = json.loads(self.authoritative_memory_write(item))
                if receipt.get("operation_key"):
                    operation_keys.append(receipt["operation_key"])
                if receipt.get("success"):
                    if receipt.get("operation_id"):
                        completed_ids.append(receipt["operation_id"])
                    continue
                error_class = receipt.get("error_class") or "durable_write_pending"
                return json.dumps({
                    "success": False,
                    "partial_write": bool(completed_ids),
                    "operation_ids": completed_ids,
                    "operation_keys": operation_keys,
                    "error_class": error_class,
                    "last_error_class": error_class,
                    "retryable": receipt.get("retryable", True),
                })
            return json.dumps({
                "success": True,
                "partial_write": False,
                "operation_ids": completed_ids,
                "operation_keys": operation_keys,
            })
        error_class = validation_error(action, content, old_text)
        if error_class is not None:
            return json.dumps({
                "success": False,
                "error_class": error_class,
                "retryable": False,
            })

        track_key = f"{target}:{self._wing}"
        room = self._memory_room_for_target(target)
        if action == "remove":
            body = self._with_project_id({})
            kind = "delete"
        else:
            body = self._with_project_id({
                "content": content,
                "wing": self._wing,
                "room": room,
            })
            if action == "replace":
                body["replace_text"] = old_text
            if room == self._facts_room:
                body.update({
                    "memory_kind": "profile_fact",
                    "importance": self._safe_min_importance,
                    "source_type": "user_explicit",
                })
            kind = "ingest"

        try:
            operation = self._spool_write(
                kind,
                body,
                track_key=track_key,
                action="delete" if action == "remove" else str(action),
                wake=False,
            )
        except Exception:
            return json.dumps({
                "success": False,
                "error_class": "local_durable_admission_failed",
                "retryable": True,
            })

        key = operation.operation_key
        if self._is_breaker_open():
            try:
                self._wake_spool_worker()
            except Exception:
                logger.warning("mempal authoritative write replay wake failed")
            return json.dumps({
                "success": False,
                "error_class": "breaker_open",
                "retryable": True,
                "operation_key": key,
            })
        try:
            outcome = self._write_spool.replay_operation_key(
                key,
                self._post,
                self._get,
                ignore_retry_delay=True,
                replay_allowed=lambda: not self._is_breaker_open(),
            )
        except Exception:
            outcome = None
        if outcome is not None and outcome.completed and outcome.drawer_id:
            try:
                self._record_success()
            except Exception:
                logger.warning("mempal authoritative write success bookkeeping failed")
            return json.dumps({
                "success": True,
                "drawer_id": outcome.drawer_id,
                "operation_id": outcome.operation_id or "",
                "operation_key": key,
            })
        error_class = (
            outcome.error_class if outcome is not None and outcome.error_class
            else "durable_write_pending"
        )
        if error_class == "breaker_open":
            retryable = True
        else:
            retryable = not error_class.startswith("terminal_") and error_class != "target_unresolved"
            try:
                self._record_failure()
            except Exception:
                logger.warning("mempal authoritative write failure bookkeeping failed")
        payload = {
            "success": False,
            "error_class": error_class,
            "retryable": retryable,
            "operation_key": key,
        }
        if outcome is not None and outcome.operation_id:
            payload["operation_id"] = outcome.operation_id
        return json.dumps(payload)

    def handle_tool_call(self, tool_name, args, **kwargs):
        if (
            tool_name not in {"mempal_profile", "mempal_search"}
            and self._is_breaker_open()
            and tool_name != "mempal_conclude"
        ):
            return json.dumps({"error": "mempal temporarily unavailable. Will retry automatically."})
        if tool_name == "mempal_profile":
            try:
                try:
                    limit = min(int(args.get("limit", 20)), 100)
                except (ValueError, TypeError):
                    limit = 20
                entries = self._get("/api/timeline", self._retrieval_params({"wing": self._wing, "limit": limit}))
                try:
                    self._record_success()
                except Exception:
                    logger.warning("mempal profile success bookkeeping failed")
                if not entries:
                    return json.dumps({"result": "No memories stored yet."})
                items = [
                    _strip_none({
                        "content": e.get("content", ""),
                        "drawer_id": e.get("drawer_id"),
                        "source": e.get("source_file"),
                        "importance": e.get("importance"),
                        "added_at": e.get("added_at"),
                        "authority": self._memory_authority_label(e),
                    })
                    for e in self._safe_filter_items(entries)
                    if e.get("content")
                ]
                return json.dumps({"results": items, "count": len(items)})
            except Exception as exc:
                try:
                    self._record_failure()
                except Exception:
                    logger.warning("mempal profile failure bookkeeping failed")
                return json.dumps(_rest_error_payload(
                    "Mempal profile lookup failed.",
                    "/api/timeline",
                    exc,
                ))
        elif tool_name == "mempal_search":
            q = args.get("query", "")
            if not q:
                return json.dumps({"error": "Missing required parameter: query"})
            try:
                top_k = min(int(args.get("top_k", 10)), 50)
            except (ValueError, TypeError):
                top_k = 10
            correlation_id = f"search-{secrets.token_hex(8)}"
            started = time.monotonic()
            try:
                results, degraded_reason, metadata = self._get_search(
                    self._retrieval_params({"q": q, "wing": self._wing, "top_k": top_k}),
                    correlation_id,
                )
                if degraded_reason:
                    try:
                        self._record_failure()
                    except Exception:
                        logger.warning("mempal search failure bookkeeping failed")
                    logger.debug("mempal search degraded: %s", degraded_reason)
                else:
                    try:
                        self._record_success()
                    except Exception:
                        logger.warning("mempal search success bookkeeping failed")
                if not results:
                    payload = {"result": "No relevant memories found."}
                    if metadata and (metadata["partial"] or metadata["fallback_used"]):
                        payload["search_metadata"] = metadata
                    return json.dumps(payload)
                items = [_strip_none({"memory": r.get("content", ""), "score": r.get("similarity", 0), "drawer_id": r.get("drawer_id"), "source": r.get("source"), "source_type": r.get("source_type"), "provenance": r.get("provenance"), "status": r.get("status"), "memory_kind": r.get("memory_kind"), "domain": r.get("domain"), "field": r.get("field"), "importance": r.get("importance"), "is_pinned": r.get("is_pinned"), "confidence": r.get("confidence"), "authority": self._memory_authority_label(r)}) for r in self._safe_filter_items(results)]
                payload = {"results": items, "count": len(items)}
                if metadata and (metadata["partial"] or metadata["fallback_used"]):
                    payload["search_metadata"] = metadata
                return json.dumps(payload)
            except Exception as exc:
                try:
                    self._record_failure()
                except Exception:
                    logger.warning("mempal search failure bookkeeping failed")
                daemon_timeout = search_timeout_metadata_from_http_error(
                    exc,
                    correlation_id,
                )
                if daemon_timeout:
                    return json.dumps({
                        "error": "Mempal search reached the daemon deadline.",
                        "error_details": {
                            "kind": "search_timeout",
                            **daemon_timeout,
                        },
                    })
                if self._is_search_timeout(exc):
                    elapsed_ms = int((time.monotonic() - started) * 1000)
                    return json.dumps({
                        "error": "Mempal search transport timed out before the daemon returned terminal metadata.",
                        "error_details": {
                            "kind": "search_timeout",
                            "correlation_id": correlation_id,
                            "elapsed_ms": elapsed_ms,
                            "partial": False,
                            "retry_safe": True,
                            "fallback_used": [],
                            "timeouts": [{
                                "stage": "transport",
                                "boundary": "plugin.rest_transport",
                            }],
                        },
                    })
                status = getattr(exc, "code", None)
                failure = {
                    "stage": "transport",
                    "boundary": "plugin.rest_transport",
                    "error_class": exc.__class__.__name__,
                }
                if isinstance(status, int) and not isinstance(status, bool):
                    failure["http_status"] = status
                return json.dumps({
                    "error": "Mempal search transport failed.",
                    "error_details": {
                        "kind": "search_transport_failure",
                        "correlation_id": correlation_id,
                        "elapsed_ms": int((time.monotonic() - started) * 1000),
                        "partial": False,
                        "retry_safe": True,
                        "fallback_used": [],
                        "timeouts": [],
                        "failures": [failure],
                    },
                })
        elif tool_name == "mempal_conclude":
            conclusion = args.get("conclusion", "")
            if not conclusion:
                return json.dumps({"error": "Missing required parameter: conclusion"})
            try:
                result = submit_conclusion(
                    self._write_spool,
                    self._post,
                    self._get,
                    conclusion_request(
                        conclusion, self._wing, self._facts_room,
                        self._safe_min_importance, self._project_id,
                    ),
                    operation_key=args.get("operation_key"),
                    wait_timeout=self._conclude_wait_timeout,
                    transport_allowed=not self._is_breaker_open(),
                    replay_allowed=lambda: not self._is_breaker_open(),
                )
                if result.stored:
                    try:
                        self._record_success()
                    except Exception:
                        logger.warning("mempal conclude success bookkeeping failed")
                else:
                    details = result.payload.get("error_details", {})
                    local_admission = (
                        result.payload.get("state") == "local_admitted"
                        or result.payload.get("durability", {}).get("kind")
                        == "durable_replay_deferred"
                        or details.get("kind") == "durable_replay_deferred"
                    )
                    if not local_admission:
                        try:
                            self._record_failure()
                        except Exception:
                            logger.warning("mempal conclude failure bookkeeping failed")
                    if details.get("kind") != "local_durable_admission_failed":
                        try:
                            self._wake_spool_worker()
                        except Exception:
                            logger.warning("mempal conclude replay wake failed")
                return json.dumps(result.payload)
            except Exception as exc:
                self._record_failure()
                return json.dumps(_rest_error_payload(
                    "Failed to store memory via mempal REST API.",
                    "/api/ingest/durable",
                    exc,
                ))
        return json.dumps({"error": f"Unknown tool: {tool_name}"})

    def on_session_end(self, messages):
        assistant_msgs = [m.get("content", "") for m in messages if m.get("role") == "assistant" and m.get("content")]
        if not assistant_msgs:
            return
        summary = assistant_msgs[-1][:2000]
        body = self._with_project_id({"content": f"[Session summary] {summary}", "wing": self._wing, "room": self._session_room()})
        operation = self._spool_write(
            "ingest", body, action="session_summary", wake=False
        )
        try:
            if self._should_enhance() and self._enhancer:
                all_text = "\n".join(f"{m.get('role', 'unknown')}: {m.get('content', '')}" for m in messages[-10:] if m.get("content"))
                enhanced = self._enhancer.enhance_summary(all_text)
                if enhanced:
                    enhanced_body = dict(body)
                    enhanced_body["content"] = f"[Session summary] {enhanced}"
                    self._write_spool.replace_body(operation.operation_key, enhanced_body)
        except Exception:
            logger.warning("mempal summary enhancement failed; deterministic evidence retained")
        finally:
            self._wake_spool_worker()

    def on_memory_write(self, action, target, content, metadata=None):
        track_key = f"{target}:{self._wing}"
        room = self._memory_room_for_target(target)
        if action == "remove":
            self._spool_write(
                "delete",
                self._with_project_id({}),
                track_key=track_key,
                action="delete",
            )
            return
        body = self._with_project_id({"content": content, "wing": self._wing, "room": room})
        if room == self._facts_room:
            body.setdefault("memory_kind", "profile_fact")
            body.setdefault("importance", self._safe_min_importance)
            body.setdefault("source_type", "user_explicit")
        if metadata:
            for field in ("memory_kind", "domain", "field", "importance", "status", "is_pinned", "source_type"):
                if field in metadata and metadata[field] is not None:
                    body[field] = metadata[field]
        self._spool_write(
            "ingest",
            body,
            track_key=track_key,
            action="replace" if action == "replace" else "add",
        )

    def on_session_switch(self, new_session_id, reason="", **kwargs):
        del reason
        with self._prefetch_lock:
            self._prefetch_result = ""
            self._prefetch_results.clear()
            self._prefetch_pending = None
            self._prefetch_generation += 1
        with self._pinned_facts_lock:
            self._pinned_facts_fetched_at = 0.0
        self._configure_scope(new_session_id, kwargs, preserve_existing=True)

    def shutdown(self):
        deadline = time.monotonic() + self._write_drain_timeout
        self._write_stop.set()
        self._prefetch_stop.set()
        with self._prefetch_lock:
            self._prefetch_pending = None
            prefetch_thread = self._prefetch_thread
        if prefetch_thread and prefetch_thread.is_alive():
            prefetch_thread.join(
                timeout=min(3.0, max(0.0, deadline - time.monotonic()))
            )
        if self._write_worker and self._write_worker.is_alive():
            self._write_worker.join(timeout=max(0.0, deadline - time.monotonic()))
            if self._write_worker.is_alive():
                pending = self._write_spool.count() if self._write_spool else 0
                logger.warning(
                    "mempal write worker shutdown timed out pending=%d", pending
                )
    def get_config_schema(self):
        return [
            {"key": "base_url", "description": "mempal REST endpoint", "default": "http://127.0.0.1:3080", "env_var": "MEMPAL_BASE_URL"},
            {"key": "user_id", "description": "User identifier for memory scoping", "default": "hermes-user", "env_var": "MEMPAL_USER_ID"},
            {"key": "turn_storage_mode", "description": "Raw turn storage mode: off, raw_evidence, or summarized", "default": "raw_evidence", "env_var": "MEMPAL_TURN_STORAGE_MODE"},
            {"key": "memory_intelligence.mode", "description": "Intelligence mode: deterministic, local_llm, cloud_llm, or auto", "default": "deterministic"},
            {"key": "memory_intelligence.llm.base_url", "description": "OpenAI-compatible LLM endpoint for enhancement", "default": ""},
            {"key": "memory_intelligence.llm.model", "description": "Model name for LLM enhancement", "default": ""},
            {"key": "memory_intelligence.llm.api_key", "description": "API key for LLM endpoint (optional for local)", "default": ""},
            {"key": "memory_intelligence.llm.timeout_secs", "description": "LLM request timeout in seconds", "default": "30"},
            {"key": "safe_mode.enabled", "description": "Conservative coding-agent retrieval mode", "default": "true"},
            {"key": "safe_mode.min_importance", "description": "Minimum importance for non-authoritative recalled evidence", "default": str(_DEFAULT_SAFE_MIN_IMPORTANCE)},
            {"key": "safe_mode.memory_kinds", "description": "REST memory_kind values allowed below the importance threshold", "default": "knowledge,profile_fact"},
            {"key": "safe_mode.context_budget_chars", "description": "Maximum injected mempal context characters", "default": str(_DEFAULT_SAFE_CONTEXT_BUDGET_CHARS)},
            {"key": "safe_mode.include_raw_turns", "description": "Allow raw turn retrieval in injected/search context", "default": "false"},
        ]

    def save_config(self, values, hermes_home):
        config_path = os.path.join(hermes_home, "mempal.json")
        existing = {}
        if os.path.exists(config_path):
            try:
                existing = json.loads(open(config_path, encoding="utf-8").read())
            except Exception:
                pass
        existing.update(values)
        with open(config_path, "w", encoding="utf-8") as f:
            json.dump(existing, f, indent=2)


def register(ctx):
    ctx.register_memory_provider(MempalMemoryProvider())

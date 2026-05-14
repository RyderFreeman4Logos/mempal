"""Mempal memory plugin for hermes-agent.

Local memory backend via mempal REST API.
BM25 + vector hybrid search, zero cloud dependency.

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
import threading
import time
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120
_WRITE_QUEUE_MAX = 1000
_WRITE_DRAIN_TIMEOUT = 10.0
_WRITE_RETRY_MAX = 3
_WRITE_RETRY_DELAY = 2.0
_HEALTH_CHECK_INTERVAL = 60.0
_PINNED_FACTS_TTL = 300.0

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
    "description": (
        "Store a durable fact about the user in mempal. "
        "Stored verbatim via local BM25 + vector index. "
        "Use for explicit preferences, corrections, or decisions."
    ),
    "parameters": {
        "type": "object",
        "properties": {
            "conclusion": {"type": "string", "description": "The fact to store."},
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
                file_cfg = json.loads(open(config_path, encoding="utf-8").read())
                config.update({k: v for k, v in file_cfg.items() if v})
            except Exception:
                pass
    return config


def _strip_none(d: Dict[str, Any]) -> Dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


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
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._write_queue: queue.Queue = queue.Queue(maxsize=_WRITE_QUEUE_MAX)
        self._write_worker: Optional[threading.Thread] = None
        self._write_stop = threading.Event()
        self._drawer_map: Dict[str, str] = {}
        self._drawer_map_lock = threading.Lock()
        self._pinned_facts_cache: List[Dict[str, Any]] = []
        self._pinned_facts_fetched_at: float = 0.0
        self._pinned_facts_lock = threading.Lock()
        self._is_healthy = True
        self._last_health_at: float = 0.0

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
                continue
            self._process_write(item)
            self._write_queue.task_done()
        while True:
            try:
                item = self._write_queue.get_nowait()
            except queue.Empty:
                break
            self._process_write(item)
            self._write_queue.task_done()

    def _process_write(self, item: Dict[str, Any]) -> None:
        op = item.get("op")
        for attempt in range(_WRITE_RETRY_MAX):
            try:
                if op == "ingest":
                    result = self._post("/api/ingest", item["body"])
                    drawer_id = result.get("drawer_id", "") if isinstance(result, dict) else ""
                    if drawer_id and item.get("track_key"):
                        with self._drawer_map_lock:
                            self._drawer_map[item["track_key"]] = drawer_id
                elif op == "delete":
                    self._post("/api/delete", item["body"])
                    if item.get("track_key"):
                        with self._drawer_map_lock:
                            self._drawer_map.pop(item["track_key"], None)
                self._record_success()
                self._update_health(True)
                return
            except Exception as exc:
                if "HTTP Error 4" in str(exc):
                    logger.warning("mempal write rejected (4xx): %s", exc)
                    self._record_failure()
                    return
                if attempt < _WRITE_RETRY_MAX - 1:
                    time.sleep(_WRITE_RETRY_DELAY)
                else:
                    logger.warning("mempal write failed after %d retries: %s", _WRITE_RETRY_MAX, exc)
                    self._record_failure()
                    self._update_health(False)

    def _enqueue_write(self, item: Dict[str, Any]) -> bool:
        self._start_write_worker()
        try:
            self._write_queue.put_nowait(item)
            return True
        except queue.Full:
            logger.warning("mempal write queue full (%d), dropping write", _WRITE_QUEUE_MAX)
            return False

    def _update_health(self, healthy: bool) -> None:
        self._is_healthy = healthy
        self._last_health_at = time.monotonic()

    @property
    def name(self) -> str:
        return "mempal"

    def is_available(self) -> bool:
        cfg = _load_config(self._hermes_home)
        if not cfg.get("base_url"):
            return False
        if time.monotonic() - self._last_health_at < _HEALTH_CHECK_INTERVAL:
            return self._is_healthy
        return True

    def initialize(self, session_id: str, **kwargs) -> None:
        self._configure_scope(session_id, kwargs, preserve_existing=False)
        self._start_write_worker()

    def _configure_scope(self, session_id, context, *, preserve_existing):
        self._session_id = session_id
        if "hermes_home" in context or not preserve_existing:
            self._hermes_home = str(context.get("hermes_home", ""))
        cfg = _load_config(self._hermes_home)
        self._base_url = cfg["base_url"].rstrip("/")
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
        try:
            status = self._get("/api/status")
            mode = status.get("turn_storage", {}).get("storage_mode", "").strip()
            if mode:
                return mode
        except Exception as exc:
            logger.debug("mempal status lookup for turn storage failed: %s", exc)
        cfg = _load_config(self._hermes_home)
        return str(cfg.get("turn_storage_mode", "raw_evidence")).strip() or "raw_evidence"

    def _is_breaker_open(self):
        if self._consecutive_failures < _BREAKER_THRESHOLD:
            return False
        if time.monotonic() >= self._breaker_open_until:
            self._consecutive_failures = 0
            return False
        return True

    def _record_success(self):
        self._consecutive_failures = 0

    def _record_failure(self):
        self._consecutive_failures += 1
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            self._breaker_open_until = time.monotonic() + _BREAKER_COOLDOWN_SECS
            logger.warning("mempal circuit breaker tripped after %d failures. Pausing %ds.", self._consecutive_failures, _BREAKER_COOLDOWN_SECS)

    def _get(self, path, params=None):
        import urllib.parse, urllib.request
        url = self._base_url + path
        if params:
            url += "?" + urllib.parse.urlencode({k: v for k, v in params.items() if v is not None})
        with urllib.request.urlopen(url, timeout=10) as resp:
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
        for f in facts:
            kind = f.get("memory_kind", "fact")
            imp = f.get("importance", 0)
            content = f.get("content", "").replace("\n", " ")[:200]
            lines.append(f"- [{kind}] {content} (importance: {imp})")
        return "## Pinned Facts (always active)\n" + "\n".join(lines)

    def system_prompt_block(self):
        base = (
            "# Mempal Memory\n"
            f"Active. User: {self._user_id}. Profile: {self._profile}. "
            "Local BM25+vector backend, zero cloud.\n"
            "Use mempal_search to recall facts, mempal_conclude to store, "
            "mempal_profile for recent overview."
        )
        pinned = self._fetch_pinned_facts()
        pinned_block = self._format_pinned_block(pinned)
        if pinned_block:
            return f"{base}\n\n{pinned_block}"
        return base

    def prefetch(self, query, *, session_id=""):
        if self._prefetch_thread and self._prefetch_thread.is_alive():
            self._prefetch_thread.join(timeout=3.0)
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
        generation = self._prefetch_generation
        params = self._with_project_id({"q": query, "wing": wing, "top_k": 5, "include_raw_turns": False})
        def _run():
            try:
                results = self._get("/api/search", params)
                if results:
                    lines = [r.get("content", "") for r in results if r.get("content")]
                    with self._prefetch_lock:
                        if generation == self._prefetch_generation:
                            result = "\n".join(f"- {line}" for line in lines)
                            self._prefetch_results[key] = result
                            if key == self._session_id:
                                self._prefetch_result = result
                self._record_success()
            except Exception as exc:
                self._record_failure()
                logger.debug("mempal prefetch failed: %s", exc)
        self._prefetch_thread = threading.Thread(target=_run, daemon=True, name="mempal-prefetch")
        self._prefetch_thread.start()

    def sync_turn(self, user_content, assistant_content, *, session_id=""):
        if self._is_breaker_open():
            return
        if self._turn_storage_mode() != "raw_evidence":
            return
        content = f"User: {user_content}\nAssistant: {assistant_content}"
        body = self._with_project_id({"content": content, "wing": self._wing, "room": self._turns_room})
        self._enqueue_write({"op": "ingest", "body": body})

    def get_tool_schemas(self):
        return [PROFILE_SCHEMA, SEARCH_SCHEMA, CONCLUDE_SCHEMA]

    def handle_tool_call(self, tool_name, args, **kwargs):
        if self._is_breaker_open():
            return json.dumps({"error": "mempal temporarily unavailable. Will retry automatically."})
        if tool_name == "mempal_profile":
            try:
                try:
                    limit = min(int(args.get("limit", 20)), 100)
                except (ValueError, TypeError):
                    limit = 20
                entries = self._get("/api/timeline", self._with_project_id({"wing": self._wing, "limit": limit, "include_raw_turns": False}))
                self._record_success()
                if not entries:
                    return json.dumps({"result": "No memories stored yet."})
                items = [_strip_none({"content": e.get("content", ""), "drawer_id": e.get("drawer_id"), "importance": e.get("importance"), "added_at": e.get("added_at")}) for e in entries if e.get("content")]
                return json.dumps({"results": items, "count": len(items)})
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Failed to fetch profile: {exc}"})
        elif tool_name == "mempal_search":
            q = args.get("query", "")
            if not q:
                return json.dumps({"error": "Missing required parameter: query"})
            try:
                top_k = min(int(args.get("top_k", 10)), 50)
            except (ValueError, TypeError):
                top_k = 10
            try:
                results = self._get("/api/search", self._with_project_id({"q": q, "wing": self._wing, "top_k": top_k, "include_raw_turns": False}))
                self._record_success()
                if not results:
                    return json.dumps({"result": "No relevant memories found."})
                items = [_strip_none({"memory": r.get("content", ""), "score": r.get("similarity", 0), "drawer_id": r.get("drawer_id"), "source": r.get("source"), "source_type": r.get("source_type"), "provenance": r.get("provenance"), "status": r.get("status"), "memory_kind": r.get("memory_kind"), "domain": r.get("domain"), "field": r.get("field"), "importance": r.get("importance"), "is_pinned": r.get("is_pinned"), "confidence": r.get("confidence")}) for r in results]
                return json.dumps({"results": items, "count": len(items)})
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Search failed: {exc}"})
        elif tool_name == "mempal_conclude":
            conclusion = args.get("conclusion", "")
            if not conclusion:
                return json.dumps({"error": "Missing required parameter: conclusion"})
            try:
                result = self._post("/api/ingest", self._with_project_id({"content": conclusion, "wing": self._wing, "room": self._facts_room}))
                drawer_id = result.get("drawer_id", "") if isinstance(result, dict) else ""
                self._record_success()
                resp = {"result": "Fact stored."}
                if drawer_id:
                    resp["drawer_id"] = drawer_id
                return json.dumps(resp)
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Failed to store: {exc}"})
        return json.dumps({"error": f"Unknown tool: {tool_name}"})

    def on_session_end(self, messages):
        if self._is_breaker_open():
            return
        assistant_msgs = [m.get("content", "") for m in messages if m.get("role") == "assistant" and m.get("content")]
        if not assistant_msgs:
            return
        summary = assistant_msgs[-1][:2000]
        body = self._with_project_id({"content": f"[Session summary] {summary}", "wing": self._wing, "room": self._session_room()})
        self._enqueue_write({"op": "ingest", "body": body})

    def on_memory_write(self, action, target, content, metadata=None):
        if self._is_breaker_open():
            return
        track_key = f"{target}:{self._wing}"
        room = self._memory_room_for_target(target)
        if action == "remove":
            with self._drawer_map_lock:
                drawer_id = self._drawer_map.get(track_key)
            if drawer_id:
                self._enqueue_write({"op": "delete", "body": self._with_project_id({"drawer_id": drawer_id}), "track_key": track_key})
            else:
                logger.debug("mempal remove: no tracked drawer_id for %s", target)
            return
        body = self._with_project_id({"content": content, "wing": self._wing, "room": room})
        if metadata:
            for field in ("memory_kind", "domain", "field", "importance", "status", "is_pinned", "source_type"):
                if field in metadata and metadata[field] is not None:
                    body[field] = metadata[field]
        if action == "replace":
            with self._drawer_map_lock:
                old_drawer_id = self._drawer_map.get(track_key)
            if old_drawer_id:
                body["supersedes"] = old_drawer_id
        self._enqueue_write({"op": "ingest", "body": body, "track_key": track_key})

    def on_session_switch(self, new_session_id, reason="", **kwargs):
        del reason
        with self._prefetch_lock:
            self._prefetch_result = ""
            self._prefetch_results.clear()
            self._prefetch_generation += 1
        with self._pinned_facts_lock:
            self._pinned_facts_fetched_at = 0.0
        self._configure_scope(new_session_id, kwargs, preserve_existing=True)

    def shutdown(self):
        self._write_stop.set()
        if self._prefetch_thread and self._prefetch_thread.is_alive():
            self._prefetch_thread.join(timeout=3.0)
        if self._write_worker and self._write_worker.is_alive():
            try:
                self._write_queue.join()
            except Exception:
                pass
            self._write_worker.join(timeout=_WRITE_DRAIN_TIMEOUT)
            if self._write_worker.is_alive():
                logger.warning("mempal write worker did not drain in time")

    def get_config_schema(self):
        return [{"key": "base_url", "description": "mempal REST endpoint", "default": "http://127.0.0.1:3080", "env_var": "MEMPAL_BASE_URL"}, {"key": "user_id", "description": "User identifier for memory scoping", "default": "hermes-user", "env_var": "MEMPAL_USER_ID"}, {"key": "turn_storage_mode", "description": "Raw turn storage mode: off, raw_evidence, or summarized", "default": "raw_evidence", "env_var": "MEMPAL_TURN_STORAGE_MODE"}]

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

"""Mempal memory plugin for hermes-agent.

Local memory backend via mempal REST API.
BM25 + vector hybrid search, zero cloud dependency.

Config via environment variables:
  MEMPAL_BASE_URL  — mempal REST endpoint (default: http://127.0.0.1:3080)
  MEMPAL_USER_ID   — user identifier (default: hermes-user)

Or via $HERMES_HOME/mempal.json.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import time
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

# Circuit breaker: pause API calls after this many consecutive failures
_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120

# Tool schemas
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
        "Returns relevant facts ranked by similarity."
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
    """Load config from env vars with $HERMES_HOME/mempal.json overrides."""
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


class MempalMemoryProvider:
    """Mempal local memory backend implementing the MemoryProvider interface."""

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
        self._sync_thread: Optional[threading.Thread] = None
        self._session_end_thread: Optional[threading.Thread] = None
        self._mirror_thread: Optional[threading.Thread] = None
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0

    @property
    def name(self) -> str:
        return "mempal"

    def is_available(self) -> bool:
        """Check if mempal REST endpoint is reachable."""
        try:
            import urllib.request
            cfg = _load_config(self._hermes_home)
            url = cfg["base_url"] + "/api/status"
            with urllib.request.urlopen(url, timeout=3) as resp:
                return resp.status == 200
        except Exception:
            return False

    def initialize(self, session_id: str, **kwargs) -> None:
        self._configure_scope(session_id, kwargs, preserve_existing=False)

    def _configure_scope(
        self,
        session_id: str,
        context: Dict[str, Any],
        *,
        preserve_existing: bool,
    ) -> None:
        self._session_id = session_id
        if "hermes_home" in context or not preserve_existing:
            self._hermes_home = str(context.get("hermes_home", ""))

        cfg = _load_config(self._hermes_home)
        self._base_url = cfg["base_url"].rstrip("/")

        user_id = context.get("user_id") or (
            self._user_id if preserve_existing else cfg.get("user_id", "hermes-user")
        )
        profile = (
            context.get("agent_identity")
            or context.get("profile")
            or (self._profile if preserve_existing else "default")
        )
        platform = context.get("platform") or (self._platform if preserve_existing else "cli")

        if "chat_id" in context:
            chat_id = str(context.get("chat_id") or "")
        else:
            chat_id = self._chat_id if preserve_existing else ""
        if "thread_id" in context:
            thread_id = str(context.get("thread_id") or "")
        else:
            thread_id = self._thread_id if preserve_existing else ""

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
    def _derive_turns_room(platform: str, chat_id: str, thread_id: str) -> str:
        if not chat_id:
            return "turns"
        if thread_id:
            return f"turns/{platform}/{chat_id}/{thread_id}"
        return f"turns/{platform}/{chat_id}"

    def _session_key(self, session_id: str = "") -> str:
        return session_id or self._session_id

    def _with_project_id(self, payload: Dict[str, Any]) -> Dict[str, Any]:
        scoped = dict(payload)
        if self._project_id:
            scoped["project_id"] = self._project_id
        return scoped

    def _session_room(self) -> str:
        if self._session_id:
            return f"sessions/{self._session_id}"
        return "sessions"

    def _memory_room_for_target(self, target: str) -> str:
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

    def _turn_storage_mode(self) -> str:
        try:
            status = self._get("/api/status")
            mode = (
                status.get("turn_storage", {})
                .get("storage_mode", "")
                .strip()
            )
            if mode:
                return mode
        except Exception as exc:
            logger.debug("mempal status lookup for turn storage failed: %s", exc)
        cfg = _load_config(self._hermes_home)
        return str(cfg.get("turn_storage_mode", "raw_evidence")).strip() or "raw_evidence"

    # -- Circuit breaker ----------------------------------------------------

    def _is_breaker_open(self) -> bool:
        if self._consecutive_failures < _BREAKER_THRESHOLD:
            return False
        if time.monotonic() >= self._breaker_open_until:
            self._consecutive_failures = 0
            return False
        return True

    def _record_success(self) -> None:
        self._consecutive_failures = 0

    def _record_failure(self) -> None:
        self._consecutive_failures += 1
        if self._consecutive_failures >= _BREAKER_THRESHOLD:
            self._breaker_open_until = time.monotonic() + _BREAKER_COOLDOWN_SECS
            logger.warning(
                "mempal circuit breaker tripped after %d failures. Pausing %ds.",
                self._consecutive_failures,
                _BREAKER_COOLDOWN_SECS,
            )

    # -- HTTP helpers -------------------------------------------------------

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        import urllib.request
        import urllib.parse

        url = self._base_url + path
        if params:
            url += "?" + urllib.parse.urlencode(
                {k: v for k, v in params.items() if v is not None}
            )
        with urllib.request.urlopen(url, timeout=10) as resp:
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

    # -- MemoryProvider interface --------------------------------------------

    def system_prompt_block(self) -> str:
        return (
            "# Mempal Memory\n"
            f"Active. User: {self._user_id}. Profile: {self._profile}. "
            "Local BM25+vector backend, zero cloud.\n"
            "Use mempal_search to recall facts, mempal_conclude to store, "
            "mempal_profile for recent overview."
        )

    def prefetch(self, query: str, *, session_id: str = "") -> str:
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

    def queue_prefetch(self, query: str, *, session_id: str = "") -> None:
        if self._is_breaker_open():
            return
        key = self._session_key(session_id)
        wing = self._wing
        generation = self._prefetch_generation
        params = self._with_project_id({
            "q": query,
            "wing": wing,
            "top_k": 5,
            "include_raw_turns": False,
        })

        def _run():
            try:
                results = self._get("/api/search", params)
                if results:
                    lines = [r.get("content", "") for r in results if r.get("content")]
                    with self._prefetch_lock:
                        if generation == self._prefetch_generation:
                            result = "\n".join(f"- {l}" for l in lines)
                            self._prefetch_results[key] = result
                            if key == self._session_id:
                                self._prefetch_result = result
                self._record_success()
            except Exception as exc:
                self._record_failure()
                logger.debug("mempal prefetch failed: %s", exc)

        self._prefetch_thread = threading.Thread(
            target=_run, daemon=True, name="mempal-prefetch"
        )
        self._prefetch_thread.start()

    def sync_turn(
        self, user_content: str, assistant_content: str, *, session_id: str = ""
    ) -> None:
        """Ingest turn summary to mempal (non-blocking)."""
        if self._is_breaker_open():
            return
        if self._turn_storage_mode() != "raw_evidence":
            return

        content = f"User: {user_content}\nAssistant: {assistant_content}"
        body = self._with_project_id({
            "content": content,
            "wing": self._wing,
            "room": self._turns_room,
        })

        def _sync():
            try:
                self._post("/api/ingest", body)
                self._record_success()
            except Exception as exc:
                self._record_failure()
                logger.warning("mempal sync_turn failed: %s", exc)

        if self._sync_thread and self._sync_thread.is_alive():
            self._sync_thread.join(timeout=5.0)
        self._sync_thread = threading.Thread(
            target=_sync, daemon=True, name="mempal-sync"
        )
        self._sync_thread.start()

    def get_tool_schemas(self) -> List[Dict[str, Any]]:
        return [PROFILE_SCHEMA, SEARCH_SCHEMA, CONCLUDE_SCHEMA]

    def handle_tool_call(self, tool_name: str, args: Dict[str, Any], **kwargs) -> str:
        if self._is_breaker_open():
            return json.dumps(
                {
                    "error": (
                        "mempal temporarily unavailable (too many failures). "
                        "Will retry automatically."
                    )
                }
            )

        if tool_name == "mempal_profile":
            try:
                try:
                    limit = min(int(args.get("limit", 20)), 100)
                except (ValueError, TypeError):
                    limit = 20
                entries = self._get(
                    "/api/timeline",
                    self._with_project_id({
                        "wing": self._wing,
                        "limit": limit,
                        "include_raw_turns": False,
                    }),
                )
                self._record_success()
                if not entries:
                    return json.dumps({"result": "No memories stored yet."})
                lines = [e.get("content", "") for e in entries if e.get("content")]
                return json.dumps({"result": "\n".join(f"- {l}" for l in lines), "count": len(lines)})
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Failed to fetch profile: {exc}"})

        elif tool_name == "mempal_search":
            query = args.get("query", "")
            if not query:
                return json.dumps({"error": "Missing required parameter: query"})
            try:
                top_k = min(int(args.get("top_k", 10)), 50)
            except (ValueError, TypeError):
                top_k = 10
            try:
                results = self._get(
                    "/api/search",
                    self._with_project_id({
                        "q": query,
                        "wing": self._wing,
                        "top_k": top_k,
                        "include_raw_turns": False,
                    }),
                )
                self._record_success()
                if not results:
                    return json.dumps({"result": "No relevant memories found."})
                items = [
                    {"memory": r.get("content", ""), "score": r.get("similarity", 0)}
                    for r in results
                ]
                return json.dumps({"results": items, "count": len(items)})
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Search failed: {exc}"})

        elif tool_name == "mempal_conclude":
            conclusion = args.get("conclusion", "")
            if not conclusion:
                return json.dumps({"error": "Missing required parameter: conclusion"})
            try:
                self._post(
                    "/api/ingest",
                    self._with_project_id({
                        "content": conclusion,
                        "wing": self._wing,
                        "room": self._facts_room,
                    }),
                )
                self._record_success()
                return json.dumps({"result": "Fact stored."})
            except Exception as exc:
                self._record_failure()
                return json.dumps({"error": f"Failed to store: {exc}"})

        return json.dumps({"error": f"Unknown tool: {tool_name}"})

    def on_session_end(self, messages: List[Dict[str, Any]]) -> None:
        """Ingest a session summary when the session ends."""
        if self._is_breaker_open():
            return
        assistant_msgs = [
            m.get("content", "")
            for m in messages
            if m.get("role") == "assistant" and m.get("content")
        ]
        if not assistant_msgs:
            return
        summary = assistant_msgs[-1][:2000]
        body = self._with_project_id({
            "content": f"[Session summary] {summary}",
            "wing": self._wing,
            "room": self._session_room(),
        })

        def _ingest():
            try:
                self._post("/api/ingest", body)
                self._record_success()
            except Exception as exc:
                self._record_failure()
                logger.warning("mempal on_session_end failed: %s", exc)

        self._session_end_thread = threading.Thread(
            target=_ingest, daemon=True, name="mempal-session-end"
        )
        self._session_end_thread.start()

    def on_memory_write(
        self,
        action: str,
        target: str,
        content: str,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> None:
        """Mirror built-in memory writes to mempal."""
        if self._is_breaker_open() or action == "remove":
            return
        body = self._with_project_id({
            "content": content,
            "wing": self._wing,
            "room": self._memory_room_for_target(target),
        })

        def _mirror():
            try:
                self._post("/api/ingest", body)
                self._record_success()
            except Exception as exc:
                self._record_failure()
                logger.debug("mempal on_memory_write failed: %s", exc)

        self._mirror_thread = threading.Thread(
            target=_mirror, daemon=True, name="mempal-mirror"
        )
        self._mirror_thread.start()

    def on_session_switch(self, new_session_id: str, reason: str = "", **kwargs) -> None:
        """Handle reset/new/resume/branch/compression continuation events."""
        del reason
        with self._prefetch_lock:
            self._prefetch_result = ""
            self._prefetch_results.clear()
            self._prefetch_generation += 1

        for thread in (self._sync_thread, self._session_end_thread, self._mirror_thread):
            if thread and thread.is_alive():
                thread.join(timeout=5.0)

        self._configure_scope(new_session_id, kwargs, preserve_existing=True)

    def shutdown(self) -> None:
        for t in (
            self._prefetch_thread,
            self._sync_thread,
            self._session_end_thread,
            self._mirror_thread,
        ):
            if t and t.is_alive():
                t.join(timeout=5.0)

    def get_config_schema(self) -> List[Dict[str, Any]]:
        return [
            {
                "key": "base_url",
                "description": "mempal REST endpoint",
                "default": "http://127.0.0.1:3080",
                "env_var": "MEMPAL_BASE_URL",
            },
            {
                "key": "user_id",
                "description": "User identifier for memory scoping",
                "default": "hermes-user",
                "env_var": "MEMPAL_USER_ID",
            },
            {
                "key": "turn_storage_mode",
                "description": "Raw turn storage mode: off, raw_evidence, or summarized",
                "default": "raw_evidence",
                "env_var": "MEMPAL_TURN_STORAGE_MODE",
            },
        ]

    def save_config(self, values: Dict[str, Any], hermes_home: str) -> None:
        config_path = os.path.join(hermes_home, "mempal.json")
        existing: dict = {}
        if os.path.exists(config_path):
            try:
                existing = json.loads(open(config_path, encoding="utf-8").read())
            except Exception:
                pass
        existing.update(values)
        with open(config_path, "w", encoding="utf-8") as f:
            json.dump(existing, f, indent=2)


def register(ctx) -> None:
    """Register mempal as a memory provider plugin."""
    ctx.register_memory_provider(MempalMemoryProvider())

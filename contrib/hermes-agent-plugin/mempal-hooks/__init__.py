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
import logging
import os
import threading
import time
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

_CONTEXT_TOP_K = 8
_OBSERVE_MIN_RESULT_LEN = 50
_OBSERVE_MAX_CONTENT_LEN = 2000
_BREAKER_THRESHOLD = 5
_BREAKER_COOLDOWN_SECS = 120
_OBSERVE_TOOL_ALLOWLIST = {
    "bash", "shell", "run_command", "execute",
    "web_search", "search", "browse",
    "read_file", "write_file",
    "python", "code_interpreter",
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


class _MempalHooks:
    def __init__(self) -> None:
        self._base_url = "http://127.0.0.1:3080"
        self._user_id = "hermes-user"
        self._wing = "hermes-user/hermes-user/default"
        self._hermes_home = ""
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0
        self._initialized = False
        self._init_lock = threading.Lock()

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
            self._user_id = cfg.get("user_id", "hermes-user")
            self._wing = f"hermes-user/{self._user_id}/default"
            self._initialized = True

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
                "mempal-hooks breaker tripped after %d failures, pausing %ds",
                self._consecutive_failures, _BREAKER_COOLDOWN_SECS,
            )

    def _get(self, path: str, params: Optional[Dict[str, Any]] = None) -> Any:
        import urllib.parse
        import urllib.request

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
            results = self._get(
                "/api/search",
                {
                    "q": user_message,
                    "wing": self._wing,
                    "top_k": _CONTEXT_TOP_K,
                    "include_raw_turns": False,
                },
            )
            self._record_success()
        except Exception as exc:
            self._record_failure()
            logger.debug("mempal-hooks pre_llm_call search failed: %s", exc)
            return None

        if not results:
            return None

        lines: List[str] = []
        for r in results:
            content = (r.get("content") or "").strip()
            if not content:
                continue
            kind = r.get("memory_kind", "")
            importance = r.get("importance")
            prefix = f"[{kind}]" if kind else ""
            suffix = f"(importance:{importance})" if importance else ""
            lines.append(f"- {prefix} {content} {suffix}".strip())

        if not lines:
            return None

        block = (
            "## Relevant memories (mempal)\n"
            + "\n".join(lines)
        )
        return {"context": block}

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


def register(ctx):
    hooks = _MempalHooks()
    ctx.register_hook("pre_llm_call", hooks.pre_llm_call)
    ctx.register_hook("post_tool_call", hooks.post_tool_call)
    ctx.register_hook("on_session_start", hooks.on_session_start)

"""Optional LLM-backed intelligence enhancement for the mempal provider."""

from __future__ import annotations

import json
import logging
import re
import time
from typing import Any, Dict, List, Optional


logger = logging.getLogger(__package__ or "mempal")

_LLM_DEFAULT_TIMEOUT = 30
_LLM_BREAKER_THRESHOLD = 3
_LLM_BREAKER_COOLDOWN = 300.0
_VALID_MEMORY_KINDS = {
    "fact", "preference", "decision", "correction", "rule",
    "observation", "summary", "context", "goal", "constraint",
}
_VALID_DOMAINS = {
    "coding", "communication", "workflow", "architecture",
    "debugging", "testing", "deployment", "personal", "project",
}


class _LLMClient:
    """OpenAI-compatible chat completion client using stdlib only."""

    def __init__(self, cfg: Dict[str, Any]) -> None:
        self._base_url = (cfg.get("base_url") or "").rstrip("/")
        self._model = cfg.get("model") or ""
        self._api_key = cfg.get("api_key") or ""
        self._timeout = int(cfg.get("timeout_secs") or _LLM_DEFAULT_TIMEOUT)
        self._extra_body = cfg.get("extra_body") or {}
        self._consecutive_failures = 0
        self._breaker_open_until = 0.0

    @property
    def is_configured(self) -> bool:
        return bool(self._base_url and self._model)

    def _is_breaker_open(self) -> bool:
        if self._consecutive_failures < _LLM_BREAKER_THRESHOLD:
            return False
        if time.monotonic() >= self._breaker_open_until:
            self._consecutive_failures = 0
            return False
        return True

    def chat(self, system: str, user: str, *, temperature: float = 0.1) -> Optional[str]:
        if not self.is_configured or self._is_breaker_open():
            return None
        import urllib.request
        body: Dict[str, Any] = {
            "model": self._model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "temperature": temperature,
            "max_tokens": 1024,
        }
        body.update(self._extra_body)
        headers = {"Content-Type": "application/json"}
        if self._api_key:
            headers["Authorization"] = f"Bearer {self._api_key}"
        url = self._base_url + "/chat/completions"
        req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                result = json.loads(resp.read().decode())
            content = result.get("choices", [{}])[0].get("message", {}).get("content")
            if content:
                self._consecutive_failures = 0
            return content or None
        except Exception as exc:
            self._consecutive_failures += 1
            if self._consecutive_failures >= _LLM_BREAKER_THRESHOLD:
                self._breaker_open_until = time.monotonic() + _LLM_BREAKER_COOLDOWN
                logger.warning("mempal LLM breaker tripped after %d failures, pausing %ds", self._consecutive_failures, _LLM_BREAKER_COOLDOWN)
            else:
                logger.debug("mempal LLM call failed: %s", exc)
            return None

    @property
    def status(self) -> str:
        if not self.is_configured:
            return "not_configured"
        if self._is_breaker_open():
            return "breaker_open"
        return "available"


class _IntelligenceEnhancer:
    """Enhancement pipeline: LLM-powered metadata extraction with deterministic gates."""

    _METADATA_SYSTEM = (
        "You classify memory entries. Return ONLY a JSON object with these fields:\n"
        '  "memory_kind": one of: fact, preference, decision, correction, rule, '
        "observation, summary, context, goal, constraint\n"
        '  "domain": one of: coding, communication, workflow, architecture, '
        "debugging, testing, deployment, personal, project\n"
        '  "importance": integer 1-5 (1=trivial, 5=critical)\n'
        '  "tags": list of 1-3 short keyword tags\n'
        "Return ONLY valid JSON, no explanation."
    )

    _FACTS_SYSTEM = (
        "Extract durable facts from this conversation turn. Return a JSON array of objects, "
        "each with:\n"
        '  "fact": the extracted fact as a concise statement\n'
        '  "memory_kind": one of: fact, preference, decision, correction, rule, '
        "observation, context, goal, constraint\n"
        '  "importance": integer 1-5\n'
        "Only extract facts worth remembering long-term. If nothing is worth extracting, "
        "return an empty array []. Return ONLY valid JSON."
    )

    _SUMMARY_SYSTEM = (
        "Summarize this conversation for long-term memory. Focus on:\n"
        "- Key decisions made\n"
        "- User preferences discovered\n"
        "- Important context established\n"
        "Keep it concise (under 500 characters). Return ONLY the summary text, no JSON."
    )

    def __init__(self, llm: _LLMClient) -> None:
        self._llm = llm

    def extract_metadata(self, content: str) -> Optional[Dict[str, Any]]:
        raw = self._llm.chat(self._METADATA_SYSTEM, content)
        if not raw:
            return None
        return self._validate_metadata(raw)

    def extract_facts(self, turn_content: str) -> Optional[List[Dict[str, Any]]]:
        if len(turn_content) < 50:
            return None
        raw = self._llm.chat(self._FACTS_SYSTEM, turn_content)
        if not raw:
            return None
        return self._validate_facts(raw, turn_content)

    def enhance_summary(self, messages_text: str) -> Optional[str]:
        raw = self._llm.chat(self._SUMMARY_SYSTEM, messages_text)
        if not raw or len(raw) < 10:
            return None
        if len(raw) > 2000:
            return raw[:2000]
        return raw

    @staticmethod
    def _validate_metadata(raw: str) -> Optional[Dict[str, Any]]:
        raw = raw.strip()
        if raw.startswith("```"):
            raw = re.sub(r"^```(?:json)?\s*", "", raw)
            raw = re.sub(r"\s*```$", "", raw)
        try:
            parsed = json.loads(raw)
        except (json.JSONDecodeError, ValueError):
            return None
        if not isinstance(parsed, dict):
            return None
        result: Dict[str, Any] = {}
        kind = str(parsed.get("memory_kind", "")).strip().lower()
        if kind in _VALID_MEMORY_KINDS:
            result["memory_kind"] = kind
        domain = str(parsed.get("domain", "")).strip().lower()
        if domain in _VALID_DOMAINS:
            result["domain"] = domain
        try:
            importance = int(parsed.get("importance", 0))
            if 1 <= importance <= 5:
                result["importance"] = importance
        except (ValueError, TypeError):
            pass
        tags = parsed.get("tags")
        if isinstance(tags, list):
            clean = [str(t).strip()[:30] for t in tags[:5] if isinstance(t, str) and t.strip()]
            if clean:
                result["tags"] = clean
        return result if result else None

    @staticmethod
    def _validate_facts(raw: str, source_content: str) -> Optional[List[Dict[str, Any]]]:
        raw = raw.strip()
        if raw.startswith("```"):
            raw = re.sub(r"^```(?:json)?\s*", "", raw)
            raw = re.sub(r"\s*```$", "", raw)
        try:
            parsed = json.loads(raw)
        except (json.JSONDecodeError, ValueError):
            return None
        if not isinstance(parsed, list):
            return None
        source_lower = source_content.lower()
        validated: List[Dict[str, Any]] = []
        for item in parsed[:10]:
            if not isinstance(item, dict):
                continue
            fact = str(item.get("fact", "")).strip()
            if not fact or len(fact) < 5:
                continue
            words = fact.lower().split()
            grounded = sum(1 for w in words if len(w) > 3 and w in source_lower)
            if grounded < min(2, len(words) // 3):
                continue
            entry: Dict[str, Any] = {"fact": fact}
            kind = str(item.get("memory_kind", "fact")).strip().lower()
            entry["memory_kind"] = kind if kind in _VALID_MEMORY_KINDS else "fact"
            try:
                imp = int(item.get("importance", 2))
                entry["importance"] = max(1, min(5, imp))
            except (ValueError, TypeError):
                entry["importance"] = 2
            validated.append(entry)
        return validated if validated else None

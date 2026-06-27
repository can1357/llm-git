"""Token counting with an async API attempt and character fallback."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any
from urllib.parse import urljoin

import httpx


@dataclass(slots=True)
class TokenCounter:
    """Count prompt tokens through an Anthropic-compatible endpoint when possible."""

    api_base_url: str
    api_key: str | None
    model: str
    timeout: float = 10.0

    @classmethod
    def new(cls, api_base_url: str, api_key: str | None, model: str) -> TokenCounter:
        """Create a token counter from raw API settings."""

        return cls(api_base_url=api_base_url, api_key=api_key, model=model)

    async def count(self, text: str) -> int:
        """Count tokens asynchronously, falling back to a 4-character estimate."""

        api_count = await self._try_api_count(text)
        return api_count if api_count is not None else self.count_sync(text)

    def count_sync(self, text: str) -> int:
        """Return a deterministic local estimate of tokens in ``text``."""

        return len(text) // 4

    async def _try_api_count(self, text: str) -> int | None:
        if not self.api_key or _is_openai_base_url(self.api_base_url):
            return None
        url = urljoin(self.api_base_url.rstrip("/") + "/", "messages/count_tokens")
        headers = {
            "x-api-key": self.api_key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
            "authorization": f"Bearer {self.api_key}",
        }
        payload = {"model": self.model, "messages": [{"role": "user", "content": text}]}
        try:
            async with httpx.AsyncClient(timeout=self.timeout) as client:
                response = await client.post(url, headers=headers, json=payload)
                response.raise_for_status()
                body = response.json()
        except Exception:
            return None
        return _extract_token_count(body)


def create_token_counter(config: object) -> TokenCounter:
    """Create a ``TokenCounter`` from lgit configuration fields."""

    return TokenCounter(
        api_base_url=str(getattr(config, "api_base_url", "http://localhost:4000")),
        api_key=getattr(config, "api_key", None),
        model=str(getattr(config, "analysis_model", getattr(config, "model", "claude-opus-4.5"))),
        timeout=float(getattr(config, "connect_timeout_secs", 10) or 10),
    )


def _is_openai_base_url(url: str) -> bool:
    lowered = url.lower()
    return "openai.com" in lowered or "api.openai" in lowered


def _extract_token_count(body: Any) -> int | None:
    if not isinstance(body, dict):
        return None
    for key in ("input_tokens", "tokens", "token_count"):
        value = body.get(key)
        if isinstance(value, int) and value >= 0:
            return value
    usage = body.get("usage")
    if isinstance(usage, dict):
        value = usage.get("input_tokens") or usage.get("prompt_tokens")
        if isinstance(value, int) and value >= 0:
            return value
    return None


__all__ = ["TokenCounter", "create_token_counter"]

"""Model token pricing, usage parsing, and per-run LLM spend accounting.

Rates live in ``lgit/resources/model_pricing.json`` (USD per million tokens,
substring-matched against the model name). ``lgit.api`` records every real API
response here; the CLI reads the session totals to print spend on completion.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from functools import lru_cache
from importlib import resources
from typing import Any


@dataclass(frozen=True, slots=True)
class TokenUsage:
    """Token counts for one LLM response, normalized across API shapes.

    ``input_tokens`` counts only fresh (uncached) input; cache reads and
    writes are tracked separately because they bill at their own rates.
    """

    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_write_tokens: int = 0

    @property
    def total_tokens(self) -> int:
        """Total billed tokens across all categories."""
        return self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens

    def __add__(self, other: TokenUsage) -> TokenUsage:
        return TokenUsage(
            self.input_tokens + other.input_tokens,
            self.output_tokens + other.output_tokens,
            self.cache_read_tokens + other.cache_read_tokens,
            self.cache_write_tokens + other.cache_write_tokens,
        )


@dataclass(frozen=True, slots=True)
class ModelRates:
    """USD per million tokens for one model family."""

    input: float
    output: float
    cache_read: float
    cache_write: float


@dataclass(frozen=True, slots=True)
class SessionSpend:
    """Accumulated LLM usage and estimated spend for this process."""

    usage: TokenUsage
    cost_usd: float
    saved_usd: float
    unpriced_models: tuple[str, ...]


def parse_usage(payload: Mapping[str, Any]) -> TokenUsage | None:
    """Extract normalized token usage from an OpenAI or Anthropic response body.

    OpenAI reports cached tokens as a subset of ``prompt_tokens``; Anthropic
    reports cache reads/writes separately from ``input_tokens``. Both are
    normalized so ``input_tokens`` never double-counts cached input.
    """
    usage = payload.get("usage")
    if not isinstance(usage, Mapping):
        return None
    if "prompt_tokens" in usage or "completion_tokens" in usage:
        prompt = _as_int(usage.get("prompt_tokens"))
        details = usage.get("prompt_tokens_details")
        cached = _as_int(details.get("cached_tokens")) if isinstance(details, Mapping) else 0
        return TokenUsage(max(0, prompt - cached), _as_int(usage.get("completion_tokens")), cached, 0)
    if "input_tokens" in usage or "output_tokens" in usage:
        return TokenUsage(
            _as_int(usage.get("input_tokens")),
            _as_int(usage.get("output_tokens")),
            _as_int(usage.get("cache_read_input_tokens")),
            _as_int(usage.get("cache_creation_input_tokens")),
        )
    return None


def rates_for_model(model: str) -> ModelRates | None:
    """Return substring-matched rates for ``model`` from the bundled table, or None."""
    name = model.lower()
    for match, rates in _pricing_table():
        if match in name:
            return rates
    return None


def cost_usd(model: str, usage: TokenUsage) -> float | None:
    """Estimate the USD cost of one response, or None when the model is unpriced."""
    rates = rates_for_model(model)
    if rates is None:
        return None
    return (
        usage.input_tokens * rates.input
        + usage.output_tokens * rates.output
        + usage.cache_read_tokens * rates.cache_read
        + usage.cache_write_tokens * rates.cache_write
    ) / 1_000_000


def record_usage(model: str, usage: TokenUsage, reported_cost_usd: float | None = None) -> float | None:
    """Add one response's usage to the session totals; returns its cost when known.

    ``reported_cost_usd`` (e.g. from a LiteLLM ``x-litellm-response-cost``
    header) takes precedence over the bundled rate table.
    """
    cost = reported_cost_usd if reported_cost_usd is not None else cost_usd(model, usage)
    _SESSION.usage = _SESSION.usage + usage
    if cost is None:
        _SESSION.unpriced.add(model)
    else:
        _SESSION.cost_usd += cost
    return cost


def record_saved(cost_usd: float | None) -> None:
    """Credit a cache hit's stored original cost to the session's saved total."""
    _SESSION.saved_usd += cost_usd or 0.0


def session_spend() -> SessionSpend:
    """Snapshot the usage and spend recorded since process start."""
    return SessionSpend(_SESSION.usage, _SESSION.cost_usd, _SESSION.saved_usd, tuple(sorted(_SESSION.unpriced)))


def reset_session() -> None:
    """Clear session totals (test isolation)."""
    _SESSION.usage = TokenUsage()
    _SESSION.cost_usd = 0.0
    _SESSION.saved_usd = 0.0
    _SESSION.unpriced.clear()


class _Session:
    __slots__ = ("cost_usd", "saved_usd", "unpriced", "usage")

    def __init__(self) -> None:
        self.usage = TokenUsage()
        self.cost_usd = 0.0
        self.saved_usd = 0.0
        self.unpriced: set[str] = set()


_SESSION = _Session()


@lru_cache(maxsize=1)
def _pricing_table() -> tuple[tuple[str, ModelRates], ...]:
    resource = resources.files("lgit.resources").joinpath("model_pricing.json")
    data = json.loads(resource.read_text(encoding="utf-8"))
    entries: list[tuple[str, ModelRates]] = []
    for row in data.get("rates", ()):
        input_rate = float(row["input"])
        entries.append(
            (
                str(row["match"]).lower(),
                ModelRates(
                    input=input_rate,
                    output=float(row["output"]),
                    cache_read=float(row.get("cache_read", input_rate * 0.1)),
                    cache_write=float(row.get("cache_write", input_rate * 1.25)),
                ),
            )
        )
    return tuple(entries)


def _as_int(value: Any) -> int:
    try:
        return max(0, int(value))
    except TypeError, ValueError:
        return 0


__all__ = [
    "ModelRates",
    "SessionSpend",
    "TokenUsage",
    "cost_usd",
    "parse_usage",
    "rates_for_model",
    "record_saved",
    "record_usage",
    "reset_session",
    "session_spend",
]

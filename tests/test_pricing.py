from __future__ import annotations

import asyncio
import sqlite3
from pathlib import Path

import httpx
import pytest
from lgit import pricing
from lgit.api import OneShotSpec, _record_spend
from lgit.cache import LlmCache
from lgit.pricing import TokenUsage, cost_usd, parse_usage, rates_for_model


@pytest.fixture(autouse=True)
def _fresh_session() -> None:
    pricing.reset_session()


def _spec(model: str = "claude-haiku-4-5") -> OneShotSpec:
    return OneShotSpec(operation="test", model=model, system_prompt="s", user_prompt="u")


def test_parse_usage_openai_excludes_cached_from_input() -> None:
    usage = parse_usage(
        {
            "usage": {
                "prompt_tokens": 1_000,
                "completion_tokens": 200,
                "prompt_tokens_details": {"cached_tokens": 700},
            }
        }
    )
    assert usage == TokenUsage(input_tokens=300, output_tokens=200, cache_read_tokens=700)


def test_parse_usage_anthropic_shape() -> None:
    usage = parse_usage(
        {
            "usage": {
                "input_tokens": 400,
                "output_tokens": 100,
                "cache_read_input_tokens": 5_000,
                "cache_creation_input_tokens": 2_000,
            }
        }
    )
    assert usage == TokenUsage(400, 100, cache_read_tokens=5_000, cache_write_tokens=2_000)


def test_parse_usage_missing_or_malformed() -> None:
    assert parse_usage({}) is None
    assert parse_usage({"usage": "n/a"}) is None
    assert parse_usage({"usage": {"prompt_tokens": "bogus"}}) == TokenUsage()


def test_cost_uses_cache_rates_and_unknown_model_is_unpriced() -> None:
    usage = TokenUsage(input_tokens=1_000_000, output_tokens=1_000_000)
    assert cost_usd("claude-haiku-4-5-20251001", usage) == pytest.approx(6.0)

    cached = TokenUsage(cache_read_tokens=1_000_000, cache_write_tokens=1_000_000)
    rates = rates_for_model("claude-haiku-4-5")
    assert rates is not None
    assert cost_usd("claude-haiku-4-5", cached) == pytest.approx(0.1 + 1.25)

    assert cost_usd("mystery-model-9000", usage) is None


def test_record_usage_accumulates_and_tracks_unpriced() -> None:
    pricing.record_usage("claude-haiku-4-5", TokenUsage(input_tokens=1_000_000))
    pricing.record_usage("mystery-model-9000", TokenUsage(output_tokens=10))

    spend = pricing.session_spend()
    assert spend.cost_usd == pytest.approx(1.0)
    assert spend.usage == TokenUsage(input_tokens=1_000_000, output_tokens=10)
    assert spend.unpriced_models == ("mystery-model-9000",)


def test_record_spend_prefers_litellm_header_over_estimate() -> None:
    response = httpx.Response(
        200,
        headers={"x-litellm-response-cost": "0.123"},
        json={"usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}},
    )
    _record_spend(_spec(), response, response.text)

    spend = pricing.session_spend()
    assert spend.cost_usd == pytest.approx(0.123)
    assert spend.unpriced_models == ()


def test_record_spend_falls_back_to_rate_table() -> None:
    response = httpx.Response(200, json={"usage": {"prompt_tokens": 1_000_000, "completion_tokens": 0}})
    _record_spend(_spec(), response, response.text)

    assert pricing.session_spend().cost_usd == pytest.approx(1.0)


def test_record_spend_ignores_bodies_without_usage() -> None:
    response = httpx.Response(200, text="not json")
    _record_spend(_spec(), response, response.text)

    assert pricing.session_spend().usage.total_tokens == 0


def test_cache_records_usage_rows(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite")
    cache.record_usage("m", "op", TokenUsage(10, 20, 30, 40), 0.05)
    cache.record_usage("m2", "op2", TokenUsage(1, 2), None)
    cache.close()

    conn = sqlite3.connect(tmp_path / "c.sqlite")
    rows = conn.execute(
        "SELECT model, operation, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd"
        " FROM usage ORDER BY id"
    ).fetchall()
    conn.close()
    assert rows == [("m", "op", 10, 20, 30, 40, 0.05), ("m2", "op2", 1, 2, 0, 0, None)]


def test_cached_response_stores_and_returns_cost(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite")
    cache.put("k", "m", "op", "{}", "payload", 0.07)
    cache.put("k2", "m", "op", "{}", "payload")

    entry = cache.get_entry("k")
    assert entry is not None
    assert entry.cost_usd == pytest.approx(0.07)
    uncosted = cache.get_entry("k2")
    assert uncosted is not None
    assert uncosted.cost_usd is None
    cache.close()


def test_record_saved_accumulates_and_ignores_none() -> None:
    pricing.record_saved(None)
    pricing.record_saved(0.02)
    pricing.record_saved(0.03)

    assert pricing.session_spend().saved_usd == pytest.approx(0.05)


def test_cache_hit_credits_stored_cost_as_saved(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    from lgit import api as api_module
    from lgit.api import OneShotSource, encode_cache_payload
    from lgit.config import CommitConfig

    cache = LlmCache.open(tmp_path / "c.sqlite")
    stored = encode_cache_payload(OneShotSource.CACHE, {"summary": "cached"})
    assert stored is not None
    cache.put("k", "claude-haiku-4-5", "test", "{}", stored, 0.07)
    monkeypatch.setattr(api_module, "_build_cache_entry", lambda config, spec: (cache, "k"))

    response = asyncio.run(api_module._run_oneshot_response(CommitConfig(), _spec()))

    assert response.source is OneShotSource.CACHE
    spend = pricing.session_spend()
    assert spend.saved_usd == pytest.approx(0.07)
    assert spend.cost_usd == 0
    cache.close()

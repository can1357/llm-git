from __future__ import annotations

import asyncio
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import httpx
import pytest
from lgit import cache as llm_cache
from lgit import pricing
from lgit.config import CommitConfig
from lgit.judgment import Choice, ChoiceAnswer, Judge, JudgmentError, JudgmentRequest, Noul, NoulAnswer, chunk_by_chars

_REAL_CLIENT = httpx.AsyncClient


def _judge() -> Judge:
    return Judge(base_url="https://judge.test/", model="jev-test", api_key="k")


def _mock_client(
    monkeypatch: pytest.MonkeyPatch, handler: Callable[[httpx.Request, int], httpx.Response]
) -> list[httpx.Request]:
    seen: list[httpx.Request] = []

    def transport_handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return handler(request, len(seen))

    transport = httpx.MockTransport(transport_handler)
    monkeypatch.setattr(httpx, "AsyncClient", lambda **kw: _REAL_CLIENT(transport=transport, **kw))
    monkeypatch.setattr(asyncio, "sleep", _no_sleep)
    return seen


async def _no_sleep(_: float) -> None:
    return None


def _ok(answers: dict[str, Any], input_tokens: int = 100) -> httpx.Response:
    return httpx.Response(
        200, json={"model": "jev-1", "answers": answers, "usage": {"input_tokens": input_tokens, "output_tokens": 5}}
    )


def test_from_config_requires_enabled_and_key(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("LLM_GIT_JUDGMENT_DISABLED", raising=False)
    assert Judge.from_config(CommitConfig()) is None
    assert Judge.from_config(CommitConfig(judgment_api_key="k", judgment_enabled=False)) is None
    judge = Judge.from_config(CommitConfig(judgment_api_key="k", judgment_base_url="https://x/"))
    assert judge is not None
    assert judge.base_url == "https://x"


def test_env_key_is_fallback_and_disable_env_wins(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("LLM_GIT_JUDGMENT_DISABLED", raising=False)
    monkeypatch.setenv("TYPESAFE_API_KEY", "from-env")
    assert CommitConfig.from_mapping({}).judgment_api_key is None  # from_mapping skips env
    loaded = CommitConfig.from_mapping({})
    loaded._finalize()
    assert loaded.judgment_api_key == "from-env"
    explicit = CommitConfig.from_mapping({"judgment_api_key": "explicit"})
    explicit._finalize()
    assert explicit.judgment_api_key == "explicit"
    monkeypatch.setenv("LLM_GIT_JUDGMENT_DISABLED", "1")
    disabled = CommitConfig.from_mapping({})
    disabled._finalize()
    assert Judge.from_config(disabled) is None


def test_question_wire_shapes() -> None:
    assert Noul("q").to_wire() == {"type": "noul", "instructions": "q"}
    assert Noul("q", yes="Y").to_wire() == {"type": "noul", "instructions": "q", "criteria": {"true": "Y"}}
    assert Choice("q", {"a": None, "b": "B"}).to_wire() == {
        "type": "choice",
        "instructions": "q",
        "criteria": {"a": None, "b": "B"},
    }


def test_judge_parses_answers_and_records_spend(monkeypatch: pytest.MonkeyPatch) -> None:
    pricing.reset_session()
    seen = _mock_client(
        monkeypatch,
        lambda request, _: _ok(
            {
                "m": {"type": "noul", "noul": 0.8},
                "c": {"type": "choice", "choice": "b", "confidence": 0.9, "probabilities": {"a": 0.1, "b": 0.9}},
            },
            input_tokens=1_000_000,
        ),
    )
    request = JudgmentRequest({"diff": "x"}, {"m": Noul("q"), "c": Choice("q", {"a": None, "b": None})})
    answers = asyncio.run(_judge().judge("op", request))
    assert answers == {"m": NoulAnswer(0.8), "c": ChoiceAnswer("b", {"a": 0.1, "b": 0.9}, 0.9)}
    body = json.loads(seen[0].content)
    assert body["model"] == "jev-test" and body["state"] == {"diff": "x"}
    assert seen[0].headers["authorization"] == "Bearer k"
    spend = pricing.session_spend()
    assert spend.usage.input_tokens == 1_000_000
    assert spend.cost_usd == pytest.approx(0.042)


def test_judge_rejects_unknown_choice_and_missing_answer(monkeypatch: pytest.MonkeyPatch) -> None:
    _mock_client(monkeypatch, lambda request, _: _ok({"c": {"type": "choice", "choice": "zzz"}}))
    with pytest.raises(JudgmentError, match="unknown option"):
        asyncio.run(_judge().judge("op", JudgmentRequest("s", {"c": Choice("q", {"a": None})})))
    _mock_client(monkeypatch, lambda request, _: _ok({}))
    with pytest.raises(JudgmentError, match="missing an answer"):
        asyncio.run(_judge().judge("op", JudgmentRequest("s", {"m": Noul("q")})))


def test_judge_retries_transient_errors_but_not_client_errors(monkeypatch: pytest.MonkeyPatch) -> None:
    seen = _mock_client(
        monkeypatch,
        lambda request, n: httpx.Response(503, text="busy") if n == 1 else _ok({"m": {"type": "noul", "noul": 0.1}}),
    )
    answers = asyncio.run(_judge().judge("op", JudgmentRequest("s", {"m": Noul("q")})))
    assert answers["m"] == NoulAnswer(0.1)
    assert len(seen) == 2

    seen = _mock_client(monkeypatch, lambda request, _: httpx.Response(400, text="max_tokens_exceeded"))
    with pytest.raises(JudgmentError, match="HTTP 400"):
        asyncio.run(_judge().judge("op", JudgmentRequest("s", {"m": Noul("q")})))
    assert len(seen) == 1


def test_judge_all_isolates_failures_with_one_warning(monkeypatch: pytest.MonkeyPatch) -> None:
    def handler(request: httpx.Request, _: int) -> httpx.Response:
        state = json.loads(request.content)["state"]
        if state == "bad":
            return httpx.Response(400, text="nope")
        return _ok({"m": {"type": "noul", "noul": 0.5}})

    _mock_client(monkeypatch, handler)
    requests = [JudgmentRequest(state, {"m": Noul("q")}) for state in ("good", "bad", "good")]
    with pytest.warns(RuntimeWarning, match="1/3 op judgments failed"):
        results = asyncio.run(_judge().judge_all("op", requests))
    assert results == [{"m": NoulAnswer(0.5)}, None, {"m": NoulAnswer(0.5)}]


def test_judge_uses_cache_and_credits_saved_cost(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    pricing.reset_session()
    store = llm_cache.LlmCache.open(tmp_path / "cache.sqlite")
    monkeypatch.setattr(llm_cache.LlmCache, "instance", classmethod(lambda cls: store))
    seen = _mock_client(
        monkeypatch, lambda request, _: _ok({"m": {"type": "noul", "noul": 0.3}}, input_tokens=1_000_000)
    )
    request = JudgmentRequest("same", {"m": Noul("q")})
    first = asyncio.run(_judge().judge("op", request))
    second = asyncio.run(_judge().judge("op", request))
    assert first == second == {"m": NoulAnswer(0.3)}
    assert len(seen) == 1
    assert pricing.session_spend().saved_usd == pytest.approx(0.042)


def test_chunk_by_chars_keeps_order_and_isolates_oversized_items() -> None:
    items = ["aaa", "bb", "cccccccc", "d", "ee"]
    assert chunk_by_chars(items, len, 5) == [["aaa", "bb"], ["cccccccc"], ["d", "ee"]]
    assert chunk_by_chars([], len, 5) == []

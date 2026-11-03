"""Typed judgments over small states via TypeSafe's System One API (jev).

A judgment answers named questions about one JSON state in a single ~0.5s call
that bills input tokens only (~$0.04 per million), so hundreds of hunk-level
decisions cost well under a cent. Two question kinds are used here:

- :class:`Noul` — probability that a yes/no condition holds.
- :class:`Choice` — pick one option from a fixed, described set.

Every question in a request sees the same state and is answered independently,
so callers put all questions about one state (e.g. every hunk of a file) into
one request and let :meth:`Judge.judge_all` fan requests out concurrently.
Requests above roughly 32k tokens are rejected by the API; :func:`chunk_by_chars`
splits large inputs under that ceiling.

:meth:`Judge.from_config` returns ``None`` when judgments are disabled or no API
key is configured; every caller treats that as "no judge" and keeps its LLM-only
behaviour, so the feature degrades to the previous flow rather than failing.
Answers are cached in the shared :class:`~lgit.cache.LlmCache` and usage is
recorded through :mod:`lgit.pricing` like any other model call, including billed
responses whose answers fail validation.
"""

from __future__ import annotations

import asyncio
import json
import warnings
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any, Self

import httpx

from . import cache as llm_cache
from . import pricing, profile
from .errors import LgitError

if TYPE_CHECKING:
    from .config import CommitConfig

JUDGMENT_ROUTE = "/v1/systemone"

# jev rejects requests above ~32k tokens (`max_tokens_exceeded`). Diff text runs
# ~3.5 chars/token, so this keeps a chunked state near 17k tokens with room for
# a few hundred question lines on top.
JUDGMENT_STATE_CHAR_BUDGET = 60_000
JUDGMENT_CONCURRENCY = 32
_TIMEOUT_SECS = 15.0
_MAX_ATTEMPTS = 3
_BACKOFF_BASE_SECS = 0.5
_BACKOFF_MAX_SECS = 5.0
_TRANSIENT_STATUSES = frozenset({408, 429})


class JudgmentError(LgitError):
    """The judgment API failed or returned an unusable answer."""


@dataclass(frozen=True, slots=True)
class Noul:
    """Yes/no question answered as a probability; ``yes``/``no`` optionally spell out each side."""

    instructions: str
    yes: str | None = None
    no: str | None = None

    def to_wire(self) -> dict[str, Any]:
        """Serialize to the System One question shape."""
        wire: dict[str, Any] = {"type": "noul", "instructions": self.instructions}
        criteria = {key: value for key, value in (("true", self.yes), ("false", self.no)) if value}
        if criteria:
            wire["criteria"] = criteria
        return wire


@dataclass(frozen=True, slots=True)
class Choice:
    """Pick one option; ``criteria`` maps option label to its rubric (``None`` when the label suffices)."""

    instructions: str
    criteria: Mapping[str, str | None]

    def to_wire(self) -> dict[str, Any]:
        """Serialize to the System One question shape."""
        return {"type": "choice", "instructions": self.instructions, "criteria": dict(self.criteria)}


Question = Noul | Choice


@dataclass(frozen=True, slots=True)
class NoulAnswer:
    """Probability (0-1) that the :class:`Noul` condition holds."""

    probability: float


@dataclass(frozen=True, slots=True)
class ChoiceAnswer:
    """Highest-probability option with the full distribution and its concentration (0-1)."""

    choice: str
    probabilities: Mapping[str, float]
    confidence: float


Answer = NoulAnswer | ChoiceAnswer
Answers = dict[str, Answer]
State = str | Mapping[str, Any] | Sequence[Any]


@dataclass(frozen=True, slots=True)
class JudgmentRequest:
    """One state plus the questions asked about it."""

    state: State
    questions: Mapping[str, Question]


# Hunks larger than this are judged on their head and tail only; the middle of
# a huge hunk never changes whether it is an import list or a real change.
CLIP_HEAD_CHARS = 6_000
CLIP_TAIL_CHARS = 2_000


def clip_text(text: str, head: int = CLIP_HEAD_CHARS, tail: int = CLIP_TAIL_CHARS) -> str:
    """Keep the first ``head`` and last ``tail`` characters of an oversized text, marking the gap."""
    if len(text) <= head + tail:
        return text
    return f"{text[:head]}\n[... {len(text) - head - tail} chars omitted ...]\n{text[-tail:]}"


def chunk_by_chars[T](items: Sequence[T], size_of: Callable[[T], int], budget: int) -> list[list[T]]:
    """Split ``items`` in order into runs whose summed size stays within ``budget``.

    An item larger than the budget gets a run of its own; callers truncate such
    items before judging if the API would reject them.
    """
    chunks: list[list[T]] = []
    current: list[T] = []
    used = 0
    for item in items:
        size = size_of(item)
        if current and used + size > budget:
            chunks.append(current)
            current = []
            used = 0
        current.append(item)
        used += size
    if current:
        chunks.append(current)
    return chunks


class Judge:
    """Client for one judgment model; stateless and safe to share across tasks."""

    __slots__ = ("api_key", "base_url", "model", "_semaphore")

    def __init__(self, *, base_url: str, model: str, api_key: str) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.api_key = api_key
        self._semaphore = asyncio.Semaphore(JUDGMENT_CONCURRENCY)

    @classmethod
    def from_config(cls, config: CommitConfig) -> Self | None:
        """Build a judge from configuration, or ``None`` when judgments are disabled or unauthenticated."""
        if not config.judgment_enabled or not config.judgment_api_key:
            return None
        return cls(base_url=config.judgment_base_url, model=config.judgment_model, api_key=config.judgment_api_key)

    async def judge(self, operation: str, request: JudgmentRequest) -> Answers:
        """Answer every question in ``request``; raises :class:`JudgmentError` on failure."""
        async with httpx.AsyncClient(timeout=_TIMEOUT_SECS) as client:
            return await self._judge_with_client(client, operation, request)

    async def judge_all(self, operation: str, requests: Sequence[JudgmentRequest]) -> list[Answers | None]:
        """Answer many requests concurrently; a failed request yields ``None`` in its slot.

        Failures are reported once as a warning rather than raised: every caller
        has a judgment-free fallback, and a partial batch is still useful.
        """
        if not requests:
            return []
        async with httpx.AsyncClient(timeout=_TIMEOUT_SECS) as client:

            async def one(request: JudgmentRequest) -> Answers | _Failed:
                async with self._semaphore:
                    try:
                        return await self._judge_with_client(client, operation, request)
                    except (JudgmentError, httpx.HTTPError) as exc:
                        return _Failed(str(exc))

            results: list[Answers | _Failed] = await asyncio.gather(*(one(request) for request in requests))
        failures = [item.message for item in results if isinstance(item, _Failed)]
        if failures:
            warnings.warn(
                f"{len(failures)}/{len(requests)} {operation} judgments failed; falling back without them: {failures[0]}",
                RuntimeWarning,
                stacklevel=2,
            )
        return [None if isinstance(item, _Failed) else item for item in results]

    async def _judge_with_client(self, client: httpx.AsyncClient, operation: str, request: JudgmentRequest) -> Answers:
        wire_questions = {key: question.to_wire() for key, question in request.questions.items()}
        body = {"state": request.state, "model": self.model, "questions": wire_questions}
        cache_operation = f"judgment/{operation}"
        sink = llm_cache.LlmCache.instance()
        key = None
        if sink is not None:
            key = llm_cache.compute_key(
                llm_cache.CacheMaterial(
                    operation=cache_operation,
                    model=self.model,
                    tool_name="",
                    system_prompt=json.dumps(wire_questions, sort_keys=True),
                    user_prompt=json.dumps(request.state, sort_keys=True),
                    api_mode="systemone",
                )
            )
            cached = sink.get_entry(key)
            if cached is not None:
                pricing.record_saved(cached.cost_usd)
                return _parse_answers(json.loads(cached.response), request.questions)
        profile.print_llm_progress(lambda: f"judge {operation} model={self.model} questions={len(wire_questions)}")
        payload = await self._post(client, body)
        # A completed request is billed even when its answers cannot be used.
        cost = self._record_spend(cache_operation, payload.get("usage"))
        answers = _parse_answers(payload.get("answers"), request.questions)
        if sink is not None and key is not None:
            sink.put(key, self.model, cache_operation, json.dumps(body), json.dumps(payload.get("answers")), cost)
        return answers

    async def _post(self, client: httpx.AsyncClient, body: Mapping[str, Any]) -> dict[str, Any]:
        url = f"{self.base_url}{JUDGMENT_ROUTE}"
        headers = {"authorization": f"Bearer {self.api_key}", "content-type": "application/json"}
        for attempt in range(_MAX_ATTEMPTS):
            last = attempt + 1 == _MAX_ATTEMPTS
            try:
                response = await client.post(url, headers=headers, json=body)
            except httpx.HTTPError as exc:
                if last:
                    raise JudgmentError(f"judgment request failed: {exc}") from exc
                await asyncio.sleep(_backoff_secs(attempt))
                continue
            if response.is_success:
                payload = response.json()
                if not isinstance(payload, Mapping):
                    raise JudgmentError("judgment response is not an object")
                return dict(payload)
            transient = response.status_code in _TRANSIENT_STATUSES or response.status_code >= 500
            if not transient or last:
                raise JudgmentError(f"judgment API error (HTTP {response.status_code}): {response.text[:300]}")
            await asyncio.sleep(_retry_after_secs(response) or _backoff_secs(attempt))
        raise JudgmentError("judgment request exhausted retries")

    def _record_spend(self, operation: str, usage: Any) -> float | None:
        if not isinstance(usage, Mapping):
            return None
        tokens = pricing.TokenUsage(int(usage.get("input_tokens") or 0), int(usage.get("output_tokens") or 0))
        cost = pricing.record_usage(self.model, tokens)
        sink = llm_cache.LlmCache.instance()
        if sink is not None:
            sink.record_usage(self.model, operation, tokens, cost)
        return cost


@dataclass(frozen=True, slots=True)
class _Failed:
    message: str


def _backoff_secs(attempt: int) -> float:
    return min(_BACKOFF_BASE_SECS * 2**attempt, _BACKOFF_MAX_SECS)


def _retry_after_secs(response: httpx.Response) -> float | None:
    raw = response.headers.get("retry-after")
    if not raw:
        return None
    try:
        return min(float(raw), _BACKOFF_MAX_SECS)
    except ValueError:
        return None


def _parse_answers(raw: Any, questions: Mapping[str, Question]) -> Answers:
    if not isinstance(raw, Mapping):
        raise JudgmentError("judgment response has no answers")
    answers: Answers = {}
    for key, question in questions.items():
        item = raw.get(key)
        if not isinstance(item, Mapping):
            raise JudgmentError(f"judgment response is missing an answer for {key!r}")
        if isinstance(question, Noul):
            if "noul" not in item:
                raise JudgmentError(f"judgment answer for {key!r} is not a yes/no probability")
            answers[key] = NoulAnswer(float(item["noul"]))
        else:
            choice = item.get("choice")
            if not isinstance(choice, str) or choice not in question.criteria:
                raise JudgmentError(f"judgment answer for {key!r} chose an unknown option: {choice!r}")
            probabilities = item.get("probabilities")
            distribution = (
                {str(label): float(value) for label, value in probabilities.items()}
                if isinstance(probabilities, Mapping)
                else {choice: 1.0}
            )
            answers[key] = ChoiceAnswer(choice, distribution, float(item.get("confidence") or 0.0))
    return answers


__all__ = [
    "Answer",
    "Answers",
    "Choice",
    "ChoiceAnswer",
    "JUDGMENT_STATE_CHAR_BUDGET",
    "Judge",
    "JudgmentError",
    "JudgmentRequest",
    "Noul",
    "NoulAnswer",
    "Question",
    "State",
    "chunk_by_chars",
    "clip_text",
]

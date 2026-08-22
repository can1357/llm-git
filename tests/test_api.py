from __future__ import annotations

import asyncio
import json
from dataclasses import replace
from typing import Any

import lgit.api as api_module
import pytest
from lgit.config import CommitConfig
from lgit.errors import ApiContextLengthExceeded, LgitError
from lgit.profile import env_flag_value_enabled


def _summary_spec() -> api_module.OneShotSpec:
    return api_module.OneShotSpec(
        operation="summary",
        model="gpt-4o-mini-probe-clear-test",
        prompt_family="summary",
        system_prompt="Summarize.",
        user_prompt="A large diff.",
        tool_name="create_commit_summary",
        progress_label="summary",
        cacheable=False,
    )


def test_strip_type_prefix_exact_scope() -> None:
    assert api_module.strip_type_prefix("fix(api): fixed bug", "fix", "api") == "fixed bug"


def test_strip_type_prefix_no_scope() -> None:
    assert api_module.strip_type_prefix("fix: fixed bug", "fix", None) == "fixed bug"


def test_strip_type_prefix_different_scope() -> None:
    assert api_module.strip_type_prefix("fix(tui): fixed bug", "fix", None) == "fixed bug"
    assert api_module.strip_type_prefix("fix(tui): fixed bug", "fix", "api") == "fixed bug"


def test_strip_type_prefix_no_prefix() -> None:
    assert api_module.strip_type_prefix("fixed bug", "fix", None) == "fixed bug"


def test_strip_type_prefix_wrong_type_not_stripped() -> None:
    assert api_module.strip_type_prefix("feat(api): added feature", "fix", None) == "feat(api): added feature"


def test_strip_type_prefix_capitalized_type_with_scope() -> None:
    assert api_module.strip_type_prefix("Fix(tui): fixed bug", "fix", None) == "fixed bug"
    assert api_module.strip_type_prefix("Fix(tui): fixed bug", "fix", "api") == "fixed bug"


def test_strip_type_prefix_capitalized_type_no_scope() -> None:
    assert api_module.strip_type_prefix("Feat: added feature", "feat", None) == "added feature"


def test_strip_type_prefix_uppercase_type() -> None:
    assert api_module.strip_type_prefix("FIX(api): fixed bug", "fix", "api") == "fixed bug"


def test_openai_request_reasoning_effort() -> None:
    config = CommitConfig()
    spec = _summary_spec()

    assert "reasoning_effort" not in api_module._openai_request(config, spec)

    low = api_module.OneShotSpec(operation="changelog", model="m", user_prompt="diff", reasoning_effort="low")
    assert api_module._openai_request(config, low)["reasoning_effort"] == "low"
    assert "reasoning_effort" not in api_module._anthropic_request(config, low)


def test_env_flag_value_enabled_uses_boolean_semantics() -> None:
    assert env_flag_value_enabled(None) is False
    assert env_flag_value_enabled("") is False
    assert env_flag_value_enabled("0") is False
    assert env_flag_value_enabled("false") is False
    assert env_flag_value_enabled("NO") is False
    assert env_flag_value_enabled("off") is False
    assert env_flag_value_enabled("1") is True
    assert env_flag_value_enabled("true") is True
    assert env_flag_value_enabled("yes") is True
    assert env_flag_value_enabled("anything") is True


def test_context_length_error_detection() -> None:
    assert api_module._is_context_length_error(
        '{"error":{"message":"Your input exceeds the context window of this model. (code=context_length_exceeded)"}}'
    )
    assert api_module._is_context_length_error("This model's maximum context length is 128000 tokens.")
    assert not api_module._is_context_length_error("upstream temporarily overloaded")


def test_retry_api_call_does_not_retry_context_length_errors(monkeypatch: pytest.MonkeyPatch) -> None:
    attempts = 0

    async def fake_send_oneshot(
        config: CommitConfig,
        spec: api_module.OneShotSpec,
        mode: Any,
    ) -> tuple[dict[str, Any], str]:
        nonlocal attempts
        del config, spec, mode
        attempts += 1
        raise ApiContextLengthExceeded(
            operation="analysis",
            model="codex",
            status=502,
            body="context_length_exceeded",
        )

    monkeypatch.setattr(api_module, "_send_oneshot", fake_send_oneshot)
    config = CommitConfig(max_retries=3, initial_backoff_ms=0, cache_enabled=False)

    with pytest.raises(ApiContextLengthExceeded):
        asyncio.run(api_module._run_oneshot_response(config, _summary_spec()))

    assert attempts == 1


def _chain_spec(chain: str) -> api_module.OneShotSpec:
    return replace(_summary_spec(), model=chain)


def _summary_send(attempted: list[str], failing: str, error: Exception):
    async def fake_send_oneshot(
        config: CommitConfig,
        spec: api_module.OneShotSpec,
        mode: Any,
    ) -> tuple[dict[str, Any], str, None]:
        del config, mode
        attempted.append(spec.model or "")
        if spec.model == failing:
            raise error
        return {}, json.dumps({"choices": [{"message": {"content": "<summary>rebuilt the parser</summary>"}}]}), None

    return fake_send_oneshot


def test_model_chain_falls_back_after_retries_are_exhausted(monkeypatch: pytest.MonkeyPatch) -> None:
    attempted: list[str] = []
    monkeypatch.setattr(
        api_module,
        "_send_oneshot",
        _summary_send(
            attempted,
            "gemini-3.1-flash-lite",
            api_module._RetryableResponse("server error 502: Thinking loop detected"),
        ),
    )
    config = CommitConfig(max_retries=2, initial_backoff_ms=0, cache_enabled=False)

    response = asyncio.run(api_module._run_oneshot_response(config, _chain_spec("lite;haiku")))

    # The stalling model burns its whole retry budget before the chain moves on.
    assert attempted == ["gemini-3.1-flash-lite", "gemini-3.1-flash-lite", "claude-haiku-4-5"]
    assert response.output == {"summary": "rebuilt the parser"}


def test_model_chain_falls_back_on_context_length_without_retrying(monkeypatch: pytest.MonkeyPatch) -> None:
    attempted: list[str] = []
    monkeypatch.setattr(
        api_module,
        "_send_oneshot",
        _summary_send(
            attempted,
            "gemini-3.1-flash-lite",
            ApiContextLengthExceeded(operation="summary", model="gemini-3.1-flash-lite", status=400, body="too long"),
        ),
    )
    config = CommitConfig(max_retries=3, initial_backoff_ms=0, cache_enabled=False)

    response = asyncio.run(api_module._run_oneshot_response(config, _chain_spec("lite;haiku")))

    assert attempted == ["gemini-3.1-flash-lite", "claude-haiku-4-5"]
    assert response.output == {"summary": "rebuilt the parser"}


def test_model_chain_raises_when_every_candidate_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    attempted: list[str] = []

    async def always_failing(
        config: CommitConfig,
        spec: api_module.OneShotSpec,
        mode: Any,
    ) -> tuple[dict[str, Any], str, None]:
        del config, mode
        attempted.append(spec.model or "")
        raise api_module._RetryableResponse("server error 502: Thinking loop detected")

    monkeypatch.setattr(api_module, "_send_oneshot", always_failing)
    config = CommitConfig(max_retries=2, initial_backoff_ms=0, cache_enabled=False)

    with pytest.raises(LgitError, match="Max retries exceeded"):
        asyncio.run(api_module._run_oneshot_response(config, _chain_spec("lite;haiku")))

    assert attempted == [
        "gemini-3.1-flash-lite",
        "gemini-3.1-flash-lite",
        "claude-haiku-4-5",
        "claude-haiku-4-5",
    ]


def test_run_oneshot_returns_context_length_error(monkeypatch: pytest.MonkeyPatch) -> None:
    async def fake_run_oneshot_response(
        config: CommitConfig,
        spec: api_module.OneShotSpec,
    ) -> api_module.OneShotResponse:
        del config, spec
        raise ApiContextLengthExceeded(
            operation="summary",
            model="gpt-4o-mini-probe-clear-test",
            status=400,
            body='{"error":{"message":"context_length_exceeded"}}',
        )

    monkeypatch.setattr(api_module, "_run_oneshot_response", fake_run_oneshot_response)

    with pytest.raises(ApiContextLengthExceeded):
        asyncio.run(api_module.run_oneshot(CommitConfig(cache_enabled=False), _summary_spec()))


def test_extract_json_from_content_code_block() -> None:
    content = """Here is the payload:

```json
{"summary":"added support"}
```
"""
    assert api_module._extract_json_from_content(content) == '{"summary":"added support"}'


def test_build_fast_commit_coerces_invalid_scope_output() -> None:
    commit = api_module._coerce_fast_commit(
        {"type": "chore", "scope": ".", "summary": "updated tooling", "details": []},
        None,
        default_type="chore",
    )

    assert commit.scope is None


def test_build_fast_commit_sanitizes_path_like_scope_output() -> None:
    commit = api_module._coerce_fast_commit(
        {
            "type": "chore",
            "scope": ".github/Release Notes",
            "summary": "updated tooling",
            "details": [],
        },
        None,
        default_type="chore",
    )

    assert commit.scope is not None
    assert commit.scope.as_str() == "github/release-notes"


def _refactor_analysis() -> Any:
    from lgit.models import ConventionalAnalysis

    return ConventionalAnalysis.from_raw(
        commit_type="refactor",
        details=(
            "Added custom core implementations for ULID generation, secret management, and caching in omp-core.",
            "Replaced external dependencies with internal counterparts.",
        ),
    )


def _summary_response(text: str) -> api_module.OneShotResponse:
    return api_module.OneShotResponse(
        output=None,
        source=api_module.OneShotSource.PLAIN_TEXT_CONTENT,
        text_content=f"<summary>{text}</summary>",
    )


def test_generate_summary_repairs_present_tense_without_retry(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[api_module.OneShotSpec] = []

    async def fake_run(config: CommitConfig, spec: api_module.OneShotSpec) -> api_module.OneShotResponse:
        del config
        calls.append(spec)
        return _summary_response("replace third-party dependencies with custom core implementations")

    monkeypatch.setattr(api_module, "_run_oneshot_response", fake_run)
    summary = asyncio.run(
        api_module.generate_summary_from_analysis(CommitConfig(cache_enabled=False), _refactor_analysis())
    )

    assert summary == "replaced third-party dependencies with custom core implementations"
    assert len(calls) == 1


def test_generate_summary_rewrites_unrepairable_draft_with_small_task(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[api_module.OneShotSpec] = []

    async def fake_run(config: CommitConfig, spec: api_module.OneShotSpec) -> api_module.OneShotResponse:
        del config
        calls.append(spec)
        if len(calls) == 1:
            return _summary_response("dependency story goes internal")
        return _summary_response("migrated dependencies to internal implementations")

    monkeypatch.setattr(api_module, "_run_oneshot_response", fake_run)
    summary = asyncio.run(
        api_module.generate_summary_from_analysis(CommitConfig(cache_enabled=False), _refactor_analysis())
    )

    assert summary == "migrated dependencies to internal implementations"
    assert len(calls) == 2
    rewrite = calls[1]
    assert rewrite.operation == "summary-rewrite"
    assert "dependency story goes internal" in rewrite.user_prompt
    assert "past-tense verb" in rewrite.user_prompt
    # The rewrite is a pure text-editing task: no detail points or diff stat leak in.
    assert "<detail_points>" not in rewrite.user_prompt
    assert "<diff_stat>" not in rewrite.user_prompt


def test_generate_summary_falls_back_to_full_first_detail_after_rewrite(monkeypatch: pytest.MonkeyPatch) -> None:
    async def fake_run(config: CommitConfig, spec: api_module.OneShotSpec) -> api_module.OneShotResponse:
        del config, spec
        return _summary_response("dependency story goes internal")

    monkeypatch.setattr(api_module, "_run_oneshot_response", fake_run)
    summary = asyncio.run(
        api_module.generate_summary_from_analysis(CommitConfig(cache_enabled=False), _refactor_analysis())
    )

    # Deterministic fallback keeps the whole first detail instead of clamping to 50 chars.
    assert (
        summary == "Added custom core implementations for ULID generation, secret management, and caching in omp-core"
    )

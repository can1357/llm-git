from __future__ import annotations

import asyncio
from typing import Any

import lgit.api as api_module
import pytest
from lgit.config import CommitConfig
from lgit.errors import ApiContextLengthExceeded
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

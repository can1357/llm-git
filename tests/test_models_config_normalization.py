from dataclasses import FrozenInstanceError

import pytest
from lgit.api import render_prompt
from lgit.config import (
    DEFAULT_ANALYSIS_MODEL,
    DEFAULT_API_BASE_URL,
    DEFAULT_SUMMARY_MODEL,
    CommitConfig,
)
from lgit.diffing import WhitespaceReport
from lgit.models import CommitType, ConventionalCommit, default_types, resolve_model_name
from lgit.normalization import post_process_commit_message
from lgit.validation import is_past_tense_verb, present_to_past, validate_commit_message

_LGIT_ENV_KEYS = (
    "LLM_GIT_CONFIG",
    "LLM_GIT_API_URL",
    "LLM_GIT_API_KEY",
    "LLM_GIT_API_MODE",
    "LLM_GIT_DISABLE_GIT_BACKGROUND_FEATURES",
    "LLM_GIT_CACHE_DISABLED",
    "LLM_GIT_CACHE_TTL_DAYS",
    "LLM_GIT_CACHE_DIR",
)


def _clear_lgit_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for key in _LGIT_ENV_KEYS:
        monkeypatch.delenv(key, raising=False)


def test_commit_type_vocabulary_and_model_aliases_are_resource_backed() -> None:
    types = default_types()

    assert "feat" in types
    assert "feature" in types["feat"].aliases
    assert CommitType.from_raw("feature") == "feat"
    assert ConventionalCommit.from_raw(commit_type="configuration", summary="updated settings").commit_type == "config"

    assert resolve_model_name("sonnet") == "claude-sonnet-4.6"
    assert resolve_model_name("gpt5-codex") == "gpt-5.3-codex"
    assert resolve_model_name("custom/provider-model") == "custom/provider-model"


def test_config_loads_defaults_when_env_config_is_absent(tmp_path, monkeypatch) -> None:
    _clear_lgit_env(monkeypatch)
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("LLM_GIT_CONFIG", str(tmp_path / "missing.toml"))

    config = CommitConfig.load()

    assert config.api_base_url == DEFAULT_API_BASE_URL
    assert config.analysis_model == DEFAULT_ANALYSIS_MODEL
    assert config.summary_model == DEFAULT_SUMMARY_MODEL
    assert "fmt" in config.types["style"].aliases


def test_config_loads_llm_git_config_and_legacy_model(tmp_path, monkeypatch) -> None:
    _clear_lgit_env(monkeypatch)
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    config_path = tmp_path / "config.toml"
    config_path.write_text(
        "\n".join(
            (
                'model = "legacy-model"',
                'api_base_url = "https://api.anthropic.com"',
                "cache_ttl_days = 9",
            )
        ),
        encoding="utf-8",
    )
    monkeypatch.setenv("LLM_GIT_CONFIG", str(config_path))
    monkeypatch.setenv("LLM_GIT_API_URL", "http://localhost:9876")
    monkeypatch.setenv("LLM_GIT_CACHE_DISABLED", "true")

    config = CommitConfig.load()

    assert config.analysis_model == "legacy-model"
    assert config.summary_model == "legacy-model"
    assert config.api_base_url == "http://localhost:9876"
    assert config.cache_enabled is False
    assert config.cache_ttl_days == 9


def test_post_process_rebuilds_frozen_conventional_commit() -> None:
    commit = ConventionalCommit.from_raw(
        commit_type="feature",
        scope="api",
        summary="Add API",
        body=("* normalize unicode bullets", " trims trailing punctuation,"),
        footers=("Refs: #123",),
    )
    with pytest.raises(FrozenInstanceError):
        commit.summary = "mutated"  # type: ignore[misc]

    processed = post_process_commit_message(commit, CommitConfig(max_detail_tokens=100))

    assert processed is not commit
    assert str(commit.summary) == "Add API"
    assert str(processed.commit_type) == "feat"
    assert str(processed.summary) == "added API"
    assert processed.body == ("Normalize unicode bullets.", "Trims trailing punctuation.")
    assert processed.footers == ("Refs: #123",)
    assert processed.format_commit_message().startswith("feat(api): added API")


def test_whitespace_report_alias_tracks_all_whitespace() -> None:
    whitespace_only = WhitespaceReport(whitespace_only_files=["src/app.py"], has_substantive=False)
    mixed = WhitespaceReport(whitespace_only_files=["src/app.py"], has_substantive=True)
    empty = WhitespaceReport()

    assert whitespace_only.all_whitespace is True
    assert whitespace_only.is_whitespace_only is True
    assert mixed.all_whitespace is False
    assert mixed.is_whitespace_only is False
    assert empty.all_whitespace is False
    assert empty.is_whitespace_only is False


def test_fast_prompt_renders_from_packaged_resources(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))

    system, user = render_prompt(
        "fast",
        {
            "stat": "1 file changed, 2 insertions(+)",
            "diff": "diff --git a/lgit/config.py b/lgit/config.py",
            "scope_candidates": "config",
            "user_context": "prefer config scope",
            "types_description": "- config: App/runtime config",
        },
    )

    assert system.startswith("You are a senior engineer writing a conventional commit message.")
    assert "type(scope): summary" in system
    assert "<file_changes>\n1 file changed, 2 insertions(+)\n</file_changes>" in user
    assert "<scope_candidates>\nconfig\n</scope_candidates>" in user
    assert "<commit_types>\n- config: App/runtime config\n</commit_types>" in user
    assert "<user_context>\nprefer config scope\n</user_context>" in user
    assert "diff --git a/lgit/config.py b/lgit/config.py" in user


def test_validation_data_drives_past_tense_lookup() -> None:
    assert present_to_past("add") == "added"
    assert present_to_past("migrate") == "migrated"
    assert present_to_past("does-not-exist") is None
    assert is_past_tense_verb("added") is True
    assert is_past_tense_verb("migrated") is True


def test_project_name_scope_constructs_then_flags_for_drop() -> None:
    # Regression: a scope equal to the project name OR its package alias must NOT
    # raise during construction (it crashed mid-generation before); validation
    # emits ``project_name_scope`` so the CLI drops the scope for project-wide
    # changes. Here ``llm-git`` is the repo dir and ``lgit`` its top-level package.
    names = ("llm-git", "lgit")
    for scope in names:
        commit = ConventionalCommit.from_raw(commit_type="chore", scope=scope, summary="updated project tooling")
        assert str(commit.scope) == scope
        report = validate_commit_message(commit, CommitConfig(), project_names=names)
        assert any(issue.code == "project_name_scope" for issue in report.errors)

    dropped = ConventionalCommit.from_raw(commit_type="chore", summary="updated project tooling")
    cleared = validate_commit_message(dropped, CommitConfig(), project_names=names)
    assert not any(issue.code == "project_name_scope" for issue in cleared.errors)

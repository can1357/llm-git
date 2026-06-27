from __future__ import annotations

from lgit.config import DEFAULT_SUMMARY_MODEL, CommitConfig


def test_normalize_models_legacy_model_sets_summary_when_default() -> None:
    config = CommitConfig(legacy_model="gpt-5.3-codex-spark")

    config._normalize_models()

    assert config.analysis_model == "gpt-5.3-codex-spark"
    assert config.summary_model == "gpt-5.3-codex-spark"
    assert config.legacy_model == "gpt-5.3-codex-spark"


def test_normalize_models_preserves_explicit_summary_model() -> None:
    config = CommitConfig(summary_model="gpt-5-mini", legacy_model="gpt-5.3-codex-spark")

    config._normalize_models()

    assert config.analysis_model == "gpt-5.3-codex-spark"
    assert config.summary_model == "gpt-5-mini"
    assert config.summary_model != DEFAULT_SUMMARY_MODEL

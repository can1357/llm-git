from __future__ import annotations

from pathlib import Path

import lgit.templates as templates_module
import pytest
from lgit.config import DEFAULT_SUMMARY_MODEL, CommitConfig
from lgit.templates import load_template_file


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


def test_prompts_dir_wires_template_overrides(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # Register the current override dir for teardown restoration before loads mutate it.
    monkeypatch.setattr(templates_module, "_PROMPTS_DIR", templates_module._PROMPTS_DIR)
    override = tmp_path / "prompts"
    override.mkdir()
    (override / "summary.md").write_text("OVERRIDE SYSTEM\n<!-- USER -->\nx", encoding="utf-8")

    config_file = tmp_path / "config.toml"
    config_file.write_text(f'prompts_dir = "{override}"\n', encoding="utf-8")
    CommitConfig.load(config_file)
    assert load_template_file("summary").startswith("OVERRIDE SYSTEM")

    # An empty/absent prompts_dir resets to packaged prompts and never writes to disk.
    empty_file = tmp_path / "empty.toml"
    empty_file.write_text('api_base_url = "http://localhost:1"\n', encoding="utf-8")
    CommitConfig.load(empty_file)
    assert not load_template_file("summary").startswith("OVERRIDE SYSTEM")
    assert not (tmp_path / ".llm-git").exists()

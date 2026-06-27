from __future__ import annotations

from pathlib import Path

import pytest
from lgit.templates import (
    AnalysisParams,
    ComposeBindPromptParams,
    ComposeIntentPromptParams,
    FastPromptParams,
    ensure_prompts_dir,
    render_analysis_prompt,
    render_changelog_prompt,
    render_compose_bind_prompt,
    render_compose_intent_prompt,
    render_fast_prompt,
    render_reduce_prompt,
    render_summary_prompt,
    split_prompt_template,
)


def _prepare_prompts(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    ensure_prompts_dir()


def test_split_prompt_template_lf() -> None:
    content = "system text\nmore system\n======USER=======\nuser body\n"

    system, user = split_prompt_template(content)

    assert system == "system text\nmore system"
    assert user == "user body\n"


def test_split_prompt_template_crlf() -> None:
    content = "system text\r\nmore system\r\n======USER=======\r\nuser body\r\n"

    system, user = split_prompt_template(content)

    assert system == "system text\r\nmore system"
    assert user == "user body\r\n"


def test_split_prompt_template_no_separator() -> None:
    content = "no separator here"

    system, user = split_prompt_template(content)

    assert system is None
    assert user == content


def test_render_analysis_prompt_requests_holistic_summary(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_analysis_prompt(
        AnalysisParams(
            variant="default",
            stat="src/api/client.rs | 24 +++++++++++++++---------",
            diff="diff --git a/src/api/client.rs b/src/api/client.rs\n",
            scope_candidates="api",
        )
    )

    assert "Generate Summary" in parts.system
    assert '"summary"' in parts.system
    assert "umbrella headline for the whole changeset" in parts.system
    assert "Does not copy detail #1" in parts.system


def test_render_changelog_prompt_variants_render(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    for variant in ("default", "markdown"):
        parts = render_changelog_prompt(
            variant,
            "CHANGELOG.md",
            False,
            "src/api.rs | 4 ++--",
            "diff --git a/src/api.rs b/src/api.rs\n",
            "- Added existing entry",
        )

        assert "src/api.rs" in parts.user
        assert "Added existing entry" in parts.user
        if variant == "markdown":
            assert "# Added" in parts.system
            assert '{"entries"' not in parts.system
            assert "<exception>" in parts.system
        else:
            assert '{"entries"' in parts.system


def test_render_fast_prompt_surfaces_type_guidance(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_fast_prompt(
        FastPromptParams(
            variant="default",
            stat="prompts/analysis/default.md | 5 +++++",
            diff="diff --git a/prompts/analysis/default.md b/prompts/analysis/default.md\n",
            scope_candidates="prompts",
            types_description="**docs**: Documentation only changes\n  Note: Excludes prompt template files.",
        )
    )

    assert "<commit_types>" in parts.user
    assert "Excludes prompt template files." in parts.user
    assert "not `docs`" in parts.system


def test_render_fast_prompt_omits_commit_types_when_absent(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_fast_prompt(
        FastPromptParams(
            variant="default",
            stat="src/main.rs | 5 +++++",
            diff="diff --git a/src/main.rs b/src/main.rs\n",
            scope_candidates="",
        )
    )

    assert "<commit_types>" not in parts.user


def test_render_reduce_prompt_guides_grouped_synthesis(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_reduce_prompt(
        "default",
        '[{"file":"src/a.rs","observations":["Added retry handling."]}]',
        "src/a.rs | 10 +++++-----",
        "api",
    )

    assert "3-4 strong grouped details" in parts.system
    assert "Synthesize repeated file observations" in parts.system
    assert "over enumerating files" in parts.system


def test_render_compose_intent_prompt(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_compose_intent_prompt(
        ComposeIntentPromptParams(
            variant="default",
            max_commits=3,
            stat="src/foo.rs | 10 +++++-----",
            snapshot_summary="- F1 src/foo.rs",
            planning_targets="file IDs",
            planning_notes="Prefer conservative grouping over speculative splitting.",
            split_bias="Prefer fewer groups when the split is uncertain.",
            types_description="**feat**: new capability\n**ux**: ergonomics",
        )
    )

    assert "create_compose_intent_plan" in parts.system
    assert "max_commits: 3" in parts.user
    assert "src/foo.rs" in parts.user
    assert "<commit_types>" in parts.user
    assert "new capability" in parts.user


def test_render_summary_prompt_guides_umbrella_title(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_summary_prompt(
        "default",
        "feat",
        "api",
        "72",
        "Added websocket reconnects.\nUpdated client retry tests.",
        "src/api/client.rs | 24 +++++++++++++++---------",
    )

    assert "umbrella description for the whole changeset" in parts.system
    assert "not as candidate titles to copy" in parts.system
    assert "does not copy or narrowly paraphrase one detail point" in parts.system
    assert "<detail_points>" in parts.user
    assert "Added websocket reconnects." in parts.user
    assert "Updated client retry tests." in parts.user


def test_render_compose_bind_prompt(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    _prepare_prompts(tmp_path, monkeypatch)

    parts = render_compose_bind_prompt(
        ComposeBindPromptParams(
            variant="default",
            groups="- G1 [feat(api)] Added endpoint",
            ambiguous_files="- F2 src/api.rs candidates: G1",
        )
    )

    assert "bind_compose_hunks" in parts.system
    assert "G1" in parts.user
    assert "src/api.rs" in parts.user

from __future__ import annotations

import asyncio
from argparse import Namespace
from pathlib import Path

from lgit.api import summary_from_holistic_analysis
from lgit.config import CommitConfig
from lgit.models import AnalysisDetail, ConventionalAnalysis
from lgit.testing.compare import compare_analysis
from lgit.testing.fixture import (
    Fixture,
    FixtureContext,
    FixtureInput,
    FixtureMeta,
    Manifest,
    analysis_to_json,
)
from lgit.testing.report import render_report
from lgit.testing.runner import TestRunner as FixtureTestRunner
from lgit.testing.runner import TestSummary as FixtureTestSummary
from lgit.testing.runner import run_test_mode


def _analysis(commit_type: str = "feat", scope: str | None = "api") -> ConventionalAnalysis:
    return ConventionalAnalysis(
        commit_type=commit_type,
        scope=scope,
        summary="added fixture runner",
        details=(AnalysisDetail.simple("Added fixture runner."),),
    )


def _fixture(name: str = "sample") -> Fixture:
    return Fixture(
        name=name,
        meta=FixtureMeta(
            source_repo="repo",
            source_commit="abc123",
            description="sample fixture",
            captured_at="2026-06-26T00:00:00Z",
            tags=["smoke"],
        ),
        input=FixtureInput(
            diff="diff --git a/a.py b/a.py\n+print(1)\n",
            stat=" a.py | 1 +\n",
            scope_candidates="api (100%, high confidence)",
            context=FixtureContext(user_context="prefer api scope"),
        ),
        golden=None,
    )


def test_fixture_manifest_and_golden_round_trip(tmp_path: Path) -> None:
    fixture = _fixture()
    fixture.update_golden(_analysis(), "feat(api): added fixture runner")
    fixture.save(tmp_path)

    loaded = Fixture.load(tmp_path, "sample")

    assert loaded.meta.description == "sample fixture"
    assert loaded.input.context.user_context == "prefer api scope"
    assert loaded.golden is not None
    assert loaded.golden.analysis.commit_type.value == "feat"
    assert loaded.golden.final_message == "feat(api): added fixture runner"


def test_manifest_save_loads_nested_fixture_entries(tmp_path: Path) -> None:
    manifest = Manifest()
    from lgit.testing.fixture import FixtureEntry

    manifest.add("sample-fixture", FixtureEntry(description="Sample", tags=["smoke", "unit"]))
    manifest.save(tmp_path)

    loaded = Manifest.load(tmp_path)
    assert loaded.fixtures["sample-fixture"].description == "Sample"
    assert loaded.fixtures["sample-fixture"].tags == ["smoke", "unit"]


def test_analysis_json_round_trips_structured_details() -> None:
    analysis = ConventionalAnalysis(
        commit_type="fix",
        scope="cli",
        summary="fixed fixture parsing",
        details=(AnalysisDetail.simple("Fixed fixture parsing."),),
        issue_refs=("#123",),
    )

    from lgit.testing.fixture import analysis_from_json

    decoded = analysis_from_json(analysis_to_json(analysis))
    assert decoded.commit_type.value == "fix"
    assert str(decoded.scope) == "cli"
    assert decoded.body_texts() == ["Fixed fixture parsing."]
    assert decoded.issue_refs == ("#123",)


def test_compare_analysis_matches_rust_pass_rules() -> None:
    golden = _analysis("feat", "api")
    same_type_different_scope = _analysis("feat", "cli")
    different_type = _analysis("fix", "api")

    warning = compare_analysis(golden, same_type_different_scope)
    failure = compare_analysis(golden, different_type)

    assert warning.passed is True
    assert warning.scope_match is False
    assert failure.passed is False


def test_holistic_summary_uses_rust_prefix_stripping() -> None:
    analysis = ConventionalAnalysis(
        commit_type="feat",
        scope="api",
        summary="feat(api): added fixture runner",
        details=(AnalysisDetail.simple("Added fixture runner."),),
    )

    assert summary_from_holistic_analysis(analysis, CommitConfig()) == "added fixture runner"


def test_generate_summary_falls_back_when_model_summary_fails_validation(monkeypatch) -> None:
    import lgit.api as api_module

    async def fake_response(config, spec, *, markdown_output=None):
        del config, spec, markdown_output
        return api_module.OneShotResponse(
            output={"summary": "adds invalid present tense"},
            source=api_module.OneShotSource.TOOL_CALL,
        )

    monkeypatch.setattr(api_module, "_run_oneshot_response", fake_response)
    analysis = ConventionalAnalysis(
        commit_type="feat",
        summary=None,
        details=(AnalysisDetail.simple("Added fixture runner."),),
    )

    summary = asyncio.run(api_module.generate_summary_from_analysis(CommitConfig(), analysis, stat=" a.py | 1 +"))

    assert summary == "Added fixture runner"


def test_test_runner_uses_fixture_inputs_without_network(tmp_path: Path, monkeypatch) -> None:
    fixture = _fixture()
    fixture.update_golden(_analysis(), "feat(api): added fixture runner")
    fixture.save(tmp_path)
    summary_calls = {"count": 0}

    async def fake_analysis(config, stat, diff, scope_candidates, **kwargs):
        assert stat == fixture.input.stat
        assert diff == fixture.input.diff
        assert scope_candidates == fixture.input.scope_candidates
        assert kwargs["user_context"] == "prefer api scope"
        return ConventionalAnalysis(
            commit_type="feat",
            scope="api",
            summary=None,
            details=(AnalysisDetail.simple("Added fixture runner."),),
        )

    async def fake_summary(config, analysis, stat="", **kwargs):
        summary_calls["count"] += 1
        assert stat == fixture.input.stat
        return "added fixture runner"

    import lgit.testing.runner as runner_module

    monkeypatch.setattr(runner_module, "generate_analysis_with_map_reduce", fake_analysis)
    monkeypatch.setattr(runner_module, "generate_summary_from_analysis", fake_summary)

    result = asyncio.run(FixtureTestRunner(tmp_path, CommitConfig()).run_fixture("sample"))

    assert result.error is None
    assert result.comparison is not None
    assert result.comparison.passed is True
    assert result.final_message.startswith("feat(api): added fixture runner")
    assert summary_calls["count"] == 1


def test_run_test_mode_lists_fixtures(tmp_path: Path, capsys) -> None:
    fixture = _fixture()
    fixture.save(tmp_path)
    args = Namespace(fixtures_dir=tmp_path, test_list=True, test_add=None)

    summary = asyncio.run(run_test_mode(args, CommitConfig()))
    output = capsys.readouterr().out

    assert "Available fixtures (1)" in output
    assert "sample" in output
    assert summary.total == 1


def test_html_report_contains_fixture_status() -> None:
    fixture = _fixture()
    analysis = _analysis()
    from lgit.testing.runner import RunResult

    run_result = RunResult(
        name="sample",
        comparison=compare_analysis(analysis, analysis),
        analysis=analysis,
        final_message="feat(api): added fixture runner",
    )

    html = render_report([run_result], [fixture], FixtureTestSummary.from_results([run_result]))
    assert "lgit Fixture Test Report" in html
    assert "sample" in html
    assert "Passed" in html

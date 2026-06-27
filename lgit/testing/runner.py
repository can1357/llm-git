"""Async fixture runner for lgit test mode."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any, Self

from lgit.api import generate_analysis_with_map_reduce, generate_summary_from_analysis, summary_from_holistic_analysis
from lgit.errors import ValidationFailure
from lgit.markdown_output import fallback_summary
from lgit.models import ConventionalAnalysis, ConventionalCommit
from lgit.normalization import format_commit_message, post_process_commit_message

from .compare import CompareResult, compare_analysis
from .fixture import Fixture, add_fixture, discover_fixtures, load_fixtures

if TYPE_CHECKING:
    from lgit.config import CommitConfig


@dataclass(slots=True)
class RunResult:
    """Result of running one fixture."""

    name: str
    comparison: CompareResult | None
    analysis: ConventionalAnalysis
    final_message: str
    error: str | None = None


@dataclass(slots=True)
class TestSummary:
    """Aggregate fixture test counts."""

    total: int = 0
    passed: int = 0
    failed: int = 0
    no_golden: int = 0
    errors: int = 0

    @classmethod
    def from_results(cls, results: list[RunResult]) -> Self:
        summary = cls(total=len(results))
        for result in results:
            if result.error is not None:
                summary.errors += 1
            elif result.comparison is None:
                summary.no_golden += 1
            elif result.comparison.passed:
                summary.passed += 1
            else:
                summary.failed += 1
        return summary

    def all_passed(self) -> bool:
        return self.failed == 0 and self.errors == 0


class TestRunner:
    """Run lgit fixture tests and update golden files."""

    def __init__(self, fixtures_dir: str | Path, config: CommitConfig) -> None:
        self.fixtures_dir = Path(fixtures_dir)
        self.config = config
        self.filter: str | None = None

    def with_filter(self, filter: str | None) -> Self:
        self.filter = filter or None
        return self

    def fixture_names(self) -> list[str]:
        names = discover_fixtures(self.fixtures_dir)
        if self.filter:
            names = [name for name in names if self.filter in name]
        return names

    async def run_all(self) -> list[RunResult]:
        results: list[RunResult] = []
        for name in self.fixture_names():
            results.append(await self.run_fixture(name))
        return results

    async def run_fixture(self, name: str) -> RunResult:
        try:
            return await self._run_fixture_inner(name)
        except Exception as exc:
            return RunResult(
                name=name,
                comparison=None,
                analysis=ConventionalAnalysis.from_raw(commit_type="chore"),
                final_message="",
                error=str(exc),
            )

    async def _run_fixture_inner(self, name: str) -> RunResult:
        fixture = Fixture.load(self.fixtures_dir, name)
        context = fixture.input.context
        debug_output = _fixture_debug_dir(name)

        analysis = await generate_analysis_with_map_reduce(
            self.config,
            fixture.input.stat,
            fixture.input.diff,
            fixture.input.scope_candidates,
            user_context=context.user_context,
            recent_commits=context.recent_commits,
            common_scopes=context.common_scopes,
            project_context=context.project_context,
            debug_output=debug_output,
            debug_prefix=None,
        )
        final_message = await self._final_message(fixture, analysis)
        comparison = None if fixture.golden is None else compare_analysis(fixture.golden.analysis, analysis)
        return RunResult(name=name, comparison=comparison, analysis=analysis, final_message=final_message)

    async def _final_message(self, fixture: Fixture, analysis: ConventionalAnalysis) -> str:
        limit = self.config.summary_hard_limit
        details = analysis.body_texts()

        def build_fallback_summary() -> str:
            return fallback_summary(stat=fixture.input.stat, details=details, limit=limit)

        summary: str | None
        if analysis.summary:
            summary = analysis.summary
        else:
            try:
                summary = summary_from_holistic_analysis(analysis, self.config, fixture.input.stat)
            except ValidationFailure:
                summary = build_fallback_summary()
            else:
                if summary is None:
                    try:
                        summary = await generate_summary_from_analysis(
                            self.config,
                            analysis,
                            fixture.input.stat,
                            user_context=fixture.input.context.user_context,
                            debug_output=_fixture_debug_dir(fixture.name),
                            debug_prefix=None,
                        )
                    except Exception:
                        summary = build_fallback_summary()
        if summary is None:
            summary = build_fallback_summary()
        summary = summary[:limit].rstrip(" .")
        commit = ConventionalCommit.from_raw(
            commit_type=str(analysis.commit_type),
            scope=None if analysis.scope is None else str(analysis.scope),
            summary=summary,
            body=details,
            footers=(),
            summary_max_length=limit,
        )
        normalized = post_process_commit_message(commit, self.config)
        return format_commit_message(normalized)

    async def update_all(self) -> list[str]:
        updated: list[str] = []
        for name in self.fixture_names():
            await self.update_fixture(name)
            updated.append(name)
        return updated

    async def update_fixture(self, name: str) -> None:
        result = await self.run_fixture(name)
        if result.error is not None:
            raise RuntimeError(f"failed to run fixture {name!r}: {result.error}")
        fixture = Fixture.load(self.fixtures_dir, name)
        fixture.update_golden(result.analysis, result.final_message)
        fixture.save(self.fixtures_dir)


async def run_test_mode(args: Any, config: CommitConfig) -> TestSummary:
    """CLI entry point for ``--test`` fixture mode."""

    from . import fixtures_dir as default_fixtures_dir
    from . import list_fixtures as package_list_fixtures
    from .report import generate_html_report

    root = Path(_arg(args, "fixtures_dir") or default_fixtures_dir())

    if bool(_arg(args, "test_list", False)):
        names = package_list_fixtures(root)
        if names:
            print(f"Available fixtures ({len(names)}):")
            for name in names:
                print(f"  {name}")
        else:
            print(f"No fixtures found in {root}")
        return TestSummary(total=len(names), no_golden=len(names))

    commit_hash = _arg(args, "test_add")
    if commit_hash:
        name = _arg(args, "test_name")
        if not name:
            raise ValueError("--test-name is required with --test-add")
        print(f"Creating fixture '{name}' from commit {commit_hash}...")
        await add_fixture(root, str(commit_hash), str(name), _arg(args, "dir", "."), config)
        print(f"Created fixture at {root / str(name)}")
        print("Run with --test-update to generate golden files")
        return TestSummary(total=1, no_golden=1)

    runner = TestRunner(root, config).with_filter(_arg(args, "test_filter"))

    if bool(_arg(args, "test_update", False)):
        print("Updating golden files...")
        updated = await runner.update_all()
        print(f"Updated {len(updated)} fixtures:")
        for name in updated:
            print(f"  {name}")
        return TestSummary(total=len(updated), passed=len(updated))

    print(f"Running fixture tests from {root}...\n")
    results = await runner.run_all()
    if not results:
        print("No fixtures found.")
        return TestSummary()

    for result in results:
        if result.error is not None:
            print(f"FAIL {result.name} - ERROR: {result.error}")
        elif result.comparison is None:
            print(f"NO GOLDEN {result.name} - no golden file")
        else:
            status = "PASS" if result.comparison.passed else "FAIL"
            print(f"{status} {result.name} - {result.comparison.summary}")

    summary = TestSummary.from_results(results)
    print("\n-------------------------------------")
    print(
        f"Total: {summary.total} | Passed: {summary.passed} | Failed: {summary.failed} | No golden: {summary.no_golden} | Errors: {summary.errors}"
    )

    report_path = _arg(args, "test_report")
    if report_path:
        fixtures = load_fixtures(root, runner.fixture_names())
        generate_html_report(results, fixtures, report_path)
        print(f"\nHTML report generated: {Path(report_path)}")

    if not summary.all_passed():
        raise RuntimeError("Some tests failed")
    return summary


def _fixture_debug_dir(name: str) -> Path | None:
    root = os.environ.get("LLM_GIT_TEST_DEBUG_DIR")
    if not root:
        return None
    path = Path(root) / name
    path.mkdir(parents=True, exist_ok=True)
    return path


def _arg(args: Any, name: str, default: Any = None) -> Any:
    return getattr(args, name, default)


__all__ = ["RunResult", "TestRunner", "TestSummary", "run_test_mode"]

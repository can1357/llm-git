"""Comparison logic for fixture golden analysis."""

from __future__ import annotations

from dataclasses import dataclass

from lgit.models import ConventionalAnalysis


@dataclass(frozen=True, slots=True)
class CompareResult:
    """Result of comparing actual analysis to a golden analysis."""

    type_match: bool
    scope_match: bool
    scope_diff: str | None
    golden_detail_count: int
    actual_detail_count: int
    passed: bool
    summary: str


def compare_analysis(golden: ConventionalAnalysis, actual: ConventionalAnalysis) -> CompareResult:
    """Compare two analyses using Rust harness-compatible pass rules."""

    type_match = golden.commit_type == actual.commit_type
    scope_match = golden.scope == actual.scope
    scope_diff = None if scope_match else f"{_scope_text(golden)} -> {_scope_text(actual)}"
    golden_detail_count = len(golden.details)
    actual_detail_count = len(actual.details)

    passed = type_match
    if passed and scope_match:
        summary = (
            f"PASS {actual.commit_type} | {_scope_text(actual, none='(no scope)')} | {actual_detail_count} details"
        )
    elif passed:
        summary = f"WARN {actual.commit_type} | scope: {scope_diff} | {actual_detail_count} details"
    else:
        summary = f"FAIL type: {golden.commit_type} -> {actual.commit_type} | {actual_detail_count} details"

    return CompareResult(
        type_match=type_match,
        scope_match=scope_match,
        scope_diff=scope_diff,
        golden_detail_count=golden_detail_count,
        actual_detail_count=actual_detail_count,
        passed=passed,
        summary=summary,
    )


def _scope_text(analysis: ConventionalAnalysis, *, none: str = "null") -> str:
    return none if analysis.scope is None else str(analysis.scope)


__all__ = ["CompareResult", "compare_analysis"]

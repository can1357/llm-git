from __future__ import annotations

import pytest
from lgit.models import ConventionalAnalysis
from lgit.testing.compare import compare_analysis


def test_compare_exact_match() -> None:
    golden = ConventionalAnalysis(commit_type="feat", scope="api")
    actual = ConventionalAnalysis(commit_type="feat", scope="api")

    result = compare_analysis(golden, actual)

    assert result.passed is True
    assert result.type_match is True
    assert result.scope_match is True


def test_compare_type_mismatch() -> None:
    golden = ConventionalAnalysis(commit_type="feat")
    actual = ConventionalAnalysis(commit_type="fix")

    result = compare_analysis(golden, actual)

    assert result.passed is False
    assert result.type_match is False


def test_compare_scope_mismatch() -> None:
    golden = ConventionalAnalysis(commit_type="feat", scope="api")
    actual = ConventionalAnalysis(commit_type="feat", scope="api/client")

    result = compare_analysis(golden, actual)

    assert result.passed is True
    assert result.scope_match is False
    assert result.scope_diff is not None


@pytest.mark.skip(
    reason="jaccard_similarity exists only as a Rust test-local helper; lgit.testing.compare has no Python equivalent"
)
def test_jaccard_similarity() -> None:
    pass

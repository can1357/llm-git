from __future__ import annotations

from collections.abc import Iterable
from types import SimpleNamespace

import pytest
from lgit.config import CommitConfig
from lgit.errors import InvalidCommitType, InvalidScope, SummaryTooLong, ValidationFailure
from lgit.models import ConventionalCommit, Scope
from lgit.validation import ValidationIssue, validate_commit_message


def _commit(
    commit_type: str,
    summary: str,
    *,
    scope: str | None = None,
    body: Iterable[str] = (),
    summary_max_length: int = 128,
) -> ConventionalCommit:
    return ConventionalCommit.from_raw(
        commit_type=commit_type,
        scope=scope,
        summary=summary,
        body=body,
        summary_max_length=summary_max_length,
    )


def _error_codes(issues: Iterable[ValidationIssue]) -> set[str]:
    return {issue.code for issue in issues}


def _raw_commit(commit_type: str, summary: str, *, scope: str | None = None) -> SimpleNamespace:
    return SimpleNamespace(commit_type=commit_type, scope=scope, summary=summary, body=())


def test_validate_valid_commit() -> None:
    report = validate_commit_message(_commit("feat", "added new endpoint", scope="api"), CommitConfig())

    assert report.ok
    assert report.errors == ()


def test_validate_valid_commit_no_scope() -> None:
    report = validate_commit_message(_commit("fix", "corrected race condition"), CommitConfig())

    assert report.ok
    assert report.errors == ()


def test_validate_invalid_type() -> None:
    report = validate_commit_message(_raw_commit("invalid", "added endpoint"), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"invalid_type"}
    with pytest.raises(InvalidCommitType):
        ConventionalCommit.from_raw(commit_type="invalid", summary="added endpoint")


def test_validate_summary_ends_with_period() -> None:
    report = validate_commit_message(_commit("feat", "added endpoint.", scope="api"), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"trailing_period"}


def test_validate_summary_too_long() -> None:
    summary = f"added {'x' * 123}"
    report = validate_commit_message(
        _commit("feat", summary, scope="scope", summary_max_length=256),
        CommitConfig(),
    )

    assert not report.ok
    assert _error_codes(report.errors) == {"summary_too_long"}
    with pytest.raises(SummaryTooLong):
        ConventionalCommit.from_raw(commit_type="feat", summary="a" * 129)


def test_validate_summary_empty() -> None:
    report = validate_commit_message(_raw_commit("feat", ""), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"empty_summary"}
    with pytest.raises(ValidationFailure):
        ConventionalCommit.from_raw(commit_type="feat", summary="")


def test_validate_summary_empty_whitespace() -> None:
    report = validate_commit_message(_raw_commit("feat", "   "), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"empty_summary"}
    with pytest.raises(ValidationFailure):
        ConventionalCommit.from_raw(commit_type="feat", summary="   ")


def test_validate_wrong_verb() -> None:
    report = validate_commit_message(_commit("feat", "adding new feature"), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"present_tense_first_word"}


def test_validate_present_tense_verb() -> None:
    report = validate_commit_message(_commit("feat", "adds new feature"), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"present_tense_first_word"}


def test_validate_no_type_verb_overlap() -> None:
    docs_report = validate_commit_message(_commit("docs", "documented new api", scope="api"), CommitConfig())
    test_report = validate_commit_message(_commit("test", "added unit tests", scope="api"), CommitConfig())

    assert docs_report.ok
    assert test_report.ok
    assert "type_word_repetition" not in _error_codes(docs_report.errors)
    assert "type_word_repetition" not in _error_codes(test_report.errors)


@pytest.mark.parametrize(
    "verb",
    ["added", "configured", "exposed", "formatted", "clarified", "made", "built", "ran", "wrote", "split"],
)
def test_validate_accepts_past_tense_verb(verb: str) -> None:
    report = validate_commit_message(_commit("feat", f"{verb} something"), CommitConfig())
    assert report.ok, f"verb {verb!r} should be accepted"


@pytest.mark.parametrize("word", ["hundred", "red", "bed"])
def test_validate_rejects_non_verb_first_word(word: str) -> None:
    report = validate_commit_message(_commit("feat", f"{word} something"), CommitConfig())
    assert not report.ok, f"non-verb {word!r} should be rejected"
    assert _error_codes(report.errors) == {"present_tense_first_word"}


def test_validate_scope_empty_string() -> None:
    report = validate_commit_message(_raw_commit("feat", "added endpoint", scope=""), CommitConfig())

    assert not report.ok
    assert _error_codes(report.errors) == {"empty_scope"}
    with pytest.raises(InvalidScope):
        ConventionalCommit.from_raw(commit_type="feat", scope="", summary="added endpoint")


def test_validate_scope_invalid_chars() -> None:
    with pytest.raises(InvalidScope):
        ConventionalCommit.from_raw(commit_type="feat", scope="API/New", summary="added endpoint")


def test_validate_scope_too_many_segments() -> None:
    with pytest.raises(InvalidScope, match="max 2 allowed"):
        ConventionalCommit.from_raw(commit_type="feat", scope="core/api/http", summary="added endpoint")


def test_validate_scope_valid_single() -> None:
    commit = _commit("feat", "added endpoint", scope="api")

    assert commit.scope == Scope.from_raw("api")
    assert validate_commit_message(commit, CommitConfig()).ok


def test_validate_scope_valid_two_segments() -> None:
    commit = _commit("feat", "added endpoint", scope="core/api")

    assert commit.scope == Scope.from_raw("core/api")
    assert validate_commit_message(commit, CommitConfig()).ok


def test_validate_scope_with_dash_underscore() -> None:
    commit = _commit("feat", "added endpoint", scope="core_api/http-client")

    assert commit.scope == Scope.from_raw("core_api/http-client")
    assert validate_commit_message(commit, CommitConfig()).ok


def test_validate_total_length_at_guideline() -> None:
    summary = f"added {'x' * 53}"
    report = validate_commit_message(_commit("feat", summary, scope="scope"), CommitConfig())

    assert report.ok
    assert report.errors == ()
    assert report.warnings == ()

from __future__ import annotations

import pytest
from lgit.errors import InvalidCommitType, InvalidScope
from lgit.models import (
    CommitType,
    Scope,
    coerce_commit_type,
    coerce_optional_scope,
    default_classifier_hint,
    default_types,
    resolve_model_name,
)


@pytest.mark.parametrize(
    ("alias", "resolved"),
    [
        ("sonnet", "claude-sonnet-4.5"),
        ("s", "claude-sonnet-4.5"),
        ("opus", "claude-opus-4.5"),
        ("o", "claude-opus-4.5"),
        ("haiku", "claude-haiku-4-5"),
        ("h", "claude-haiku-4-5"),
        ("gpt5", "gpt-5"),
        ("g5", "gpt-5"),
        ("gemini", "gemini-2.5-pro"),
        ("flash", "gemini-2.5-flash"),
        ("claude-sonnet-4.5", "claude-sonnet-4.5"),
        ("custom-model", "custom-model"),
    ],
)
def test_resolve_model_name(alias: str, resolved: str) -> None:
    assert resolve_model_name(alias) == resolved


@pytest.mark.parametrize(
    "raw",
    ["feat", "fix", "refactor", "docs", "test", "chore", "style", "perf", "build", "ci", "revert"],
)
def test_commit_type_valid(raw: str) -> None:
    assert CommitType.from_raw(raw).as_str() == raw


@pytest.mark.parametrize(("raw", "canonical"), [("FEAT", "feat"), ("Fix", "fix"), ("ReFaCtOr", "refactor")])
def test_commit_type_case_normalization(raw: str, canonical: str) -> None:
    assert CommitType.from_raw(raw).as_str() == canonical


@pytest.mark.parametrize("raw", ["invalid", "update", "change", "random", "xyz", "123"])
def test_commit_type_invalid(raw: str) -> None:
    with pytest.raises(InvalidCommitType):
        CommitType.from_raw(raw)


@pytest.mark.parametrize(
    ("alias", "canonical"),
    [("ui", "ux"), ("feature", "feat"), ("bug", "fix"), ("formatting", "style")],
)
def test_commit_type_aliases_normalize_to_canonical(alias: str, canonical: str) -> None:
    assert CommitType.from_raw(alias).as_str() == canonical


def test_coerce_commit_type_falls_back_to_chore() -> None:
    assert coerce_commit_type("ui").as_str() == "ux"
    assert coerce_commit_type("totallyunknown").as_str() == "chore"


def test_embedded_vocabulary_is_valid() -> None:
    types = default_types()
    seen_aliases: set[str] = set()

    assert "feat" in types
    assert default_classifier_hint()
    for canonical, config in types.items():
        for alias in config.aliases:
            assert alias not in types, f"alias {alias!r} shadows a canonical name"
            assert alias not in seen_aliases, f"alias {alias!r} is duplicated"
            seen_aliases.add(alias)
            assert CommitType.from_raw(alias).as_str() == canonical


def test_commit_type_empty() -> None:
    with pytest.raises(InvalidCommitType):
        CommitType.from_raw("")


def test_commit_type_display() -> None:
    assert str(CommitType.from_raw("feat")) == "feat"


def test_commit_type_len() -> None:
    assert len(CommitType.from_raw("feat")) == 4
    assert len(CommitType.from_raw("refactor")) == 8


@pytest.mark.parametrize("raw", ["core", "api", "lib", "client", "server", "ui", "test-123", "foo_bar"])
def test_scope_valid_single_segment(raw: str) -> None:
    assert Scope.from_raw(raw).as_str() == raw


@pytest.mark.parametrize("raw", ["api/client", "lib/core", "ui/components", "test-1/foo_2"])
def test_scope_valid_two_segments(raw: str) -> None:
    assert Scope.from_raw(raw).as_str() == raw


def test_scope_invalid_three_segments() -> None:
    with pytest.raises(InvalidScope, match="3 segments"):
        Scope.from_raw("a/b/c")


@pytest.mark.parametrize("raw", ["Core", "API", "MyScope", "api/Client"])
def test_scope_invalid_uppercase(raw: str) -> None:
    with pytest.raises(InvalidScope):
        Scope.from_raw(raw)


@pytest.mark.parametrize("raw", ["", "a//b", "/foo", "bar/"])
def test_scope_invalid_empty_segments(raw: str) -> None:
    with pytest.raises(InvalidScope):
        Scope.from_raw(raw)


@pytest.mark.parametrize("raw", ["a b", "foo bar", "test@scope", "api/client!", "a.b"])
def test_scope_invalid_chars(raw: str) -> None:
    with pytest.raises(InvalidScope):
        Scope.from_raw(raw)


def test_scope_segments() -> None:
    assert Scope.from_raw("core").segments() == ("core",)
    assert Scope.from_raw("api/client").segments() == ("api", "client")


def test_scope_display() -> None:
    assert str(Scope.from_raw("api/client")) == "api/client"


@pytest.mark.parametrize("raw", [None, "null", "Null", "NULL", " null ", "none", "n/a", ""])
def test_coerce_optional_scope_nullish_values(raw: str | None) -> None:
    assert coerce_optional_scope(raw) is None


def test_coerce_optional_scope_returns_existing_scope() -> None:
    scope = Scope.from_raw("api")
    assert coerce_optional_scope(scope) is scope


@pytest.mark.parametrize(
    ("raw", "coerced"),
    [(".github", "github"), ("docs//Release Notes", "docs/release-notes"), ("API\\Client", "api/client")],
)
def test_coerce_optional_scope_sanitizes_model_output(raw: str, coerced: str) -> None:
    scope = coerce_optional_scope(raw)
    assert scope is not None
    assert scope.as_str() == coerced


def test_coerce_optional_scope_drops_unusable_model_output() -> None:
    assert coerce_optional_scope("!!!") is None

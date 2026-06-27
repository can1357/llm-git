"""Commit-message validation helpers.

The lookup tables in this module are loaded from package resources so installed
CLI behavior does not depend on the source checkout layout.
"""

from __future__ import annotations

import json
from collections.abc import Iterable
from dataclasses import dataclass
from functools import cache
from importlib import resources
from pathlib import PurePosixPath
from typing import Any, Literal

type IssueSeverity = Literal["error", "warning"]

_DEFAULT_GUIDELINE = 72
_DEFAULT_SOFT_LIMIT = 96
_DEFAULT_HARD_LIMIT = 128

_FALLBACK_TYPES_ORDERED = (
    "feat",
    "fix",
    "refactor",
    "docs",
    "test",
    "chore",
    "style",
    "perf",
    "build",
    "ci",
    "revert",
    "deps",
    "security",
    "config",
    "ux",
    "release",
    "hotfix",
    "infra",
    "init",
    "merge",
    "hack",
    "wip",
)
_FALLBACK_TYPES = frozenset(_FALLBACK_TYPES_ORDERED)


@dataclass(slots=True, frozen=True)
class ValidationIssue:
    """One structured validation diagnostic."""

    severity: IssueSeverity
    field: str
    code: str
    message: str
    value: str | None = None


@dataclass(slots=True, frozen=True)
class ValidationReport:
    """Structured validation result with separate errors and warnings."""

    errors: tuple[ValidationIssue, ...] = ()
    warnings: tuple[ValidationIssue, ...] = ()

    @property
    def ok(self) -> bool:
        """Return whether validation found no blocking errors."""

        return not self.errors

    def __bool__(self) -> bool:
        """Treat the report as true only when it has no errors."""

        return self.ok


@dataclass(slots=True, frozen=True)
class _ValidationData:
    past_tense: dict[str, str]
    irregular_past: frozenset[str]
    ed_blocklist: frozenset[str]
    d_blocklist: frozenset[str]
    code_extensions: frozenset[str]
    doc_extensions: frozenset[str]
    filler_words: tuple[str, ...]
    meta_phrases: tuple[str, ...]
    body_present_tense: frozenset[str]


@dataclass(slots=True)
class _IssueBuilder:
    errors: list[ValidationIssue]
    warnings: list[ValidationIssue]

    @classmethod
    def empty(cls) -> _IssueBuilder:
        return cls(errors=[], warnings=[])

    def error(self, field: str, code: str, message: str, value: str | None = None) -> None:
        self.errors.append(ValidationIssue("error", field, code, message, value))

    def warning(self, field: str, code: str, message: str, value: str | None = None) -> None:
        self.warnings.append(ValidationIssue("warning", field, code, message, value))

    def report(self) -> ValidationReport:
        return ValidationReport(tuple(self.errors), tuple(self.warnings))


@cache
def _load_validation_data() -> _ValidationData:
    raw = (resources.files("lgit.resources") / "validation_data.json").read_text(encoding="utf-8")
    data = json.loads(raw)
    pairs = [(str(present).lower(), str(past).lower()) for present, past in data["past_tense"]]
    past_tense = dict(pairs)
    unchanged = {past for present, past in pairs if present == past}
    irregular = unchanged | {str(value).lower() for value in data["irregular_past"]}
    return _ValidationData(
        past_tense=past_tense,
        irregular_past=frozenset(irregular),
        ed_blocklist=frozenset(str(value).lower() for value in data["ed_blocklist"]),
        d_blocklist=frozenset(str(value).lower() for value in data["d_blocklist"]),
        code_extensions=frozenset(str(value).lower() for value in data["code_extensions"]),
        doc_extensions=frozenset(str(value).lower() for value in data["doc_extensions"]),
        filler_words=tuple(str(value).lower() for value in data["filler_words"]),
        meta_phrases=tuple(str(value).lower() for value in data["meta_phrases"]),
        body_present_tense=frozenset(str(value).lower() for value in data["body_present_tense"]),
    )


@cache
def _valid_types_ordered() -> tuple[str, ...]:
    try:
        raw = (resources.files("lgit.resources") / "commit_types.json").read_text(encoding="utf-8")
        data = json.loads(raw)
    except FileNotFoundError, json.JSONDecodeError, KeyError, TypeError:
        return _FALLBACK_TYPES_ORDERED
    types = tuple(str(item["name"]).strip().lower() for item in data.get("types", ()) if item.get("name"))
    return types or _FALLBACK_TYPES_ORDERED


@cache
def _valid_types() -> frozenset[str]:
    return frozenset(_valid_types_ordered()) or _FALLBACK_TYPES


def _byte_len(value: str) -> int:
    return len(value.encode("utf-8"))


def present_to_past(present: str) -> str | None:
    """Return the configured past-tense form for a lowercase present-tense verb."""

    return _load_validation_data().past_tense.get(present.lower())


def split_verb_token(token: str) -> tuple[str, str] | None:
    """Split a first token into its leading ASCII verb segment and suffix."""

    index = 0
    for character in token:
        if not character.isascii() or not character.isalpha():
            break
        index += 1
    if index == 0:
        return None
    return token[:index], token[index:]


def verb_stem(token: str) -> str | None:
    """Return a lowercase leading ASCII verb stem, skipping acronyms and numbers."""

    split = split_verb_token(token)
    if split is None:
        return None
    stem, _suffix = split
    if stem.isupper():
        return None
    return stem.lower()


def is_past_tense_verb(word: str) -> bool:
    """Return whether a bare word looks like a past-tense verb."""

    lower = word.lower()
    data = _load_validation_data()
    if any(past == lower and present != past for present, past in data.past_tense.items()):
        return True
    if lower.endswith("ed"):
        return lower not in data.ed_blocklist
    if len(lower) >= 4 and lower.endswith("d") and lower[-2] in "aeiou":
        return lower not in data.d_blocklist
    return lower in data.irregular_past


def is_past_tense_first_word(token: str) -> bool:
    """Return whether a raw first summary token is acceptable past tense."""

    if not token:
        return False
    if is_past_tense_verb(token.lower()):
        return True
    stem = verb_stem(token)
    if stem is not None and is_past_tense_verb(stem):
        return True
    split = split_verb_token(token)
    if split is None:
        return False
    stem_raw, suffix = split
    if stem_raw.lower() != "re" or not suffix.startswith("-"):
        return False
    rest = suffix[1:]
    inner_length = 0
    for character in rest:
        if not character.isascii() or not character.isalpha():
            break
        inner_length += 1
    if inner_length == 0:
        return False
    inner = rest[:inner_length].lower()
    return is_past_tense_verb(inner) or present_to_past(inner) is not None


def validate_commit_message(
    msg: Any,
    config: Any | None = None,
    *,
    stat: str = "",
    project_names: Iterable[str] = (),
) -> ValidationReport:
    """Validate a conventional commit object and return structured diagnostics."""

    builder = _IssueBuilder.empty()
    commit_type = _commit_type_text(msg)
    scope = _scope_text(msg)
    summary = _summary_text(msg)
    body = tuple(_iter_strings(getattr(msg, "body", ())))

    _validate_type(commit_type, builder)
    _validate_scope(scope, project_names, builder)
    _validate_summary(summary, commit_type, scope, config, builder)
    if summary.strip():
        _validate_summary_content(summary, commit_type, stat, builder)
    _validate_body(body, builder)
    if stat:
        _type_scope_consistency(commit_type, stat, body, builder)
    return builder.report()


def validate_summary_quality(summary: str, commit_type: str, stat: str = "") -> ValidationReport:
    """Validate a generated summary before building a commit object."""

    builder = _IssueBuilder.empty()
    cleaned = str(summary).strip()
    if not cleaned:
        builder.error("summary", "empty_summary", "summary is empty")
        return builder.report()
    _validate_summary_content(cleaned, str(commit_type), stat, builder)
    return builder.report()


def check_type_scope_consistency(msg: Any, stat: str) -> ValidationReport:
    """Return warnings for commit type/file-stat consistency heuristics."""

    builder = _IssueBuilder.empty()
    _type_scope_consistency(_commit_type_text(msg), stat, tuple(_iter_strings(getattr(msg, "body", ()))), builder)
    return builder.report()


def _validate_type(commit_type: str, builder: _IssueBuilder) -> None:
    if commit_type not in _valid_types():
        allowed = ", ".join(_valid_types_ordered())
        builder.error(
            "type",
            "invalid_type",
            f"Invalid commit type: {commit_type!r}. Must be one of: {allowed}",
            commit_type,
        )


def _validate_scope(scope: str | None, project_names: Iterable[str], builder: _IssueBuilder) -> None:
    if scope is None:
        return
    if not scope:
        builder.error("scope", "empty_scope", "Scope cannot be empty string; omit it instead", scope)
        return
    names = (project_names,) if isinstance(project_names, str) else project_names
    project = {_normalize_name(name) for name in names if name}
    if _normalize_name(scope) in project:
        builder.error(
            "scope",
            "project_name_scope",
            f"Scope {scope!r} is the project name; omit scope for project-wide changes",
            scope,
        )


def _validate_summary(
    summary: str,
    commit_type: str,
    scope: str | None,
    config: Any | None,
    builder: _IssueBuilder,
) -> None:
    if not summary.strip():
        builder.error("summary", "empty_summary", "Summary cannot be empty", summary)
        return
    if summary.rstrip().endswith("."):
        builder.error(
            "summary",
            "trailing_period",
            "Summary must NOT end with a period (conventional commits style)",
            summary,
        )

    first_line_len = _byte_len(commit_type) + (_byte_len(scope) + 2 if scope else 0) + 2 + _byte_len(summary)
    guideline = int(getattr(config, "summary_guideline", _DEFAULT_GUIDELINE))
    soft_limit = int(getattr(config, "summary_soft_limit", _DEFAULT_SOFT_LIMIT))
    hard_limit = int(getattr(config, "summary_hard_limit", _DEFAULT_HARD_LIMIT))
    if first_line_len > hard_limit:
        builder.error(
            "summary",
            "summary_too_long",
            f"Summary line exceeds hard limit: {first_line_len} > {hard_limit} chars",
            str(first_line_len),
        )
    elif first_line_len > soft_limit:
        builder.warning(
            "summary",
            "summary_soft_limit",
            f"Summary line exceeds soft limit: {first_line_len} > {soft_limit} chars",
            str(first_line_len),
        )
    elif first_line_len > guideline:
        builder.warning(
            "summary",
            "summary_guideline",
            f"Summary line exceeds guideline: {first_line_len} > {guideline} chars",
            str(first_line_len),
        )


def _validate_summary_content(summary: str, commit_type: str, stat: str, builder: _IssueBuilder) -> None:
    first_word = summary.split(maxsplit=1)[0] if summary.split() else ""
    if not first_word:
        builder.error("summary", "summary_missing_word", "Summary must contain at least one word")
        return
    if not is_past_tense_first_word(first_word):
        builder.error(
            "summary",
            "present_tense_first_word",
            f"Summary must start with a past-tense verb (ending in -ed/-d or irregular). Got {first_word!r}",
            first_word,
        )
    if first_word.lower() == commit_type:
        builder.error(
            "summary",
            "type_word_repetition",
            f"Summary repeats commit type {commit_type!r}: first word is {first_word!r}",
            first_word,
        )

    lower_summary = summary.lower()
    data = _load_validation_data()
    for filler in data.filler_words:
        if filler in lower_summary:
            builder.warning(
                "summary",
                "filler_word",
                f"Summary contains filler word {filler!r}",
                filler,
            )
    for phrase in data.meta_phrases:
        if phrase in lower_summary:
            builder.warning(
                "summary",
                "meta_phrase",
                f"Summary contains meta-phrase {phrase!r}; describe what changed",
                phrase,
            )

    if stat:
        _summary_file_mismatch(summary, commit_type, stat, builder)


def _validate_body(body: Iterable[str], builder: _IssueBuilder) -> None:
    present_words = _load_validation_data().body_present_tense
    for index, item in enumerate(body):
        stripped = item.strip()
        first_word = stripped.split(maxsplit=1)[0].lower() if stripped.split() else ""
        if first_word in present_words:
            builder.warning(
                "body",
                "present_tense_body_item",
                f"Body item uses present tense: {stripped!r}",
                str(index),
            )
        if stripped and not stripped.endswith("."):
            builder.warning(
                "body",
                "missing_period_body_item",
                f"Body item is missing a period: {stripped!r}",
                str(index),
            )


def _summary_file_mismatch(summary: str, commit_type: str, stat: str, builder: _IssueBuilder) -> None:
    del summary
    extensions = [
        extension for path in _stat_paths(stat) if (extension := PurePosixPath(path).suffix.lstrip(".").lower())
    ]
    if not extensions:
        return
    total = len(extensions)
    markdown_count = sum(1 for extension in extensions if extension == "md")
    if markdown_count * 100 // total > 80 and commit_type != "docs":
        builder.warning(
            "type",
            "markdown_type_mismatch",
            f"Type mismatch: {markdown_count * 100 // total}% .md files but type is {commit_type!r}; consider docs",
            commit_type,
        )
    code_count = sum(1 for extension in extensions if extension in _load_validation_data().code_extensions)
    if code_count == 0 and commit_type in {"feat", "fix"}:
        builder.warning(
            "type",
            "no_code_type_mismatch",
            f"Type mismatch: no code files changed but type is {commit_type!r}",
            commit_type,
        )


def _type_scope_consistency(commit_type: str, stat: str, body: tuple[str, ...], builder: _IssueBuilder) -> None:
    paths = tuple(_stat_paths(stat))
    lower_paths = tuple(path.lower() for path in paths)
    data = _load_validation_data()
    if commit_type == "docs":
        has_docs = any(
            PurePosixPath(path).suffix.lstrip(".").lower() in data.doc_extensions
            or "/docs/" in lower_path
            or "readme" in lower_path
            for path, lower_path in zip(paths, lower_paths, strict=False)
        )
        if not has_docs:
            builder.warning("type", "docs_without_docs", "Commit type 'docs' but no documentation files changed")
    elif commit_type == "test":
        has_test = any("/test" in path or "_test." in path or ".test." in path for path in lower_paths)
        if not has_test:
            builder.warning("type", "test_without_tests", "Commit type 'test' but no test files changed")
    elif commit_type == "style":
        has_code = any(PurePosixPath(path).suffix.lstrip(".").lower() in data.code_extensions for path in paths)
        if has_code:
            builder.warning("type", "style_with_code", "Commit type 'style' but code files changed")
    elif commit_type == "ci":
        has_ci = any(
            ".github/workflows" in path or ".gitlab-ci" in path or "jenkinsfile" in path for path in lower_paths
        )
        if not has_ci:
            builder.warning("type", "ci_without_ci", "Commit type 'ci' but no CI configuration files changed")
    elif commit_type == "build":
        has_build = any(
            "cargo.toml" in path or "package.json" in path or "makefile" in path or "build." in path
            for path in lower_paths
        )
        if not has_build:
            builder.warning("type", "build_without_build", "Commit type 'build' but no build files changed")
    elif commit_type == "refactor":
        has_new_files = any(
            line.strip().startswith("create mode") or "new file" in line.lower() for line in stat.splitlines()
        )
        if has_new_files:
            builder.warning(
                "type",
                "refactor_with_new_files",
                "Commit type 'refactor' but new files were created; verify no new capabilities were added",
            )
    elif commit_type == "perf":
        has_perf_files = any("bench" in path or "perf" in path or "profile" in path for path in lower_paths)
        details_text = " ".join(body).lower()
        has_perf_details = any(term in details_text for term in ("faster", "optimization", "performance", "optimized"))
        if not has_perf_files and not has_perf_details:
            builder.warning(
                "type",
                "perf_without_evidence",
                "Commit type 'perf' but no performance files or optimization keywords were found",
            )


def _stat_paths(stat: str) -> list[str]:
    paths: list[str] = []
    for line in stat.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if stripped.startswith("create mode"):
            parts = stripped.split(maxsplit=3)
            if len(parts) == 4:
                paths.append(parts[3])
            continue
        path = stripped.split("|", maxsplit=1)[0].strip()
        if path and not path[0].isdigit():
            paths.append(path)
    return paths


def _commit_type_text(msg: Any) -> str:
    return str(getattr(msg, "commit_type", getattr(msg, "type", ""))).strip().lower()


def _scope_text(msg: Any) -> str | None:
    scope = getattr(msg, "scope", None)
    if scope is None:
        return None
    return str(scope).strip().lower()


def _summary_text(msg: Any) -> str:
    summary = getattr(msg, "summary", "")
    value = getattr(summary, "value", summary)
    return str(value)


def _iter_strings(values: Iterable[Any]) -> Iterable[str]:
    for value in values:
        yield str(value)


def _normalize_name(name: str) -> str:
    return name.lower().replace("-", "").replace("_", "")


__all__ = [
    "ValidationIssue",
    "ValidationReport",
    "check_type_scope_consistency",
    "is_past_tense_first_word",
    "is_past_tense_verb",
    "present_to_past",
    "split_verb_token",
    "validate_commit_message",
    "validate_summary_quality",
    "verb_stem",
]

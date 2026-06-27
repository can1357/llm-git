"""Domain models for conventional commits, analysis, and compose mode."""

from __future__ import annotations

import json
from collections.abc import Iterable, Mapping
from dataclasses import InitVar, dataclass, field
from enum import StrEnum
from functools import lru_cache
from importlib import resources
from typing import Any, Self

from .errors import InvalidCommitType, InvalidScope, SummaryTooLong, ValidationFailure

DEFAULT_SUMMARY_MAX_LENGTH = 128
SUMMARY_GUIDELINE_LENGTH = 72


class Mode(StrEnum):
    """Input mode for generating a commit message."""

    STAGED = "staged"
    COMMIT = "commit"
    UNSTAGED = "unstaged"
    COMPOSE = "compose"

    @classmethod
    def from_raw(cls, raw: str | Self) -> Self:
        """Parse a mode token."""
        if isinstance(raw, cls):
            return raw
        normalized = raw.strip().lower().replace("_", "-")
        for mode in cls:
            if mode.value == normalized:
                return mode
        raise ValidationFailure(f"unknown mode: {raw!r}", field="mode", value=raw)


class ApiMode(StrEnum):
    """Configured API protocol selection."""

    AUTO = "auto"
    CHAT_COMPLETIONS = "chat-completions"
    ANTHROPIC_MESSAGES = "anthropic-messages"

    @classmethod
    def from_raw(cls, raw: str | Self) -> Self:
        """Parse an API mode token using the accepted config aliases."""
        if isinstance(raw, cls):
            return raw
        match raw.strip().lower().replace("_", "-"):
            case "auto":
                return cls.AUTO
            case "chat" | "chat-completions":
                return cls.CHAT_COMPLETIONS
            case "anthropic" | "messages" | "anthropic-messages":
                return cls.ANTHROPIC_MESSAGES
            case _:
                raise ValidationFailure(f"unknown API mode: {raw!r}", field="api_mode", value=raw)


class ResolvedApiMode(StrEnum):
    """Concrete API protocol after resolving auto mode."""

    CHAT_COMPLETIONS = "chat-completions"
    ANTHROPIC_MESSAGES = "anthropic-messages"

    @classmethod
    def from_api_mode(cls, mode: ApiMode, api_base_url: str = "") -> Self:
        """Resolve an API mode with the same auto heuristic as the Rust implementation."""
        match mode:
            case ApiMode.CHAT_COMPLETIONS:
                return cls.CHAT_COMPLETIONS
            case ApiMode.ANTHROPIC_MESSAGES:
                return cls.ANTHROPIC_MESSAGES
            case ApiMode.AUTO:
                if "anthropic" in api_base_url.lower():
                    return cls.ANTHROPIC_MESSAGES
                return cls.CHAT_COMPLETIONS


_MODEL_ALIASES = {
    "sonnet": "claude-sonnet-4.6",
    "s": "claude-sonnet-4.6",
    "sonnet-4.6": "claude-sonnet-4.6",
    "opus": "claude-opus-4.8",
    "o": "claude-opus-4.8",
    "o4.8": "claude-opus-4.8",
    "haiku": "claude-haiku-4-5",
    "h": "claude-haiku-4-5",
    "gpt5": "gpt-5.5",
    "g5": "gpt-5.5",
    "gpt5-pro": "gpt-5-pro",
    "gpt5-mini": "gpt-5.4-mini",
    "gpt5-codex": "gpt-5.3-codex",
    "spark": "gpt-5.3-codex-spark",
    "gemini": "gemini-3.5-flash",
    "g3.5": "gemini-3.5-flash",
    "flash": "gemini-3.5-flash",
    "g3.5-flash": "gemini-3.5-flash",
    "lite": "gemini-3.1-flash-lite",
    "flash-lite": "gemini-3.1-flash-lite",
    "qwen": "qwen-3-coder-480b",
    "q480b": "qwen-3-coder-480b",
    "glm": "glm-4.7",
    "glm4.7": "glm-4.7",
    "glm4.6": "glm-4.6",
    "glm4.5": "glm-4.5",
    "glm-flash": "glm-4.7-flash",
    "glm-air": "glm-4.5-air",
}


def resolve_model_name(name: str) -> str:
    """Resolve a short model alias to the full LiteLLM model name."""
    return _MODEL_ALIASES.get(name, name)


@dataclass(frozen=True, slots=True)
class TypeConfig:
    """Classification guidance for one conventional commit type."""

    description: str
    diff_indicators: tuple[str, ...] = ()
    file_patterns: tuple[str, ...] = ()
    examples: tuple[str, ...] = ()
    hint: str = ""
    aliases: tuple[str, ...] = ()

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Self:
        """Build a type config from JSON-compatible data."""
        return cls(
            description=str(data.get("description", "")),
            diff_indicators=_string_tuple(data.get("diff_indicators", ())),
            file_patterns=_string_tuple(data.get("file_patterns", ())),
            examples=_string_tuple(data.get("examples", ())),
            hint=str(data.get("hint", "")),
            aliases=_string_tuple(data.get("aliases", ())),
        )


@dataclass(frozen=True, slots=True)
class CategoryMatch:
    """Rules for mapping commit details to a changelog category."""

    types: tuple[str, ...] = ()
    body_contains: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class CategoryConfig:
    """Configurable changelog category mapping."""

    name: str
    header: str | None = None
    match: CategoryMatch = field(default_factory=CategoryMatch)
    default: bool = False


@dataclass(frozen=True, slots=True)
class _Vocabulary:
    types: dict[str, TypeConfig]
    aliases: dict[str, str]
    classifier_hint: str


@lru_cache(maxsize=1)
def _vocabulary() -> _Vocabulary:
    resource = resources.files("lgit.resources").joinpath("commit_types.json")
    data = json.loads(resource.read_text(encoding="utf-8"))
    types: dict[str, TypeConfig] = {}
    aliases: dict[str, str] = {}
    for entry in data.get("types", ()):
        name = str(entry["name"]).strip().lower()
        config = TypeConfig.from_mapping(entry)
        types[name] = config
        for alias in config.aliases:
            aliases[alias.lower()] = name
    return _Vocabulary(types=types, aliases=aliases, classifier_hint=str(data.get("classifier_hint", "")))


def default_types() -> dict[str, TypeConfig]:
    """Return the default commit-type vocabulary in priority order."""
    return dict(_vocabulary().types)


def default_classifier_hint() -> str:
    """Return the global commit-type disambiguation hint."""
    return _vocabulary().classifier_hint


@dataclass(frozen=True, slots=True, eq=False)
class CommitType:
    """Validated conventional commit type, canonicalized through package resources."""

    value: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "value", _canonical_commit_type(self.value))

    @classmethod
    def from_raw(cls, raw: str | Self) -> Self:
        """Create a commit type from a canonical name or known alias."""
        if isinstance(raw, str):
            return cls(raw)
        return raw

    def __str__(self) -> str:
        return self.value

    def __repr__(self) -> str:
        return f"CommitType({self.value!r})"

    def as_str(self) -> str:
        """Return the canonical commit type string."""
        return self.value

    def __len__(self) -> int:
        return len(self.value)

    def __eq__(self, other: object) -> bool:
        if isinstance(other, CommitType):
            return self.value == other.value
        if isinstance(other, str):
            return self.value == other
        return NotImplemented

    def __hash__(self) -> int:
        return hash(self.value)


def _canonical_commit_type(raw: str) -> str:
    normalized = raw.strip().lower()
    vocab = _vocabulary()
    if normalized in vocab.types:
        return normalized
    if normalized in vocab.aliases:
        return vocab.aliases[normalized]
    valid = ", ".join(vocab.types)
    raise InvalidCommitType(
        f"invalid commit type {raw!r}; must be one of: {valid}",
        field="type",
        value=raw,
    )


def coerce_commit_type(raw: str | CommitType) -> CommitType:
    """Coerce a raw type token, falling back to ``chore`` when unknown."""
    if isinstance(raw, CommitType):
        return raw
    try:
        return CommitType.from_raw(raw)
    except InvalidCommitType:
        return CommitType.from_raw("chore")


@dataclass(frozen=True, slots=True, eq=False)
class Scope:
    """Validated conventional-commit scope."""

    value: str

    def __post_init__(self) -> None:
        _validate_scope(self.value)

    @classmethod
    def from_raw(cls, raw: str | Self) -> Self:
        """Create a scope after strict validation."""
        if isinstance(raw, str):
            return cls(raw)
        return raw

    def __str__(self) -> str:
        return self.value

    def __repr__(self) -> str:
        return f"Scope({self.value!r})"

    def as_str(self) -> str:
        """Return the scope string."""
        return self.value

    def __len__(self) -> int:
        return _byte_len(self.value)

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Scope):
            return self.value == other.value
        if isinstance(other, str):
            return self.value == other
        return NotImplemented

    def __hash__(self) -> int:
        return hash(self.value)

    def segments(self) -> tuple[str, ...]:
        """Split the scope into slash-delimited segments."""
        return tuple(self.value.split("/"))


def _validate_scope(scope: str) -> None:
    if scope != scope.lower():
        raise InvalidScope("scope must be lowercase", field="scope", value=scope)
    segments = scope.split("/")
    if len(segments) > 2:
        raise InvalidScope(f"scope has {len(segments)} segments, max 2 allowed", field="scope", value=scope)
    for segment in segments:
        if not segment:
            raise InvalidScope("scope contains empty segment", field="scope", value=scope)
        if not all(ch.isascii() and (ch.isalnum() or ch in "-_") for ch in segment):
            raise InvalidScope(f"invalid characters in scope segment: {segment}", field="scope", value=scope)


def coerce_optional_scope(raw: str | Scope | None) -> Scope | None:
    """Lossily coerce model-emitted scope text, returning ``None`` when unusable."""
    null_markers = {"null", "none", "n/a"}
    if raw is None or isinstance(raw, Scope):
        return raw
    trimmed = raw.strip()
    if not trimmed or trimmed.lower() in null_markers:
        return None
    normalized = trimmed.replace("\\", "/").lower()
    segments = []
    for segment in normalized.split("/"):
        cleaned = _sanitize_scope_segment(segment)
        if cleaned:
            segments.append(cleaned)
        if len(segments) == 2:
            break
    if not segments:
        return None
    try:
        return Scope.from_raw("/".join(segments))
    except InvalidScope:
        return None


def _sanitize_scope_segment(segment: str) -> str | None:
    out: list[str] = []
    last_was_separator = False
    for char in segment.strip():
        if char.isascii() and (char.islower() or char.isdigit()):
            out.append(char)
            last_was_separator = False
        elif char in "-_":
            if out and not last_was_separator:
                out.append(char)
                last_was_separator = True
        elif (char.isspace() or char == ".") and out and not last_was_separator:
            out.append("-")
            last_was_separator = True
    cleaned = "".join(out).strip("-_")
    return cleaned or None


def _byte_len(value: str) -> int:
    return len(value.encode())


@dataclass(frozen=True, slots=True)
class CommitSummary:
    """Validated first line of a conventional commit message."""

    value: str
    max_length: InitVar[int] = DEFAULT_SUMMARY_MAX_LENGTH
    warnings: tuple[str, ...] = field(init=False, default=())

    def __post_init__(self, max_length: int) -> None:
        summary = self.value
        summary_len = _byte_len(summary)
        if not summary.strip():
            raise ValidationFailure("commit summary cannot be empty", field="summary", value=summary)
        if summary_len > max_length:
            raise SummaryTooLong(summary_len, max_length)
        warnings: list[str] = []
        first = summary[0]
        if first.isupper():
            warnings.append("summary should start with lowercase")
        if summary_len > SUMMARY_GUIDELINE_LENGTH:
            warnings.append(f"summary exceeds {SUMMARY_GUIDELINE_LENGTH} character guideline")
        if summary.rstrip().endswith("."):
            warnings.append("summary should not end with a period")
        object.__setattr__(self, "warnings", tuple(warnings))

    @classmethod
    def from_raw(cls, raw: str | Self, *, max_length: int = DEFAULT_SUMMARY_MAX_LENGTH) -> Self:
        """Create a summary with a configurable hard length limit."""
        if isinstance(raw, str):
            return cls(raw, max_length=max_length)
        return raw

    def __str__(self) -> str:
        return self.value

    def __repr__(self) -> str:
        return f"CommitSummary({self.value!r})"

    def as_str(self) -> str:
        """Return the summary string."""
        return self.value

    def __len__(self) -> int:
        return _byte_len(self.value)


@dataclass(frozen=True, slots=True)
class ConventionalCommit:
    """A complete conventional commit message."""

    commit_type: CommitType
    summary: CommitSummary
    scope: Scope | None = None
    body: tuple[str, ...] = ()
    footers: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "commit_type", CommitType.from_raw(self.commit_type))
        if self.scope is not None:
            object.__setattr__(self, "scope", Scope.from_raw(self.scope))
        object.__setattr__(self, "summary", CommitSummary.from_raw(self.summary))
        object.__setattr__(self, "body", _string_tuple(self.body))
        object.__setattr__(self, "footers", _string_tuple(self.footers))

    @classmethod
    def from_raw(
        cls,
        *,
        commit_type: str | CommitType,
        summary: str | CommitSummary,
        scope: str | Scope | None = None,
        body: Iterable[str] = (),
        footers: Iterable[str] = (),
        summary_max_length: int = DEFAULT_SUMMARY_MAX_LENGTH,
    ) -> Self:
        """Create a commit from raw model or CLI values."""
        return cls(
            commit_type=CommitType.from_raw(commit_type),
            scope=None if scope is None else Scope.from_raw(scope),
            summary=CommitSummary.from_raw(summary, max_length=summary_max_length),
            body=_string_tuple(body),
            footers=_string_tuple(footers),
        )

    def format_commit_message(self) -> str:
        """Render the conventional commit message."""
        scope = f"({self.scope})" if self.scope else ""
        lines = [f"{self.commit_type}{scope}: {self.summary}"]
        if self.body:
            lines.append("")
            lines.extend(_format_body_line(line) for line in self.body)
        if self.footers:
            lines.append("")
            lines.extend(self.footers)
        return "\n".join(lines)

    def __str__(self) -> str:
        return self.format_commit_message()


@dataclass(frozen=True, slots=True)
class AnalysisDetail:
    """A single analyzed change with optional changelog metadata."""

    text: str
    changelog_category: ChangelogCategory | None = None
    user_visible: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(self, "text", str(self.text))
        if self.changelog_category is not None and not isinstance(self.changelog_category, ChangelogCategory):
            category = _strict_changelog_category(str(self.changelog_category))
            object.__setattr__(self, "changelog_category", category)
        object.__setattr__(self, "user_visible", bool(self.user_visible))

    @classmethod
    def simple(cls, text: str) -> Self:
        """Create a detail without changelog metadata."""
        return cls(text=text)


@dataclass(frozen=True, slots=True)
class ScopeCandidate:
    """Candidate conventional-commit scope with percentage and confidence metadata."""

    path: str
    percentage: float
    confidence: float


def _coerce_analysis_detail(value: Any) -> AnalysisDetail | None:
    if isinstance(value, AnalysisDetail):
        return value if value.text else None
    if isinstance(value, Mapping):
        raw_text = value.get("text")
        text = "" if raw_text is None else str(raw_text)
        if not text:
            return None
        raw_category = value.get("changelog_category")
        category = _strict_changelog_category(raw_category) if isinstance(raw_category, str) else None
        raw_visible = value.get("user_visible")
        user_visible = raw_visible if isinstance(raw_visible, bool) else False
        return AnalysisDetail(text=text, changelog_category=category, user_visible=user_visible)
    if isinstance(value, str):
        return AnalysisDetail.simple(value) if value else None
    return None


def _analysis_details_tuple(values: Any) -> tuple[AnalysisDetail, ...]:
    if values is None:
        return ()
    if isinstance(values, str):
        return (AnalysisDetail.simple(values),) if values else ()
    if isinstance(values, Mapping):
        return ()
    return tuple(detail for value in values if (detail := _coerce_analysis_detail(value)) is not None)


def _value_to_strings(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        trimmed = value.strip()
        if trimmed.startswith("["):
            try:
                decoded = json.loads(trimmed)
            except json.JSONDecodeError:
                decoded = None
            if isinstance(decoded, list):
                strings: list[str] = []
                for item in decoded:
                    strings.extend(_value_to_strings(item))
                return strings
        return [line.strip() for line in value.splitlines() if line.strip()]
    if isinstance(value, Mapping):
        strings = []
        for key, inner in value.items():
            inner_values = _value_to_strings(inner)
            strings.extend([str(key)] if not inner_values else [f"{key}: {item}" for item in inner_values])
        return strings
    if isinstance(value, Iterable):
        strings = []
        for item in value:
            strings.extend(_value_to_strings(item))
        return strings
    return [str(value)]


@dataclass(frozen=True, slots=True)
class ConventionalAnalysis:
    """Structured model analysis for one conventional commit."""

    commit_type: CommitType
    scope: Scope | None = None
    summary: str | None = None
    details: tuple[AnalysisDetail, ...] = ()
    issue_refs: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "commit_type", CommitType.from_raw(self.commit_type))
        object.__setattr__(self, "scope", coerce_optional_scope(self.scope))
        object.__setattr__(self, "details", _analysis_details_tuple(self.details))
        object.__setattr__(self, "issue_refs", _string_tuple(self.issue_refs))

    @classmethod
    def from_raw(
        cls,
        *,
        commit_type: str | CommitType,
        scope: str | Scope | None = None,
        summary: str | None = None,
        details: Iterable[Any] = (),
        issue_refs: Iterable[str] = (),
    ) -> Self:
        """Create an analysis from raw model output, coercing each field."""
        return cls(
            commit_type=CommitType.from_raw(commit_type),
            scope=coerce_optional_scope(scope),
            summary=summary,
            details=_analysis_details_tuple(details),
            issue_refs=_string_tuple(issue_refs),
        )

    @property
    def type(self) -> CommitType:
        """Return the commit type under the JSON field name used by prompts."""
        return self.commit_type

    def body_texts(self) -> list[str]:
        """Return detail text for summary generation."""
        return [detail.text for detail in self.details]


@dataclass(frozen=True, slots=True)
class CommitMetadata:
    """Author, committer, message, parent, and tree metadata for a git commit."""

    hash: str
    message: str
    author_name: str
    author_email: str
    author_date: str
    committer_name: str
    committer_email: str
    committer_date: str
    parents: tuple[str, ...]
    tree_hash: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "parents", _string_tuple(self.parents))

    @property
    def parent_hashes(self) -> tuple[str, ...]:
        """Return parent hashes using the Rust-era field name."""
        return self.parents


class ChangelogCategory(StrEnum):
    """Keep a Changelog section names in render order."""

    ADDED = "Added"
    CHANGED = "Changed"
    FIXED = "Fixed"
    DEPRECATED = "Deprecated"
    REMOVED = "Removed"
    SECURITY = "Security"
    BREAKING = "Breaking Changes"

    @classmethod
    def from_name(cls, name: str) -> Self:
        """Parse a category name, falling back to Changed."""
        normalized = name.strip().lower()
        for category in cls:
            if category.value.lower() == normalized or category.name.lower() == normalized:
                return category
        if normalized == "breaking":
            return cls.BREAKING
        return cls.CHANGED

    @classmethod
    def render_order(cls) -> tuple[Self, ...]:
        """Return changelog render order."""
        return (
            cls.BREAKING,
            cls.ADDED,
            cls.CHANGED,
            cls.DEPRECATED,
            cls.REMOVED,
            cls.FIXED,
            cls.SECURITY,
        )


def _strict_changelog_category(name: str) -> ChangelogCategory:
    normalized = name.strip().lower()
    for category in ChangelogCategory:
        if category.value.lower() == normalized or category.name.lower() == normalized:
            return category
    if normalized == "breaking":
        return ChangelogCategory.BREAKING
    raise ValidationFailure(f"unknown changelog category: {name!r}", field="changelog_category", value=name)


def default_categories() -> list[CategoryConfig]:
    """Return changelog category defaults in render order."""
    return [
        CategoryConfig(
            name="Breaking",
            header="Breaking Changes",
            match=CategoryMatch(body_contains=("breaking", "incompatible")),
        ),
        CategoryConfig(name="Added", match=CategoryMatch(types=("feat",))),
        CategoryConfig(name="Changed", default=True),
        CategoryConfig(name="Deprecated"),
        CategoryConfig(name="Removed", match=CategoryMatch(types=("revert",))),
        CategoryConfig(name="Fixed", match=CategoryMatch(types=("fix",))),
        CategoryConfig(name="Security"),
    ]


@dataclass(frozen=True, slots=True)
class HunkSelector:
    """Selector for hunks included in a file change."""

    kind: str
    start: int | None = None
    end: int | None = None
    pattern: str | None = None

    @classmethod
    def all(cls) -> Self:
        """Select all hunks in a file."""
        return cls(kind="ALL")

    @classmethod
    def lines(cls, start: int, end: int) -> Self:
        """Select a 1-indexed inclusive line range."""
        return cls(kind="Lines", start=start, end=end)

    @classmethod
    def search(cls, pattern: str) -> Self:
        """Select hunks matching a search pattern."""
        return cls(kind="Search", pattern=pattern)


@dataclass(frozen=True, slots=True)
class FileChange:
    """A file path and the hunks selected from it."""

    path: str
    hunks: tuple[HunkSelector, ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "hunks", tuple(self.hunks))


@dataclass(frozen=True, slots=True)
class ChangeGroup:
    """A logical compose group emitted by planning."""

    changes: tuple[FileChange, ...]
    commit_type: CommitType
    scope: Scope | None
    rationale: str
    dependencies: tuple[int, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "commit_type", CommitType.from_raw(self.commit_type))
        if self.scope is not None:
            object.__setattr__(self, "scope", Scope.from_raw(self.scope))
        object.__setattr__(self, "changes", tuple(self.changes))
        object.__setattr__(self, "dependencies", tuple(self.dependencies))

    @property
    def type(self) -> CommitType:
        """Return the commit type under the JSON field name used by prompts."""
        return self.commit_type


@dataclass(frozen=True, slots=True)
class ComposeAnalysis:
    """Result of compose grouping analysis."""

    groups: tuple[ChangeGroup, ...]
    dependency_order: tuple[int, ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "groups", tuple(self.groups))
        object.__setattr__(self, "dependency_order", tuple(self.dependency_order))


@dataclass(frozen=True, slots=True)
class ComposeHunk:
    """A captured diff hunk in a compose snapshot."""

    hunk_id: str
    file_id: str
    path: str
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    header: str
    raw_patch: str
    snippet: str
    semantic_key: str
    synthetic: bool = False


@dataclass(frozen=True, slots=True)
class ComposeFile:
    """A file captured in a compose snapshot."""

    file_id: str
    path: str
    patch_header: str
    full_patch: str
    summary: str
    hunk_ids: tuple[str, ...]
    additions: int
    deletions: int
    is_binary: bool = False
    synthetic_only: bool = False
    old_path: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "hunk_ids", _string_tuple(self.hunk_ids))


class WorktreePinKind(StrEnum):
    """Kinds of worktree pins captured for compose staging."""

    OBJECT = "object"
    DELETED = "deleted"


@dataclass(frozen=True, slots=True)
class WorktreePin:
    """A captured worktree path state for compose snapshot staging."""

    kind: WorktreePinKind
    mode: str | None = None
    oid: str | None = None

    @classmethod
    def object(cls, *, mode: str, oid: str) -> Self:
        """Pin a path to an object already written to the object database."""
        return cls(kind=WorktreePinKind.OBJECT, mode=mode, oid=oid)

    @classmethod
    def deleted(cls) -> Self:
        """Pin a path as absent from the worktree."""
        return cls(kind=WorktreePinKind.DELETED)

    def __post_init__(self) -> None:
        kind = WorktreePinKind(self.kind)
        object.__setattr__(self, "kind", kind)
        if kind is WorktreePinKind.OBJECT and (not self.mode or not self.oid):
            raise ValidationFailure("object pins require mode and oid", field="pins")
        if kind is WorktreePinKind.DELETED and (self.mode is not None or self.oid is not None):
            raise ValidationFailure("deleted pins cannot include mode or oid", field="pins")


@dataclass(frozen=True, slots=True)
class ComposeSnapshot:
    """Diff, file, hunk, and pin data captured once for compose mode."""

    diff: str
    stat: str
    files: tuple[ComposeFile, ...]
    hunks: tuple[ComposeHunk, ...]
    pins: Mapping[str, WorktreePin] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "files", tuple(self.files))
        object.__setattr__(self, "hunks", tuple(self.hunks))
        object.__setattr__(self, "pins", dict(self.pins))

    def file_by_id(self, file_id: str) -> ComposeFile | None:
        """Return a snapshot file by stable file id."""
        return next((file for file in self.files if file.file_id == file_id), None)

    def file_by_path(self, path: str) -> ComposeFile | None:
        """Return a snapshot file by path."""
        return next((file for file in self.files if file.path == path), None)

    def hunk_by_id(self, hunk_id: str) -> ComposeHunk | None:
        """Return a snapshot hunk by stable hunk id."""
        return next((hunk for hunk in self.hunks if hunk.hunk_id == hunk_id), None)

    def hunks_for_file(self, file_id: str) -> list[ComposeHunk]:
        """Return all hunks belonging to a snapshot file."""
        return [hunk for hunk in self.hunks if hunk.file_id == file_id]

    def touched_paths(self) -> list[str]:
        """Worktree paths affected by the snapshot, including pre-rename source paths."""
        paths: list[str] = []
        for file in self.files:
            paths.append(file.path)
            if file.old_path is not None:
                paths.append(file.old_path)
        return paths


def _format_body_line(line: str) -> str:
    stripped = line.strip()
    if stripped.startswith(("- ", "* ")):
        return stripped
    return f"- {stripped}"


def _string_tuple(values: Iterable[Any]) -> tuple[str, ...]:
    if isinstance(values, str):
        return (values,)
    return tuple(str(value) for value in values)


__all__ = [
    "DEFAULT_SUMMARY_MAX_LENGTH",
    "SUMMARY_GUIDELINE_LENGTH",
    "Mode",
    "ApiMode",
    "ResolvedApiMode",
    "resolve_model_name",
    "TypeConfig",
    "CategoryMatch",
    "CategoryConfig",
    "default_types",
    "default_classifier_hint",
    "default_categories",
    "CommitType",
    "coerce_commit_type",
    "Scope",
    "coerce_optional_scope",
    "CommitSummary",
    "ConventionalCommit",
    "AnalysisDetail",
    "ScopeCandidate",
    "ConventionalAnalysis",
    "CommitMetadata",
    "ChangelogCategory",
    "HunkSelector",
    "FileChange",
    "ChangeGroup",
    "ComposeAnalysis",
    "ComposeHunk",
    "ComposeFile",
    "WorktreePinKind",
    "WorktreePin",
    "ComposeSnapshot",
]

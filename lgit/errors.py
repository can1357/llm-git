"""Shared exception hierarchy for lgit."""

from __future__ import annotations

import shlex
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class LgitError(Exception):
    """Base class for all expected lgit failures."""


@dataclass(slots=True)
class GitError(LgitError):
    """A git subprocess or repository operation failed."""

    message: str

    def __str__(self) -> str:
        return f"git: {self.message}"


class GitIndexLocked(GitError):
    """The repository index lock exists and prevents git operations."""

    lock_path: Path

    def __init__(self, lock_path: Path) -> None:
        super().__init__("git index is locked")
        self.lock_path = lock_path

    def __str__(self) -> str:
        quoted = shlex.quote(str(self.lock_path))
        return (
            f"{self.message}: {self.lock_path} — another git process may be running; "
            f"if not, remove the stale lock with: rm {quoted}"
        )


@dataclass(slots=True)
class ApiError(LgitError):
    """An API request failed with a non-successful response."""

    status: int
    body: str

    def __str__(self) -> str:
        return f"API request failed (HTTP {self.status}): {self.body}"


class ApiContextLengthExceeded(ApiError):
    """The selected model could not fit the request in its context window."""

    operation: str
    model: str

    def __init__(self, *, operation: str, model: str, status: int, body: str) -> None:
        super().__init__(status=status, body=body)
        self.operation = operation
        self.model = model

    def __str__(self) -> str:
        return (
            "API request exceeded the model context window during "
            f"{self.operation} ({self.model}, HTTP {self.status}): {self.body}"
        )


@dataclass(slots=True)
class ValidationFailure(LgitError):
    """Domain validation rejected a value."""

    message: str
    field: str | None = None
    value: Any | None = None

    def __str__(self) -> str:
        if self.field is None:
            return self.message
        return f"{self.field}: {self.message}"


@dataclass(slots=True)
class NoChanges(LgitError):
    """No staged, unstaged, or compose changes were available to analyze."""

    mode: str

    def __str__(self) -> str:
        return f"No changes found in {self.mode} mode"


@dataclass(slots=True)
class ConfigError(LgitError):
    """Configuration loading or validation failed."""

    message: str

    def __str__(self) -> str:
        return self.message


class InvalidCommitType(ValidationFailure):
    """A commit type token is not canonical and is not a known alias."""


class InvalidScope(ValidationFailure):
    """A conventional-commit scope has invalid syntax."""


@dataclass(slots=True)
class SummaryTooLong(ValidationFailure):
    """A commit summary exceeded the configured hard limit."""

    length: int = 0
    max_length: int = 0

    def __init__(self, length: int, max_length: int) -> None:
        super().__init__(
            f"summary too long: {length} chars (max {max_length})",
            field="summary",
            value=length,
        )
        self.length = length
        self.max_length = max_length


__all__ = [
    "LgitError",
    "GitError",
    "GitIndexLocked",
    "ApiError",
    "ApiContextLengthExceeded",
    "ValidationFailure",
    "NoChanges",
    "ConfigError",
    "InvalidCommitType",
    "InvalidScope",
    "SummaryTooLong",
]

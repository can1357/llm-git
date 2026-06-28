"""Public package interface for llm-git."""

from __future__ import annotations

from importlib import metadata
from typing import TYPE_CHECKING, Any

try:
    __version__ = metadata.version("lgit-cli")
except metadata.PackageNotFoundError:
    __version__ = "4.2.0"

_CORE_MODEL_EXPORTS = (
    "Mode",
    "ApiMode",
    "ResolvedApiMode",
    "CommitType",
    "Scope",
    "CommitSummary",
    "ConventionalCommit",
    "AnalysisDetail",
    "ConventionalAnalysis",
    "TypeConfig",
    "CategoryConfig",
    "CategoryMatch",
    "ChangelogCategory",
    "FileChange",
    "ChangeGroup",
    "ComposeAnalysis",
    "ComposeHunk",
    "ComposeFile",
    "ComposeSnapshot",
)
_CORE_MODEL_EXPORT_SET = frozenset(_CORE_MODEL_EXPORTS)

__all__ = ["__version__", *_CORE_MODEL_EXPORTS]

if TYPE_CHECKING:
    from .models import (  # noqa: F401
        AnalysisDetail,
        ApiMode,
        CategoryConfig,
        CategoryMatch,
        ChangeGroup,
        ChangelogCategory,
        CommitSummary,
        CommitType,
        ComposeAnalysis,
        ComposeFile,
        ComposeHunk,
        ComposeSnapshot,
        ConventionalAnalysis,
        ConventionalCommit,
        FileChange,
        Mode,
        ResolvedApiMode,
        Scope,
        TypeConfig,
    )


def __getattr__(name: str) -> Any:
    """Load public model exports on first access without importing CLI code."""
    if name in _CORE_MODEL_EXPORT_SET:
        from . import models

        value = getattr(models, name)
        globals()[name] = value
        return value
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


def __dir__() -> list[str]:
    """Return stable public names for interactive help."""
    return sorted({*globals(), *_CORE_MODEL_EXPORTS})

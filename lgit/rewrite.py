"""History rewrite orchestration for regenerating conventional commit messages."""

from __future__ import annotations

import asyncio
import os
import sys
from collections.abc import Sequence
from dataclasses import dataclass
from datetime import datetime
from types import SimpleNamespace
from typing import Any

from . import style
from .analysis import extract_scope_candidates
from .api import generate_conventional_analysis, generate_summary_from_analysis
from .diffing import smart_truncate_diff
from .errors import ValidationFailure
from .git import (
    check_working_tree_clean,
    get_commit_list,
    get_commit_metadata,
    get_git_diff,
    get_git_stat,
    get_head_hash,
    rewrite_history,
    run_git,
)
from .normalization import format_commit_message, post_process_commit_message
from .validation import validate_commit_message


@dataclass(frozen=True, slots=True)
class RewriteConversion:
    """One old-to-new commit-message conversion result."""

    index: int
    commit_hash: str
    old_subject: str
    new_subject: str
    old_message: str
    new_message: str
    error: str | None = None


@dataclass(frozen=True, slots=True)
class RewriteResult:
    """Result of a rewrite-mode run."""

    conversions: tuple[RewriteConversion, ...]
    applied: bool
    dry_run: bool
    preview: int | None
    backup_branch: str | None = None
    error: str | None = None


@dataclass(frozen=True, slots=True)
class _RewriteFailure:
    index: int
    commit_hash: str
    error: str


class _GeneratedMessages(list[str]):
    __slots__ = ("failures",)

    failures: tuple[_RewriteFailure, ...]

    def __init__(self, messages: Sequence[str], failures: tuple[_RewriteFailure, ...]) -> None:
        super().__init__(messages)
        self.failures = failures


async def run_rewrite_mode(args: Any, config: Any) -> RewriteResult:
    """Regenerate commit messages and optionally rewrite history."""

    repo_dir = os.fspath(getattr(args, "dir", "."))
    preview = _optional_int(getattr(args, "rewrite_preview", None))
    dry_run = bool(getattr(args, "rewrite_dry_run", False) or getattr(args, "dry_run", False))

    if not dry_run and preview is None and not check_working_tree_clean(repo_dir):
        raise ValidationFailure("Working directory not clean. Commit or stash changes first.", field="rewrite")

    hashes = get_commit_list(getattr(args, "rewrite_start", None), repo_dir)
    if preview is not None:
        hashes = hashes[:preview]
    commits = [get_commit_metadata(commit_hash, repo_dir) for commit_hash in hashes]

    if dry_run and preview is not None:
        conversions = tuple(
            RewriteConversion(
                index=index,
                commit_hash=commit.hash,
                old_subject=_subject(commit.message, bool(getattr(args, "rewrite_hide_old_types", False))),
                new_subject="",
                old_message=commit.message,
                new_message=commit.message,
            )
            for index, commit in enumerate(commits, start=1)
        )
        return RewriteResult(conversions, applied=False, dry_run=True, preview=preview)

    rewrite_config = _rewrite_config(config)
    messages = await generate_messages_parallel(commits, rewrite_config, args, repo_dir)
    failures = {failure.index: failure.error for failure in getattr(messages, "failures", ())}
    hide_old_types = bool(getattr(args, "rewrite_hide_old_types", False))
    conversions = tuple(
        RewriteConversion(
            index=index,
            commit_hash=commit.hash,
            old_subject=_subject(commit.message, hide_old_types),
            new_subject=_subject(new_message, False),
            old_message=commit.message,
            new_message=new_message,
            error=failures.get(index - 1),
        )
        for index, (commit, new_message) in enumerate(zip(commits, messages, strict=True), start=1)
    )
    error = _rewrite_error(len(failures))

    if dry_run or preview is not None or not commits:
        return RewriteResult(conversions, applied=False, dry_run=dry_run, preview=preview, error=error)

    backup = create_backup_branch(repo_dir)
    rewrite_history(commits, messages, repo_dir)
    return RewriteResult(conversions, applied=True, dry_run=False, preview=None, backup_branch=backup, error=error)


async def generate_messages_parallel(
    commits: Sequence[Any],
    config: Any,
    args: Any,
    dir: str | os.PathLike[str] = ".",
) -> list[str]:
    """Generate replacement commit messages with bounded concurrency."""

    limit = max(1, int(getattr(args, "rewrite_parallel", 10) or 10))
    semaphore = asyncio.Semaphore(limit)
    results: list[str] = [""] * len(commits)
    failures: list[_RewriteFailure | None] = [None] * len(commits)

    async def worker(index: int, commit: Any) -> None:
        async with semaphore:
            try:
                results[index] = await generate_for_commit(commit, config, dir)
            except Exception as exc:
                results[index] = commit.message
                failures[index] = _RewriteFailure(
                    index=index,
                    commit_hash=str(commit.hash),
                    error=str(exc) or type(exc).__name__,
                )

    await asyncio.gather(*(worker(index, commit) for index, commit in enumerate(commits)))
    failure_records = tuple(failure for failure in failures if failure is not None)
    for failure in failure_records:
        _print_conversion_failure(failure, len(commits))
    if failure_records:
        _print_conversion_summary(len(failure_records))
    return _GeneratedMessages(results, failure_records)


async def generate_for_commit(commit: Any, config: Any, dir: str | os.PathLike[str] = ".") -> str:
    """Generate and validate one replacement conventional commit message."""

    commit_hash = commit.hash
    diff = get_git_diff("commit", commit_hash, dir, config)
    stat = get_git_stat("commit", commit_hash, dir, config)
    max_diff_length = int(getattr(config, "max_diff_length", 100_000))
    if len(diff) > max_diff_length:
        diff = smart_truncate_diff(diff, max_diff_length, config)

    scope_candidates, _ = extract_scope_candidates("commit", commit_hash, dir, config)
    analysis = await generate_conventional_analysis(
        config, stat, diff, scope_candidates, user_context=None, debug_output=None
    )
    commit_type = str(analysis.commit_type)
    scope = None if analysis.scope is None else str(analysis.scope)
    details = analysis.body_texts()
    summary = analysis.summary or str(
        await generate_summary_from_analysis(config, analysis, stat=stat, user_context=None)
    )

    message = SimpleNamespace(commit_type=commit_type, scope=scope, summary=summary, body=details, footers=[])
    post_process_commit_message(message, config)
    report = validate_commit_message(message, config, stat=stat)
    if not report.ok:
        joined = "; ".join(issue.message for issue in report.errors)
        raise ValidationFailure(joined or "invalid generated commit message", field="rewrite")
    return format_commit_message(message)


def create_backup_branch(dir: str | os.PathLike[str] = ".") -> str:
    """Create a timestamped backup branch at the current HEAD."""

    head = get_head_hash(dir)
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    branch = f"backup-rewrite-{timestamp}"
    run_git(["branch", branch, head], cwd=dir)
    return branch


def _rewrite_config(config: Any) -> Any:
    return _ExcludeOldMessageProxy(config)


@dataclass(frozen=True, slots=True)
class _ExcludeOldMessageProxy:
    base: Any
    exclude_old_message: bool = True

    def __getattr__(self, name: str) -> Any:
        return getattr(self.base, name)


def _print_conversion_failure(failure: _RewriteFailure, total: int) -> None:
    print(
        f"[{failure.index + 1:3}/{total:3}] "
        f"{style.dim(failure.commit_hash[:8])} {style.error('❌ ERROR:')} {failure.error}",
        file=sys.stderr,
    )


def _print_conversion_summary(failure_count: int) -> None:
    print(
        f"\n{style.warning('⚠️')} {style.bold(str(failure_count))} commits failed, kept original messages",
        file=sys.stderr,
    )


def _rewrite_error(failure_count: int) -> str | None:
    if failure_count == 0:
        return None
    return f"{failure_count} commits failed, kept original messages"


def _subject(message: str, hide_type: bool) -> str:
    first = message.splitlines()[0] if message else ""
    if not hide_type or ":" not in first:
        return first
    return first.split(":", 1)[1].strip()


def _optional_int(value: Any) -> int | None:
    if value in (None, False):
        return None
    return int(value)


__all__ = [
    "RewriteConversion",
    "RewriteResult",
    "create_backup_branch",
    "generate_for_commit",
    "generate_messages_parallel",
    "run_rewrite_mode",
]

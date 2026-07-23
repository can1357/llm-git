"""Public argparse CLI and orchestration for lgit."""

from __future__ import annotations

import argparse
import asyncio
import inspect
import json
import os
import platform
import shutil
import subprocess
import sys
from collections.abc import Callable, Iterable, Sequence
from dataclasses import replace
from pathlib import Path
from typing import Any

import httpx

from . import cache, git, pricing, profile, repo, style
from .analysis import ScopeAnalyzer, extract_scope_candidates
from .api import generate_analysis_with_map_reduce, generate_fast_commit, generate_summary_from_analysis
from .changelog import (
    PreparedChangelogFlow,
    apply_changelog_updates,
    generate_changelog_updates,
    prepare_changelog_flow,
    run_changelog_flow,
)
from .config import CommitConfig
from .diffing import classify_diff_whitespace, smart_truncate_diff, strip_whitespace_only_files, truncate_diff_by_lines
from .errors import LgitError, NoChanges, ValidationFailure
from .map_reduce import FileObservation, should_use_map_reduce
from .markdown_output import fallback_summary
from .models import ConventionalAnalysis, ConventionalCommit, Mode, resolve_model_name
from .normalization import post_process_commit_message
from .tokens import create_token_counter
from .validation import check_type_scope_consistency, validate_commit_message, validate_summary_quality

_COMPLETION_SHELLS = ("bash", "zsh", "fish", "powershell", "elvish")


class _ScopeUnchanged:
    pass


_SCOPE_UNCHANGED = _ScopeUnchanged()


def build_parser() -> argparse.ArgumentParser:
    """Build the public command-line parser."""
    parser = argparse.ArgumentParser(
        prog="lgit",
        description="Generate conventional git commit messages with an LLM.",
        allow_abbrev=False,
    )

    parser.add_argument("context", nargs="*", help="additional context passed to the model")

    standard = parser.add_argument_group("standard")
    standard.add_argument(
        "--mode", choices=[mode.value for mode in Mode], default=Mode.STAGED.value, help="change source to analyze"
    )
    standard.add_argument("--target", help="commit/ref to analyze when --mode commit is used")
    standard.add_argument(
        "--copy", action="store_true", help="copy the generated message to the clipboard instead of committing"
    )
    standard.add_argument("--dry-run", action="store_true", help="print the generated message without committing")
    standard.add_argument("--push", "-p", action="store_true", help="push after creating a commit")
    standard.add_argument("--dir", default=".", help="repository directory")
    standard.add_argument("--model", "-m", help="model override for analysis and summary calls")

    footers = parser.add_argument_group("footers and git commit options")
    footers.add_argument("--fixes", nargs="+", action="append", metavar="REF", help="add Fixes trailers")
    footers.add_argument("--closes", nargs="+", action="append", metavar="REF", help="add Closes trailers")
    footers.add_argument("--resolves", nargs="+", action="append", metavar="REF", help="add Resolves trailers")
    footers.add_argument("--refs", nargs="+", action="append", metavar="REF", help="add Refs trailers")
    footers.add_argument("--breaking", action="store_true", help="add a BREAKING CHANGE trailer")
    footers.add_argument("--sign", "-S", action="store_true", help="GPG-sign the commit")
    footers.add_argument("--signoff", "-s", action="store_true", help="add a Signed-off-by trailer")
    footers.add_argument("--amend", action="store_true", help="amend HEAD instead of creating a new commit")
    footers.add_argument("--skip-hooks", "-n", action="store_true", help="pass --no-verify to git commit")

    config = parser.add_argument_group("config and completion")
    config.add_argument("--config", help="config TOML path override")
    config.add_argument("--completions", choices=_COMPLETION_SHELLS, help="print a shell-completion script")

    routes = parser.add_argument_group("modes")
    routes.add_argument(
        "--fast", "-f", action="store_true", help="use one LLM call to generate the complete commit message"
    )
    routes.add_argument("--rewrite", action="store_true", help="rewrite recent commit messages")
    routes.add_argument("--test", action="store_true", help="run fixture-test mode")

    rewrite = parser.add_argument_group("rewrite")
    rewrite.add_argument(
        "--rewrite-preview", type=int, metavar="N", help="preview the first N commits without rewriting"
    )
    rewrite.add_argument("--rewrite-start", metavar="REF", help="oldest commit/ref to include in rewrite mode")
    rewrite.add_argument(
        "--rewrite-parallel", type=int, default=10, metavar="N", help="maximum parallel rewrite generations"
    )
    rewrite.add_argument(
        "--rewrite-dry-run", action="store_true", help="generate rewrite messages without applying them"
    )
    rewrite.add_argument(
        "--rewrite-hide-old-types", action="store_true", help="hide old conventional types in rewrite previews"
    )
    rewrite.add_argument(
        "--exclude-old-message", action="store_true", help="exclude old commit messages from commit-mode diffs/prompts"
    )

    compose = parser.add_argument_group("compose")
    compose.add_argument("--compose", action="store_true", help="split current worktree changes into multiple commits")
    compose.add_argument("--compose-preview", action="store_true", help="plan compose commits without applying them")
    compose.add_argument("--compose-max-commits", type=int, metavar="N", help="maximum compose commits to plan")
    compose.add_argument(
        "--compose-test-after-each", action="store_true", help="run configured test command after each compose commit"
    )

    changelog = parser.add_argument_group("changelog")
    changelog.add_argument(
        "--no-changelog", action="store_true", help="disable changelog generation for this invocation"
    )

    debug = parser.add_argument_group("debug")
    debug.add_argument("--debug-output", metavar="DIR", help="write raw LLM/debug artifacts to DIR")
    debug.add_argument("--trace-output", metavar="FILE", help="write JSONL profile trace to FILE")

    tests = parser.add_argument_group("test")
    tests.add_argument("--test-update", action="store_true", help="update fixture snapshots")
    tests.add_argument("--test-add", metavar="COMMIT", help="add a fixture from COMMIT")
    tests.add_argument("--test-name", metavar="NAME", help="fixture name for --test-add")
    tests.add_argument("--test-filter", metavar="PATTERN", help="only run fixtures matching PATTERN")
    tests.add_argument("--test-list", action="store_true", help="list fixtures")
    tests.add_argument("--fixtures-dir", metavar="DIR", help="fixture directory")
    tests.add_argument("--test-report", metavar="FILE", help="write fixture-test report")

    return parser


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = build_parser()
    args = parser.parse_args(argv)
    _validate_arg_conflicts(parser, args)
    return args


def main(argv: Sequence[str] | None = None) -> int:
    """Console-script entry point."""
    try:
        args = parse_args(argv)
        return asyncio.run(run_cli(args))
    except KeyboardInterrupt:
        print("lgit: interrupted", file=sys.stderr)
        return 130
    except BrokenPipeError:
        return 1
    except LgitError as exc:
        print(f"lgit: {exc}", file=sys.stderr)
        return 1


async def run_cli(args: argparse.Namespace) -> int:
    """Run the selected CLI workflow."""
    if args.completions:
        print(_completion_script(args.completions), end="")
        return 0

    trace_path = profile.trace_file_path(args)
    trace_guard = profile.init_file_tracing(trace_path) if trace_path is not None else None
    collector = profile.create_timing_collector(profile.timings_enabled(args))
    args._timing_collector = collector
    try:
        with profile.section("load_config", collector):
            config = _load_config(args)
        with profile.section("init_git_command_settings", collector):
            git.init_git_command_settings(config)
        with profile.section("init_cache", collector):
            cache.LlmCache.init(config)

        if _wants_test(args):
            return await _run_test_mode(args, config)

        with profile.section("ensure_git_repo", collector):
            git.ensure_git_repo(args.dir)

        if _wants_rewrite(args):
            return await _run_rewrite(args, config)
        if _wants_compose(args):
            return await _run_compose(args, config)
        return await _run_standard(args, config)
    finally:
        try:
            if collector.enabled:
                report = profile.emit_timing_report(args, collector)
                if args.debug_output:
                    profile.write_timings_json(Path(args.debug_output) / "timings.json", report)
        finally:
            if trace_guard is not None:
                trace_guard.close()


async def _run_standard(args: argparse.Namespace, config: CommitConfig) -> int:
    collector = _timing_collector(args)
    mode = Mode.from_raw(args.mode)
    if mode is Mode.COMPOSE:
        return await _run_compose(args, config)

    if mode is Mode.STAGED:
        with profile.section("auto_stage_if_needed", collector):
            _auto_stage_if_needed(args, config)

    if mode is Mode.STAGED and not args.dry_run:
        with profile.section("write_real_index_tree", collector):
            staged_index_tree = git.write_real_index_tree(args.dir)
    else:
        staged_index_tree = None
    user_context = " ".join(args.context).strip() or None
    with profile.section("read_change_inputs", collector):
        diff, stat, numstat = _read_change_inputs(mode, args, config)
    changelog_runner = (
        _ChangelogRunner(args, config)
        if mode is Mode.STAGED and not args.dry_run and _should_update_changelog(args, config, mode)
        else None
    )

    with profile.section("detect_reformat_shortcut", collector):
        reformat_commit = _detect_reformat_shortcut(diff, config, args)
    if reformat_commit is not None:
        style.status(f"{style.info('›')} {style.dim('Detected whitespace-only changes; recording as reformat')}")
        message = reformat_commit
    else:
        if args.fast:
            message = await _generate_fast_workflow(
                mode, config, args, user_context, diff, stat, numstat, collector, changelog_runner
            )
        else:
            if int(config.auto_fast_threshold_lines) > 0:
                with profile.section("auto_fast_changed_lines", collector):
                    changed_lines = _auto_fast_changed_lines(numstat, config)
                if changed_lines is not None:
                    style.status(
                        f"{style.info('›')} "
                        f"{style.dim(f'Auto-switching to fast mode ({changed_lines} changed lines <= {config.auto_fast_threshold_lines})')}"
                    )
                    message = await _generate_fast_workflow(
                        mode, config, args, user_context, diff, stat, numstat, collector, changelog_runner
                    )
                else:
                    message = await _generate_standard_workflow(
                        mode,
                        args,
                        config,
                        user_context,
                        diff,
                        stat,
                        numstat,
                        collector,
                        changelog_runner,
                    )
            else:
                message = await _generate_standard_workflow(
                    mode,
                    args,
                    config,
                    user_context,
                    diff,
                    stat,
                    numstat,
                    collector,
                    changelog_runner,
                )

    async with profile.section("validate_and_process", collector):
        message, validation_failed = await _validate_and_process(
            _with_cli_footers(message, args, config),
            stat,
            tuple(message.body),
            user_context,
            config,
            args,
            collector,
        )
    if validation_failed:
        print(f"Warning: Generated message failed validation even after retry: {validation_failed}", file=sys.stderr)
        print("You may want to manually edit the message before committing.", file=sys.stderr)

    with profile.section("check_type_scope_consistency", collector):
        _warn_type_scope_consistency(message, stat)
    with profile.section("format_commit_message", collector):
        formatted_message = message.format_commit_message()
    with profile.section("display_output", collector):
        _print_message(message, title="Generated Commit Message")

    if args.copy:
        with profile.section("copy_to_clipboard", collector):
            try:
                _copy_to_clipboard(formatted_message)
                style.status(f"\n{style.success('Copied to clipboard')}")
            except Exception as exc:
                style.status(f"\nNote: Failed to copy to clipboard ({type(exc).__name__}): {exc}")

    if mode in (Mode.STAGED, Mode.UNSTAGED):
        if validation_failed:
            if changelog_runner is not None:
                changelog_runner.cancel()
            print(
                f"\n{style.warning('Skipping commit due to validation failure. Use --dry-run to test or manually commit.')}",
                file=sys.stderr,
            )
            raise ValidationFailure("Commit message validation failed", field="commit")

        snapshot_tree = staged_index_tree
        if mode is Mode.UNSTAGED and not args.dry_run:
            with profile.section("stage_all", collector):
                _stage_all(args.dir)

        if _should_update_changelog(args, config, mode) and not args.dry_run:
            async with profile.section("run_changelog_flow", collector):
                if changelog_runner is not None:
                    await changelog_runner.finish()
                else:
                    await run_changelog_flow(args, config)
            with profile.section("write_real_index_tree_after_changelog", collector):
                snapshot_tree = git.write_real_index_tree(args.dir)
        elif mode is Mode.UNSTAGED and not args.dry_run:
            with profile.section("write_real_index_tree_after_stage", collector):
                snapshot_tree = git.write_real_index_tree(args.dir)

        style.status(f"\n{style.info('Preparing to commit...')}")
        with profile.section("git_commit", collector):
            commit_hash = _commit_staged_message(formatted_message, snapshot_tree, args, config)
        _emit_commit_hash(commit_hash)
        if args.push and not args.dry_run:
            with profile.section("git_push", collector):
                _push_changes(args.dir)

    _print_llm_spend()
    return 0


async def _run_compose(args: argparse.Namespace, config: CommitConfig) -> int:
    from .compose import run_compose_mode

    # Compose commits only the staged tree, using the same scope rule as the regular path:
    # staged changes are used as-is; otherwise stage everything before splitting.
    with profile.section("auto_stage_if_needed", _timing_collector(args)):
        _auto_stage_if_needed(args, config)
    hashes = await run_compose_mode(args, config)
    if hashes:
        noun = "commit" if len(hashes) == 1 else "commits"
        print(style.success(f"{style.icons.SUCCESS} Created {len(hashes)} compose {noun}"))
    elif args.compose_preview:
        print("Compose preview written; no commits created.")
    else:
        print("No compose commits created.")
    _print_llm_spend()
    if hashes and args.push:
        git.run_git(["push"], cwd=args.dir)
    return 0


def _print_llm_spend() -> None:
    """Print the run's LLM token usage and estimated cost, if any was recorded."""
    spend = pricing.session_spend()
    if spend.usage.total_tokens == 0 and spend.cost_usd == 0 and spend.saved_usd == 0:
        return
    usage = spend.usage
    total_in = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens
    tokens = f"{_compact_count(total_in)} in / {_compact_count(usage.output_tokens)} out"
    if spend.cost_usd == 0 and spend.saved_usd == 0:
        line = f"LLM usage: {tokens}"
    else:
        line = f"LLM cost: ${spend.cost_usd:.4f} ({tokens})"
        if spend.saved_usd > 0:
            line += f", saved ${spend.saved_usd:.4f} via cache"
    print(style.dim(line))


def _compact_count(count: int) -> str:
    """Format a token count compactly: 812, 18.9k, 2.3M."""
    if count < 1_000:
        return str(count)
    value, suffix = count / 1_000, "k"
    if round(value, 1) >= 1_000:
        value, suffix = count / 1_000_000, "M"
    return f"{value:.1f}".removesuffix(".0") + suffix


async def _run_rewrite(args: argparse.Namespace, config: CommitConfig) -> int:
    from .rewrite import run_rewrite_mode

    result = await run_rewrite_mode(args, config)
    for conversion in result.conversions:
        old = conversion.old_subject
        new = conversion.new_subject or "(unchanged)"
        print(f"{conversion.index}. {old} -> {new}")
    if result.backup_branch:
        print(f"Backup branch: {result.backup_branch}")
    _print_llm_spend()
    return 0


async def _run_test_mode(args: argparse.Namespace, config: CommitConfig) -> int:
    try:
        from .testing import run_test_mode as runner
    except ImportError:
        from .testing.runner import run_test_mode as runner
    try:
        result: Any = runner(args, config)
        if inspect.isawaitable(result):
            result = await result
    except RuntimeError as exc:
        print(f"lgit test: {exc}", file=sys.stderr)
        return 1
    if isinstance(result, int):
        return result
    all_passed = getattr(result, "all_passed", None)
    if callable(all_passed):
        return 0 if all_passed() else 1
    return 0


def _load_config(args: argparse.Namespace) -> CommitConfig:
    config = CommitConfig.load(args.config)
    if args.model:
        resolved_model = resolve_model_name(args.model)
        config.analysis_model = resolved_model
        config.summary_model = resolved_model
    if args.sign:
        config.gpg_sign = True
    if args.signoff:
        config.signoff = True
    if args.no_changelog:
        config.changelog_enabled = False
    if args.exclude_old_message:
        config.exclude_old_message = True
    return config


def _timing_collector(args: argparse.Namespace) -> profile.TimingCollector | None:
    return getattr(args, "_timing_collector", None)


def _read_change_inputs(mode: Mode, args: argparse.Namespace, config: CommitConfig) -> tuple[str, str, str]:
    return (
        git.get_git_diff(mode, args.target, args.dir, config),
        git.get_git_stat(mode, args.target, args.dir, config),
        git.get_git_numstat(mode, args.target, args.dir, config),
    )


def _auto_stage_if_needed(args: argparse.Namespace, config: CommitConfig) -> None:
    try:
        git.get_git_diff(Mode.STAGED, args.target, args.dir, config)
    except NoChanges:
        has_unstaged = True
        try:
            git.get_git_diff(Mode.UNSTAGED, args.target, args.dir, config)
        except NoChanges:
            has_unstaged = False
        untracked = git.run_git(["ls-files", "--others", "--exclude-standard"], cwd=args.dir).stdout
        if not has_unstaged and not untracked:
            raise NoChanges("working directory (nothing to commit)") from None
        style.status(f"{style.info('›')} {style.dim('No staged changes; running git add -A')}")
        _stage_all(args.dir)


def _stage_all(dir: str | os.PathLike[str]) -> None:
    git.run_git(["add", "-A"], cwd=dir)


class _ChangelogRunner:
    """Overlap changelog generation with commit-message generation for staged commits.

    ``start_with_diff``/``start_with_observations`` kick off boundary generation at
    the earliest safe moment (idempotent; the first call wins). ``finish`` awaits
    generation and applies the updates at the same point the sequential flow used
    to run, so index/worktree side effects still happen only on the commit path.
    """

    __slots__ = ("args", "config", "_prepared", "_task")

    def __init__(self, args: argparse.Namespace, config: CommitConfig) -> None:
        self.args = args
        self.config = config
        self._prepared: PreparedChangelogFlow | None = None
        self._task: asyncio.Task[Any] | None = None

    def start_with_diff(self) -> None:
        """Start generation using each boundary's staged diff."""
        self._start(None)

    def start_with_observations(self, observations: Sequence[FileObservation]) -> None:
        """Start generation from map-phase observations instead of raw diffs."""
        self._start(observations)

    def _start(self, observations: Sequence[FileObservation] | None) -> None:
        if self._task is not None:
            return
        self._prepared = prepare_changelog_flow(self.args, self.config)
        self._task = asyncio.create_task(generate_changelog_updates(self._prepared, self.config, observations))
        # Mark exceptions retrieved in case the run aborts before finish().
        self._task.add_done_callback(lambda task: None if task.cancelled() else task.exception())

    async def finish(self) -> None:
        """Await generation (starting it diff-based if never started) and apply updates."""
        if self._task is None:
            self._start(None)
        assert self._task is not None and self._prepared is not None
        apply_changelog_updates(self._prepared, await self._task)

    def cancel(self) -> None:
        """Drop any in-flight generation without applying it."""
        if self._task is not None:
            self._task.cancel()


async def _generate_fast_workflow(
    mode: Mode,
    config: CommitConfig,
    args: argparse.Namespace,
    user_context: str | None,
    diff: str,
    stat: str,
    numstat: str,
    collector: profile.TimingCollector | None = None,
    changelog_runner: _ChangelogRunner | None = None,
) -> ConventionalCommit:
    with profile.section("strip_whitespace_only", collector):
        diff = strip_whitespace_only_files(diff) or diff
    with profile.section("truncate_diff_by_lines", collector):
        diff = truncate_diff_by_lines(diff, 10_000, config)
    with profile.section("extract_scope_candidates", collector):
        scope_candidates, _wide = (
            extract_scope_candidates(numstat, args.target, args.dir, config) if numstat.strip() else ("(none)", False)
        )
    style.status(f"{style.dim('›')} {style.dim('fast mode:')} {style.model(_resolve_fast_mode_model(args, config))}")
    style.status(f"{style.info('›')} Analyzing {style.bold(mode.value)} changes...")
    if changelog_runner is not None:
        changelog_runner.start_with_diff()
    return await _generate_fast_message(config, stat, diff, scope_candidates, user_context, args, collector)


async def _generate_standard_workflow(
    mode: Mode,
    args: argparse.Namespace,
    config: CommitConfig,
    user_context: str | None,
    diff: str,
    stat: str,
    numstat: str,
    collector: profile.TimingCollector | None = None,
    changelog_runner: _ChangelogRunner | None = None,
) -> ConventionalCommit:
    style.status(f"{style.info('›')} Analyzing {style.bold(mode.value)} changes...")
    with profile.section("strip_whitespace_only", collector):
        diff = strip_whitespace_only_files(diff) or diff
    with profile.section("extract_scope_candidates", collector):
        scope_candidates, _wide = (
            extract_scope_candidates(numstat, args.target, args.dir, config) if numstat.strip() else ("(none)", False)
        )
    with profile.section("create_token_counter", collector):
        token_counter = create_token_counter(config)
    if config.analysis_model == config.summary_model:
        style.status(f"{style.dim('›')} {style.dim('model:')} {style.model(config.analysis_model)}")
    else:
        style.status(
            f"{style.dim('›')} {style.dim('models:')} {style.dim('analysis')} "
            f"{style.model(config.analysis_model)} {style.dim('summary')} {style.model(config.summary_model)}"
        )
    with profile.section("prepare_diff", collector):
        use_map_reduce = should_use_map_reduce(diff, config, token_counter)
        if use_map_reduce:
            analysis_diff = diff
        elif len(diff) > int(config.max_diff_length):
            print(style.warning(f"Applying smart truncation (diff size: {len(diff)} characters)"))
            analysis_diff = smart_truncate_diff(diff, int(config.max_diff_length), config, token_counter)
        else:
            analysis_diff = diff
    on_observations: Callable[[Sequence[FileObservation]], None] | None = None
    if changelog_runner is not None:
        if use_map_reduce:
            # Changelog generation reuses the map phase's per-file observations
            # and overlaps with the reduce call; started by the hook below.
            on_observations = changelog_runner.start_with_observations
        else:
            changelog_runner.start_with_diff()
    analysis = await _generate_analysis(
        config, stat, analysis_diff, scope_candidates, user_context, args, collector, on_observations
    )
    return await _message_from_analysis(analysis, config, stat, user_context, args, collector)


def _resolve_fast_mode_model(args: argparse.Namespace, config: CommitConfig) -> str:
    return str(config.analysis_model if args.model or config.legacy_model else resolve_model_name("haiku"))


async def _generate_fast_message(
    config: CommitConfig,
    stat: str,
    diff: str,
    scope_candidates: str,
    user_context: str | None,
    args: argparse.Namespace,
    collector: profile.TimingCollector | None = None,
) -> ConventionalCommit:
    fast_config = replace(config, analysis_model=_resolve_fast_mode_model(args, config))
    try:
        async with profile.section("generate_fast_commit", collector):
            message = await generate_fast_commit(
                fast_config,
                stat,
                diff,
                scope_candidates,
                user_context=user_context,
                debug_output=args.debug_output,
                debug_prefix="fast",
            )
        with profile.section("validate_fast_commit", collector):
            if validate_commit_message(message, config, stat=stat).ok:
                return message
    except (LgitError, httpx.HTTPError, json.JSONDecodeError, ValueError, TypeError) as exc:
        # Fast generation is best-effort; surface the failure, then fall back to full analysis.
        style.status(style.dim(f"fast commit generation failed ({exc}); using full analysis"))
    analysis = await _generate_analysis(config, stat, diff, scope_candidates, user_context, args, collector)
    return await _message_from_analysis(analysis, config, stat, user_context, args, collector)


async def _generate_analysis(
    config: CommitConfig,
    stat: str,
    diff: str,
    scope_candidates: str,
    user_context: str | None,
    args: argparse.Namespace,
    collector: profile.TimingCollector | None = None,
    on_observations: Callable[[Sequence[FileObservation]], None] | None = None,
) -> ConventionalAnalysis:
    with profile.section("collect_analysis_context", collector):
        project_context = None
        detected = repo.detect(args.dir)
        if detected is not None:
            project_context = detected.format_for_prompt()
        common_scopes = _format_common_scopes(args.dir)
        recent_commits = _format_recent_commits(args.dir)
    async with profile.section("generate_analysis", collector):
        return await generate_analysis_with_map_reduce(
            config,
            stat,
            diff,
            scope_candidates,
            user_context=user_context,
            recent_commits=recent_commits,
            common_scopes=common_scopes,
            project_context=project_context,
            debug_output=args.debug_output,
            debug_prefix="analysis",
            on_observations=on_observations,
        )


async def _message_from_analysis(
    analysis: ConventionalAnalysis,
    config: CommitConfig,
    stat: str,
    user_context: str | None,
    args: argparse.Namespace,
    collector: profile.TimingCollector | None = None,
) -> ConventionalCommit:
    commit_type = str(analysis.commit_type)
    scope = None if analysis.scope is None else str(analysis.scope)
    summary = analysis.summary or ""
    max_retries = max(1, int(config.max_retries))
    for attempt in range(max_retries):
        if not summary or attempt > 0:
            async with profile.section("generate_summary", collector):
                summary = await generate_summary_from_analysis(
                    config,
                    analysis,
                    stat,
                    user_context=user_context,
                    debug_output=args.debug_output,
                    debug_prefix=f"summary-{attempt + 1}",
                )
        with profile.section("validate_summary_quality", collector):
            summary_report = validate_summary_quality(summary, commit_type, stat)
        if summary_report.ok:
            break
        summary = ""
    if not summary:
        with profile.section("fallback_summary", collector):
            summary = fallback_summary(stat, analysis.body_texts(), limit=int(config.summary_guideline))
    with profile.section("build_commit_message", collector):
        message = ConventionalCommit.from_raw(
            commit_type=commit_type,
            scope=scope,
            summary=summary,
            body=analysis.body_texts(),
            summary_max_length=int(config.summary_hard_limit),
        )
        return post_process_commit_message(message, config)


def _with_cli_footers(
    message: ConventionalCommit, args: argparse.Namespace, config: CommitConfig
) -> ConventionalCommit:
    footers = [*message.footers, *_cli_footers(args)]
    if not footers:
        return message
    updated = ConventionalCommit.from_raw(
        commit_type=str(message.commit_type),
        scope=None if message.scope is None else str(message.scope),
        summary=str(message.summary),
        body=message.body,
        footers=footers,
        summary_max_length=int(config.summary_hard_limit),
    )
    return post_process_commit_message(updated, config)


def _cli_footers(args: argparse.Namespace) -> list[str]:
    footers: list[str] = []
    for attr, label in (("fixes", "Fixes"), ("closes", "Closes"), ("resolves", "Resolves"), ("refs", "Refs")):
        for value in _flatten(getattr(args, attr, None)):
            footers.append(f"{label} #{value.strip().lstrip('#')}")
    if args.breaking:
        footers.append("BREAKING CHANGE: This commit introduces breaking changes")
    return footers


async def _validate_and_process(
    message: ConventionalCommit,
    stat: str,
    detail_points: tuple[str, ...],
    user_context: str | None,
    config: CommitConfig,
    args: argparse.Namespace,
    collector: profile.TimingCollector | None = None,
) -> tuple[ConventionalCommit, str | None]:
    current = message
    validation_error: str | None = None
    for attempt in range(3):
        with profile.section("post_process_commit_message", collector):
            current = post_process_commit_message(current, config)
        if attempt == 0 and _first_line_length(current) > int(config.summary_soft_limit):
            print(f"Summary too long ({_first_line_length(current)} chars), retrying generation...", file=sys.stderr)
            try:
                retry_analysis = ConventionalAnalysis.from_raw(
                    commit_type=str(current.commit_type),
                    scope=None if current.scope is None else str(current.scope),
                    details=detail_points,
                )
                async with profile.section("generate_validation_retry_summary", collector):
                    summary = await generate_summary_from_analysis(
                        config,
                        retry_analysis,
                        stat,
                        user_context=user_context,
                        debug_output=None,
                        debug_prefix=None,
                    )
            except Exception as exc:
                print(f"Retry generation failed ({type(exc).__name__}): {exc}, using fallback", file=sys.stderr)
                with profile.section("fallback_validation_summary", collector):
                    summary = fallback_summary(stat, detail_points, limit=int(config.summary_guideline))
            current = _replace_commit(current, summary=summary, config=config, args=args)
            continue

        with profile.section("validate_commit_message", collector):
            report = validate_commit_message(current, config, stat=stat, project_names=_project_names(args.dir))
        if report.ok:
            return current, None

        if current.scope is not None and any(issue.code == "project_name_scope" for issue in report.errors):
            style.warn("Scope matches project name, removing scope...")
            current = _replace_commit(current, scope=None, config=config, args=args)
            with profile.section("validate_commit_message_after_scope_removal", collector):
                report = validate_commit_message(current, config, stat=stat, project_names=_project_names(args.dir))
            if report.ok:
                return current, None
            print(
                f"Validation failed after scope removal: {'; '.join(issue.message for issue in report.errors)}",
                file=sys.stderr,
            )

        validation_error = "; ".join(issue.message for issue in report.errors)
        print(f"Validation attempt {attempt + 1} failed: {validation_error}", file=sys.stderr)
        if attempt < 2:
            with profile.section("fallback_validation_summary", collector):
                summary = fallback_summary(stat, detail_points, limit=int(config.summary_guideline))
            current = _replace_commit(current, summary=summary, config=config, args=args)
            continue
        break
    return current, validation_error


def _first_line_length(message: ConventionalCommit) -> int:
    return len(message.format_commit_message().splitlines()[0])


def _replace_commit(
    message: ConventionalCommit,
    *,
    summary: str | None = None,
    scope: str | None | _ScopeUnchanged = _SCOPE_UNCHANGED,
    config: CommitConfig,
    args: argparse.Namespace,
) -> ConventionalCommit:
    scope_value = (
        (None if message.scope is None else str(message.scope)) if isinstance(scope, _ScopeUnchanged) else scope
    )
    updated = ConventionalCommit.from_raw(
        commit_type=str(message.commit_type),
        scope=scope_value,
        summary=str(message.summary) if summary is None else summary,
        body=message.body,
        footers=message.footers,
        summary_max_length=int(config.summary_hard_limit),
    )
    return post_process_commit_message(updated, config)


def _detect_reformat_shortcut(diff: str, config: CommitConfig, args: argparse.Namespace) -> ConventionalCommit | None:
    report = classify_diff_whitespace(diff)
    if not report.all_whitespace:
        return None
    return _build_reformat_commit(report.whitespace_only_files, config, args)


def _build_reformat_commit(files: Sequence[str], config: CommitConfig, args: argparse.Namespace) -> ConventionalCommit:
    if len(files) == 1:
        name = files[0].rsplit("/", 1)[-1]
        summary = f"reformatted {name}"
    else:
        summary = f"reformatted {len(files)} files"
    message = ConventionalCommit.from_raw(
        commit_type="style",
        summary=summary,
        summary_max_length=int(config.summary_hard_limit),
    )
    return post_process_commit_message(message, config)


def _auto_fast_changed_lines(numstat: str, config: CommitConfig) -> int | None:
    if int(config.auto_fast_threshold_lines) == 0:
        return None
    changed_lines = ScopeAnalyzer.count_changed_lines(numstat, config)
    if changed_lines == 0 or changed_lines > int(config.auto_fast_threshold_lines):
        return None
    return changed_lines


def _commit_staged_message(
    message: str, snapshot_tree: str | None, args: argparse.Namespace, config: CommitConfig
) -> str | None:
    sign = bool(args.sign or config.gpg_sign)
    signoff = bool(args.signoff or config.signoff)
    if args.dry_run:
        _print_dry_run_commit(message, args, config)
        return None
    if snapshot_tree is not None and not git.index_matches_tree(snapshot_tree, args.dir):
        style.status(
            f"{style.info('›')} "
            f"{style.dim('Index changed during generation; committing the analyzed snapshot (hooks skipped)')}"
        )
        commit_hash = git.commit_snapshot_tree(
            message, snapshot_tree, args.dir, sign=sign, signoff=signoff, amend=args.amend
        )
        if commit_hash:
            style.status(
                f"{style.success(style.icons.SUCCESS)} {style.success(f'Successfully committed snapshot as {commit_hash[:8]}')}"
            )
        else:
            style.status(f"{style.info('›')} {style.dim('Snapshot already committed; nothing to do')}")
        return commit_hash
    if snapshot_tree is not None and not args.amend and git.head_tree_is(snapshot_tree, args.dir):
        style.status(
            f"{style.info('›')} "
            f"{style.dim('Staged changes were already committed while generating (HEAD moved); nothing to do')}"
        )
        return None

    git_args = ["commit", "-F", "-"]
    if sign:
        git_args.append("-S")
    if signoff:
        git_args.append("--signoff")
    if args.skip_hooks:
        git_args.append("--no-verify")
    if args.amend:
        git_args.append("--amend")
    git.run_git(git_args, cwd=args.dir, input_text=message)
    commit_hash = git.get_head_hash(args.dir)
    style.status(
        f"{style.success(style.icons.SUCCESS)} {style.success(f'Successfully committed as {commit_hash[:8]}')}"
    )
    return commit_hash


def _emit_commit_hash(commit_hash: str | None) -> None:
    """Print the raw commit hash to stdout for scripts (`h=$(lgit)`).

    Suppressed on a TTY, where ``_commit_staged_message`` already reports the hash in its success
    line — otherwise the full hash would appear redundantly right after it.
    """
    if commit_hash and style.pipe_mode():
        print(commit_hash)


def _print_message(message: ConventionalCommit, *, title: str) -> None:
    text = message.format_commit_message()
    if style.pipe_mode():
        sys.stdout.write(text)
        return
    print(f"\n{style.boxed_message(title, text, style.term_width())}")


def _print_dry_run_commit(message: str, args: argparse.Namespace, config: CommitConfig) -> None:
    sign_flag = " -S" if bool(args.sign or config.gpg_sign) else ""
    signoff_flag = " -s" if bool(args.signoff or config.signoff) else ""
    hooks_flag = " --no-verify" if args.skip_hooks else ""
    amend_flag = " --amend" if args.amend else ""
    escaped = message.replace("\n", "\\n")
    command = f'git commit{sign_flag}{signoff_flag}{hooks_flag}{amend_flag} -m "{escaped}"'
    output = style.boxed_message("DRY RUN", command, min(style.term_width(), 60))
    style.status(f"\n{output}")


def _copy_to_clipboard(text: str) -> None:
    candidates: list[list[str]] = []
    system = platform.system().lower()
    if system == "darwin":
        candidates.append(["pbcopy"])
    elif system == "windows":
        candidates.append(["clip"])
    else:
        candidates.extend((["wl-copy"], ["xclip", "-selection", "clipboard"], ["xsel", "--clipboard", "--input"]))
    for command in candidates:
        if shutil.which(command[0]):
            subprocess.run(command, input=text, text=True, check=True)
            return
    raise ValidationFailure("no supported clipboard command found", field="copy")


def _warn_type_scope_consistency(message: ConventionalCommit, stat: str) -> None:
    report = check_type_scope_consistency(message, stat)
    for issue in report.warnings:
        style.warn(issue.message)


def _should_update_changelog(args: argparse.Namespace, config: CommitConfig, mode: Mode) -> bool:
    return mode in (Mode.STAGED, Mode.UNSTAGED) and bool(config.changelog_enabled) and not bool(args.no_changelog)


def _push_changes(dir: str | os.PathLike[str]) -> None:
    style.status(f"\n{style.info('Pushing changes...')}")
    result = git.run_git(["push"], cwd=dir)
    if result.stdout:
        style.status_text(result.stdout)
    if result.stderr:
        style.status_text(result.stderr)
    style.status(f"{style.success(style.icons.SUCCESS)} {style.success('Successfully pushed!')}")


def _format_recent_commits(dir: str | os.PathLike[str]) -> str | None:
    try:
        commits = git.get_recent_commits(dir, count=10)
    except LgitError:
        return None
    return "\n".join(commits) if commits else None


def _format_common_scopes(dir: str | os.PathLike[str]) -> str | None:
    try:
        scopes = git.get_common_scopes(dir, limit=100)
    except LgitError:
        return None
    if not scopes:
        return None
    return ", ".join(f"{scope} ({count})" for scope, count in scopes[:10])


def _project_names(dir: str | os.PathLike[str]) -> tuple[str, ...]:
    """Names that count as project-wide for scope dropping.

    Combines the repository directory name with the repo's sole top-level Python
    package (e.g. ``lgit`` in the ``llm-git`` repo), so a scope naming the whole
    project is dropped rather than kept. A multi-package repo contributes only its
    directory name, leaving per-package scopes meaningful.
    """
    try:
        root = git.run_git(["rev-parse", "--show-toplevel"], cwd=dir).stdout.strip()
    except LgitError:
        return ()
    if not root:
        return ()
    root_path = Path(root)
    names = [root_path.name]
    packages = [child.name for child in root_path.iterdir() if (child / "__init__.py").is_file()]
    if len(packages) == 1:
        names.append(packages[0])
    return tuple(dict.fromkeys(names))


def _flatten(values: Iterable[Iterable[str]] | None) -> list[str]:
    if not values:
        return []
    return [item for group in values for item in group if item]


def _wants_compose(args: argparse.Namespace) -> bool:
    return bool(args.compose or args.compose_preview or args.mode == Mode.COMPOSE.value)


def _wants_rewrite(args: argparse.Namespace) -> bool:
    return bool(args.rewrite or args.rewrite_preview is not None or args.rewrite_dry_run)


def _wants_test(args: argparse.Namespace) -> bool:
    return bool(
        args.test
        or args.test_update
        or args.test_add
        or args.test_name
        or args.test_filter
        or args.test_list
        or args.fixtures_dir
        or args.test_report
    )


def _validate_arg_conflicts(parser: argparse.ArgumentParser, args: argparse.Namespace) -> None:
    selected_routes = [
        name
        for name, active in (
            ("--compose", _wants_compose(args)),
            ("--rewrite", _wants_rewrite(args)),
            ("--test", _wants_test(args)),
        )
        if active
    ]
    if len(selected_routes) > 1:
        parser.error(f"conflicting modes: {' and '.join(selected_routes)}")
    if args.fast and selected_routes:
        parser.error("--fast cannot be combined with compose, rewrite, or test mode")
    if args.mode == Mode.COMMIT.value and not args.target:
        parser.error("--target is required with --mode commit")
    if args.test_add and not args.test_name:
        parser.error("--test-name is required with --test-add")
    if args.rewrite_parallel is not None and args.rewrite_parallel < 1:
        parser.error("--rewrite-parallel must be at least 1")
    if args.rewrite_preview is not None and args.rewrite_preview < 0:
        parser.error("--rewrite-preview must be non-negative")
    if args.compose_max_commits is not None and args.compose_max_commits < 1:
        parser.error("--compose-max-commits must be at least 1")


def _completion_script(shell: str) -> str:
    specs = _completion_specs(build_parser())
    if shell == "bash":
        return _bash_completion(specs)
    if shell == "zsh":
        return _zsh_completion(specs)
    if shell == "fish":
        return _fish_completion(specs)
    if shell == "powershell":
        return _powershell_completion(specs)
    if shell == "elvish":
        return _elvish_completion(specs)
    raise ValueError(f"unsupported shell: {shell}")


def _completion_specs(parser: argparse.ArgumentParser) -> list[dict[str, Any]]:
    specs: list[dict[str, Any]] = []
    for action in parser._actions:
        if not action.option_strings or action.help is argparse.SUPPRESS:
            continue
        choices = tuple(str(choice) for choice in action.choices) if action.choices is not None else ()
        specs.append(
            {
                "options": tuple(action.option_strings),
                "help": (action.help or "").rstrip("."),
                "choices": choices,
                "takes_value": _action_takes_value(action),
                "metavar": _completion_metavar(action),
            }
        )
    return specs


def _action_takes_value(action: argparse.Action) -> bool:
    return not isinstance(
        action,
        (
            argparse._HelpAction,
            argparse._StoreConstAction,
            argparse._StoreFalseAction,
            argparse._StoreTrueAction,
        ),
    )


def _completion_metavar(action: argparse.Action) -> str:
    if action.metavar is not None:
        if isinstance(action.metavar, tuple):
            return str(action.metavar[0]).lower()
        return str(action.metavar).lower()
    if action.dest == argparse.SUPPRESS:
        return "value"
    return str(action.dest).replace("_", "-")


def _bash_completion(specs: list[dict[str, Any]]) -> str:
    options = " ".join(option for spec in specs for option in spec["options"])
    cases = []
    for spec in specs:
        choices = spec["choices"]
        if not choices:
            continue
        patterns = "|".join(spec["options"])
        cases.append(
            f"        {patterns})\n"
            f'            COMPREPLY=( $(compgen -W {_sh_single_quote(" ".join(choices))} -- "$cur") )\n'
            "            return\n"
            "            ;;"
        )
    case_block = "\n".join(cases)
    return (
        "# bash completion for lgit\n"
        "_lgit() {\n"
        "    local cur prev\n"
        "    cur=${COMP_WORDS[COMP_CWORD]}\n"
        "    prev=${COMP_WORDS[COMP_CWORD-1]}\n"
        '    case "$prev" in\n'
        f"{case_block}\n"
        "    esac\n"
        "    if [[ $cur == -* ]]; then\n"
        f'        COMPREPLY=( $(compgen -W {_sh_single_quote(options)} -- "$cur") )\n'
        "    else\n"
        "        COMPREPLY=()\n"
        "    fi\n"
        "}\n"
        "complete -F _lgit lgit\n"
    )


def _zsh_completion(specs: list[dict[str, Any]]) -> str:
    lines = ["#compdef lgit", "", "_lgit() {", "  _arguments -s -S \\"]
    for spec in specs:
        lines.append(f"    {_zsh_option_spec(spec)} \\")
    lines.append("    '*:context:_files'")
    lines.append("}")
    lines.append("")
    # Work whether the file is autoloaded from $fpath (compsys calls `_lgit`) or
    # sourced directly from a startup file (register the completer via compdef).
    lines.append('if [ "$funcstack[1]" = "_lgit" ]; then')
    lines.append('    _lgit "$@"')
    lines.append("else")
    lines.append("    compdef _lgit lgit")
    lines.append("fi")
    return "\n".join(lines) + "\n"


def _zsh_option_spec(spec: dict[str, Any]) -> str:
    # A brace alternation must stay unquoted so zsh expands `{-h,--help}` into one
    # `_arguments` spec per flag; quoting the whole token makes zsh treat the literal
    # `{-h,--help}[...]` as a single (invalid) argument. The body is quoted separately.
    options = spec["options"]
    opt_expr = "{" + ",".join(options) + "}" if len(options) > 1 else _sh_single_quote(options[0])
    return opt_expr + _sh_single_quote(_zsh_option_body(spec))


def _zsh_option_body(spec: dict[str, Any]) -> str:
    desc = _zsh_escape(spec["help"])
    if spec["choices"]:
        values = " ".join(_zsh_escape(choice) for choice in spec["choices"])
        return f"[{desc}]:{spec['metavar']}:({values})"
    if spec["takes_value"]:
        return f"[{desc}]:{spec['metavar']}:_files"
    return f"[{desc}]"


def _fish_completion(specs: list[dict[str, Any]]) -> str:
    lines = ["# fish completion for lgit", "complete -c lgit -f"]
    for spec in specs:
        parts = ["complete", "-c", "lgit"]
        short = next(
            (option[1:] for option in spec["options"] if option.startswith("-") and not option.startswith("--")), None
        )
        long = next((option[2:] for option in spec["options"] if option.startswith("--")), None)
        if short is not None:
            parts.extend(("-s", short))
        if long is not None:
            parts.extend(("-l", long))
        if spec["choices"]:
            parts.extend(("-xa", " ".join(spec["choices"])))
        elif spec["takes_value"]:
            parts.append("-r")
        if spec["help"]:
            parts.extend(("-d", spec["help"]))
        lines.append(" ".join(_fish_quote(part) for part in parts))
    return "\n".join(lines) + "\n"


def _powershell_completion(specs: list[dict[str, Any]]) -> str:
    options = [option for spec in specs for option in spec["options"]]
    value_entries: list[str] = []
    for spec in specs:
        if spec["choices"]:
            values = ", ".join(_ps_single_quote(choice) for choice in spec["choices"])
            for option in spec["options"]:
                value_entries.append(f"    {_ps_single_quote(option)} = @({values})")
    option_array = ", ".join(_ps_single_quote(option) for option in options)
    value_map = "\n".join(value_entries)
    return (
        "# PowerShell completion for lgit\n"
        "Register-ArgumentCompleter -Native -CommandName lgit -ScriptBlock {\n"
        "    param($wordToComplete, $commandAst, $cursorPosition)\n"
        f"    $options = @({option_array})\n"
        "    $valueMap = @{\n"
        f"{value_map}\n"
        "    }\n"
        "    $tokens = $commandAst.CommandElements | ForEach-Object { $_.ToString() }\n"
        "    $previous = if ($tokens.Count -gt 1) { $tokens[$tokens.Count - 2] } else { '' }\n"
        "    $candidates = if ($valueMap.ContainsKey($previous)) { $valueMap[$previous] } else { $options }\n"
        "    $candidates |\n"
        '        Where-Object { $_ -like "$wordToComplete*" } |\n'
        "        ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }\n"
        "}\n"
    )


def _elvish_completion(specs: list[dict[str, Any]]) -> str:
    options = " ".join(option for spec in specs for option in spec["options"])
    mode_choices = next((" ".join(spec["choices"]) for spec in specs if "--mode" in spec["options"]), "")
    return (
        "# elvish completion for lgit\n"
        "set edit:completion:arg-completer[lgit] = {|@words|\n"
        "    var previous = ''\n"
        "    if (> (count $words) 1) { set previous = $words[-2] }\n"
        f"    if (== $previous '--mode') {{ put {mode_choices} }} else {{ put {options} }}\n"
        "}\n"
    )


def _sh_single_quote(value: str) -> str:
    return "'" + value.replace("'", "'\"'\"'") + "'"


def _fish_quote(value: str) -> str:
    return "'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'"


def _ps_single_quote(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def _zsh_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace("[", "\\[").replace("]", "\\]").replace(":", "\\:")


__all__ = ["build_parser", "main", "parse_args", "run_cli"]

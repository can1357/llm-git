from __future__ import annotations

import argparse
import subprocess
from collections.abc import Callable
from pathlib import Path

import pytest
from lgit import cli, git, profile
from lgit.api import summary_from_holistic_analysis
from lgit.config import CommitConfig
from lgit.models import ConventionalAnalysis


def _args(*argv: str) -> argparse.Namespace:
    return cli.parse_args(argv)


def _analysis(summary: str | None) -> ConventionalAnalysis:
    return ConventionalAnalysis(commit_type="feat", scope="api", summary=summary)


def test_cli_definition_builds_and_parses_representative_args() -> None:
    parser = cli.build_parser()

    completions_args = parser.parse_args(["--completions", "bash"])
    commit_args = cli.parse_args(["--mode", "commit", "--target", "HEAD", "--dry-run"])

    assert parser.prog == "lgit"
    assert completions_args.completions == "bash"
    assert commit_args.mode == "commit"
    assert commit_args.target == "HEAD"
    assert commit_args.dry_run is True


def test_commit_staged_commits_snapshot_on_index_drift(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
) -> None:
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    snapshot_tree = git.write_real_index_tree(repo)

    (repo / "b.txt").write_text("drift\n", encoding="utf-8")
    run_git(repo, "add", "b.txt")

    args = _args("--dir", str(repo))
    commit_hash = cli._commit_staged_message("feat: snapshot commit", snapshot_tree, args, CommitConfig())

    assert commit_hash is not None
    assert run_git(repo, "rev-parse", "HEAD^{tree}").stdout.strip() == snapshot_tree
    assert run_git(repo, "log", "-1", "--format=%s").stdout.strip() == "feat: snapshot commit"
    assert run_git(repo, "diff", "--cached", "--name-only").stdout.strip() == "b.txt"
    assert (repo / "app.py").read_text(encoding="utf-8") == "def value():\n    return 2\n"
    assert (repo / "b.txt").read_text(encoding="utf-8") == "drift\n"


def test_emit_commit_hash_only_prints_to_stdout_when_piped(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    from lgit import style

    # Piped (no TTY): the bare hash is the machine-readable stdout output for scripts.
    monkeypatch.setattr(style, "_PIPE_MODE", True)
    cli._emit_commit_hash("b6eed6edf4b3805b55640b1d69dd665652b9370a")
    assert capsys.readouterr().out.strip() == "b6eed6edf4b3805b55640b1d69dd665652b9370a"

    # On a TTY: suppressed, since the success status line already reports the hash.
    monkeypatch.setattr(style, "_PIPE_MODE", False)
    cli._emit_commit_hash("b6eed6edf4b3805b55640b1d69dd665652b9370a")
    assert capsys.readouterr().out == ""

    # Nothing committed: never prints.
    monkeypatch.setattr(style, "_PIPE_MODE", True)
    cli._emit_commit_hash(None)
    assert capsys.readouterr().out == ""


def test_trace_output_flag_enables_file_profiling() -> None:
    args = _args("--trace-output", "profile.jsonl", "--dry-run")

    assert args.trace_output == "profile.jsonl"
    assert profile.trace_file_path(args) == Path("profile.jsonl")
    assert profile.timings_enabled(args) is True


@pytest.mark.parametrize("shell", ["bash", "zsh", "fish", "powershell", "elvish"])
def test_completions_generate_for_all_shells(shell: str) -> None:
    script = cli._completion_script(shell)

    assert script
    assert "lgit" in script


def test_summary_from_holistic_analysis_ignores_blank_summary() -> None:
    assert summary_from_holistic_analysis(_analysis("   "), CommitConfig()) is None


def test_build_footers_empty() -> None:
    assert cli._cli_footers(_args()) == []


def test_build_footers_cli_fixes() -> None:
    args = _args("--fixes", "123", "#456")

    assert cli._cli_footers(args) == ["Fixes #123", "Fixes #456"]


def test_build_footers_cli_all_types() -> None:
    args = _args("--fixes", "1", "--closes", "2", "--resolves", "3", "--refs", "4")

    assert cli._cli_footers(args) == ["Fixes #1", "Closes #2", "Resolves #3", "Refs #4"]


def test_build_footers_cli_only() -> None:
    args = _args("--fixes", "123")

    assert cli._cli_footers(args) == ["Fixes #123"]


def test_build_footers_breaking_change() -> None:
    args = _args("--breaking")

    assert cli._cli_footers(args) == ["BREAKING CHANGE: This commit introduces breaking changes"]


def test_build_footers_combined() -> None:
    args = _args("--fixes", "100", "--refs", "200", "--breaking")

    assert cli._cli_footers(args) == [
        "Fixes #100",
        "Refs #200",
        "BREAKING CHANGE: This commit introduces breaking changes",
    ]


def test_resolve_fast_mode_model_defaults_to_haiku() -> None:
    assert cli._resolve_fast_mode_model(_args(), CommitConfig()) == "claude-haiku-4-5"


def test_resolve_fast_mode_model_uses_legacy_selector() -> None:
    config = CommitConfig(analysis_model="gpt-5.3-codex-spark", legacy_model="gpt-5.3-codex-spark")

    assert cli._resolve_fast_mode_model(_args(), config) == "gpt-5.3-codex-spark"


def test_auto_fast_changed_lines_matches_small_diff() -> None:
    config = CommitConfig(auto_fast_threshold_lines=200)
    numstat = "120\t70\tsrc/main.rs\n-\t-\tlogo.png"

    assert cli._auto_fast_changed_lines(numstat, config) == 190


def test_auto_fast_changed_lines_skips_large_diff() -> None:
    config = CommitConfig(auto_fast_threshold_lines=200)
    numstat = "120\t90\tsrc/main.rs"

    assert cli._auto_fast_changed_lines(numstat, config) is None


def test_auto_fast_changed_lines_can_be_disabled() -> None:
    config = CommitConfig(auto_fast_threshold_lines=0)
    numstat = "10\t5\tsrc/main.rs"

    assert cli._auto_fast_changed_lines(numstat, config) is None

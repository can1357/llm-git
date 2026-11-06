from __future__ import annotations

import argparse
import asyncio
import os
import shutil
import subprocess
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace

import pytest
from lgit import api, cli, git, profile
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


def test_commit_staged_skips_when_snapshot_already_committed_externally(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
) -> None:
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    snapshot_tree = git.write_real_index_tree(repo)

    # Another process commits the staged changes while lgit is generating.
    run_git(repo, "commit", "-m", "external: committed mid-run")
    external_head = run_git(repo, "rev-parse", "HEAD").stdout.strip()

    args = _args("--dir", str(repo))
    commit_hash = cli._commit_staged_message("feat: snapshot commit", snapshot_tree, args, CommitConfig())

    assert commit_hash is None
    assert run_git(repo, "rev-parse", "HEAD").stdout.strip() == external_head


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


def test_dry_run_pipe_stdout_carries_only_the_commit_message(
    capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    """Pipe-mode stdout must be the commit message only; LLM spend goes to stderr."""
    from lgit import pricing, style
    from lgit.models import ConventionalCommit

    monkeypatch.setattr(style, "_PIPE_MODE", True)
    pricing.reset_session()
    pricing.record_usage("claude-haiku-4-5", pricing.TokenUsage(input_tokens=100, output_tokens=20))

    message = ConventionalCommit.from_raw(
        commit_type="feat", scope="sandbox", summary="added mock feature", body=["Added a sample feature."]
    )
    cli._print_message(message, title="Generated Commit Message")
    cli._print_llm_spend()

    captured = capsys.readouterr()
    assert captured.out == message.format_commit_message()
    assert "LLM cost" in captured.err


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


def test_zsh_completion_brace_expansion_is_unquoted() -> None:
    """Multi-flag options must brace-expand; only the description body is quoted.

    Regression for ``_arguments:comparguments: invalid argument: {-h,--help}[...]``: when the
    whole spec was single-quoted, zsh passed the literal ``{-h,--help}[...]`` as one argument.
    """
    script = cli._completion_script("zsh")

    # Brace group bare, body quoted separately.
    assert "{-h,--help}'[" in script
    # The buggy whole-token quoting must never reappear.
    assert "'{-h,--help}" not in script
    # Choice values stay inside the quoted body (the bare `(...)` is not shell syntax).
    assert ":mode:(staged commit unstaged compose)'" in script
    # Sourced installs register the completer instead of calling it at source time.
    assert "compdef _lgit lgit" in script


@pytest.mark.skipif(shutil.which("zsh") is None, reason="zsh not installed")
def test_zsh_completion_sources_without_error() -> None:
    """The generated script loads cleanly under a real zsh completion system."""
    script = cli._completion_script("zsh")
    result = subprocess.run(
        ["zsh", "-fc", 'autoload -Uz compinit && compinit -u 2>/dev/null; eval "$ZSCRIPT"; echo OK'],
        env={**os.environ, "ZSCRIPT": script},
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    assert "OK" in result.stdout
    assert "can only be called" not in result.stderr
    assert "invalid argument" not in result.stderr


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


def test_load_config_model_override_reaches_every_model_role(tmp_path: Path) -> None:
    config_file = tmp_path / "config.toml"
    config_file.write_text('summary_model = "small-model"\n', encoding="utf-8")

    config = cli._load_config(_args("--config", str(config_file), "-m", "opus"))

    assert config.analysis_model == "claude-opus-4.8"
    assert config.effective_map_model == "claude-opus-4.8"
    assert config.effective_fast_model == "claude-opus-4.8"


def test_load_config_keeps_configured_fast_model_without_override(tmp_path: Path) -> None:
    config_file = tmp_path / "config.toml"
    config_file.write_text('summary_model = "small-model"\nanalysis_model = "big-model"\n', encoding="utf-8")

    config = cli._load_config(_args("--config", str(config_file)))

    assert config.effective_fast_model == "small-model"


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


@pytest.mark.parametrize("auto_fast", [0, 200])
def test_shared_generation_repairs_summary_and_preserves_changelog_copies(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
    monkeypatch: pytest.MonkeyPatch,
    auto_fast: int,
) -> None:
    original = "# Changelog\n\n## [Unreleased]\n\n### Changed\n\n- Returned one as the default value\n"
    authored = "- Added hand-authored migration guidance\n"
    staged = original + authored
    worktree = staged + "- Retained local draft notes\n"
    (repo / "CHANGELOG.md").write_text(original, encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: recorded unreleased behavior")
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    (repo / "CHANGELOG.md").write_text(staged, encoding="utf-8")
    run_git(repo, "add", "app.py", "CHANGELOG.md")
    (repo / "CHANGELOG.md").write_text(worktree, encoding="utf-8")
    operations: list[str] = []
    models: list[str | None] = []

    async def fake_response(config: CommitConfig, spec: api.OneShotSpec) -> SimpleNamespace:
        operations.append(spec.operation)
        models.append(spec.model)
        if spec.operation == "synthesis":
            (repo / "later.py").write_text("later = True\n", encoding="utf-8")
            run_git(repo, "add", "later.py")
            return SimpleNamespace(
                output={
                    "commit": {
                        "type": "fix",
                        "summary": "default result is now two",
                        "details": ["Changed the default result from one to two."],
                    },
                    "changelogs": [
                        {
                            "path": "CHANGELOG.md",
                            "entries": {"Changed": ["Added hand-authored migration guidance"]},
                            "revisions": [
                                {"old": "- Returned one as the default value", "new": "- Returned two by default"},
                            ],
                        }
                    ],
                },
                text_content="",
            )
        if spec.operation == "summary":
            return SimpleNamespace(output={"summary": "default result is now two"}, text_content="")
        if spec.operation == "summary-rewrite":
            return SimpleNamespace(output={"summary": "fixed the default result"}, text_content="")
        raise AssertionError(f"Unexpected independent diff read: {spec.operation}")

    monkeypatch.setattr(api, "_run_oneshot_response", fake_response)
    config = CommitConfig(
        auto_fast_threshold_lines=auto_fast,
        analysis_model="analysis-model",
        fast_model="fast-model",
        summary_model="summary-model",
        judgment_enabled=False,
    )
    asyncio.run(cli._run_standard(_args("--dir", str(repo)), config))

    assert operations == ["synthesis", "summary", "summary-rewrite"]
    assert models[0] == ("fast-model" if auto_fast else "analysis-model")
    assert models[1:] == ["summary-model", "summary-model"]
    assert run_git(repo, "log", "-1", "--format=%s").stdout.strip() == "fix: fixed the default result"
    assert run_git(repo, "show", "HEAD:app.py").stdout == "def value():\n    return 2\n"
    committed = run_git(repo, "show", "HEAD:CHANGELOG.md").stdout
    assert "- Returned two by default" in committed
    assert "- Returned one as the default value" not in committed
    assert committed.count(authored.strip()) == 1
    assert "local draft" not in committed
    assert "local draft" in (repo / "CHANGELOG.md").read_text(encoding="utf-8")
    assert run_git(repo, "diff", "--cached", "--name-only").stdout.strip() == "later.py"
    assert run_git(repo, "cat-file", "-e", "HEAD:later.py", check=False).returncode != 0


def test_shared_generation_does_not_force_docs_type_from_boundary_files(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    package = repo / "packages" / "example"
    package.mkdir(parents=True)
    (package / "CHANGELOG.md").write_text("# Changelog\n\n## [Unreleased]\n", encoding="utf-8")
    (package / "README.md").write_text("Example package.\n", encoding="utf-8")
    prompt = repo / "prompts" / "response.md"
    prompt.parent.mkdir()
    prompt.write_text("Return a successful result.\n", encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "feat: added response policy")

    (package / "README.md").write_text("Example package documentation.\n", encoding="utf-8")
    prompt.write_text("Return a successful result or explain validation errors.\n", encoding="utf-8")
    run_git(repo, "add", ".")

    async def fake_response(config: CommitConfig, spec: api.OneShotSpec) -> SimpleNamespace:
        assert spec.operation == "synthesis"
        return SimpleNamespace(
            output={
                "commit": {
                    "type": "fix",
                    "summary": "clarified validation failures in the response policy",
                    "details": ["Documented validation failures in generated responses."],
                },
                "changelogs": [{"path": "packages/example/CHANGELOG.md", "entries": {}, "revisions": []}],
            },
            text_content="",
        )

    monkeypatch.setattr(api, "_run_oneshot_response", fake_response)
    asyncio.run(cli._run_standard(_args("--dir", str(repo)), CommitConfig(judgment_enabled=False)))

    assert run_git(repo, "log", "-1", "--format=%s").stdout.strip().startswith("fix:")
    assert run_git(repo, "show", "HEAD:prompts/response.md").stdout.endswith("explain validation errors.\n")
    assert run_git(repo, "show", "HEAD:packages/example/CHANGELOG.md").stdout == "# Changelog\n\n## [Unreleased]\n"


@pytest.mark.parametrize("option", ["--dry-run", "--no-changelog"])
def test_nonwriting_or_changelog_disabled_mode_does_not_synthesize_updates(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
    monkeypatch: pytest.MonkeyPatch,
    option: str,
) -> None:
    changelog = "# Changelog\n\n## [Unreleased]\n"
    (repo / "CHANGELOG.md").write_text(changelog, encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: added changelog")
    original_head = run_git(repo, "rev-parse", "HEAD").stdout.strip()
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")

    async def fake_response(config: CommitConfig, spec: api.OneShotSpec) -> SimpleNamespace:
        assert spec.operation == "fast"
        return SimpleNamespace(
            output={"type": "fix", "summary": "fixed the default result", "details": ["Returned two by default."]},
            text_content="",
        )

    monkeypatch.setattr(api, "_run_oneshot_response", fake_response)
    asyncio.run(cli._run_standard(_args("--dir", str(repo), option), CommitConfig(judgment_enabled=False)))

    assert (repo / "CHANGELOG.md").read_text(encoding="utf-8") == changelog
    if option == "--dry-run":
        assert run_git(repo, "rev-parse", "HEAD").stdout.strip() == original_head
        assert run_git(repo, "diff", "--cached", "--name-only").stdout.strip() == "app.py"
    else:
        assert run_git(repo, "show", "HEAD:app.py").stdout == "def value():\n    return 2\n"
        assert run_git(repo, "show", "HEAD:CHANGELOG.md").stdout == changelog


def test_incomplete_shared_changelog_falls_back_without_losing_updates(
    repo: Path,
    run_git: Callable[..., subprocess.CompletedProcess[str]],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    (repo / "CHANGELOG.md").write_text("# Changelog\n\n## [Unreleased]\n", encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: added changelog")
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    operations: list[str] = []

    async def fake_response(config: CommitConfig, spec: api.OneShotSpec) -> SimpleNamespace:
        operations.append(spec.operation)
        commit = {"type": "fix", "summary": "fixed the default result", "details": ["Returned two by default."]}
        if spec.operation == "synthesis":
            return SimpleNamespace(output={"commit": commit, "changelogs": []}, text_content="")
        if spec.operation == "fast":
            return SimpleNamespace(output=commit, text_content="")
        if spec.operation == "changelog":
            return SimpleNamespace(output={"entries": {"Fixed": ["Returned two by default"]}}, text_content="")
        raise AssertionError(spec.operation)

    monkeypatch.setattr(api, "_run_oneshot_response", fake_response)
    asyncio.run(cli._run_standard(_args("--dir", str(repo)), CommitConfig(judgment_enabled=False)))

    assert operations[0] == "synthesis"
    assert sorted(operations[1:]) == ["changelog", "fast"]
    assert run_git(repo, "show", "HEAD:app.py").stdout == "def value():\n    return 2\n"
    assert "- Returned two by default" in run_git(repo, "show", "HEAD:CHANGELOG.md").stdout

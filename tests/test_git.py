from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from subprocess import CompletedProcess
from types import SimpleNamespace

import pytest
from lgit import git

RunGit = Callable[..., CompletedProcess[str]]


def test_git_command_env_applies_background_feature_overrides_when_enabled(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("GIT_CONFIG_COUNT", raising=False)
    monkeypatch.delenv("GIT_CONFIG_KEY_0", raising=False)
    monkeypatch.delenv("GIT_CONFIG_VALUE_0", raising=False)
    monkeypatch.delenv("GIT_CONFIG_KEY_1", raising=False)
    monkeypatch.delenv("GIT_CONFIG_VALUE_1", raising=False)

    env = git.git_command_env(disable_background_features=True)

    assert env["GIT_CONFIG_COUNT"] == "2"
    assert env["GIT_CONFIG_KEY_0"] == "core.fsmonitor"
    assert env["GIT_CONFIG_VALUE_0"] == "false"
    assert env["GIT_CONFIG_KEY_1"] == "core.untrackedCache"
    assert env["GIT_CONFIG_VALUE_1"] == "false"


def test_git_command_env_skips_background_feature_overrides_when_disabled(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("GIT_CONFIG_COUNT", raising=False)
    monkeypatch.delenv("GIT_CONFIG_KEY_0", raising=False)
    monkeypatch.delenv("GIT_CONFIG_VALUE_0", raising=False)

    env = git.git_command_env(disable_background_features=False)

    assert "GIT_CONFIG_COUNT" not in env
    assert "GIT_CONFIG_KEY_0" not in env
    assert "GIT_CONFIG_VALUE_0" not in env


def test_commit_snapshot_tree_commits_snapshot_and_keeps_drifted_staging(repo: Path, run_git: RunGit) -> None:
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    snapshot_tree = git.write_real_index_tree(repo)

    (repo / "drift.txt").write_text("drift\n", encoding="utf-8")
    run_git(repo, "add", "drift.txt")

    commit_hash = git.commit_snapshot_tree("feat: snapshot", snapshot_tree, repo)

    assert commit_hash is not None
    assert run_git(repo, "rev-parse", "HEAD").stdout.strip() == commit_hash
    assert run_git(repo, "rev-parse", "HEAD^{tree}").stdout.strip() == snapshot_tree
    assert run_git(repo, "show", "HEAD:app.py").stdout == "def value():\n    return 2\n"
    assert "drift.txt" not in run_git(repo, "ls-tree", "--name-only", "HEAD").stdout.splitlines()
    assert run_git(repo, "diff", "--cached", "--name-only").stdout.strip() == "drift.txt"
    assert (repo / "drift.txt").read_text(encoding="utf-8") == "drift\n"

    again = git.commit_snapshot_tree("feat: again", snapshot_tree, repo)

    assert again is None
    assert run_git(repo, "rev-parse", "HEAD").stdout.strip() == commit_hash


def test_get_git_diff_uses_minimal_context_when_large(repo: Path, run_git: RunGit) -> None:
    base = "".join(f"base {index}\n" if index % 5 == 0 else f"stable {index}\n" for index in range(200))
    (repo / "file.txt").write_text(base, encoding="utf-8")
    run_git(repo, "add", "file.txt")
    run_git(repo, "commit", "-m", "test: add context fixture")

    changed = "".join(f"changed {index}\n" if index % 5 == 0 else f"stable {index}\n" for index in range(200))
    (repo / "file.txt").write_text(changed, encoding="utf-8")
    run_git(repo, "add", "file.txt")

    config = SimpleNamespace(max_diff_length=500)
    minimal_diff = git.get_git_diff("staged", dir=repo, config=config)

    default_diff = run_git(repo, "diff", "--cached").stdout
    assert len(default_diff.encode()) > config.max_diff_length
    assert minimal_diff == run_git(repo, "diff", "--cached", "-U1").stdout

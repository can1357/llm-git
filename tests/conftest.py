"""Shared pytest scaffolding for the lgit test suite.

Provides git-repo fixtures and subprocess helpers used by the ported Rust unit
tests. Pure (non-git) tests import directly from ``lgit`` and need none of this.
"""

from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

import pytest

PROJECT_ROOT = Path(__file__).resolve().parents[1]


def git_run(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    """Run a git subcommand inside ``repo`` and capture text output."""
    return subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True, check=check)


def lgit_run(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    """Invoke the ``lgit`` CLI as a subprocess against ``repo`` with caching disabled."""
    env = os.environ.copy()
    pythonpath = str(PROJECT_ROOT)
    if existing := env.get("PYTHONPATH"):
        pythonpath = os.pathsep.join((pythonpath, existing))
    env.update(
        {
            "PYTHONPATH": pythonpath,
            "LLM_GIT_CONFIG": str(repo / "missing-config.toml"),
            "LLM_GIT_CACHE_DISABLED": "1",
        }
    )
    return subprocess.run(
        [sys.executable, "-m", "lgit", "--dir", str(repo), *args],
        cwd=PROJECT_ROOT,
        env=env,
        text=True,
        encoding="utf-8",
        capture_output=True,
        check=False,
    )


def init_repo(repo: Path) -> Path:
    """Initialize a git repo with one committed ``app.py`` and return its path."""
    repo.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    git_run(repo, "config", "user.name", "Test User")
    git_run(repo, "config", "user.email", "test@example.com")
    (repo / "app.py").write_text("def value():\n    return 1\n", encoding="utf-8")
    git_run(repo, "add", "app.py")
    git_run(repo, "commit", "-m", "feat: initial")
    return repo


@pytest.fixture
def run_git() -> Callable[..., subprocess.CompletedProcess[str]]:
    """Return the :func:`git_run` helper for invoking git inside a repo."""
    return git_run


@pytest.fixture
def run_lgit() -> Callable[..., subprocess.CompletedProcess[str]]:
    """Return the :func:`lgit_run` helper for invoking the CLI as a subprocess."""
    return lgit_run


@pytest.fixture
def repo(tmp_path: Path) -> Path:
    """An initialized git repo with a single committed file."""
    return init_repo(tmp_path / "repo")


@pytest.fixture
def empty_repo(tmp_path: Path) -> Path:
    """An initialized git repo with no commits yet."""
    repo_dir = tmp_path / "repo"
    repo_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "-q"], cwd=repo_dir, check=True)
    git_run(repo_dir, "config", "user.name", "Test User")
    git_run(repo_dir, "config", "user.email", "test@example.com")
    return repo_dir

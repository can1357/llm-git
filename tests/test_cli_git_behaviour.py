from __future__ import annotations

import pytest
from lgit import git
from lgit.errors import GitIndexLocked


def test_staged_mode_with_no_changes_reports_clean_tree_error(repo, run_lgit) -> None:
    result = run_lgit(repo, "--mode", "staged", "--dry-run")

    assert result.returncode == 1
    assert result.stdout == ""
    assert "No changes found in working directory (nothing to commit) mode" in result.stderr


def test_whitespace_only_staged_changes_dry_run_without_llm(repo, run_git, run_lgit) -> None:
    (repo / "app.py").write_text("def value():\n  return 1\n", encoding="utf-8")
    run_git(repo, "add", "app.py")

    result = run_lgit(repo, "--mode", "staged", "--dry-run")

    assert result.returncode == 0
    assert "style: reformatted" in result.stdout


def test_create_backup_branch_creates_ref_at_head(repo, run_git) -> None:
    head = run_git(repo, "rev-parse", "HEAD").stdout.strip()

    branch = git.create_backup_branch(repo)

    assert branch.startswith("backup-rewrite-")
    ref = run_git(repo, "show-ref", "--verify", f"refs/heads/{branch}").stdout.split()[0]
    assert ref == head


def test_commit_snapshot_tree_preserves_newer_staged_changes(repo, run_git) -> None:
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    captured_tree = git.write_real_index_tree(repo)

    (repo / "app.py").write_text("def value():\n    return 3\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    mid_run_tree = git.write_real_index_tree(repo)

    commit_hash = git.commit_snapshot_tree("feat: commit captured tree", captured_tree, repo)

    assert commit_hash is not None
    assert run_git(repo, "rev-parse", f"{commit_hash}^{{tree}}").stdout.strip() == captured_tree
    assert git.write_real_index_tree(repo) == mid_run_tree
    assert run_git(repo, "diff", "--name-only").stdout == ""

    staged_names = run_git(repo, "diff", "--cached", "--name-only").stdout.splitlines()
    assert staged_names == ["app.py"]
    staged_diff = run_git(repo, "diff", "--cached", "--", "app.py").stdout
    assert "-    return 2" in staged_diff
    assert "+    return 3" in staged_diff


def test_run_git_retries_while_index_lock_clears(repo, monkeypatch) -> None:
    """A lock released during the backoff window lets the command succeed without raising."""
    lock = repo / ".git" / "index.lock"
    lock.write_text("", encoding="utf-8")
    (repo / "app.py").write_text("def value():\n    return 9\n", encoding="utf-8")

    # Simulate a concurrent git process releasing the lock mid-retry: drop it on the
    # first backoff sleep instead of actually waiting.
    def fake_sleep(_seconds: float) -> None:
        lock.unlink(missing_ok=True)

    monkeypatch.setattr(git.time, "sleep", fake_sleep)

    result = git.run_git(["add", "app.py"], cwd=repo)

    assert result.returncode == 0
    assert not lock.exists()
    assert git.run_git(["diff", "--cached", "--name-only"], cwd=repo).stdout.splitlines() == ["app.py"]


def test_run_git_raises_actionable_error_on_persistent_index_lock(repo, monkeypatch) -> None:
    """A lock that never clears exhausts the bounded retries and surfaces remediation."""
    lock = repo / ".git" / "index.lock"
    lock.write_text("", encoding="utf-8")
    (repo / "app.py").write_text("def value():\n    return 9\n", encoding="utf-8")

    monkeypatch.setattr(git.time, "sleep", lambda _seconds: None)

    with pytest.raises(GitIndexLocked) as excinfo:
        git.run_git(["add", "app.py"], cwd=repo)

    message = str(excinfo.value)
    assert str(lock) in message
    assert "remove the stale lock" in message


def test_read_only_git_succeeds_while_index_lock_is_held(repo, monkeypatch) -> None:
    """Read ops never take `index.lock`, so they work even while another process holds it."""
    (repo / "app.py").write_text("def value():\n    return 7\n", encoding="utf-8")  # stat-dirty worktree
    lock = repo / ".git" / "index.lock"
    lock.write_text("", encoding="utf-8")
    # A read must not even attempt a retry; fail loudly if it tries to back off.
    monkeypatch.setattr(git.time, "sleep", lambda _seconds: pytest.fail("read op should not retry on lock"))

    result = git.run_git(["status", "--porcelain"], cwd=repo)

    assert result.returncode == 0
    assert "app.py" in result.stdout
    assert lock.exists()  # the held lock is left untouched
    lock.unlink()

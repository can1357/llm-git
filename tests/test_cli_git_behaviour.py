from __future__ import annotations

from lgit import git


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

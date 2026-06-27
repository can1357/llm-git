from __future__ import annotations

import subprocess
from collections.abc import Callable
from pathlib import Path

from lgit.compose import ComposeExecutableGroup
from lgit.git import TempGitIndex, get_compose_diff, get_compose_stat, read_tree_into_index
from lgit.git import run_git as lgit_run_git
from lgit.git import run_git_bytes as lgit_run_git_bytes
from lgit.models import ComposeSnapshot
from lgit.patch import (
    StageResult,
    build_compose_snapshot,
    create_executable_group_patch,
    force_stage_file_from_base_in_index,
    pin_snapshot_worktree_state,
    stage_executable_group_in_index,
)

GitRunner = Callable[..., subprocess.CompletedProcess[str]]


def _write(repo: Path, path: str, contents: str) -> None:
    full_path = repo / path
    full_path.parent.mkdir(parents=True, exist_ok=True)
    full_path.write_text(contents, encoding="utf-8", newline="")


def _commit_all(repo: Path, run_git: GitRunner, message: str) -> None:
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", message)


def _reset_staging(repo: Path, run_git: GitRunner) -> None:
    run_git(repo, "reset", "-q", "HEAD")


def _staged_diff(repo: Path, run_git: GitRunner) -> str:
    return run_git(repo, "diff", "--cached").stdout


def _staged_diff_in_index(repo: Path, index: TempGitIndex) -> str:
    return lgit_run_git(["diff", "--cached"], cwd=repo, index_file=index.path).stdout


def _show_index_blob(repo: Path, index: TempGitIndex, path: str) -> str:
    return lgit_run_git_bytes(["show", f":{path}"], cwd=repo, index_file=index.path).stdout.decode()


def _snapshot(repo: Path) -> ComposeSnapshot:
    return build_compose_snapshot(get_compose_diff(repo), get_compose_stat(repo))


def _group(
    file_id: str,
    hunk_ids: tuple[str, ...],
    *,
    group_id: str = "G1",
    commit_type: str = "refactor",
    rationale: str = "planned change",
    file_ids: tuple[str, ...] | None = None,
) -> ComposeExecutableGroup:
    return ComposeExecutableGroup(
        group_id=group_id,
        commit_type=commit_type,
        scope=None,
        file_ids=file_ids or (file_id,),
        rationale=rationale,
        dependencies=(),
        hunk_ids=hunk_ids,
    )


def _stage_live(snapshot: ComposeSnapshot, group: ComposeExecutableGroup, repo: Path):
    return stage_executable_group_in_index(snapshot, group, repo, None)


def _fixture_file_original() -> str:
    return "\n".join(
        [
            "fn alpha() {",
            '    println!("alpha");',
            "}",
            "",
            "// spacer 1",
            "// spacer 2",
            "// spacer 3",
            "// spacer 4",
            "// spacer 5",
            "// spacer 6",
            "// spacer 7",
            "// spacer 8",
            "fn beta() {",
            '    println!("beta");',
            "}",
            "",
        ]
    )


def _fixture_file_stage_only() -> str:
    return _fixture_file_original().replace("alpha", "alpha staged")


def _fixture_file_stage_and_unstaged() -> str:
    return _fixture_file_stage_only().replace("beta", "beta unstaged")


def _fixture_file_two_hunks() -> str:
    return "\n".join(
        [
            "fn alpha() {",
            '    println!("alpha changed");',
            "}",
            "",
            "// spacer 1",
            "// spacer 2",
            "// spacer 3",
            "// spacer 4",
            "// spacer 5",
            "// spacer 6",
            "// spacer 7",
            "// spacer 8",
            "fn beta() {",
            '    println!("beta changed");',
            "}",
            "",
        ]
    )


def test_build_compose_snapshot_stable_ids() -> None:
    diff = """diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn alpha() {
+fn alpha_changed() {
     println!("alpha");
 }
diff --git a/tests/lib.rs b/tests/lib.rs
index 3333333..4444444 100644
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@ -10,3 +10,4 @@
 fn test_it() {
+    assert!(true);
 }
"""
    stat = " src/lib.rs | 2 +-\n tests/lib.rs | 1 +\n"

    first = build_compose_snapshot(diff, stat)
    second = build_compose_snapshot(diff, stat)

    assert len(first.files) == 2
    assert [file.file_id for file in first.files] == [file.file_id for file in second.files]
    assert [hunk.hunk_id for hunk in first.hunks] == [hunk.hunk_id for hunk in second.hunks]


def test_get_compose_diff_merges_staged_unstaged_and_untracked(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")

    _write(empty_repo, "src/lib.rs", _fixture_file_stage_only())
    run_git(empty_repo, "add", "src/lib.rs")
    _write(empty_repo, "src/lib.rs", _fixture_file_stage_and_unstaged())
    _write(empty_repo, "notes.txt", "new untracked file\n")

    snapshot = _snapshot(empty_repo)

    assert len(snapshot.files) == 2
    assert snapshot.file_by_path("src/lib.rs") is not None
    assert snapshot.file_by_path("notes.txt") is not None
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    assert len(source_file.hunk_ids) >= 2


def test_stage_executable_group_partial_hunk_from_one_file(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks())
    snapshot = _snapshot(empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    assert len(source_file.hunk_ids) == 2

    _reset_staging(empty_repo, run_git)
    _stage_live(snapshot, _group(source_file.file_id, (source_file.hunk_ids[0],), rationale="first hunk"), empty_repo)

    staged = _staged_diff(empty_repo, run_git)
    assert "alpha changed" in staged
    assert "beta changed" not in staged


def test_stage_executable_group_across_sequential_commits_same_file(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks())
    snapshot = _snapshot(empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    assert len(source_file.hunk_ids) == 2
    first_group = _group(source_file.file_id, (source_file.hunk_ids[0],), group_id="G1", rationale="first hunk")
    second_group = _group(source_file.file_id, (source_file.hunk_ids[1],), group_id="G2", rationale="second hunk")

    _reset_staging(empty_repo, run_git)
    _stage_live(snapshot, first_group, empty_repo)
    run_git(empty_repo, "commit", "-m", "first")
    _stage_live(snapshot, second_group, empty_repo)

    staged = _staged_diff(empty_repo, run_git)
    assert "beta changed" in staged
    assert "alpha changed" not in staged


def test_pinned_staging_ignores_worktree_edits_after_snapshot(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_stage_only())
    snapshot = pin_snapshot_worktree_state(_snapshot(empty_repo), empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None

    _write(empty_repo, "src/lib.rs", _fixture_file_stage_and_unstaged())
    group = _group(source_file.file_id, source_file.hunk_ids, rationale="whole file")
    with TempGitIndex(empty_repo) as index:
        read_tree_into_index(index.path, "HEAD", empty_repo)
        outcome = stage_executable_group_in_index(snapshot, group, empty_repo, index.path)
        staged = _show_index_blob(empty_repo, index, "src/lib.rs")

    assert outcome.result == StageResult.STAGED
    assert not outcome.skipped
    assert staged == _fixture_file_stage_only()
    assert (empty_repo / "src/lib.rs").read_text(encoding="utf-8") == _fixture_file_stage_and_unstaged()


def test_pinned_staging_stages_deletion_even_if_file_recreated(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    (empty_repo / "src/lib.rs").unlink()
    snapshot = pin_snapshot_worktree_state(_snapshot(empty_repo), empty_repo)
    assert snapshot.pins.get("src/lib.rs") is not None
    _write(empty_repo, "src/lib.rs", "fn revived() {}\n")
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None

    with TempGitIndex(empty_repo) as index:
        read_tree_into_index(index.path, "HEAD", empty_repo)
        outcome = stage_executable_group_in_index(
            snapshot,
            _group(source_file.file_id, source_file.hunk_ids, rationale="delete file"),
            empty_repo,
            index.path,
        )
        listed = lgit_run_git(["ls-files", "--", "src/lib.rs"], cwd=empty_repo, index_file=index.path).stdout

    assert outcome.result == StageResult.STAGED
    assert listed == ""
    assert (empty_repo / "src/lib.rs").exists()


def test_create_executable_group_patch_derives_diff_without_staging(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks())
    snapshot = _snapshot(empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    group = _group(source_file.file_id, (source_file.hunk_ids[0],), rationale="first hunk")

    _reset_staging(empty_repo, run_git)
    group_patch = create_executable_group_patch(snapshot, group)

    assert _staged_diff(empty_repo, run_git).strip() == ""
    assert "alpha changed" in group_patch.diff
    assert "beta changed" not in group_patch.diff
    assert "src/lib.rs | 2 +-" in group_patch.stat


def test_stage_executable_groups_ignore_unplanned_files_between_commits(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/a.rs", "fn a() {}\n")
    _write(empty_repo, "src/b.rs", "fn b() {}\n")
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/a.rs", "fn a_changed() {}\n")
    _write(empty_repo, "src/b.rs", "fn b_changed() {}\n")
    snapshot = _snapshot(empty_repo)
    first_file = snapshot.file_by_path("src/a.rs")
    second_file = snapshot.file_by_path("src/b.rs")
    assert first_file is not None and second_file is not None
    first_group = _group(first_file.file_id, first_file.hunk_ids, group_id="G1", rationale="first file")
    second_group = _group(second_file.file_id, second_file.hunk_ids, group_id="G2", rationale="second file")

    _reset_staging(empty_repo, run_git)
    assert _stage_live(snapshot, first_group, empty_repo).result == StageResult.STAGED
    run_git(empty_repo, "commit", "-m", "first")
    _write(empty_repo, "Dockerfile", "FROM scratch\n")
    assert _stage_live(snapshot, second_group, empty_repo).result == StageResult.STAGED
    staged = _staged_diff(empty_repo, run_git)

    assert "b_changed" in staged
    assert "Dockerfile" not in staged
    run_git(empty_repo, "commit", "-m", "second")
    assert "Dockerfile" in get_compose_diff(empty_repo)


def test_stage_executable_group_ignores_same_file_local_edit_between_commits(
    empty_repo: Path, run_git: GitRunner
) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks())
    snapshot = _snapshot(empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    first_group = _group(source_file.file_id, (source_file.hunk_ids[0],), group_id="G1", rationale="first hunk")
    second_group = _group(source_file.file_id, (source_file.hunk_ids[1],), group_id="G2", rationale="second hunk")

    _reset_staging(empty_repo, run_git)
    _stage_live(snapshot, first_group, empty_repo)
    run_git(empty_repo, "commit", "-m", "first")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks().replace("// spacer 4", "// local edit"))
    _stage_live(snapshot, second_group, empty_repo)
    staged = _staged_diff(empty_repo, run_git)

    assert "beta changed" in staged
    assert "local edit" not in staged


def test_stage_executable_group_noops_when_snapshot_patch_already_applied(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_stage_only())
    snapshot = _snapshot(empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    group = _group(source_file.file_id, source_file.hunk_ids, rationale="all hunks")

    _reset_staging(empty_repo, run_git)
    first_result = _stage_live(snapshot, group, empty_repo)
    run_git(empty_repo, "commit", "-m", "applied")
    second_result = _stage_live(snapshot, group, empty_repo)

    # Whole-file change re-stages idempotently via `git add`, so the second run
    # reports STAGED (matching Rust) while leaving the index identical to HEAD.
    assert first_result.result == StageResult.STAGED
    assert second_result.result == StageResult.STAGED
    assert _staged_diff(empty_repo, run_git).strip() == ""


def test_stage_executable_group_reuses_snapshot_patch_not_worktree_contents(
    empty_repo: Path, run_git: GitRunner
) -> None:
    _write(empty_repo, "README.md", "initial\n")
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "notes.txt", "planned\n")
    snapshot = _snapshot(empty_repo)
    notes_file = snapshot.file_by_path("notes.txt")
    assert notes_file is not None
    group = _group(notes_file.file_id, notes_file.hunk_ids, commit_type="docs", rationale="new notes")

    _reset_staging(empty_repo, run_git)
    planned_result = _stage_live(snapshot, group, empty_repo)
    planned_staged = _staged_diff(empty_repo, run_git)
    _reset_staging(empty_repo, run_git)
    _write(empty_repo, "notes.txt", "planned\nlocal edit\n")
    reused_result = _stage_live(snapshot, group, empty_repo)
    reused_staged = _staged_diff(empty_repo, run_git)

    assert planned_result.result == StageResult.STAGED
    assert "+planned" in planned_staged
    assert "local edit" not in planned_staged
    assert reused_result.result == StageResult.STAGED
    assert reused_staged == planned_staged
    assert "local edit" not in reused_staged


def test_stage_executable_group_materializes_new_file_from_snapshot(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "README.md", "initial\n")
    _commit_all(empty_repo, run_git, "initial")
    diff = """diff --git a/notes.txt b/notes.txt
new file mode 100644
index 0000000..0000000
--- /dev/null
+++ b/notes.txt
@@ -1,1 +1,3 @@
+old
+new
+++literal plus
"""
    snapshot = build_compose_snapshot(diff, " notes.txt | 4 +++-\n")
    notes_file = snapshot.file_by_path("notes.txt")
    assert notes_file is not None
    group = _group(notes_file.file_id, notes_file.hunk_ids, commit_type="docs", rationale="new notes")

    _write(empty_repo, "notes.txt", "worktree edit\n")
    _reset_staging(empty_repo, run_git)
    result = _stage_live(snapshot, group, empty_repo)
    staged = _staged_diff(empty_repo, run_git)
    second_result = _stage_live(snapshot, group, empty_repo)

    assert result.result == StageResult.STAGED
    assert "+old" in staged
    assert "+new" in staged
    assert "+++literal plus" in staged
    assert "worktree edit" not in staged
    assert second_result.result == StageResult.ALREADY_APPLIED


def test_stage_executable_group_materializes_empty_new_file_from_snapshot(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "README.md", "initial\n")
    _commit_all(empty_repo, run_git, "initial")
    diff = """diff --git a/empty.txt b/empty.txt
new file mode 100644
index 0000000..0000000
--- /dev/null
+++ b/empty.txt
"""
    snapshot = build_compose_snapshot(diff, " empty.txt | 0\n")
    empty_file = snapshot.file_by_path("empty.txt")
    assert empty_file is not None
    group = _group(empty_file.file_id, empty_file.hunk_ids, commit_type="docs", rationale="empty notes")

    _write(empty_repo, "empty.txt", "worktree edit\n")
    _reset_staging(empty_repo, run_git)
    result = _stage_live(snapshot, group, empty_repo)
    staged = _staged_diff(empty_repo, run_git)

    assert result.result == StageResult.STAGED
    assert "new file mode 100644" in staged
    assert "worktree edit" not in staged


def test_stage_executable_group_materializes_new_gitlink_from_snapshot(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "README.md", "initial\n")
    _commit_all(empty_repo, run_git, "initial")
    oid = "1234567890abcdef1234567890abcdef12345678"
    diff = (
        "diff --git a/vendor/lib b/vendor/lib\n"
        "new file mode 160000\n"
        f"index 0000000..{oid}\n"
        "--- /dev/null\n"
        "+++ b/vendor/lib\n"
        "@@ -0,0 +1 @@\n"
        f"+Subproject commit {oid}\n"
    )
    snapshot = build_compose_snapshot(diff, " vendor/lib | 1 +\n")
    gitlink_file = snapshot.file_by_path("vendor/lib")
    assert gitlink_file is not None
    group = _group(gitlink_file.file_id, gitlink_file.hunk_ids, commit_type="chore", rationale="add submodule")

    _reset_staging(empty_repo, run_git)
    result = _stage_live(snapshot, group, empty_repo)
    staged = _staged_diff(empty_repo, run_git)

    assert result.result == StageResult.STAGED
    assert "new file mode 160000" in staged
    assert f"+Subproject commit {oid}" in staged


def test_stage_executable_group_skips_file_whose_patch_no_longer_applies(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/a.rs", _fixture_file_original())
    _write(empty_repo, "src/b.rs", "fn b() {}\n")
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/a.rs", _fixture_file_two_hunks())
    _write(empty_repo, "src/b.rs", "fn b_changed() {}\n")
    snapshot = _snapshot(empty_repo)
    a_file = snapshot.file_by_path("src/a.rs")
    b_file = snapshot.file_by_path("src/b.rs")
    assert a_file is not None and b_file is not None
    group = _group(
        a_file.file_id,
        (a_file.hunk_ids[0], *b_file.hunk_ids),
        file_ids=(a_file.file_id, b_file.file_id),
        rationale="both files",
    )

    _write(empty_repo, "src/a.rs", _fixture_file_original().replace("alpha", "alpha diverged"))
    run_git(empty_repo, "add", "src/a.rs")
    run_git(empty_repo, "commit", "-m", "diverge a")
    _reset_staging(empty_repo, run_git)
    _write(empty_repo, "src/b.rs", "fn b_changed() {}\n")
    outcome = _stage_live(snapshot, group, empty_repo)
    staged = _staged_diff(empty_repo, run_git)

    assert outcome.result == StageResult.STAGED
    assert len(outcome.skipped) == 1
    assert outcome.skipped[0].path == "src/a.rs"
    assert "b_changed" in staged
    assert "alpha changed" not in staged
    assert "src/a.rs" not in staged


def test_covers_all_modified_file_routes_to_git_add(empty_repo: Path, run_git: GitRunner) -> None:
    _write(empty_repo, "src/lib.rs", _fixture_file_original())
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/lib.rs", _fixture_file_two_hunks())
    snapshot = _snapshot(empty_repo)
    file = snapshot.file_by_path("src/lib.rs")
    assert file is not None

    group_patch = create_executable_group_patch(snapshot, _group(file.file_id, file.hunk_ids, rationale="all hunks"))

    assert group_patch.apply_patches == ()
    assert group_patch.fallback_files == ("src/lib.rs",)


def test_stage_executable_group_in_index_stages_crlf_file_via_git_add(empty_repo: Path, run_git: GitRunner) -> None:
    run_git(empty_repo, "config", "core.autocrlf", "false")
    original = "\r\n".join(
        [
            "fn alpha() {",
            '    println!("alpha");',
            "}",
            "",
            "// spacer 1",
            "// spacer 2",
            "// spacer 3",
            "// spacer 4",
            "fn beta() {",
            '    println!("beta");',
            "}",
            "",
        ]
    )
    modified = original.replace('println!("beta")', 'println!("beta changed")')
    _write(empty_repo, "src/crlf.rs", original)
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/crlf.rs", modified)
    snapshot = _snapshot(empty_repo)
    file = snapshot.file_by_path("src/crlf.rs")
    assert file is not None
    group = _group(file.file_id, file.hunk_ids, commit_type="fix", rationale="crlf change")

    with TempGitIndex(empty_repo) as index:
        read_tree_into_index(index.path, "HEAD", empty_repo)
        outcome = stage_executable_group_in_index(snapshot, group, empty_repo, index.path)
        staged = _show_index_blob(empty_repo, index, "src/crlf.rs")

    assert not outcome.skipped
    assert staged == modified


def test_force_stage_splice_partial_crlf_preserves_eol(empty_repo: Path, run_git: GitRunner) -> None:
    run_git(empty_repo, "config", "core.autocrlf", "false")
    original = "\r\n".join(
        [
            "fn alpha() {",
            '    println!("alpha");',
            "}",
            "",
            "// spacer 1",
            "// spacer 2",
            "// spacer 3",
            "// spacer 4",
            "// spacer 5",
            "// spacer 6",
            "fn beta() {",
            '    println!("beta");',
            "}",
            "",
        ]
    )
    modified = original.replace('println!("alpha")', 'println!("alpha changed")').replace(
        'println!("beta")', 'println!("beta changed")'
    )
    _write(empty_repo, "src/crlf.rs", original)
    _commit_all(empty_repo, run_git, "initial")
    _write(empty_repo, "src/crlf.rs", modified)
    snapshot = _snapshot(empty_repo)
    file = snapshot.file_by_path("src/crlf.rs")
    assert file is not None
    assert len(file.hunk_ids) >= 2
    first_hunk = (file.hunk_ids[0],)

    with TempGitIndex(empty_repo) as index:
        read_tree_into_index(index.path, "HEAD", empty_repo)
        force_stage_file_from_base_in_index(snapshot, file.file_id, first_hunk, empty_repo, index.path)
        staged = _show_index_blob(empty_repo, index, "src/crlf.rs")

    expected = original.replace('println!("alpha")', 'println!("alpha changed")')
    assert staged == expected
    assert 'println!("alpha changed");\r\n' in staged
    assert "beta changed" not in staged
    assert 'println!("beta");\r\n' in staged
    assert "\r\r" not in staged

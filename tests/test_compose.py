from __future__ import annotations

import asyncio
from argparse import Namespace
from collections import defaultdict
from collections.abc import Callable
from pathlib import Path

import pytest
from lgit import compose, git
from lgit.compose import ComposeExecutableGroup, ComposeExecutablePlan, ComposeIntentGroup
from lgit.config import CommitConfig
from lgit.errors import GitError, NoChanges, ValidationFailure
from lgit.models import Scope
from lgit.patch import build_compose_snapshot, pin_snapshot_staged_state


def _shared_file_diff() -> tuple[str, str]:
    return (
        """diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,3 @@
-fn alpha() {
+fn alpha_changed() {
     println!("alpha");
 }
@@ -12,3 +12,3 @@
-fn beta() {
+fn beta_changed() {
     println!("beta");
 }
diff --git a/tests/lib.rs b/tests/lib.rs
index 3333333..4444444 100644
--- a/tests/lib.rs
+++ b/tests/lib.rs
@@ -1,3 +1,4 @@
 fn test_it() {
+    assert!(true);
 }
""",
        " src/lib.rs | 4 ++--\n tests/lib.rs | 1 +\n",
    )


def _build_test_snapshot():
    return build_compose_snapshot(*_shared_file_diff())


def _build_large_snapshot(file_count: int, hunks_per_file: int):
    diff: list[str] = []
    for file_idx in range(file_count):
        path = f"src/module_{file_idx:03}.rs"
        diff.extend(
            [
                f"diff --git a/{path} b/{path}\n",
                "index 1111111..2222222 100644\n",
                f"--- a/{path}\n",
                f"+++ b/{path}\n",
            ]
        )
        for hunk_idx in range(hunks_per_file):
            line_no = hunk_idx * 4 + 1
            diff.extend(
                [
                    f"@@ -{line_no},1 +{line_no},1 @@\n",
                    f"-old_{file_idx}_{hunk_idx}\n",
                    f"+new_{file_idx}_{hunk_idx}\n",
                ]
            )
    return build_compose_snapshot("".join(diff), "")


def _build_multi_area_snapshot():
    diff: list[str] = []
    areas = (
        ("apps/frontend/src/server", 72),
        ("packages/model/src/models", 54),
        ("apps/daemon/src/worker", 43),
        (".github/workflows", 16),
    )
    for prefix, count in areas:
        for file_idx in range(count):
            path = f"{prefix}/file_{file_idx:03}.rs"
            diff.extend(
                [
                    f"diff --git a/{path} b/{path}\n",
                    "index 1111111..2222222 100644\n",
                    f"--- a/{path}\n",
                    f"+++ b/{path}\n",
                    "@@ -1,1 +1,1 @@\n",
                    f"-old_{file_idx}\n",
                    f"+new_{file_idx}\n",
                ]
            )
    return build_compose_snapshot("".join(diff), "")


def _build_shared_intent_plan(snapshot) -> tuple[tuple[ComposeIntentGroup, ...], tuple[int, ...]]:
    source_file = snapshot.file_by_path("src/lib.rs")
    test_file = snapshot.file_by_path("tests/lib.rs")
    assert source_file is not None and test_file is not None
    groups = (
        ComposeIntentGroup(
            group_id="G1",
            commit_type="refactor",
            scope=None,
            file_ids=(source_file.file_id, test_file.file_id),
            rationale="implementation group",
            dependencies=(),
        ),
        ComposeIntentGroup(
            group_id="G2",
            commit_type="refactor",
            scope=None,
            file_ids=(source_file.file_id,),
            rationale="shared file follow-up",
            dependencies=("G1",),
        ),
    )
    return groups, compose.compute_dependency_order(groups)


def test_compose_file_category_treats_prompts_as_functional_source() -> None:
    diff = """diff --git a/prompts/analysis/default.md b/prompts/analysis/default.md
index 1111111..2222222 100644
--- a/prompts/analysis/default.md
+++ b/prompts/analysis/default.md
@@ -1,1 +1,1 @@
-old prompt
+new prompt
diff --git a/system/analysis/default.md b/system/analysis/default.md
index 5555555..6666666 100644
--- a/system/analysis/default.md
+++ b/system/analysis/default.md
@@ -1,1 +1,1 @@
-old system
+new system
diff --git a/README.md b/README.md
index 3333333..4444444 100644
--- a/README.md
+++ b/README.md
@@ -1,1 +1,1 @@
-old docs
+new docs
"""
    snapshot = build_compose_snapshot(diff, "")
    prompt_file = snapshot.file_by_path("prompts/analysis/default.md")
    system_file = snapshot.file_by_path("system/analysis/default.md")
    readme_file = snapshot.file_by_path("README.md")
    assert prompt_file is not None and system_file is not None and readme_file is not None

    assert compose._compose_file_category(prompt_file) == "prompt"
    assert compose._compose_file_category(system_file) == "prompt"
    assert compose._compose_file_category(readme_file) == "docs"

    feat_group = ComposeIntentGroup(
        group_id="G1",
        commit_type="feat",
        scope=None,
        file_ids=(prompt_file.file_id,),
        rationale="prompt behavior change",
        dependencies=(),
    )
    assert compose._group_type_bonus(prompt_file, feat_group) == 10
    assert compose._fallback_commit_type_for_group(snapshot, [], [prompt_file.file_id]).as_str() == "refactor"


def test_execute_compose_with_temp_index_applies_two_group_plan(
    empty_repo: Path,
    run_git: Callable[..., object],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    (empty_repo / "src").mkdir()
    (empty_repo / "src" / "a.rs").write_text("fn a() {}\n", encoding="utf-8")
    (empty_repo / "src" / "b.rs").write_text("fn b() {}\n", encoding="utf-8")
    run_git(empty_repo, "add", ".")
    run_git(empty_repo, "commit", "-m", "initial")
    (empty_repo / "src" / "a.rs").write_text("fn a_changed() {}\n", encoding="utf-8")
    (empty_repo / "src" / "b.rs").write_text("fn b_changed() {}\n", encoding="utf-8")
    run_git(empty_repo, "add", ".")

    diff = git.get_compose_diff(empty_repo)
    stat = git.get_compose_stat(empty_repo)
    snapshot = pin_snapshot_staged_state(build_compose_snapshot(diff, stat), empty_repo)
    a_file = snapshot.file_by_path("src/a.rs")
    b_file = snapshot.file_by_path("src/b.rs")
    assert a_file is not None and b_file is not None
    plan = ComposeExecutablePlan(
        groups=(
            ComposeExecutableGroup(
                group_id="G1",
                commit_type="refactor",
                scope=None,
                file_ids=(a_file.file_id,),
                rationale="change a",
                dependencies=(),
                hunk_ids=a_file.hunk_ids,
            ),
            ComposeExecutableGroup(
                group_id="G2",
                commit_type="refactor",
                scope=None,
                file_ids=(b_file.file_id,),
                rationale="change b",
                dependencies=("G1",),
                hunk_ids=b_file.hunk_ids,
            ),
        ),
        dependency_order=(0, 1),
    )

    async def prepared_messages(*args: object) -> list[str]:
        return ["refactor: change a", "refactor: change b"]

    monkeypatch.setattr(compose, "_prepare_group_messages", prepared_messages)
    args = Namespace(dir=str(empty_repo), compose_preview=False, sign=False, compose_test_after_each=False)
    base_state = compose.capture_compose_base_state(empty_repo)

    hashes = asyncio.run(compose.execute_compose(snapshot, plan, CommitConfig(), args, base_state))

    assert len(hashes) == 2
    assert git.get_head_hash(empty_repo) == hashes[1]
    assert run_git(empty_repo, "diff", "--cached").stdout.strip() == ""


def test_execute_compose_failure_before_update_ref_preserves_real_index(
    empty_repo: Path,
    run_git: Callable[..., object],
) -> None:
    (empty_repo / "src").mkdir()
    (empty_repo / "src" / "lib.rs").write_text("old\n", encoding="utf-8")
    (empty_repo / "sentinel.txt").write_text("base\n", encoding="utf-8")
    run_git(empty_repo, "add", ".")
    run_git(empty_repo, "commit", "-m", "initial")
    initial_head = git.get_head_hash(empty_repo)

    (empty_repo / "src" / "lib.rs").write_text("changed\n", encoding="utf-8")
    (empty_repo / "sentinel.txt").write_text("base\nstaged sentinel\n", encoding="utf-8")
    run_git(empty_repo, "add", ".")
    staged_before = run_git(empty_repo, "diff", "--cached").stdout
    assert "staged sentinel" in staged_before

    diff = git.get_compose_diff(empty_repo)
    stat = git.get_compose_stat(empty_repo)
    snapshot = pin_snapshot_staged_state(build_compose_snapshot(diff, stat), empty_repo)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    plan = ComposeExecutablePlan(
        groups=(
            ComposeExecutableGroup(
                group_id="G1",
                commit_type="fix",
                scope=None,
                file_ids=(source_file.file_id,),
                rationale="unstageable group",
                dependencies=(),
                hunk_ids=("F999-H001",),
            ),
        ),
        dependency_order=(0,),
    )
    args = Namespace(dir=str(empty_repo), compose_preview=False, sign=False, compose_test_after_each=False)
    base_state = compose.capture_compose_base_state(empty_repo)

    with pytest.raises(ValidationFailure, match="unknown hunk id"):
        asyncio.run(compose.execute_compose(snapshot, plan, CommitConfig(), args, base_state))

    assert git.get_head_hash(empty_repo) == initial_head
    assert run_git(empty_repo, "diff", "--cached").stdout == staged_before


def test_run_compose_mode_loops_until_staged_diff_empty(monkeypatch: pytest.MonkeyPatch) -> None:
    rounds: list[int] = []

    async def fake_round(args: object, config: object, round_number: int) -> list[str]:
        rounds.append(round_number)
        return [f"hash{round_number}"]

    # First post-round check still sees staged changes; the second is clean.
    remaining = iter(["staged diff remains"])

    def fake_diff(*args: object, **kwargs: object) -> str:
        try:
            return next(remaining)
        except StopIteration:
            raise NoChanges("compose") from None

    monkeypatch.setattr(compose, "run_compose_round", fake_round)
    monkeypatch.setattr(compose.git, "get_compose_diff", fake_diff)
    args = Namespace(dir=".", compose_preview=False)

    hashes = asyncio.run(compose.run_compose_mode(args, CommitConfig()))

    assert hashes == ["hash1", "hash2"]
    assert rounds == [1, 2]


def test_run_compose_mode_errors_when_a_round_makes_no_progress(monkeypatch: pytest.MonkeyPatch) -> None:
    async def fake_round(args: object, config: object, round_number: int) -> list[str]:
        return []

    monkeypatch.setattr(compose, "run_compose_round", fake_round)
    monkeypatch.setattr(compose.git, "get_compose_diff", lambda *a, **k: "staged diff remains")
    args = Namespace(dir=".", compose_preview=False)

    with pytest.raises(GitError, match="no progress"):
        asyncio.run(compose.run_compose_mode(args, CommitConfig()))


def test_auto_assign_hunks_marks_shared_file_ambiguous() -> None:
    snapshot = _build_test_snapshot()
    intent_groups, _ = _build_shared_intent_plan(snapshot)

    assigned, ambiguous = compose._auto_assign_hunks(snapshot, intent_groups)

    assert len(ambiguous) == 1
    test_file = snapshot.file_by_path("tests/lib.rs")
    assert test_file is not None
    assigned_to_g1 = assigned["G1"]
    assert all(hunk_id in assigned_to_g1 for hunk_id in test_file.hunk_ids)


def test_ambiguous_fallback_merges_and_prunes_empty_group() -> None:
    snapshot = _build_test_snapshot()
    intent_groups, dependency_order = _build_shared_intent_plan(snapshot)
    assigned, ambiguous_files = compose._auto_assign_hunks(snapshot, intent_groups)
    assigned = defaultdict(set, assigned)
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    hunk_context = compose._ambiguous_hunk_context(ambiguous_files)
    valid_group_ids = {group.group_id for group in intent_groups}

    evaluation = compose._evaluate_binding(
        [
            {"group_id": "G1", "hunk_ids": (source_file.hunk_ids[0], source_file.hunk_ids[1])},
            {"group_id": "G2", "hunk_ids": (source_file.hunk_ids[1],)},
        ],
        hunk_context,
        valid_group_ids,
        snapshot,
    )
    for group_id, hunk_ids in evaluation.assigned.items():
        assigned[group_id].update(hunk_ids)
    group_rank = {intent_groups[idx].group_id: position for position, idx in enumerate(dependency_order)}
    compose._assign_unresolved_hunks(evaluation.unresolved, assigned, ambiguous_files, group_rank)

    executable_plan = compose._finalize_executable_plan(snapshot, intent_groups, assigned)

    assert len(executable_plan.groups) == 1
    assert executable_plan.groups[0].group_id == "G1"
    assert all(hunk_id in executable_plan.groups[0].hunk_ids for hunk_id in source_file.hunk_ids)


def test_validate_executable_plan_rejects_overlap() -> None:
    snapshot = _build_test_snapshot()
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None
    executable_plan = ComposeExecutablePlan(
        groups=(
            ComposeExecutableGroup(
                group_id="G1",
                commit_type="refactor",
                scope=None,
                file_ids=(source_file.file_id,),
                rationale="group one",
                dependencies=(),
                hunk_ids=(source_file.hunk_ids[0],),
            ),
            ComposeExecutableGroup(
                group_id="G2",
                commit_type="refactor",
                scope=None,
                file_ids=(source_file.file_id,),
                rationale="group two",
                dependencies=(),
                hunk_ids=(source_file.hunk_ids[0], source_file.hunk_ids[1]),
            ),
        ),
        dependency_order=(0, 1),
    )

    with pytest.raises(ValidationFailure, match="assigned to both"):
        compose._validate_executable_plan(snapshot, executable_plan)


def test_normalize_intent_plan_maps_path_references_to_file_ids() -> None:
    snapshot = _build_test_snapshot()
    planning_index = compose._build_planning_index(snapshot)
    groups = (
        ComposeIntentGroup(
            group_id="G1",
            commit_type="refactor",
            scope=None,
            file_ids=("src/lib.rs", "`tests/lib.rs`"),
            rationale="normalize file references",
            dependencies=(),
        ),
    )

    normalized_groups = compose._normalize_intent_plan(snapshot, planning_index, groups, CommitConfig(), 20)

    assert len(normalized_groups) == 1
    assert normalized_groups[0].file_ids == tuple(file.file_id for file in snapshot.files)


def test_normalize_intent_plan_repairs_missing_files() -> None:
    snapshot = _build_test_snapshot()
    planning_index = compose._build_planning_index(snapshot)
    source_file = snapshot.file_by_path("src/lib.rs")
    test_file = snapshot.file_by_path("tests/lib.rs")
    assert source_file is not None and test_file is not None
    groups = (
        ComposeIntentGroup(
            group_id="G1",
            commit_type="refactor",
            scope=None,
            file_ids=(source_file.file_id,),
            rationale="partial coverage",
            dependencies=(),
        ),
    )

    normalized_groups = compose._normalize_intent_plan(snapshot, planning_index, groups, CommitConfig(), 20)

    assert len(normalized_groups) == 1
    assert source_file.file_id in normalized_groups[0].file_ids
    assert test_file.file_id in normalized_groups[0].file_ids


def test_normalize_intent_plan_drops_placeholder_targets_and_repairs_dependencies() -> None:
    snapshot = _build_multi_area_snapshot()
    planning_index = compose._build_planning_index(snapshot)
    frontend_target = next(target for target in planning_index.targets if target.label.startswith("apps/frontend"))
    model_target = next(target for target in planning_index.targets if target.label.startswith("packages/model"))
    groups = (
        ComposeIntentGroup(
            group_id="G1",
            commit_type="refactor",
            scope=Scope("apps/frontend"),
            file_ids=("G3_PLACEHOLDER", frontend_target.target_id),
            rationale="frontend platform updates",
            dependencies=("group 2", "G1"),
        ),
        ComposeIntentGroup(
            group_id="G2",
            commit_type="refactor",
            scope=Scope("packages/model"),
            file_ids=("UNKNOWN_TARGET", model_target.target_id),
            rationale="model storage updates",
            dependencies=("F5",),
        ),
    )

    normalized_groups = compose._normalize_intent_plan(snapshot, planning_index, groups, CommitConfig(), 20)

    assert len(normalized_groups) == 2
    assert all(file_id.startswith("F") for file_id in normalized_groups[0].file_ids)
    assert "G3_PLACEHOLDER" not in normalized_groups[0].file_ids
    assert "UNKNOWN_TARGET" not in normalized_groups[1].file_ids
    assert normalized_groups[0].dependencies == ("G2",)
    assert normalized_groups[1].dependencies == ()


def test_render_snapshot_summary_keeps_all_hunks_for_small_snapshot() -> None:
    snapshot = _build_test_snapshot()
    summary = compose._render_snapshot_summary(snapshot, [])
    source_file = snapshot.file_by_path("src/lib.rs")
    assert source_file is not None

    assert "# snapshot compacted" not in summary
    for hunk_id in source_file.hunk_ids:
        assert hunk_id in summary


def test_render_snapshot_summary_compacts_large_snapshot() -> None:
    snapshot = _build_large_snapshot(160, 4)
    summary = compose._render_snapshot_summary(snapshot, [])

    assert "# snapshot compacted" in summary
    assert "- F001 src/module_000.rs (+4/-4, 4 hunks)" in summary
    assert "F001-H001" in summary
    assert "F001-H004" in summary
    assert "F001-H002" not in summary
    assert "F001-H003" not in summary
    assert "... 2 more hunks omitted from F001" in summary


def test_build_planning_index_uses_area_targets_for_large_snapshot() -> None:
    snapshot = _build_multi_area_snapshot()
    planning_index = compose._build_planning_index(snapshot)

    assert planning_index.mode == compose.PlanningMode.AREA
    assert len(planning_index.targets) < len(snapshot.files)
    assert any(target.label.startswith("apps/frontend") for target in planning_index.targets)
    assert "planning over" in compose._render_planning_stat(planning_index)


def test_normalize_intent_plan_expands_area_targets() -> None:
    snapshot = _build_multi_area_snapshot()
    planning_index = compose._build_planning_index(snapshot)
    midpoint = len(planning_index.targets) // 2
    first_group_targets = tuple(target.label for target in planning_index.targets[:midpoint])
    second_group_targets = tuple(target.label for target in planning_index.targets[midpoint:])
    groups = (
        ComposeIntentGroup("G1", "refactor", None, first_group_targets, "frontend and model", ()),
        ComposeIntentGroup("G2", "refactor", None, second_group_targets, "daemon and ci", ()),
    )

    normalized_groups = compose._normalize_intent_plan(snapshot, planning_index, groups, CommitConfig(), 20)

    assert len(normalized_groups) == 2
    assert all(file_id.startswith("F") for group in normalized_groups for file_id in group.file_ids)
    assert first_group_targets != normalized_groups[0].file_ids
    assert len({file_id for group in normalized_groups for file_id in group.file_ids}) == len(snapshot.files)


def test_large_patch_fallback_splits_monolithic_area_plan() -> None:
    snapshot = _build_multi_area_snapshot()
    planning_index = compose._build_planning_index(snapshot)
    monolithic_group = ComposeIntentGroup(
        group_id="G1",
        commit_type="refactor",
        scope=None,
        file_ids=tuple(file.file_id for file in snapshot.files),
        rationale="repo-wide refactor",
        dependencies=(),
    )

    assert compose._should_force_large_patch_fallback(snapshot, planning_index, [monolithic_group], 6)
    fallback_groups = compose._fallback_intent_groups(snapshot, planning_index, 6, CommitConfig())

    assert len(fallback_groups) >= 3
    assert len({file_id for group in fallback_groups for file_id in group.file_ids}) == len(snapshot.files)
    assert any("frontend" in group.rationale for group in fallback_groups)


def test_should_collect_compose_observations_skips_area_mode() -> None:
    snapshot = _build_large_snapshot(160, 4)
    config = CommitConfig(map_reduce_threshold=1_000)
    counter = compose._create_token_counter(config)

    # Large (area-mode) snapshots skip observation collection even though map-reduce would apply.
    assert compose._should_use_map_reduce(snapshot.diff, config, counter)
    collects = not compose._is_large_compose_snapshot(snapshot) and compose._should_use_map_reduce(
        snapshot.diff, config, counter
    )
    assert not collects


def test_compose_analysis_strategy_uses_map_reduce_for_large_diff() -> None:
    config = CommitConfig(map_reduce_threshold=20)
    counter = compose._create_token_counter(config)
    payload = "a" * 200
    diff = f"diff --git a/a.rs b/a.rs\n@@ -0,0 +1 @@\n+{payload}"

    assert compose._compose_analysis_strategy(diff, config, counter) == compose.ComposeAnalysisStrategy.MAP_REDUCE


def test_compose_analysis_strategy_truncates_when_map_reduce_disabled() -> None:
    config = CommitConfig(map_reduce_enabled=False, max_diff_tokens=1, max_diff_length=10_000)
    counter = compose._create_token_counter(config)

    assert compose._compose_truncation_length(config) == 4
    assert (
        compose._compose_analysis_strategy("diff --git a/models.json b/models.json\n+large", config, counter)
        == compose.ComposeAnalysisStrategy.SMART_TRUNCATE
    )


def test_compose_analysis_strategy_keeps_small_group_direct() -> None:
    config = CommitConfig(map_reduce_threshold=1_000, max_diff_tokens=1_000, max_diff_length=10_000)
    counter = compose._create_token_counter(config)

    assert (
        compose._compose_analysis_strategy("diff --git a/a.rs b/a.rs\n+a", config, counter)
        == compose.ComposeAnalysisStrategy.DIRECT
    )


def test_fallback_commit_type_classifies_dependency_files_as_build() -> None:
    diff = """diff --git a/package.json b/package.json
index 1111111..2222222 100644
--- a/package.json
+++ b/package.json
@@ -1,1 +1,1 @@
-{"dependencies": {}}
+{"dependencies": {"x": "1"}}
"""
    snapshot = build_compose_snapshot(diff, "")
    file = snapshot.file_by_path("package.json")
    assert file is not None

    assert compose._fallback_commit_type_for_group(snapshot, [], [file.file_id]).as_str() == "build"


def test_chunk_ambiguous_files_splits_large_binding_request() -> None:
    ambiguous_files = [
        {
            "file_id": "F001",
            "path": "src/alpha.rs",
            "candidate_group_ids": ("G1", "G2"),
            "hunk_ids": tuple(f"F001-H{idx:03}" for idx in range(1, 71)),
        },
        {
            "file_id": "F002",
            "path": "src/beta.rs",
            "candidate_group_ids": ("G1", "G3"),
            "hunk_ids": tuple(f"F002-H{idx:03}" for idx in range(1, 61)),
        },
        {
            "file_id": "F003",
            "path": "src/gamma.rs",
            "candidate_group_ids": ("G2", "G3"),
            "hunk_ids": tuple(f"F003-H{idx:03}" for idx in range(1, 11)),
        },
    ]

    batches = compose._chunk_ambiguous_files(ambiguous_files)
    total_hunks = sum(len(file["hunk_ids"]) for batch in batches for file in batch)

    assert len(batches) == 2
    assert len(batches[0]) == 1
    assert len(batches[1]) == 2
    assert total_hunks == 140
    assert all(
        len(batch) <= compose.MAX_BIND_FILES_PER_REQUEST
        and sum(len(file["hunk_ids"]) for file in batch) <= compose.MAX_BIND_HUNKS_PER_REQUEST
        for batch in batches
    )

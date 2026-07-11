"""Compose-mode planning and isolated execution."""

from __future__ import annotations

import asyncio
import json
import os
import warnings
from collections import defaultdict, deque
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import asdict, dataclass, is_dataclass
from enum import StrEnum
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, Any, Protocol

from . import api, diffing, git, map_reduce, style, templates, tokens
from .changelog import ChangelogWeaver
from .errors import GitError, NoChanges, ValidationFailure
from .models import (
    CommitSummary,
    CommitType,
    ComposeSnapshot,
    Scope,
    coerce_optional_scope,
)
from .normalization import format_commit_message, post_process_commit_message
from .patch import (
    StageResult,
    build_compose_snapshot,
    create_executable_group_patch,
    force_stage_file_from_base_in_index,
    pin_snapshot_staged_state,
    stage_executable_group_in_index,
)
from .validation import validate_commit_message

if TYPE_CHECKING:
    from .config import CommitConfig


class _ComposeArgs(Protocol):
    """Command-line arguments read during compose planning and execution."""

    dir: str | os.PathLike[str]
    compose_preview: bool
    compose_max_commits: int | None
    compose_test_after_each: bool
    sign: bool
    signoff: bool
    debug_output: str | None


COMPOSE_PLAN_SCHEMA_VERSION = "v3"
COMPOSE_MESSAGE_PARALLELISM = 8
# Compose planning intentionally switches representation as snapshots grow:
# small/medium snapshots preserve per-file detail, while large snapshots plan by
# path area to keep prompts bounded and avoid monolithic LLM output.
MAX_OBSERVATIONS_PER_FILE = 3
COMPOSE_SUMMARY_MEDIUM_FILE_THRESHOLD = 60
COMPOSE_SUMMARY_MEDIUM_HUNK_THRESHOLD = 200
COMPOSE_SUMMARY_LARGE_FILE_THRESHOLD = 150
COMPOSE_SUMMARY_LARGE_HUNK_THRESHOLD = 500
COMPOSE_AREA_TARGET_MAX_FILES = 60
COMPOSE_AREA_TARGET_MAX_HUNKS = 140
COMPOSE_AREA_TARGET_MAX_DEPTH = 6
COMPOSE_MONOLITH_FALLBACK_TARGET_THRESHOLD = 8
COMPOSE_MONOLITH_FALLBACK_WORKSTREAM_THRESHOLD = 3
MAX_BIND_FILES_PER_REQUEST = 18
MAX_BIND_HUNKS_PER_REQUEST = 120
_DEPENDENCY_MANIFESTS = {
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "go.mod",
    "go.sum",
    "requirements.txt",
    "Pipfile",
    "Pipfile.lock",
    "pyproject.toml",
    "Gemfile",
    "Gemfile.lock",
    "composer.json",
    "composer.lock",
    "build.gradle",
    "build.gradle.kts",
    "gradle.properties",
    "pom.xml",
}


class ComposeAnalysisStrategy(StrEnum):
    DIRECT = "direct"
    SMART_TRUNCATE = "smart_truncate"
    MAP_REDUCE = "map_reduce"


class PlanningMode(StrEnum):
    FILE = "file"
    AREA = "area"


@dataclass(frozen=True, slots=True)
class ComposeBaseState:
    """HEAD hash and symbolic ref captured before LLM calls, to guard the final ref update."""

    head_hash: str
    head_ref: str


@dataclass(frozen=True, slots=True)
class SnapshotSummaryBudget:
    max_observations_per_file: int
    max_hunks_per_file: int | None = None

    @property
    def is_compacted(self) -> bool:
        return self.max_hunks_per_file is not None


@dataclass(frozen=True, slots=True)
class PlanningTarget:
    target_id: str
    label: str
    file_ids: tuple[str, ...]
    hunk_count: int
    additions: int
    deletions: int


@dataclass(frozen=True, slots=True)
class PlanningIndex:
    mode: PlanningMode
    targets: tuple[PlanningTarget, ...]
    aliases: Mapping[str, str]

    def expand_target_ids(self, target_ids: Sequence[str]) -> list[str]:
        expanded: list[str] = []
        seen: set[str] = set()
        targets = {target.target_id: target for target in self.targets}
        for target_id in target_ids:
            target = targets.get(target_id)
            if target is None:
                continue
            for file_id in target.file_ids:
                if file_id not in seen:
                    expanded.append(file_id)
                    seen.add(file_id)
        return expanded


@dataclass(frozen=True, slots=True)
class ComposeIntentGroup:
    group_id: str
    commit_type: CommitType
    scope: Scope | None
    file_ids: tuple[str, ...]
    rationale: str
    dependencies: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "group_id", str(self.group_id))
        object.__setattr__(self, "commit_type", CommitType.from_raw(self.commit_type))
        if self.scope is not None:
            object.__setattr__(self, "scope", Scope.from_raw(self.scope))
        object.__setattr__(self, "file_ids", tuple(str(value) for value in self.file_ids))
        object.__setattr__(self, "dependencies", tuple(str(value) for value in self.dependencies))

    @property
    def type(self) -> CommitType:
        return self.commit_type


@dataclass(frozen=True, slots=True)
class ComposeExecutableGroup:
    """A fully bound compose group with file and hunk ids ready to stage."""

    group_id: str
    commit_type: CommitType
    scope: Scope | None
    file_ids: tuple[str, ...]
    rationale: str
    dependencies: tuple[str, ...] = ()
    hunk_ids: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        object.__setattr__(self, "group_id", str(self.group_id))
        object.__setattr__(self, "commit_type", CommitType.from_raw(self.commit_type))
        if self.scope is not None:
            object.__setattr__(self, "scope", Scope.from_raw(self.scope))
        object.__setattr__(self, "file_ids", tuple(str(value) for value in self.file_ids))
        object.__setattr__(self, "dependencies", tuple(str(value) for value in self.dependencies))
        object.__setattr__(self, "hunk_ids", tuple(str(value) for value in self.hunk_ids))

    @property
    def type(self) -> CommitType:
        """Return the commit type under the prompt-facing JSON field name."""
        return self.commit_type


@dataclass(frozen=True, slots=True)
class ComposeExecutablePlan:
    """Executable compose plan ordered by dependency index."""

    groups: tuple[ComposeExecutableGroup, ...]
    dependency_order: tuple[int, ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "groups", tuple(self.groups))
        object.__setattr__(self, "dependency_order", tuple(int(value) for value in self.dependency_order))


def compute_dependency_order(
    groups: Sequence[Any],
    group_id: Callable[[Any], str] | None = None,
    dependencies: Callable[[Any], Iterable[str | int]] | None = None,
) -> tuple[int, ...]:
    """Return a topological order for compose groups, validating ids and cycles."""
    id_for = group_id or (lambda group: str(getattr(group, "group_id", getattr(group, "id", ""))))
    deps_for = dependencies or (lambda group: getattr(group, "dependencies", ()))
    index_by_id: dict[str, int] = {}
    ids: list[str] = []
    for idx, group in enumerate(groups):
        raw_id = id_for(group).strip() or f"G{idx + 1:03d}"
        if raw_id in index_by_id:
            raise ValidationFailure(f"duplicate compose group_id {raw_id!r}", field="compose")
        index_by_id[raw_id] = idx
        ids.append(raw_id)

    in_degree = [0] * len(groups)
    adjacency: list[list[int]] = [[] for _ in groups]
    for idx, group in enumerate(groups):
        for dependency in deps_for(group):
            if isinstance(dependency, int):
                dep_idx = dependency
                if dep_idx < 0 or dep_idx >= len(groups):
                    raise ValidationFailure(f"group {ids[idx]} depends on unknown index {dep_idx}", field="compose")
            else:
                dep_id = str(dependency)
                if dep_id not in index_by_id:
                    raise ValidationFailure(f"group {ids[idx]} depends on unknown group_id {dep_id!r}", field="compose")
                dep_idx = index_by_id[dep_id]
            if dep_idx == idx:
                raise ValidationFailure(f"group {ids[idx]} depends on itself", field="compose")
            adjacency[dep_idx].append(idx)
            in_degree[idx] += 1

    queue = deque(idx for idx, degree in enumerate(in_degree) if degree == 0)
    order: list[int] = []
    while queue:
        node = queue.popleft()
        order.append(node)
        for neighbor in adjacency[node]:
            in_degree[neighbor] -= 1
            if in_degree[neighbor] == 0:
                queue.append(neighbor)
    if len(order) != len(groups):
        raise ValidationFailure("circular dependency detected in compose groups", field="compose")
    return tuple(order)


def capture_compose_base_state(repo_dir: str | os.PathLike[str] = ".") -> ComposeBaseState:
    """Capture HEAD hash and symbolic ref once before compose planning or LLM calls."""
    return ComposeBaseState(
        head_hash=git.get_head_hash(repo_dir),
        head_ref=git.current_head_ref(repo_dir),
    )


async def run_compose_mode(args: _ComposeArgs, config: CommitConfig) -> list[str]:
    """Split the staged tree into atomic commits, looping until that tree is fully committed.

    Compose mirrors the regular commit path exactly: it commits only the tree that was staged at
    invocation (callers auto-stage when nothing is staged). That target tree is captured ONCE up
    front, and every round diffs ``HEAD`` against it — so anything the user stages mid-run is
    ignored and stays staged, just like a normal commit. One LLM plan may not cover the whole
    tree, so each round commits what it can and re-plans the remainder. The loop is unconstrained
    but fails fast if a round makes no progress (changes remain yet nothing was committed) instead
    of spinning forever or silently leaving the target half-committed.
    """
    repo_dir = args.dir
    target_tree = git.write_real_index_tree(repo_dir)
    weaver = ChangelogWeaver.create(repo_dir, config, target_tree)
    exclude = weaver.exclude_pathspecs() if weaver else ()
    all_hashes: list[str] = []
    round_number = 1
    while True:
        hashes = await run_compose_round(args, config, target_tree, round_number, weaver)
        all_hashes.extend(hashes)
        if args.compose_preview:
            break
        try:
            git.get_compose_diff(repo_dir, config, target_tree, exclude=exclude)
        except NoChanges:
            break
        if not hashes:
            raise GitError(
                "Compose made no progress: staged changes remain but the plan produced no commits. "
                "Re-run, narrow what is staged, or split manually."
            )
        round_number += 1
    return all_hashes


def _print_executable_plan(snapshot: ComposeSnapshot, plan: ComposeExecutablePlan) -> None:
    """Print compose groups in dependency order for preview and execution."""
    print(f"\n{style.section_header('Proposed Commit Groups', 80)}")
    for display_idx, group_idx in enumerate(plan.dependency_order, start=1):
        group = plan.groups[group_idx]
        scope = f"({style.scope(group.scope.as_str())})" if group.scope is not None else ""
        print(
            f"\n{display_idx}. {style.bold(group.group_id)} "
            f"[{style.commit_type(group.commit_type.as_str())}{scope}] {group.rationale}"
        )

        print("   Files:")
        for file_id in group.file_ids:
            file = snapshot.file_by_id(file_id)
            if file is None:
                continue
            selected_hunk_ids = [hunk_id for hunk_id in group.hunk_ids if hunk_id in file.hunk_ids]
            selection = "all hunks" if len(selected_hunk_ids) == len(file.hunk_ids) else ", ".join(selected_hunk_ids)
            print(f"     - {file.file_id} {file.path} ({selection})")

        if group.dependencies:
            print(f"   Depends on: {', '.join(group.dependencies)}")


async def run_compose_round(
    args: _ComposeArgs,
    config: CommitConfig,
    target_tree: str,
    round_number: int = 1,
    weaver: ChangelogWeaver | None = None,
) -> list[str]:
    """Plan one immutable compose snapshot against the fixed target tree and execute it."""
    repo_dir = args.dir
    base_state = capture_compose_base_state(repo_dir)
    exclude = weaver.exclude_pathspecs() if weaver else ()
    diff = git.get_compose_diff(repo_dir, config, target_tree, exclude=exclude)
    stat = git.get_compose_stat(repo_dir, target_tree, exclude=exclude)
    snapshot = build_compose_snapshot(diff, stat)
    snapshot = pin_snapshot_staged_state(snapshot, repo_dir, target_tree)
    _save_debug_artifact(args, f"compose_round_{round_number}_snapshot.json", _snapshot_to_jsonable(snapshot))

    token_counter = _create_token_counter(config)
    observations = []
    if not _is_large_compose_snapshot(snapshot) and _should_use_map_reduce(snapshot.diff, config, token_counter):
        observations = await map_reduce.observe_diff_files(snapshot.diff, config.summary_model, config, token_counter)
    if observations:
        _save_debug_artifact(
            args,
            f"compose_round_{round_number}_observations.json",
            [_observation_to_jsonable(item) for item in observations],
        )

    max_commits = args.compose_max_commits or 20
    model = config.analysis_model
    plan = _load_cached_plan(repo_dir, snapshot, max_commits, model)
    if plan is None:
        plan = await plan_compose_snapshot(snapshot, config, args, max_commits=max_commits, observations=observations)
        _save_cached_plan(repo_dir, snapshot, max_commits, model, plan)
    _save_debug_artifact(args, f"compose_round_{round_number}_executable_plan.json", _plan_to_jsonable(plan))

    _print_executable_plan(snapshot, plan)

    if args.compose_preview:
        preview_message = f"{style.icons.SUCCESS} Preview complete (use --compose without --compose-preview to execute)"
        print(f"\n{style.success(preview_message)}")
        return []
    return await execute_compose(snapshot, plan, config, args, base_state, weaver)


async def plan_compose_snapshot(
    snapshot: ComposeSnapshot,
    config: CommitConfig,
    args: _ComposeArgs | None = None,
    *,
    max_commits: int = 20,
    observations: Sequence[Any] = (),
) -> ComposeExecutablePlan:
    """Build or request an executable compose plan for a pinned snapshot."""
    debug_dir = args.debug_output if args is not None else None
    intent_plan = await _analyze_compose_intent(snapshot, observations, config, max_commits, debug_dir)
    _save_debug_artifact(args, "compose_intent_plan.json", _intent_plan_to_jsonable(intent_plan))
    return await _bind_compose_plan(snapshot, intent_plan, config, debug_dir)


async def execute_compose(
    snapshot: ComposeSnapshot,
    plan: ComposeExecutablePlan,
    config: CommitConfig,
    args: _ComposeArgs,
    base_state: ComposeBaseState,
    weaver: ChangelogWeaver | None = None,
) -> list[str]:
    """Create compose commits from a temp index, then checked-update the real ref."""
    if args.compose_preview:
        return []

    repo_dir = args.dir
    ordered_groups = [plan.groups[idx] for idx in plan.dependency_order]
    group_patches = [create_executable_group_patch(snapshot, group) for group in ordered_groups]
    prepared_messages = await _prepare_group_messages(snapshot, ordered_groups, group_patches, config, args)

    with git.TempGitIndex(repo_dir) as index:
        git.read_tree_into_index(index.path, base_state.head_hash, repo_dir)
        if weaver:
            weaver.seed_temp_index(repo_dir, index.path)
        parent_hash = base_state.head_hash
        commit_hashes: list[str] = []
        for position, group in enumerate(ordered_groups):
            outcome = stage_executable_group_in_index(snapshot, group, repo_dir, index.path)
            staged_anything = outcome.result == StageResult.STAGED
            for skipped in outcome.skipped:
                file = snapshot.file_by_path(skipped.path)
                if file is None:
                    continue
                cumulative = _cumulative_file_hunk_ids(plan, position, snapshot, file.file_id)
                force_stage_file_from_base_in_index(snapshot, file.file_id, cumulative, repo_dir, index.path)
                staged_anything = True
            if not staged_anything:
                continue
            if weaver:
                patch = group_patches[position]
                group_paths = [
                    file.path
                    for file_id in group.file_ids
                    for file in [snapshot.file_by_id(file_id)]
                    if file is not None
                ]
                await weaver.weave_group(group_paths, patch.diff, patch.stat, repo_dir, index.path)
            message = prepared_messages[position]
            tree = git.write_index_tree(index.path, repo_dir)
            sign = bool(args.sign or config.gpg_sign)
            commit_hash = git.commit_tree(tree, [parent_hash], message, repo_dir, sign=sign)
            parent_hash = commit_hash
            commit_hashes.append(commit_hash)
            if args.compose_test_after_each:
                raise GitError("--compose-test-after-each is incompatible with isolated compose execution")

    if not commit_hashes:
        return []

    git.update_ref_checked(base_state.head_ref, parent_hash, base_state.head_hash, repo_dir)
    if weaver:
        weaver.flush(repo_dir)
    # Source paths stay untouched in the real index: committed paths now match HEAD while
    # uncovered or mid-run staging remains staged for the next round. The weaver separately
    # synchronizes claimed changelogs under compare-before-write guards.
    return commit_hashes


def _analyze_compose_intent_from_mapping(
    raw: Any, snapshot: ComposeSnapshot, config: CommitConfig, max_commits: int
) -> tuple[ComposeIntentGroup, ...]:
    data = _object_mapping(raw)
    raw_groups = data.get("groups", ())
    groups = [_intent_group_from_mapping(item, idx) for idx, item in enumerate(raw_groups, start=1)]
    planning_index = _build_planning_index(snapshot)
    return _normalize_intent_plan(snapshot, planning_index, groups, config, max_commits)


async def _analyze_compose_intent(
    snapshot: ComposeSnapshot,
    observations: Sequence[Any],
    config: CommitConfig,
    max_commits: int,
    debug_dir: str | os.PathLike[str] | None,
) -> tuple[ComposeIntentGroup, ...]:
    planning_index = _build_planning_index(snapshot)
    types_description = api.format_types_description(config)
    parts = templates.render_compose_intent_prompt(
        max_commits=max_commits,
        stat=_render_planning_stat(planning_index),
        snapshot_summary=_render_planning_snapshot_summary(snapshot, observations, planning_index),
        planning_targets=_render_planning_targets(planning_index, snapshot),
        planning_notes=_render_planning_notes(planning_index),
        split_bias=_render_split_bias(planning_index),
        types_description=types_description,
    )
    try:
        response = await api.run_oneshot(
            config,
            api.OneShotSpec(
                operation="compose/intent",
                model=config.analysis_model,
                prompt_family="compose-intent",
                system_prompt=parts.system,
                user_prompt=parts.user,
                tool_name="create_compose_intent_plan",
                progress_label="compose intent planner",
                debug=api.OneShotDebug(Path(debug_dir), None, "compose_intent") if debug_dir else None,
                cacheable=True,
            ),
        )
        output = response.output
        groups = _analyze_compose_intent_from_mapping(output, snapshot, config, max_commits)
    except Exception as exc:
        warnings.warn(
            f"compose intent planner failed; falling back to deterministic plan: {exc}", RuntimeWarning, stacklevel=2
        )
        groups = _fallback_intent_groups(snapshot, planning_index, max_commits, config)
    if _should_force_large_patch_fallback(snapshot, planning_index, groups, max_commits):
        groups = _fallback_intent_groups(snapshot, planning_index, max_commits, config)
    return groups


async def _bind_compose_plan(
    snapshot: ComposeSnapshot,
    intent_plan: Sequence[ComposeIntentGroup],
    config: CommitConfig,
    debug_dir: str | os.PathLike[str] | None,
) -> ComposeExecutablePlan:
    assigned_by_group, ambiguous_files = _auto_assign_hunks(snapshot, intent_plan)
    unresolved: list[str] = []
    if ambiguous_files:
        for batch_idx, batch in enumerate(_chunk_ambiguous_files(ambiguous_files), start=1):
            debug_name = "compose_bind" if len(ambiguous_files) == len(batch) else f"compose_bind_{batch_idx:03d}"
            assignments = await _request_binding(snapshot, intent_plan, batch, config, debug_dir, debug_name)
            evaluation = _evaluate_binding(
                assignments, _ambiguous_hunk_context(batch), {group.group_id for group in intent_plan}, snapshot
            )
            for group_id, hunk_ids in evaluation.assigned.items():
                assigned_by_group[group_id].update(hunk_ids)
            unresolved.extend(evaluation.unresolved)
    if unresolved:
        group_rank = {
            intent_plan[idx].group_id: position for position, idx in enumerate(compute_dependency_order(intent_plan))
        }
        repair_batches = _chunk_ambiguous_files(_filter_ambiguous_files(ambiguous_files, unresolved))
        repair_unresolved: list[str] = []
        for batch_idx, batch in enumerate(repair_batches, start=1):
            debug_name = "compose_bind_repair" if len(repair_batches) == 1 else f"compose_bind_repair_{batch_idx:03d}"
            assignments = await _request_binding(snapshot, intent_plan, batch, config, debug_dir, debug_name)
            repair = _evaluate_binding(
                assignments, _ambiguous_hunk_context(batch), {group.group_id for group in intent_plan}, snapshot
            )
            for group_id, hunk_ids in repair.assigned.items():
                assigned_by_group[group_id].update(hunk_ids)
            repair_unresolved.extend(repair.unresolved)
        if repair_unresolved:
            _assign_unresolved_hunks(repair_unresolved, assigned_by_group, ambiguous_files, group_rank)
    plan = _finalize_executable_plan(snapshot, intent_plan, assigned_by_group)
    _validate_executable_plan(snapshot, plan)
    return plan


def _fallback_intent_groups(
    snapshot: ComposeSnapshot,
    planning_index: PlanningIndex,
    max_commits: int,
    config: CommitConfig,
) -> tuple[ComposeIntentGroup, ...]:
    del config
    if planning_index.mode is PlanningMode.AREA:
        bins = _fallback_area_bins(snapshot, planning_index, max(1, max_commits))
    else:
        bins = [tuple(file.file_id for file in bucket) for bucket in _bucket_files(snapshot, max(1, max_commits))]
    groups: list[ComposeIntentGroup] = []
    for idx, file_ids in enumerate(bins, start=1):
        files = [file for file_id in file_ids for file in [snapshot.file_by_id(file_id)] if file is not None]
        labels = [file.path for file in files]
        groups.append(
            ComposeIntentGroup(
                group_id=f"G{idx:03d}",
                commit_type=_fallback_commit_type_for_group(snapshot, labels, tuple(file_ids)),
                scope=_fallback_scope_for_label(_common_path_prefix(labels)),
                file_ids=tuple(file_ids),
                rationale=_fallback_rationale_for_labels(labels),
                dependencies=(),
            )
        )
    return tuple(groups)


def _bucket_files(snapshot: ComposeSnapshot, max_commits: int) -> list[list[Any]]:
    if not snapshot.files:
        return []
    if len(snapshot.files) <= max_commits:
        return [[file] for file in snapshot.files]
    buckets: list[list[Any]] = [[] for _ in range(max_commits)]
    for idx, file in enumerate(snapshot.files):
        buckets[idx % max_commits].append(file)
    return [bucket for bucket in buckets if bucket]


def _fallback_area_bins(
    snapshot: ComposeSnapshot, planning_index: PlanningIndex, max_commits: int
) -> list[tuple[str, ...]]:
    workstreams: dict[str, set[str]] = {}
    weights: dict[str, int] = defaultdict(int)
    for target in planning_index.targets:
        key = _workstream_key_for_label(target.label)
        workstreams.setdefault(key, set()).update(target.file_ids)
        weights[key] += max(target.hunk_count, len(target.file_ids))
    ordered = sorted(workstreams, key=lambda key: (-weights[key], key))
    bins: list[tuple[list[str], int]] = [([], 0) for _ in range(max(1, min(max_commits, len(ordered) or 1)))]
    for key in ordered:
        idx = min(range(len(bins)), key=lambda i: (bins[i][1], len(bins[i][0])))
        bins[idx][0].extend(workstreams[key])
        bins[idx] = (bins[idx][0], bins[idx][1] + weights[key])
    return [tuple(_ordered_file_ids(snapshot, set(file_ids))) for file_ids, _ in bins if file_ids]


def _is_dependency_manifest(path: str) -> bool:
    name = Path(path).name
    return name in _DEPENDENCY_MANIFESTS or Path(name).suffix.lower() in {".lock", ".lockb"}


def _compose_analysis_strategy(diff: str, config: CommitConfig, counter: Any) -> ComposeAnalysisStrategy:
    if _should_use_map_reduce(diff, config, counter):
        return ComposeAnalysisStrategy.MAP_REDUCE
    diff_tokens = _count_tokens(counter, diff)
    if len(diff) > config.max_diff_length or diff_tokens > config.max_diff_tokens:
        return ComposeAnalysisStrategy.SMART_TRUNCATE
    return ComposeAnalysisStrategy.DIRECT


def _compose_truncation_length(config: CommitConfig) -> int:
    return max(1, min(config.max_diff_length, config.max_diff_tokens * 4))


def _count_tokens(counter: Any, text: str) -> int:
    count_sync = getattr(counter, "count_sync", None)
    if callable(count_sync):
        return int(count_sync(text))
    count = getattr(counter, "count", None)
    if callable(count):
        return int(count(text))
    return max(1, len(text) // 4)


def _create_token_counter(config: CommitConfig) -> Any:
    try:
        return tokens.create_token_counter(config)
    except Exception as exc:
        warnings.warn(f"token counter unavailable; using character-count fallback: {exc}", RuntimeWarning, stacklevel=2)
        return SimpleNamespace(count_sync=lambda text: max(1, len(str(text)) // 4))


def _should_use_map_reduce(diff: str, config: CommitConfig, counter: Any | None = None) -> bool:
    try:
        return bool(map_reduce.should_use_map_reduce(diff, config, counter))
    except Exception as exc:
        warnings.warn(
            f"map-reduce availability check failed; using direct analysis path: {exc}", RuntimeWarning, stacklevel=2
        )
        return False


def _is_large_compose_snapshot(snapshot: ComposeSnapshot) -> bool:
    return (
        len(snapshot.files) > COMPOSE_SUMMARY_LARGE_FILE_THRESHOLD
        or len(snapshot.hunks) > COMPOSE_SUMMARY_LARGE_HUNK_THRESHOLD
    )


def _snapshot_summary_budget(snapshot: ComposeSnapshot) -> SnapshotSummaryBudget:
    if _is_large_compose_snapshot(snapshot):
        return SnapshotSummaryBudget(1, 2)
    if (
        len(snapshot.files) > COMPOSE_SUMMARY_MEDIUM_FILE_THRESHOLD
        or len(snapshot.hunks) > COMPOSE_SUMMARY_MEDIUM_HUNK_THRESHOLD
    ):
        return SnapshotSummaryBudget(2, 3)
    return SnapshotSummaryBudget(MAX_OBSERVATIONS_PER_FILE, None)


def _sample_positions(count: int, max_samples: int) -> list[int]:
    if count <= max_samples:
        return list(range(count))
    if max_samples <= 1:
        return [0]
    last = count - 1
    out: list[int] = []
    for slot in range(max_samples):
        position = slot * last // (max_samples - 1)
        if not out or out[-1] != position:
            out.append(position)
    return out


def _sampled_hunk_ids_for_summary(file: Any, budget: SnapshotSummaryBudget) -> list[str]:
    if budget.max_hunks_per_file is None:
        return list(file.hunk_ids)
    return [file.hunk_ids[idx] for idx in _sample_positions(len(file.hunk_ids), budget.max_hunks_per_file)]


def _format_line_range(start: int, count: int) -> str:
    if count == 0:
        return "0"
    if count == 1:
        return str(start)
    return f"{start}-{start + count - 1}"


def _render_snapshot_summary(snapshot: ComposeSnapshot, observations: Sequence[Any]) -> str:
    budget = _snapshot_summary_budget(snapshot)
    observations_by_file = {
        str(getattr(item, "file", "")): list(getattr(item, "observations", ()))[: budget.max_observations_per_file]
        for item in observations
    }
    out: list[str] = []
    if budget.is_compacted:
        out.append(
            f"# snapshot compacted: all file IDs are preserved; showing up to {budget.max_hunks_per_file or 0} representative hunks and {budget.max_observations_per_file} observation(s) per file"
        )
    for file in snapshot.files:
        out.append(f"- {file.file_id} {file.summary}")
        for observation in observations_by_file.get(file.path, ()):
            out.append(f"  observation: {observation}")
        rendered = _sampled_hunk_ids_for_summary(file, budget)
        for hunk_id in rendered:
            hunk = snapshot.hunk_by_id(hunk_id)
            if hunk is None:
                continue
            if hunk.synthetic:
                out.append(f"  - {hunk.hunk_id} :: {hunk.snippet}")
            else:
                out.append(
                    f"  - {hunk.hunk_id} old:{_format_line_range(hunk.old_start, hunk.old_count)} new:{_format_line_range(hunk.new_start, hunk.new_count)} :: {hunk.snippet}"
                )
        omitted = len(file.hunk_ids) - len(rendered)
        if omitted > 0:
            out.append(f"  ... {omitted} more hunks omitted from {file.file_id}")
    return "\n".join(out)


def _build_planning_index(snapshot: ComposeSnapshot) -> PlanningIndex:
    mode = PlanningMode.AREA if _is_large_compose_snapshot(snapshot) else PlanningMode.FILE
    targets = tuple(
        _build_file_planning_targets(snapshot) if mode is PlanningMode.FILE else _build_area_planning_targets(snapshot)
    )
    aliases: dict[str, str] = {}
    for target in targets:
        aliases[target.target_id] = target.target_id
        aliases[target.target_id.upper()] = target.target_id
        aliases[_normalize_file_reference(target.label)] = target.target_id
    return PlanningIndex(mode=mode, targets=targets, aliases=aliases)


def _build_file_planning_targets(snapshot: ComposeSnapshot) -> list[PlanningTarget]:
    return [
        PlanningTarget(file.file_id, file.path, (file.file_id,), len(file.hunk_ids), file.additions, file.deletions)
        for file in snapshot.files
    ]


def _build_area_planning_targets(snapshot: ComposeSnapshot) -> list[PlanningTarget]:
    all_file_ids = [file.file_id for file in snapshot.files]
    buckets = _collect_planning_buckets(snapshot, all_file_ids, 0)
    targets: list[PlanningTarget] = []
    for idx, (label, file_ids) in enumerate(buckets, start=1):
        files = [file for file_id in file_ids for file in [snapshot.file_by_id(file_id)] if file is not None]
        targets.append(
            PlanningTarget(
                f"A{idx:03d}",
                label,
                tuple(file_ids),
                sum(len(file.hunk_ids) for file in files),
                sum(file.additions for file in files),
                sum(file.deletions for file in files),
            )
        )
    return targets


def _collect_planning_buckets(
    snapshot: ComposeSnapshot, file_ids: Sequence[str], depth: int
) -> list[tuple[str, tuple[str, ...]]]:
    files = [file for file_id in file_ids for file in [snapshot.file_by_id(file_id)] if file is not None]
    hunk_count = sum(len(file.hunk_ids) for file in files)
    max_depth = max((len(file.path.split("/")) for file in files), default=depth)
    if (
        (len(files) <= COMPOSE_AREA_TARGET_MAX_FILES and hunk_count <= COMPOSE_AREA_TARGET_MAX_HUNKS)
        or depth >= COMPOSE_AREA_TARGET_MAX_DEPTH
        or depth >= max_depth
    ):
        return [(_planning_bucket_label(snapshot, file_ids), tuple(file_ids))]
    groups: dict[str, list[str]] = defaultdict(list)
    for file in files:
        groups[_prefix_at_depth(file.path, depth + 1)].append(file.file_id)
    if len(groups) <= 1:
        return _collect_planning_buckets(snapshot, file_ids, depth + 1)
    out: list[tuple[str, tuple[str, ...]]] = []
    for group_file_ids in groups.values():
        out.extend(_collect_planning_buckets(snapshot, group_file_ids, depth + 1))
    return out


def _prefix_at_depth(path: str, depth: int) -> str:
    return "/".join(path.split("/")[: max(0, min(depth, len(path.split("/"))))])


def _planning_bucket_label(snapshot: ComposeSnapshot, file_ids: Sequence[str]) -> str:
    paths = [file.path for file_id in file_ids for file in [snapshot.file_by_id(file_id)] if file is not None]
    prefix = _common_path_prefix(paths)
    return prefix or (paths[0] if paths else "misc")


def _common_path_prefix(paths: Sequence[str]) -> str:
    if not paths:
        return ""
    prefix = paths[0].split("/")
    for path in paths[1:]:
        segments = path.split("/")
        shared = 0
        for left, right in zip(prefix, segments, strict=False):
            if left != right:
                break
            shared += 1
        prefix = prefix[:shared]
        if not prefix:
            break
    return "/".join(prefix)


def _render_planning_stat(index: PlanningIndex) -> str:
    lines = [
        "# planning over individual file IDs"
        if index.mode is PlanningMode.FILE
        else f"# planning over {len(index.targets)} area IDs spanning {sum(len(target.file_ids) for target in index.targets)} files"
    ]
    for target in index.targets:
        lines.append(
            f"{target.target_id} {target.label} | {len(target.file_ids)} files | {target.hunk_count} hunks | +{target.additions}/-{target.deletions}"
        )
    return "\n".join(lines)


def _render_planning_snapshot_summary(
    snapshot: ComposeSnapshot, observations: Sequence[Any], index: PlanningIndex
) -> str:
    if index.mode is PlanningMode.FILE:
        return _render_snapshot_summary(snapshot, observations)
    observations_by_file = {
        str(getattr(item, "file", "")): list(getattr(item, "observations", ()))[:1] for item in observations
    }
    out = ["# snapshot compacted into path-based planning areas; use the area IDs below in `file_ids`"]
    for target in index.targets:
        out.append(
            f"- {target.target_id} {target.label} ({len(target.file_ids)} files, {target.hunk_count} hunks, +{target.additions}/-{target.deletions})"
        )
        sample_files: list[str] = []
        for file_id in _sample_file_ids_for_target(target):
            file = snapshot.file_by_id(file_id)
            if file is not None:
                sample_files.append(file.path)
        if sample_files:
            out.append(f"  files: {', '.join(sample_files)}")
            omitted = len(target.file_ids) - len(sample_files)
            if omitted > 0:
                out.append(f"  ... {omitted} more files omitted from {target.target_id}")
        rendered_obs = 0
        for file_id in target.file_ids:
            file = snapshot.file_by_id(file_id)
            if file is None:
                continue
            for observation in observations_by_file.get(file.path, ()):
                out.append(f"  observation: {observation}")
                rendered_obs += 1
                if rendered_obs >= 2:
                    break
            if rendered_obs >= 2:
                break
        for hunk_id in _sample_hunk_ids_for_target(target, snapshot):
            hunk = snapshot.hunk_by_id(hunk_id)
            if hunk is None:
                continue
            if hunk.synthetic:
                out.append(f"  - {hunk.hunk_id} :: {hunk.snippet}")
            else:
                out.append(
                    f"  - {hunk.hunk_id} old:{_format_line_range(hunk.old_start, hunk.old_count)} new:{_format_line_range(hunk.new_start, hunk.new_count)} :: {hunk.snippet}"
                )
    return "\n".join(out)


def _sample_file_ids_for_target(target: PlanningTarget) -> list[str]:
    return [target.file_ids[idx] for idx in _sample_positions(len(target.file_ids), 4)]


def _sample_hunk_ids_for_target(target: PlanningTarget, snapshot: ComposeSnapshot) -> list[str]:
    hunk_ids = [
        hunk_id
        for file_id in target.file_ids
        for file in [snapshot.file_by_id(file_id)]
        if file is not None
        for hunk_id in file.hunk_ids
    ]
    return [hunk_ids[idx] for idx in _sample_positions(len(hunk_ids), 4)]


def _render_planning_targets(index: PlanningIndex, snapshot: ComposeSnapshot) -> str:
    if index.mode is PlanningMode.FILE:
        return f"File IDs only. Each target maps to exactly one file. Coverage: {len(snapshot.files)} files."
    return f"Area IDs only. Each target may expand to multiple files by shared path prefix. Coverage: {len(index.targets)} areas spanning {len(snapshot.files)} files."


def _render_planning_notes(index: PlanningIndex) -> str:
    if index.mode is PlanningMode.FILE:
        return "Use only the provided file IDs and keep the grouping conservative."
    return "This snapshot is large, so files were compacted into path-based planning areas. Split along independent subsystems or workstreams when the areas point at unrelated changes."


def _render_split_bias(index: PlanningIndex) -> str:
    if index.mode is PlanningMode.FILE:
        return "Prefer fewer groups when the split is uncertain."
    return "Prefer splitting unrelated areas into separate groups. Only return one broad group if nearly every area clearly belongs to the same atomic change."


def _normalize_file_reference(raw_file_ref: str) -> str:
    value = raw_file_ref.strip().strip("`'\"").strip()
    for token in ("file", "path", "target"):
        if value.lower().startswith(token + ":"):
            value = value[len(token) + 1 :].strip()
    return value


def _planning_text_tokens(text: str) -> list[str]:
    stop_words = {
        "and",
        "or",
        "the",
        "with",
        "from",
        "into",
        "after",
        "before",
        "over",
        "under",
        "plus",
        "across",
        "update",
        "updated",
        "refactor",
        "refactored",
        "changes",
        "change",
        "logical",
        "group",
        "groups",
        "commit",
        "commits",
    }
    tokens: list[str] = []
    current = []
    seen: set[str] = set()
    for char in text:
        if char.isascii() and char.isalnum():
            current.append(char.lower())
        else:
            if len(current) >= 3:
                token = "".join(current)
                if token not in stop_words and token not in seen:
                    tokens.append(token)
                    seen.add(token)
            current = []
    if len(current) >= 3:
        token = "".join(current)
        if token not in stop_words and token not in seen:
            tokens.append(token)
    return tokens


def _extract_group_id_candidate(raw: str) -> str | None:
    normalized = _normalize_file_reference(raw)
    uppercase = normalized.upper().strip()
    if uppercase.startswith("G") and uppercase[1:].isdigit():
        return f"G{uppercase[1:]}"
    digits = "".join(ch for ch in uppercase if ch.isdigit())
    compact = "".join(ch for ch in uppercase if ch.isalnum())
    if compact.startswith("GROUP") and digits:
        return f"G{digits}"
    if compact.startswith("G") and digits:
        return f"G{digits}"
    return None


def _intent_group_from_mapping(item: Any, idx: int) -> ComposeIntentGroup:
    data = _object_mapping(item)
    return ComposeIntentGroup(
        group_id=str(data.get("group_id") or data.get("id") or f"G{idx:03d}"),
        commit_type=CommitType.from_raw(data.get("type") or data.get("commit_type") or "chore"),
        scope=coerce_optional_scope(data.get("scope")),
        file_ids=tuple(str(value) for value in data.get("file_ids", ())),
        rationale=str(data.get("rationale") or "compose changes"),
        dependencies=tuple(str(value) for value in data.get("dependencies", ())),
    )


def _normalize_dependency_reference(raw_dependency: str, known_group_ids: set[str]) -> str | None:
    normalized = _normalize_file_reference(raw_dependency)
    if not normalized:
        return None
    if normalized in known_group_ids:
        return normalized
    upper = normalized.upper()
    if upper in known_group_ids:
        return upper
    candidate = _extract_group_id_candidate(normalized)
    if candidate in known_group_ids:
        return candidate
    return None


def _normalize_intent_plan(
    snapshot: ComposeSnapshot,
    planning_index: PlanningIndex,
    groups: Sequence[ComposeIntentGroup],
    config: CommitConfig,
    max_commits: int,
) -> tuple[ComposeIntentGroup, ...]:
    del config
    if not groups:
        raise ValidationFailure("Compose intent plan returned no groups", field="compose")
    known_target_ids = {target.target_id for target in planning_index.targets}
    covered_file_ids: set[str] = set()
    normalized_group_targets: list[list[str]] = []
    normalized_groups: list[ComposeIntentGroup] = []
    seen_group_ids: set[str] = set()
    for idx, group in enumerate(groups, start=1):
        group_id = group.group_id or f"G{idx:03d}"
        if group_id in seen_group_ids:
            group_id = f"{group_id}-{idx}"
        seen_group_ids.add(group_id)
        target_ids: list[str] = []
        seen_targets: set[str] = set()
        for raw_ref in group.file_ids:
            normalized_ref = _normalize_file_reference(raw_ref)
            target_id = (
                normalized_ref if normalized_ref in known_target_ids else planning_index.aliases.get(normalized_ref)
            )
            if target_id and target_id not in seen_targets:
                target_ids.append(target_id)
                seen_targets.add(target_id)
        if not target_ids:
            claimed_targets = {target_id for ids in normalized_group_targets for target_id in ids}
            target_ids = _seed_group_target(group, planning_index, claimed_targets)
        expanded = planning_index.expand_target_ids(target_ids)
        covered_file_ids.update(expanded)
        normalized_group_targets.append(target_ids)
        normalized_groups.append(
            ComposeIntentGroup(
                group_id, group.commit_type, group.scope, tuple(expanded), group.rationale, group.dependencies
            )
        )
    for file in snapshot.files:
        if file.file_id in covered_file_ids:
            continue
        best_idx = _best_group_for_missing_file(snapshot, normalized_groups, file)
        group = normalized_groups[best_idx]
        normalized_groups[best_idx] = ComposeIntentGroup(
            group.group_id,
            group.commit_type,
            group.scope,
            (*group.file_ids, file.file_id),
            group.rationale,
            group.dependencies,
        )
        covered_file_ids.add(file.file_id)
    max_group_count = max(1, max_commits)
    if len(normalized_groups) > max_group_count:
        kept = normalized_groups[:max_group_count]
        overflow = normalized_groups[max_group_count:]
        last = kept[-1]
        last_files = set(last.file_ids)
        overflow_file_ids = tuple(
            file_id for group in overflow for file_id in group.file_ids if file_id not in last_files
        )
        overflow_dependencies = tuple(
            dependency
            for group in overflow
            for dependency in group.dependencies
            if dependency not in last.dependencies and dependency != last.group_id
        )
        kept[-1] = ComposeIntentGroup(
            last.group_id,
            last.commit_type,
            last.scope,
            (*last.file_ids, *overflow_file_ids),
            last.rationale,
            (*last.dependencies, *overflow_dependencies),
        )
        normalized_groups = kept
        covered_file_ids = {file_id for group in normalized_groups for file_id in group.file_ids}
    known_group_ids = {group.group_id for group in normalized_groups}
    finalized: list[ComposeIntentGroup] = []
    for group in normalized_groups:
        deps: list[str] = []
        for raw_dependency in group.dependencies:
            dependency = _normalize_dependency_reference(raw_dependency, known_group_ids)
            if dependency and dependency != group.group_id and dependency not in deps:
                deps.append(dependency)
        finalized.append(
            ComposeIntentGroup(
                group.group_id, group.commit_type, group.scope, group.file_ids, group.rationale, tuple(deps)
            )
        )
    compute_dependency_order(finalized)
    return tuple(finalized)


def _seed_group_target(group: ComposeIntentGroup, planning_index: PlanningIndex, claimed: set[str]) -> list[str]:
    if not planning_index.targets:
        return []
    best: tuple[int, int, str] | None = None
    for target in planning_index.targets:
        score = _planning_target_match_score(target, group)
        if target.target_id not in claimed:
            score += 60
        candidate = (score, -target.hunk_count, target.target_id)
        if best is None or candidate > best:
            best = candidate
    if best is None:
        return []
    _, _, target_id = best
    return [target_id]


def _planning_target_match_score(target: PlanningTarget, group: ComposeIntentGroup) -> int:
    label = target.label.lower()
    workstream = _workstream_key_for_label(target.label).lower()
    score = 0
    score += min(target.hunk_count, 40)
    score += min(len(target.file_ids), 20)
    if group.scope and (str(group.scope) in label or str(group.scope) in workstream):
        score += 140
    for token in _planning_text_tokens(group.rationale):
        if token in label or token in workstream:
            score += 45
    type_name = str(group.commit_type)
    if type_name == "test" and ("test" in label or "spec" in label):
        score += 130
    if type_name == "docs" and ("docs" in label or label.endswith(".md")):
        score += 120
    if type_name in {"build", "chore"} and any(
        word in label for word in ("cargo", "package", "lock", "build", "config")
    ):
        score += 80
    return score


def _best_group_for_missing_file(
    snapshot: ComposeSnapshot, groups: Sequence[ComposeIntentGroup], missing_file: Any
) -> int:
    best_idx = 0
    best_score = -(10**9)
    best_size = 10**9
    for idx, group in enumerate(groups):
        candidates = [snapshot.file_by_id(file_id) for file_id in group.file_ids]
        similarity = max(
            (_file_similarity_score(missing_file, file) for file in candidates if file is not None), default=0
        )
        score = similarity + _group_type_bonus(missing_file, group)
        size = len(group.file_ids)
        if score > best_score or score == best_score and size < best_size:
            best_idx = idx
            best_score = score
            best_size = size
    return best_idx


def _file_similarity_score(missing_file: Any, candidate_file: Any) -> int:
    score = _common_path_prefix_depth(missing_file.path, candidate_file.path) * 25
    if Path(missing_file.path).parent == Path(candidate_file.path).parent:
        score += 40
    if Path(missing_file.path).suffix == Path(candidate_file.path).suffix:
        score += 18
    return score


def _common_path_prefix_depth(left: str, right: str) -> int:
    depth = 0
    for left_part, right_part in zip(left.split("/"), right.split("/"), strict=False):
        if left_part != right_part:
            break
        depth += 1
    return depth


def _group_type_bonus(file: Any, group: ComposeIntentGroup) -> int:
    category = _compose_file_category(file)
    type_name = str(group.commit_type)
    if category == "docs" and type_name == "docs":
        return 25
    if category == "test" and type_name == "test":
        return 25
    if category == "dependency" and type_name in {"build", "chore", "ci"}:
        return 18
    if category == "config" and type_name in {"build", "chore", "ci"}:
        return 12
    if category in {"prompt", "source"} and type_name in {"feat", "fix", "refactor", "perf"}:
        return 10
    return 0


def _compose_file_category(file: Any) -> str:
    path = file.path.lower()
    name = Path(path).name
    ext = Path(path).suffix.lower().lstrip(".")
    if getattr(file, "is_binary", False):
        return "binary"
    if _is_dependency_manifest(file.path):
        return "dependency"
    if "prompt" in path or "system" in path:
        return "prompt"
    if ext == "md" or name in {"readme", "readme.md"}:
        return "docs"
    if "test" in path or name.endswith(("_test", ".test", ".spec")):
        return "test"
    if ext in {"toml", "yaml", "yml", "json", "ini", "cfg", "conf", "env"}:
        return "config"
    if ext in {
        "rs",
        "py",
        "js",
        "jsx",
        "ts",
        "tsx",
        "go",
        "java",
        "kt",
        "c",
        "cc",
        "cpp",
        "h",
        "hpp",
        "rb",
        "php",
        "swift",
        "scala",
        "sh",
        "bash",
        "zsh",
        "fish",
        "sql",
    }:
        return "source"
    return "other"


def _should_force_large_patch_fallback(
    snapshot: ComposeSnapshot, planning_index: PlanningIndex, groups: Sequence[ComposeIntentGroup], max_commits: int
) -> bool:
    if max_commits <= 1 or planning_index.mode is not PlanningMode.AREA or not groups:
        return False
    if len(planning_index.targets) < COMPOSE_MONOLITH_FALLBACK_TARGET_THRESHOLD or not _is_monolithic_intent_plan(
        snapshot, groups
    ):
        return False
    workstream_count = len({_workstream_key_for_label(target.label) for target in planning_index.targets})
    return workstream_count >= COMPOSE_MONOLITH_FALLBACK_WORKSTREAM_THRESHOLD


def _is_monolithic_intent_plan(snapshot: ComposeSnapshot, groups: Sequence[ComposeIntentGroup]) -> bool:
    largest = max((len(set(group.file_ids)) for group in groups), default=0)
    return len(groups) <= 2 and largest * 10 >= len(snapshot.files) * 9


def _workstream_key_for_label(label: str) -> str:
    segments = [segment for segment in label.split("/") if segment]
    if not segments:
        return label
    first = segments[0]
    if first == ".github":
        return ".github"
    if first in {"apps", "packages", "crates", "services", "libs", "pass"} and len(segments) > 1:
        return f"{first}/{segments[1]}"
    return first


def _fallback_scope_for_label(label: str) -> Scope | None:
    key = _workstream_key_for_label(label)
    candidate = key.split("/")[-1].replace("_", "-").replace(".", "-")
    return coerce_optional_scope(candidate)


def _fallback_rationale_for_labels(labels: Sequence[str]) -> str:
    if not labels:
        return "compose changes"
    if len(labels) == 1:
        return f"Updated {labels[0]}"
    displays = labels[:3]
    suffix = "" if len(labels) <= 3 else f", and {len(labels) - 3} more"
    return f"Updated {', '.join(displays)}{suffix}"


def _fallback_commit_type_for_group(
    snapshot: ComposeSnapshot, labels: Sequence[str], file_ids: Sequence[str]
) -> CommitType:
    if any(label == ".github" or label.startswith(".github/") for label in labels):
        return CommitType.from_raw("ci")
    files = [file for file_id in file_ids for file in [snapshot.file_by_id(file_id)] if file is not None]
    if files and all(_compose_file_category(file) == "docs" for file in files):
        return CommitType.from_raw("docs")
    if files and all(_compose_file_category(file) == "test" for file in files):
        return CommitType.from_raw("test")
    if files and all(_is_dependency_manifest(file.path) for file in files):
        return CommitType.from_raw("build")
    if files and all(_compose_file_category(file) in {"config", "dependency"} for file in files):
        return CommitType.from_raw("chore")
    return CommitType.from_raw("refactor")


def _ordered_file_ids(snapshot: ComposeSnapshot, file_ids: set[str]) -> list[str]:
    return [file.file_id for file in snapshot.files if file.file_id in file_ids]


def _auto_assign_hunks(
    snapshot: ComposeSnapshot, intent_plan: Sequence[ComposeIntentGroup]
) -> tuple[dict[str, set[str]], list[dict[str, Any]]]:
    groups_by_file: dict[str, list[str]] = defaultdict(list)
    for group in intent_plan:
        for file_id in group.file_ids:
            groups_by_file[file_id].append(group.group_id)
    assigned: dict[str, set[str]] = defaultdict(set)
    ambiguous: list[dict[str, Any]] = []
    for file in snapshot.files:
        candidates = groups_by_file.get(file.file_id)
        if not candidates:
            raise ValidationFailure(f"No compose group claimed file {file.file_id} ({file.path})", field="compose")
        if len(candidates) == 1:
            assigned[candidates[0]].update(file.hunk_ids)
        else:
            ambiguous.append(
                {
                    "file_id": file.file_id,
                    "path": file.path,
                    "candidate_group_ids": tuple(candidates),
                    "hunk_ids": tuple(file.hunk_ids),
                }
            )
    return assigned, ambiguous


def _render_binding_groups(groups: Sequence[ComposeIntentGroup]) -> str:
    lines: list[str] = []
    for group in groups:
        scope = f"({group.scope})" if group.scope else ""
        lines.append(f"- {group.group_id}: {group.commit_type}{scope} :: {group.rationale}")
    return "\n".join(lines)


def _render_binding_ambiguous_files(snapshot: ComposeSnapshot, ambiguous_files: Sequence[Mapping[str, Any]]) -> str:
    lines: list[str] = []
    for item in ambiguous_files:
        lines.append(f"- {item['file_id']} {item['path']} candidates: {', '.join(item['candidate_group_ids'])}")
        for hunk_id in item["hunk_ids"]:
            hunk = snapshot.hunk_by_id(hunk_id)
            if hunk is None:
                continue
            if hunk.synthetic:
                lines.append(f"  - {hunk.hunk_id} :: {hunk.snippet}")
            else:
                lines.append(
                    f"  - {hunk.hunk_id} old:{_format_line_range(hunk.old_start, hunk.old_count)} new:{_format_line_range(hunk.new_start, hunk.new_count)} :: {hunk.snippet}"
                )
    return "\n".join(lines)


async def _request_binding(
    snapshot: ComposeSnapshot,
    groups: Sequence[ComposeIntentGroup],
    ambiguous_files: Sequence[Mapping[str, Any]],
    config: CommitConfig,
    debug_dir: str | os.PathLike[str] | None,
    debug_name: str,
) -> list[Mapping[str, Any]]:
    if not ambiguous_files:
        return []
    parts = templates.render_compose_bind_prompt(
        groups=_render_binding_groups(groups),
        ambiguous_files=_render_binding_ambiguous_files(snapshot, ambiguous_files),
    )
    try:
        response = await api.run_oneshot(
            config,
            api.OneShotSpec(
                operation="compose/bind",
                model=config.analysis_model,
                prompt_family="compose-bind",
                system_prompt=parts.system,
                user_prompt=parts.user,
                tool_name="bind_compose_hunks",
                progress_label="compose hunk binder",
                debug=api.OneShotDebug(Path(debug_dir), None, debug_name) if debug_dir else None,
                cacheable=True,
            ),
        )
    except Exception as exc:
        warnings.warn(f"compose hunk binder failed; using deterministic fallback: {exc}", RuntimeWarning, stacklevel=2)
        return []
    output = response.output
    data = _object_mapping(output)
    assignments = data.get("assignments", ())
    return [_object_mapping(item) for item in assignments]


def _ambiguous_hunk_context(ambiguous_files: Sequence[Mapping[str, Any]]) -> dict[str, tuple[str, ...]]:
    return {hunk_id: tuple(item["candidate_group_ids"]) for item in ambiguous_files for hunk_id in item["hunk_ids"]}


def _evaluate_binding(
    assignments: Sequence[Mapping[str, Any]],
    hunk_context: Mapping[str, tuple[str, ...]],
    valid_group_ids: set[str],
    snapshot: ComposeSnapshot,
) -> SimpleNamespace:
    assigned_hunk_to_group: dict[str, str] = {}
    for assignment in assignments:
        group_id = str(assignment.get("group_id", ""))
        if group_id not in valid_group_ids:
            continue
        seen: set[str] = set()
        for raw_hunk_id in assignment.get("hunk_ids", ()):
            hunk_id = str(raw_hunk_id)
            if hunk_id in seen:
                continue
            seen.add(hunk_id)
            candidates = hunk_context.get(hunk_id)
            if not candidates or group_id not in candidates:
                continue
            if assigned_hunk_to_group.get(hunk_id) == group_id:
                continue
            if hunk_id in assigned_hunk_to_group:
                assigned_hunk_to_group.pop(hunk_id, None)
            else:
                assigned_hunk_to_group[hunk_id] = group_id
    assigned_by_group: dict[str, list[str]] = defaultdict(list)
    for hunk in snapshot.hunks:
        assigned_group_id = assigned_hunk_to_group.get(hunk.hunk_id)
        if assigned_group_id:
            assigned_by_group[assigned_group_id].append(hunk.hunk_id)
    unresolved = [
        hunk.hunk_id
        for hunk in snapshot.hunks
        if hunk.hunk_id in hunk_context and hunk.hunk_id not in assigned_hunk_to_group
    ]
    return SimpleNamespace(assigned=dict(assigned_by_group), unresolved=unresolved)


def _filter_ambiguous_files(
    ambiguous_files: Sequence[Mapping[str, Any]], hunk_ids: Sequence[str]
) -> list[dict[str, Any]]:
    wanted = set(hunk_ids)
    out: list[dict[str, Any]] = []
    for item in ambiguous_files:
        matching = tuple(hunk_id for hunk_id in item["hunk_ids"] if hunk_id in wanted)
        if matching:
            out.append(
                {
                    "file_id": item["file_id"],
                    "path": item["path"],
                    "candidate_group_ids": tuple(item["candidate_group_ids"]),
                    "hunk_ids": matching,
                }
            )
    return out


def _chunk_ambiguous_files(ambiguous_files: Sequence[Mapping[str, Any]]) -> list[list[Mapping[str, Any]]]:
    batches: list[list[Mapping[str, Any]]] = []
    current: list[Mapping[str, Any]] = []
    hunk_count = 0
    for item in ambiguous_files:
        item_hunks = len(item["hunk_ids"])
        should_split = current and (
            len(current) >= MAX_BIND_FILES_PER_REQUEST or hunk_count + item_hunks > MAX_BIND_HUNKS_PER_REQUEST
        )
        if should_split:
            batches.append(current)
            current = []
            hunk_count = 0
        current.append(item)
        hunk_count += item_hunks
    if current:
        batches.append(current)
    return batches


def _assign_unresolved_hunks(
    unresolved_hunks: Sequence[str],
    assigned_by_group: dict[str, set[str]],
    ambiguous_files: Sequence[Mapping[str, Any]],
    group_rank: Mapping[str, int],
) -> None:
    context = _ambiguous_hunk_context(ambiguous_files)
    for hunk_id in unresolved_hunks:
        candidates = [candidate for candidate in context.get(hunk_id, ()) if candidate in group_rank]
        if not candidates:
            continue
        group_id = min(candidates, key=lambda item: group_rank.get(item, 10**9))
        assigned_by_group[group_id].add(hunk_id)


def _derive_file_ids_for_hunks(snapshot: ComposeSnapshot, hunk_ids: Sequence[str]) -> list[str]:
    hunk_set = set(hunk_ids)
    return [file.file_id for file in snapshot.files if any(hunk_id in hunk_set for hunk_id in file.hunk_ids)]


def _normalize_group_type(snapshot: ComposeSnapshot, file_ids: Sequence[str], original_type: CommitType) -> CommitType:
    dependency_only = bool(file_ids) and all(
        (file := snapshot.file_by_id(file_id)) is not None and _is_dependency_manifest(file.path)
        for file_id in file_ids
    )
    if dependency_only and str(original_type) in {"build", "chore", "ci"}:
        return CommitType.from_raw("build")
    return original_type


def _build_redirects(
    intent_plan: Sequence[ComposeIntentGroup],
    executable_groups: Sequence[ComposeExecutableGroup],
    group_rank: Mapping[str, int],
) -> dict[str, str]:
    surviving = {group.group_id: group for group in executable_groups if group.hunk_ids}
    redirects: dict[str, str] = {}
    for group in intent_plan:
        if group.group_id in surviving:
            continue
        candidates = [
            candidate
            for candidate in executable_groups
            if candidate.group_id != group.group_id and any(file_id in group.file_ids for file_id in candidate.file_ids)
        ]
        if candidates:
            redirect = min(candidates, key=lambda candidate: group_rank.get(candidate.group_id, 10**9)).group_id
            redirects[group.group_id] = redirect
    return redirects


def _resolve_redirect(group_id: str, redirects: Mapping[str, str]) -> str:
    current = group_id
    seen: set[str] = set()
    while current in redirects and current not in seen:
        seen.add(current)
        current = redirects[current]
    return current


def _prune_empty_groups(
    groups: Sequence[ComposeExecutableGroup], redirects: Mapping[str, str]
) -> ComposeExecutablePlan:
    surviving_ids = {group.group_id for group in groups if group.hunk_ids}
    surviving: list[ComposeExecutableGroup] = []
    for group in groups:
        if not group.hunk_ids:
            continue
        deps: list[str] = []
        for dependency in group.dependencies:
            rewritten = _resolve_redirect(dependency, redirects)
            if rewritten != group.group_id and rewritten in surviving_ids and rewritten not in deps:
                deps.append(rewritten)
        surviving.append(
            ComposeExecutableGroup(
                group.group_id,
                group.commit_type,
                group.scope,
                group.file_ids,
                group.rationale,
                tuple(deps),
                group.hunk_ids,
            )
        )
    return ComposeExecutablePlan(tuple(surviving), compute_dependency_order(surviving))


def _finalize_executable_plan(
    snapshot: ComposeSnapshot, intent_plan: Sequence[ComposeIntentGroup], assigned_by_group: Mapping[str, set[str]]
) -> ComposeExecutablePlan:
    order = compute_dependency_order(intent_plan)
    group_rank = {intent_plan[idx].group_id: position for position, idx in enumerate(order)}
    executable: list[ComposeExecutableGroup] = []
    for group in intent_plan:
        hunk_ids = [
            hunk.hunk_id for hunk in snapshot.hunks if hunk.hunk_id in assigned_by_group.get(group.group_id, set())
        ]
        file_ids = _derive_file_ids_for_hunks(snapshot, hunk_ids)
        executable.append(
            ComposeExecutableGroup(
                group_id=group.group_id,
                commit_type=_normalize_group_type(snapshot, file_ids, group.commit_type),
                scope=group.scope,
                file_ids=tuple(file_ids),
                rationale=group.rationale,
                dependencies=group.dependencies,
                hunk_ids=tuple(hunk_ids),
            )
        )
    redirects = _build_redirects(intent_plan, executable, group_rank)
    return _prune_empty_groups(executable, redirects)


def _validate_executable_plan(snapshot: ComposeSnapshot, plan: ComposeExecutablePlan) -> None:
    if not plan.groups:
        raise ValidationFailure("Compose executable plan returned no groups", field="compose")
    known_files = {file.file_id for file in snapshot.files}
    known_hunks = {hunk.hunk_id for hunk in snapshot.hunks}
    coverage: dict[str, str] = {}
    for group in plan.groups:
        if not group.hunk_ids:
            raise ValidationFailure(f"Compose group {group.group_id} ended up empty after binding", field="compose")
        for file_id in group.file_ids:
            if file_id not in known_files:
                raise ValidationFailure(
                    f"Compose group {group.group_id} references unknown file_id {file_id}", field="compose"
                )
        for hunk_id in group.hunk_ids:
            if hunk_id not in known_hunks:
                raise ValidationFailure(
                    f"Compose group {group.group_id} references unknown hunk_id {hunk_id}", field="compose"
                )
            existing = coverage.get(hunk_id)
            if existing is not None:
                raise ValidationFailure(
                    f"Hunk {hunk_id} was assigned to both {existing} and {group.group_id}", field="compose"
                )
            coverage[hunk_id] = group.group_id
    missing = [hunk.hunk_id for hunk in snapshot.hunks if hunk.hunk_id not in coverage]
    if missing:
        raise ValidationFailure(f"Compose plan left hunks unassigned: {', '.join(missing)}", field="compose")
    dependency_order = compute_dependency_order(plan.groups)
    if dependency_order != plan.dependency_order:
        raise ValidationFailure("Compose dependency order does not match recomputed order", field="compose")


def _plan_from_mapping(raw: Any, snapshot: ComposeSnapshot) -> ComposeExecutablePlan:
    data = _object_mapping(raw)
    groups = tuple(_group_from_mapping(item, snapshot, idx) for idx, item in enumerate(data.get("groups", ()), start=1))
    order = tuple(data.get("dependency_order") or compute_dependency_order(groups))
    plan = ComposeExecutablePlan(groups=groups, dependency_order=order)
    _validate_executable_plan(snapshot, plan)
    return plan


def _group_from_mapping(item: Any, snapshot: ComposeSnapshot, idx: int) -> ComposeExecutableGroup:
    data = _object_mapping(item)
    file_ids = tuple(str(value) for value in data.get("file_ids", ()))
    hunk_ids = tuple(str(value) for value in data.get("hunk_ids", ()))
    if not hunk_ids and file_ids:
        wanted = set(file_ids)
        hunk_ids = tuple(hunk_id for file in snapshot.files if file.file_id in wanted for hunk_id in file.hunk_ids)
    if not file_ids and hunk_ids:
        file_ids = tuple(_derive_file_ids_for_hunks(snapshot, hunk_ids))
    return ComposeExecutableGroup(
        group_id=str(data.get("group_id") or data.get("id") or f"G{idx:03d}"),
        commit_type=CommitType.from_raw(data.get("type") or data.get("commit_type") or "chore"),
        scope=coerce_optional_scope(data.get("scope")),
        file_ids=file_ids,
        rationale=str(data.get("rationale") or "compose changes"),
        dependencies=tuple(str(value) for value in data.get("dependencies", ())),
        hunk_ids=hunk_ids,
    )


def _object_mapping(value: Any) -> Mapping[str, Any]:
    if isinstance(value, Mapping):
        return value
    if is_dataclass(value) and not isinstance(value, type):
        return asdict(value)
    return vars(value)


async def _prepare_group_messages(
    snapshot: ComposeSnapshot,
    groups: Sequence[ComposeExecutableGroup],
    group_patches: Sequence[Any],
    config: CommitConfig,
    args: _ComposeArgs,
) -> list[str]:
    semaphore = asyncio.Semaphore(min(COMPOSE_MESSAGE_PARALLELISM, max(len(groups), 1)))
    counter = _create_token_counter(config)

    async def prepare(idx: int, group: ComposeExecutableGroup, patch: Any) -> str:
        async with semaphore:
            return await _generate_group_message(
                snapshot, group, patch.stat, patch.diff, config, args, counter, f"compose-{idx + 1}"
            )

    return list(
        await asyncio.gather(
            *(prepare(idx, group, patch) for idx, (group, patch) in enumerate(zip(groups, group_patches, strict=True)))
        )
    )


async def _generate_group_message(
    snapshot: ComposeSnapshot,
    group: ComposeExecutableGroup,
    stat: str,
    diff: str,
    config: CommitConfig,
    args: _ComposeArgs,
    counter: Any,
    debug_prefix: str,
) -> str:
    body, summary = await _message_parts_from_api(group, stat, diff, config, args, counter, debug_prefix)
    if summary is None:
        summary = _fallback_summary(group, snapshot)
    commit = SimpleNamespace(
        commit_type=group.commit_type,
        scope=group.scope,
        summary=CommitSummary.from_raw(summary, max_length=config.summary_hard_limit),
        body=list(body),
        footers=[],
    )
    commit = post_process_commit_message(commit, config)
    report = validate_commit_message(commit, config, stat=stat)
    if report.errors:
        first = report.errors[0]
        raise ValidationFailure(first.message, field=first.field, value=first.value)
    message = format_commit_message(commit)
    if bool(args.signoff or config.signoff):
        message = git.append_signoff_trailer(message, args.dir)
    return message


async def _message_parts_from_api(
    group: ComposeExecutableGroup,
    stat: str,
    diff: str,
    config: CommitConfig,
    args: _ComposeArgs,
    counter: Any,
    debug_prefix: str,
) -> tuple[list[str], str | None]:
    strategy = _compose_analysis_strategy(diff, config, counter)
    if strategy is ComposeAnalysisStrategy.MAP_REDUCE:
        analysis = await map_reduce.run_map_reduce(
            config,
            stat,
            diff,
            scope_candidates=group.rationale,
            model_name=config.analysis_model,
            counter=counter,
        )
    else:
        analysis_diff = diff
        if strategy is ComposeAnalysisStrategy.SMART_TRUNCATE:
            analysis_diff = diffing.smart_truncate_diff(diff, _compose_truncation_length(config), config, counter)
        analysis = await api.generate_conventional_analysis(
            config=config,
            stat=stat,
            diff=analysis_diff,
            scope_candidates=group.rationale,
            user_context=group.rationale,
            debug_output=args.debug_output,
            debug_prefix=debug_prefix,
        )
    body = analysis.body_texts() or [group.rationale]
    summary = await api.generate_summary_from_analysis(
        config=config,
        analysis=analysis,
        stat=stat,
        user_context=group.rationale,
        debug_output=args.debug_output,
        debug_prefix=debug_prefix,
    )
    return body, summary


def _fallback_summary(group: ComposeExecutableGroup, snapshot: ComposeSnapshot) -> str:
    files: list[str] = []
    for file_id in group.file_ids:
        file = snapshot.file_by_id(file_id)
        if file is not None:
            files.append(file.path)
    target = files[0] if len(files) == 1 else (str(group.scope) if group.scope else "compose changes")
    verb = "updated"
    if str(group.commit_type) == "docs":
        verb = "documented"
    elif str(group.commit_type) == "test":
        verb = "tested"
    return f"{verb} {target}"[:128]


def _cumulative_file_hunk_ids(
    plan: ComposeExecutablePlan, position: int, snapshot: ComposeSnapshot, file_id: str
) -> list[str]:
    hunk_ids: list[str] = []
    for group_idx in plan.dependency_order[: position + 1]:
        group = plan.groups[group_idx]
        for hunk_id in group.hunk_ids:
            hunk = snapshot.hunk_by_id(hunk_id)
            if hunk is not None and hunk.file_id == file_id:
                hunk_ids.append(hunk_id)
    return hunk_ids


def _cache_file(repo_dir: str | os.PathLike[str], key: str) -> Path:
    cache_dir = git.get_git_dir(repo_dir) / "llm-git"
    cache_dir.mkdir(parents=True, exist_ok=True)
    return cache_dir / f"compose-plan-{key}.json"


def _snapshot_cache_key(snapshot: ComposeSnapshot, max_commits: int, model: str) -> str:
    payload = json.dumps(
        {
            "schema": COMPOSE_PLAN_SCHEMA_VERSION,
            "model": model,
            "max_commits": max_commits,
            "files": [(file.file_id, file.path, file.hunk_ids) for file in snapshot.files],
            "hunks": [(hunk.hunk_id, hunk.semantic_key) for hunk in snapshot.hunks],
            "diff": snapshot.diff,
        },
        sort_keys=True,
    ).encode()
    try:
        from blake3 import blake3

        return blake3(payload).hexdigest()
    except Exception:
        import hashlib

        return hashlib.sha256(payload).hexdigest()


def _load_cached_plan(
    repo_dir: str | os.PathLike[str], snapshot: ComposeSnapshot, max_commits: int, model: str
) -> ComposeExecutablePlan | None:
    key = _snapshot_cache_key(snapshot, max_commits, model)
    path = _cache_file(repo_dir, key)
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError, json.JSONDecodeError:
        return None
    if data.get("schema_version") != COMPOSE_PLAN_SCHEMA_VERSION or data.get("cache_key") != key:
        return None
    try:
        return _plan_from_mapping(data.get("plan", {}), snapshot)
    except Exception:
        try:
            path.unlink()
        except OSError:
            pass
        return None


def _save_cached_plan(
    repo_dir: str | os.PathLike[str],
    snapshot: ComposeSnapshot,
    max_commits: int,
    model: str,
    plan: ComposeExecutablePlan,
) -> None:
    key = _snapshot_cache_key(snapshot, max_commits, model)
    path = _cache_file(repo_dir, key)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(
        json.dumps(
            {"schema_version": COMPOSE_PLAN_SCHEMA_VERSION, "cache_key": key, "plan": _plan_to_jsonable(plan)},
            indent=2,
            sort_keys=True,
        ),
        encoding="utf-8",
    )
    tmp.replace(path)


def _save_debug_artifact(args: _ComposeArgs | None, filename: str, value: Any) -> None:
    if args is None:
        return
    debug_dir = args.debug_output
    if not debug_dir:
        return
    path = Path(debug_dir)
    path.mkdir(parents=True, exist_ok=True)
    (path / filename).write_text(json.dumps(value, indent=2, sort_keys=True), encoding="utf-8")


def _plan_to_jsonable(plan: ComposeExecutablePlan) -> dict[str, Any]:
    return {
        "groups": [
            {
                "group_id": group.group_id,
                "type": str(group.commit_type),
                "scope": str(group.scope) if group.scope else None,
                "file_ids": list(group.file_ids),
                "rationale": group.rationale,
                "dependencies": list(group.dependencies),
                "hunk_ids": list(group.hunk_ids),
            }
            for group in plan.groups
        ],
        "dependency_order": list(plan.dependency_order),
    }


def _intent_plan_to_jsonable(plan: Sequence[ComposeIntentGroup]) -> dict[str, Any]:
    return {
        "groups": [
            {
                "group_id": group.group_id,
                "type": str(group.commit_type),
                "scope": str(group.scope) if group.scope else None,
                "file_ids": list(group.file_ids),
                "rationale": group.rationale,
                "dependencies": list(group.dependencies),
            }
            for group in plan
        ],
        "dependency_order": list(compute_dependency_order(plan)),
    }


def _snapshot_to_jsonable(snapshot: ComposeSnapshot) -> dict[str, Any]:
    return {
        "diff": snapshot.diff,
        "stat": snapshot.stat,
        "files": [asdict(file) for file in snapshot.files],
        "hunks": [asdict(hunk) for hunk in snapshot.hunks],
        "pins": {
            path: {"kind": str(pin.kind), "mode": pin.mode, "oid": pin.oid} for path, pin in snapshot.pins.items()
        },
    }


def _observation_to_jsonable(observation: Any) -> dict[str, Any]:
    if is_dataclass(observation) and not isinstance(observation, type):
        return asdict(observation)
    if isinstance(observation, Mapping):
        return dict(observation)
    return {
        "file": getattr(observation, "file", ""),
        "observations": list(getattr(observation, "observations", ())),
        "additions": getattr(observation, "additions", 0),
        "deletions": getattr(observation, "deletions", 0),
    }


__all__ = [
    "ComposeBaseState",
    "ComposeExecutableGroup",
    "ComposeExecutablePlan",
    "build_compose_snapshot",
    "capture_compose_base_state",
    "compute_dependency_order",
    "create_executable_group_patch",
    "execute_compose",
    "pin_snapshot_staged_state",
    "plan_compose_snapshot",
    "run_compose_mode",
    "run_compose_round",
    "stage_executable_group_in_index",
]

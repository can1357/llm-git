"""Changelog maintenance for staged commits."""

from __future__ import annotations

import asyncio
import json
import os
import re
from collections.abc import Collection, Iterator, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from . import style
from .diffing import scrub_diff_for_prompt, smart_truncate_diff
from .errors import GitError, ValidationFailure
from .git import run_git
from .judgment import Judge, JudgmentRequest, Noul, NoulAnswer, clip_text
from .map_reduce import render_observations_markdown
from .markdown_output import parse_changelog_response
from .models import (
    ChangelogCategory,
    ChangelogContext,
    ChangelogRevision,
    ChangelogUpdate,
    resolve_model_name,
)
from .templates import render_changelog_prompt

if TYPE_CHECKING:
    from .config import CommitConfig
    from .map_reduce import FileObservation

# Judged changelog-worthiness measured on real commits: features/fixes score 0.89-0.97,
# release bumps 0.07-0.13, refactors and developer-facing docs 0.72-0.76. The gate only
# skips the model when the change is plainly internal; anything ambiguous still runs.
_WORTHINESS_QUESTION = Noul(
    "Does this change add, alter, fix, remove, or deprecate anything a user of the project would notice — "
    "behaviour, output, CLI or API surface, defaults, error handling, performance, or documentation they rely on — "
    "such that it merits a Keep a Changelog entry under Added/Changed/Fixed/Removed/Deprecated/Security?",
    yes="user-visible: a user or integrator could observe or depend on the difference",
    no="internal only: refactoring, tests, CI/tooling, formatting, comments, version bumps, or developer-facing "
    "notes with no user-observable effect",
)
_DUPLICATE_YES = "restates an existing entry: the same change reworded, recategorized, narrowed, or broadened"
_DUPLICATE_NO = "describes a distinct change not covered by any existing entry"
# User-visibility is evident from the stat plus the top of the diff; clipping keeps the
# gate at a fraction of the changelog call it may save (jev bills the state's tokens).
_WORTHINESS_HEAD_CHARS = 12_000
_WORTHINESS_TAIL_CHARS = 3_000

_REVISE_BLOCK_RE = re.compile(r"<revise\b[^>]*>(.*?)(?:</revise>|$)", re.IGNORECASE | re.DOTALL)
_REVISION_LINE_RE = re.compile(r"^\s*(OLD|NEW)\s*:(.*)$", re.IGNORECASE)


@dataclass(frozen=True, slots=True)
class UnreleasedSection:
    """A changelog's `[Unreleased]` section, parsed from ``lines``.

    ``header_line`` and ``end_line`` index ``lines``. :func:`apply_revisions` and
    :func:`write_entries` splice into ``lines``, so subsection headings, prose and
    unknown categories around the touched bullets survive verbatim.
    """

    path: Path
    lines: tuple[str, ...]
    header_line: int
    end_line: int
    entries: dict[ChangelogCategory, list[str]]


@dataclass(frozen=True, slots=True)
class ChangelogBoundary:
    """A changelog and the staged files governed by it."""

    changelog_path: Path
    files: tuple[str, ...]
    diff: str = ""
    stat: str = ""


@dataclass(frozen=True, slots=True)
class _BoundaryPrep:
    """One boundary's collected context, ready for concurrent entry generation."""

    boundary: ChangelogBoundary
    diff: str
    stat: str
    rel_path: str
    is_tracked: bool
    unreleased: UnreleasedSection
    worktree_unreleased: UnreleasedSection | None
    head_unreleased: UnreleasedSection | None
    existing_entries: str | None
    authored_entries: str | None
    can_revise: bool


@dataclass(slots=True)
class PreparedChangelogFlow:
    """Boundary contexts collected from git, ready for generation and applying."""

    repo_dir: Path
    preps: list[_BoundaryPrep]


def prepare_changelog_flow(args: Any, config: CommitConfig) -> PreparedChangelogFlow:
    """Collect per-boundary context (diffs, stats, unreleased sections) for staged files."""

    repo_dir = Path(getattr(args, "dir", "."))
    prepared = PreparedChangelogFlow(repo_dir, [])
    staged_files = _staged_files(repo_dir)
    candidate_files = [path for path in staged_files if not path.lower().endswith("changelog.md")]
    if not candidate_files:
        return prepared

    changelogs = _find_changelogs(repo_dir)
    if not changelogs:
        return prepared

    max_diff_length = config.max_diff_length
    for boundary in detect_boundaries(candidate_files, changelogs, repo_dir):
        diff = scrub_diff_for_prompt(_diff_for_files(boundary.files, repo_dir, max_diff_length))
        if not diff.strip():
            continue
        if len(diff) > max_diff_length:
            diff = smart_truncate_diff(diff, max_diff_length, config)
        stat = _stat_for_files(boundary.files, repo_dir)

        rel_path = _relative_to(boundary.changelog_path, repo_dir)
        staged_content = _staged_changelog_content(rel_path, repo_dir)
        worktree_content = boundary.changelog_path.read_text(encoding="utf-8")
        is_tracked = staged_content is not None
        changelog_content = staged_content if staged_content is not None else worktree_content

        try:
            unreleased = parse_unreleased_section(changelog_content, boundary.changelog_path)
        except ValidationFailure:
            continue

        worktree_unreleased: UnreleasedSection | None = None
        if is_tracked and worktree_content != changelog_content:
            try:
                worktree_unreleased = parse_unreleased_section(worktree_content, boundary.changelog_path)
            except ValidationFailure:
                continue

        head_unreleased = _head_unreleased(rel_path, boundary.changelog_path, repo_dir)
        if head_unreleased is None:
            existing_entries = _format_existing_entries(unreleased)
            authored_entries = None
        else:
            existing_entries = _format_entry_map(head_unreleased.entries)
            authored_entries = _format_entry_map(_entries_added_since(head_unreleased, unreleased))

        can_revise = bool(config.changelog_revise) and head_unreleased is not None and bool(existing_entries)
        prepared.preps.append(
            _BoundaryPrep(
                boundary,
                diff,
                stat,
                rel_path,
                is_tracked,
                unreleased,
                worktree_unreleased,
                head_unreleased,
                existing_entries,
                authored_entries,
                can_revise,
            )
        )
    return prepared


def prepared_changelog_contexts(prepared: PreparedChangelogFlow) -> tuple[ChangelogContext, ...]:
    """Export the captured generation context for every prepared changelog boundary."""

    return tuple(
        ChangelogContext(
            path=prep.rel_path,
            files=prep.boundary.files,
            is_package=_is_package_changelog(prep.boundary.changelog_path, prepared.repo_dir),
            existing_entries=prep.existing_entries,
            authored_entries=prep.authored_entries,
            can_revise=prep.can_revise,
        )
        for prep in prepared.preps
    )


async def generate_changelog_updates(
    prepared: PreparedChangelogFlow,
    config: CommitConfig,
    observations: Sequence[FileObservation] | None = None,
) -> list[tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]]:
    """Run the changelog model concurrently for every prepared boundary, returning de-duplicated entries.

    When map-phase ``observations`` are given, boundaries covered by them are
    prompted with per-file change summaries instead of their raw diff. Duplicate
    detection (which may call the judge) happens here, inside the task that
    overlaps commit-message generation, so :func:`apply_changelog_updates`
    stays synchronous and off the critical path.
    """

    judge = Judge.from_config(config)

    async def generate(prep: _BoundaryPrep) -> tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]:
        entries, revisions = await generate_changelog_entries(
            prep.boundary.changelog_path,
            _is_package_changelog(prep.boundary.changelog_path, prepared.repo_dir),
            prep.stat,
            prep.diff,
            prep.existing_entries,
            prep.authored_entries,
            prep.can_revise,
            config,
            observations=_observations_markdown(observations, prep.boundary.files),
        )
        return await _prepare_generated_update(prep, entries, revisions, judge, config)

    return list(await asyncio.gather(*(generate(prep) for prep in prepared.preps)))


async def prepare_synthesized_updates(
    prepared: PreparedChangelogFlow,
    updates: Sequence[ChangelogUpdate],
    config: CommitConfig,
) -> list[tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]]:
    """Validate and prepare fused updates in boundary order using standalone reconciliation semantics."""

    by_path = _validated_updates_by_path(updates, tuple(prep.rel_path for prep in prepared.preps), require_all=True)
    judge = Judge.from_config(config)
    return list(
        await asyncio.gather(
            *(
                _prepare_generated_update(
                    prep,
                    by_path[prep.rel_path].entries,
                    by_path[prep.rel_path].revisions,
                    judge,
                    config,
                )
                for prep in prepared.preps
            )
        )
    )


async def _prepare_generated_update(
    prep: _BoundaryPrep,
    entries: Mapping[ChangelogCategory, Sequence[str]],
    revisions: Sequence[ChangelogRevision],
    judge: Judge | None,
    config: CommitConfig,
) -> tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]:
    """Apply revision protection before filtering generated entries against both changelog snapshots."""

    staged, worktree, _ = _revised_sections(prep, revisions)
    deduped = await drop_duplicate_entries(
        entries,
        staged.entries,
        worktree.entries if worktree is not None else None,
        judge=judge,
        config=config,
    )
    return deduped, list(revisions)


def _validated_updates_by_path(
    updates: Sequence[ChangelogUpdate],
    expected_paths: Collection[str],
    *,
    require_all: bool,
) -> dict[str, ChangelogUpdate]:
    """Index updates after rejecting paths that cannot be reconciled safely."""

    expected = set(expected_paths)
    by_path: dict[str, ChangelogUpdate] = {}
    for update in updates:
        if update.path not in expected:
            raise ValidationFailure(
                f"unknown changelog update path: {update.path!r}",
                field="changelog",
                value=update.path,
            )
        if update.path in by_path:
            raise ValidationFailure(
                f"duplicate changelog update path: {update.path!r}",
                field="changelog",
                value=update.path,
            )
        by_path[update.path] = update
    missing = expected.difference(by_path)
    if require_all and missing:
        paths = ", ".join(sorted(missing))
        raise ValidationFailure(f"missing changelog updates for: {paths}", field="changelog", value=paths)
    return by_path


def _revised_sections(
    prep: _BoundaryPrep, revisions: Sequence[ChangelogRevision]
) -> tuple[UnreleasedSection, UnreleasedSection | None, list[ChangelogRevision]]:
    """Staged and worktree Unreleased sections after ``revisions``, plus the revisions applied to the staged one."""
    if not prep.can_revise:
        return prep.unreleased, prep.worktree_unreleased, []
    assert prep.head_unreleased is not None
    revisable = {entry.casefold() for values in prep.head_unreleased.entries.values() for entry in values}
    staged, applied = apply_revisions(prep.unreleased, revisions, revisable)
    worktree = (
        apply_revisions(prep.worktree_unreleased, revisions, revisable)[0]
        if prep.worktree_unreleased is not None
        else None
    )
    return staged, worktree, applied


def apply_changelog_updates(
    prepared: PreparedChangelogFlow,
    generated: Sequence[tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]],
) -> list[ChangelogBoundary]:
    """Write generated (already de-duplicated) entries into worktree changelogs and stage exact blobs."""

    repo_dir = prepared.repo_dir
    updated: list[ChangelogBoundary] = []
    untracked_to_stage: list[str] = []
    for prep, (entries, revisions) in zip(prepared.preps, generated, strict=True):
        staged, worktree, applied = _revised_sections(prep, revisions)
        if not entries and not applied:
            continue

        updated_staged = write_entries(staged, entries)
        updated_worktree = write_entries(worktree, entries) if worktree is not None else updated_staged
        prep.boundary.changelog_path.write_text(updated_worktree, encoding="utf-8")

        if prep.is_tracked:
            stage_changelog_blob(prep.rel_path, updated_staged, repo_dir)
        else:
            untracked_to_stage.append(prep.rel_path)
        _report_applied_revisions(applied)
        updated.append(ChangelogBoundary(prep.boundary.changelog_path, prep.boundary.files, prep.diff, prep.stat))

    if untracked_to_stage:
        run_git(["add", "--", *untracked_to_stage], cwd=repo_dir)
    return updated


async def run_changelog_flow(
    args: Any,
    config: CommitConfig,
    observations: Sequence[FileObservation] | None = None,
) -> list[ChangelogBoundary]:
    """Generate and stage changelog entries for currently staged files."""

    prepared = prepare_changelog_flow(args, config)
    generated = await generate_changelog_updates(prepared, config, observations)
    return apply_changelog_updates(prepared, generated)


def _observations_markdown(observations: Sequence[FileObservation] | None, files: Collection[str]) -> str | None:
    """Render map-phase observations for ``files`` as per-file markdown, or None when uncovered."""

    if observations is None:
        return None
    wanted = set(files)
    covered = [item for item in observations if item.file in wanted and item.observations]
    return render_observations_markdown(covered) if covered else None


@dataclass(slots=True)
class _ComposeBoundaryState:
    rel_path: str
    changelog_path: Path
    is_package: bool
    original_head_oid: str | None
    target_mode: str
    target_oid: str
    target_content: str
    head_unreleased: UnreleasedSection
    authored: dict[ChangelogCategory, list[str]]
    chain_content: str
    worktree_content: str
    last_written_worktree: str
    staged_index_oid: str


GroupEntries = dict[Path, tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]]
"""Per-changelog generated entries and revisions for one compose group."""


@dataclass(frozen=True, slots=True)
class _BoundaryContext:
    """One boundary's Unreleased state as presented to the model."""

    parent_unreleased: UnreleasedSection
    existing_map: dict[ChangelogCategory, list[str]]
    existing_entries: str | None
    can_revise: bool


@dataclass(slots=True)
class ChangelogWeaver:
    """Weave generated changelog entries into each commit of one compose run."""

    boundaries: list[_ComposeBoundaryState]
    config: CommitConfig

    @classmethod
    def create(
        cls,
        repo_dir: str | os.PathLike[str],
        config: CommitConfig,
        target_tree: str,
    ) -> ChangelogWeaver | None:
        """Claim parseable changelogs from the compose target tree."""

        if not config.changelog_enabled:
            return None

        repo_path = Path(repo_dir)
        boundaries: list[_ComposeBoundaryState] = []
        for path in _find_changelogs(repo_path):
            rel_path = _relative_to(path, repo_path)
            target_entry = run_git(["ls-tree", target_tree, "--", rel_path], cwd=repo_path).stdout
            target_fields = target_entry.split(maxsplit=3)
            if len(target_fields) < 3:
                continue
            target_mode, target_oid = target_fields[0], target_fields[2]
            target_content = run_git(["show", f"{target_tree}:{rel_path}"], cwd=repo_path).stdout

            head_result = run_git(["rev-parse", f"HEAD:{rel_path}"], cwd=repo_path, check=False)
            original_head_oid = head_result.stdout.strip() if head_result.returncode == 0 else None
            head_unreleased = _head_unreleased(rel_path, path, repo_path)
            if head_unreleased is None:
                continue

            try:
                target_unreleased = parse_unreleased_section(target_content, path)
                worktree_content = path.read_text(encoding="utf-8")
                # weave_group re-parses the worktree copy unguarded; claim only boundaries it can parse.
                parse_unreleased_section(worktree_content, path)
            except OSError, UnicodeError, ValidationFailure:
                continue

            authored = _entries_added_since(head_unreleased, target_unreleased)
            boundaries.append(
                _ComposeBoundaryState(
                    rel_path=rel_path,
                    changelog_path=path,
                    is_package=_is_package_changelog(path, repo_path),
                    original_head_oid=original_head_oid,
                    target_mode=target_mode,
                    target_oid=target_oid,
                    target_content=target_content,
                    head_unreleased=head_unreleased,
                    authored=authored,
                    chain_content=target_content,
                    worktree_content=worktree_content,
                    last_written_worktree=worktree_content,
                    staged_index_oid=target_oid,
                )
            )

        return cls(boundaries, config) if boundaries else None

    def exclude_pathspecs(self) -> list[str]:
        """Return root-anchored pathspecs for changelogs owned by this run."""

        return [f":(exclude,top){boundary.rel_path}" for boundary in self.boundaries]

    def seed_temp_index(
        self,
        repo_dir: str | os.PathLike[str],
        index_file: str | os.PathLike[str],
    ) -> None:
        """Seed authored target changelog blobs unless an earlier round already wove them."""

        for state in self.boundaries:
            current_result = run_git(
                ["rev-parse", f"HEAD:{state.rel_path}"],
                cwd=repo_dir,
                check=False,
            )
            current_oid = current_result.stdout.strip() if current_result.returncode == 0 else None
            if current_oid == state.original_head_oid:
                run_git(
                    [
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        f"{state.target_mode},{state.target_oid},{state.rel_path}",
                    ],
                    cwd=repo_dir,
                    index_file=index_file,
                )
                state.chain_content = state.target_content
            else:
                state.chain_content = run_git(
                    ["show", f"HEAD:{state.rel_path}"],
                    cwd=repo_dir,
                ).stdout

    async def generate_groups(
        self, groups: Sequence[tuple[Sequence[str], str, str]], repo_dir: str | os.PathLike[str]
    ) -> list[GroupEntries]:
        """Generate every group's changelog entries concurrently, ahead of the serial weave loop.

        ``groups`` holds each group's ``(files, diff, stat)``. Generation runs
        against the chain state as it stands now, so a later group's model does
        not see an earlier group's new entries; :meth:`weave_group` reconciles
        each result against the chain as it has accumulated by then — duplicate
        detection drops cross-group repeats (judged in the uncertain band) and
        revisions that no longer match are skipped. This turns N serial
        changelog calls (the bulk of compose wall time) into one parallel wave.
        """

        changelog_paths = [state.changelog_path for state in self.boundaries]
        states = {state.changelog_path: state for state in self.boundaries}
        matched_per_group = [detect_boundaries(files, changelog_paths, repo_dir) for files, _, _ in groups]
        prompt_diffs = [self._prompt_diff(diff) for _, diff, _ in groups]
        work: list[tuple[int, Path]] = []
        tasks = []
        for index, ((_, _, stat), matched) in enumerate(zip(groups, matched_per_group, strict=True)):
            for boundary in matched:
                context = self._boundary_context(states[boundary.changelog_path])
                if context is None:
                    continue
                work.append((index, boundary.changelog_path))
                tasks.append(self._generate(states[boundary.changelog_path], context, stat, prompt_diffs[index]))
        results = await asyncio.gather(*tasks)
        generated: list[GroupEntries] = [{} for _ in groups]
        for (index, path), result in zip(work, results, strict=True):
            generated[index][path] = result
        return generated

    async def _generate(
        self, state: _ComposeBoundaryState, context: _BoundaryContext, stat: str, group_diff: str
    ) -> tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]:
        return await generate_changelog_entries(
            state.changelog_path,
            state.is_package,
            stat,
            group_diff,
            context.existing_entries,
            _format_entry_map(state.authored),
            context.can_revise,
            self.config,
        )

    def _boundary_context(self, state: _ComposeBoundaryState) -> _BoundaryContext | None:
        """The boundary's current Unreleased state as the model should see it, or None if unparseable."""
        try:
            parent_unreleased = parse_unreleased_section(state.chain_content, state.changelog_path)
        except ValidationFailure:
            return None
        authored_casefolds = {entry.casefold() for values in state.authored.values() for entry in values}
        existing_map: dict[ChangelogCategory, list[str]] = {}
        for category, values in parent_unreleased.entries.items():
            existing = [entry for entry in values if entry.casefold() not in authored_casefolds]
            if existing:
                existing_map[category] = existing
        existing_entries = _format_entry_map(existing_map)
        can_revise = bool(self.config.changelog_revise) and bool(existing_entries)
        return _BoundaryContext(parent_unreleased, existing_map, existing_entries, can_revise)

    def contexts_for_group(
        self,
        files: Sequence[str],
        repo_dir: str | os.PathLike[str],
    ) -> tuple[ChangelogContext, ...]:
        """Export seeded boundary contexts for one compose group without re-reading its diff."""

        states = {state.changelog_path: state for state in self.boundaries}
        contexts: list[ChangelogContext] = []
        for boundary in detect_boundaries(files, tuple(states), repo_dir):
            state = states[boundary.changelog_path]
            context = self._boundary_context(state)
            if context is None:
                continue
            contexts.append(
                ChangelogContext(
                    path=state.rel_path,
                    files=boundary.files,
                    is_package=state.is_package,
                    existing_entries=context.existing_entries,
                    authored_entries=_format_entry_map(state.authored),
                    can_revise=context.can_revise,
                )
            )
        return tuple(contexts)

    def updates_for_group(
        self,
        updates: Sequence[ChangelogUpdate],
        repo_dir: str | os.PathLike[str],
    ) -> GroupEntries:
        """Convert validated fused updates to the original paths consumed by :meth:`weave_group`."""

        repo_path = Path(repo_dir)
        states = {_relative_to(state.changelog_path, repo_path): state for state in self.boundaries}
        by_path = _validated_updates_by_path(updates, states, require_all=False)
        return {states[path].changelog_path: (update.entries, update.revisions) for path, update in by_path.items()}

    def _prompt_diff(self, diff: str) -> str:
        group_diff = scrub_diff_for_prompt(diff)
        if len(group_diff) > self.config.max_diff_length:
            group_diff = smart_truncate_diff(group_diff, self.config.max_diff_length, self.config)
        return group_diff

    async def weave_group(
        self,
        files: Sequence[str],
        diff: str,
        stat: str,
        repo_dir: str | os.PathLike[str],
        index_file: str | os.PathLike[str],
        generated: GroupEntries | None = None,
    ) -> None:
        """Reconcile and stage changelog entries for one compose group.

        ``generated`` supplies the group's entries from :meth:`generate_groups`;
        a boundary missing from it is generated here, serially.
        """

        matched = detect_boundaries(
            files,
            [state.changelog_path for state in self.boundaries],
            repo_dir,
        )
        states = {state.changelog_path: state for state in self.boundaries}
        group_diff = self._prompt_diff(diff)
        for boundary in matched:
            state = states[boundary.changelog_path]
            context = self._boundary_context(state)
            if context is None:
                continue
            parent_unreleased, existing_map, can_revise = (
                context.parent_unreleased,
                context.existing_map,
                context.can_revise,
            )
            if generated is not None and boundary.changelog_path in generated:
                entries, revisions = generated[boundary.changelog_path]
            else:
                entries, revisions = await self._generate(state, context, stat, group_diff)
            revisable = {entry.casefold() for values in existing_map.values() for entry in values}
            permitted = revisions if can_revise else ()
            revised, applied = apply_revisions(parent_unreleased, permitted, revisable)
            shadow, _ = apply_revisions(
                parse_unreleased_section(state.worktree_content, state.changelog_path),
                permitted,
                revisable,
            )

            entries = await drop_duplicate_entries(
                entries, revised.entries, shadow.entries, judge=Judge.from_config(self.config), config=self.config
            )
            if not entries and not applied:
                continue

            state.chain_content = write_entries(revised, entries)
            state.worktree_content = write_entries(shadow, entries)
            stage_changelog_blob(
                state.rel_path,
                state.chain_content,
                repo_dir,
                index_file=index_file,
            )
            _report_applied_revisions(applied)

    def flush(self, repo_dir: str | os.PathLike[str]) -> None:
        """Publish woven content unless the user changed the worktree or index mid-run."""

        for state in self.boundaries:
            try:
                current_worktree = state.changelog_path.read_text(encoding="utf-8")
            except OSError, UnicodeError:
                current_worktree = None
            if current_worktree == state.last_written_worktree:
                if current_worktree != state.worktree_content:
                    state.changelog_path.write_text(state.worktree_content, encoding="utf-8")
                state.last_written_worktree = state.worktree_content
            else:
                style.status(
                    f"{style.info('›')} "
                    f"{style.dim('changelog: worktree edited mid-run; left untouched:')} "
                    f"{state.rel_path}"
                )

            current_index_oid = _staged_changelog_oid(state.rel_path, repo_dir)
            if current_index_oid == state.staged_index_oid:
                state.staged_index_oid = stage_changelog_blob(
                    state.rel_path,
                    state.chain_content,
                    repo_dir,
                )
            else:
                style.status(
                    f"{style.info('›')} "
                    f"{style.dim('changelog: index staged mid-run; left untouched:')} "
                    f"{state.rel_path}"
                )


def parse_unreleased_section(content: str, path: str | os.PathLike[str] = "CHANGELOG.md") -> UnreleasedSection:
    """Parse the `[Unreleased]` section boundaries and existing entries."""

    lines = tuple(content.split("\n"))
    header_line: int | None = None
    for index, line in enumerate(lines):
        trimmed = line.strip().lower()
        if "[unreleased]" in trimmed or trimmed == "## unreleased":
            header_line = index
            break
    if header_line is None:
        raise ValidationFailure(f"No [Unreleased] section in {path}", field="changelog")

    end_line = len(lines)
    for index in range(header_line + 1, len(lines)):
        trimmed = lines[index].strip()
        if (trimmed.startswith("## [") and "]" in trimmed) or (
            trimmed.startswith("## ")
            and len(trimmed) > 3
            and (trimmed[3].isdigit() or (trimmed[3] in "vV" and len(trimmed) > 4 and trimmed[4].isdigit()))
        ):
            end_line = index
            break

    entries: dict[ChangelogCategory, list[str]] = {}
    for _, category, entry in _section_bullets(lines, header_line + 1, end_line):
        entries.setdefault(category, []).append(entry)
    return UnreleasedSection(Path(path), lines, header_line, end_line, entries)


def write_entries(
    section: UnreleasedSection,
    new_entries: Mapping[ChangelogCategory, Sequence[str]] | Mapping[str, Sequence[str]],
) -> str:
    """Return the changelog with ``new_entries`` added at the top of their Unreleased categories.

    Only the inserted lines change: subsection headings, prose, unknown categories and
    existing bullets stay byte-for-byte. New bullets join a category's leading list (in its
    marker style) or, when the category opens with a subsection, sit as their own list above
    it. A missing category is created at its :meth:`ChangelogCategory.render_order` slot.
    """

    lines = list(section.lines)
    start, end = section.header_line + 1, section.end_line
    order = ChangelogCategory.render_order()
    for category, fresh in sorted(_coerce_entries(new_entries).items(), key=lambda item: order.index(item[0])):
        headings = _category_headings(lines, start, end)
        heading = next((index for index, found in headings if found is category), None)
        if heading is None:
            later = next((index for index, found in headings if order.index(found) > order.index(category)), None)
            if later is None:
                later = next(index + 1 for index in range(end - 1, section.header_line - 1, -1) if lines[index].strip())
            end += _splice(lines, later, [f"### {category.value}", "", *fresh])
            continue
        first = next((index for index in range(heading + 1, end) if lines[index].strip()), None)
        if first is not None and _is_list_item(lines[first]):
            prefix = _bullet_prefix(lines[first])
            lines[first:first] = [prefix + entry.removeprefix("- ") for entry in fresh]
            end += len(fresh)
        else:
            end += _splice(lines, heading + 1 if first is None else first, fresh)
    return "\n".join(lines)


def apply_revisions(
    section: UnreleasedSection,
    revisions: Sequence[ChangelogRevision],
    revisable: Collection[str],
) -> tuple[UnreleasedSection, list[ChangelogRevision]]:
    """Reword or drop existing Unreleased bullets in place; returns the revised section and the revisions that matched.

    Each revision takes the first bullet (file order) whose normalized text matches ``old``
    case-insensitively, and only when that text is in ``revisable``. A reworded bullet keeps
    its indent and marker; a dropped one takes its continuation lines along, plus any
    subsection or category heading it leaves empty.
    """

    lines = list(section.lines)
    start, end = section.header_line + 1, section.end_line
    applied: list[ChangelogRevision] = []
    for revision in revisions:
        target = revision.old.casefold()
        if target not in revisable:
            continue
        bullet = next((i for i, _, entry in _section_bullets(lines, start, end) if entry.casefold() == target), None)
        if bullet is None:
            continue
        if revision.new is None:
            end -= _drop_item(lines, bullet, start, end)
        else:
            lines[bullet] = _bullet_prefix(lines[bullet]) + revision.new.removeprefix("- ")
        applied.append(revision)
    if not applied:
        return section, applied
    return parse_unreleased_section("\n".join(lines), section.path), applied


def _section_bullets(lines: Sequence[str], start: int, end: int) -> Iterator[tuple[int, ChangelogCategory, str]]:
    """Index, category and normalized text of each bullet filed under a known `###` category."""

    category: ChangelogCategory | None = None
    for index in range(start, end):
        trimmed = lines[index].strip()
        if (name := _subheading(trimmed)) is not None:
            category = _category_or_none(name)
        elif category is not None and trimmed.startswith(("- ", "* ")):
            entry = normalize_changelog_entry(trimmed)
            if entry is not None:
                yield index, category, entry


def _category_headings(lines: Sequence[str], start: int, end: int) -> list[tuple[int, ChangelogCategory]]:
    """Index and category of each `###` heading naming a known category, in file order."""

    return [
        (index, category)
        for index in range(start, end)
        if (name := _subheading(lines[index])) is not None and (category := _category_or_none(name)) is not None
    ]


def _subheading(line: str) -> str | None:
    """Title of a `###` heading line, or None for any other line."""

    trimmed = line.strip()
    return trimmed.removeprefix("### ") if trimmed.startswith("### ") else None


def _heading_level(line: str) -> int:
    """ATX heading level of ``line``; 0 when it is not a heading."""

    stripped = line.strip()
    level = len(stripped) - len(stripped.lstrip("#"))
    return level if 1 <= level <= 6 and stripped[level : level + 1] in ("", " ", "\t") else 0


def _indent(line: str) -> int:
    return len(line) - len(line.lstrip())


def _is_list_item(line: str) -> bool:
    return line.lstrip().startswith(("- ", "* "))


def _bullet_prefix(line: str) -> str:
    """Indent and marker (``- ``/``* ``) of a list item line."""

    return line[: _indent(line) + 2]


def _splice(lines: list[str], index: int, block: Sequence[str]) -> int:
    """Insert ``block`` at ``index`` set off by blank lines from non-blank neighbours; returns lines added."""

    lead = [""] if index > 0 and lines[index - 1].strip() else []
    trail = [""] if index < len(lines) and lines[index].strip() else []
    padded = [*lead, *block, *trail]
    lines[index:index] = padded
    return len(padded)


def _item_end(lines: Sequence[str], index: int, end: int) -> int:
    """Index just past the list item at ``index``: its bullet line plus continuation lines and nested items."""

    indent = _indent(lines[index])
    stop = index + 1
    while stop < end:
        line = lines[stop]
        if not line.strip():
            following = next((i for i in range(stop + 1, end) if lines[i].strip()), end)
            if following == end or _indent(lines[following]) <= indent:
                break
            stop = following
        elif _heading_level(line) or (_is_list_item(line) and _indent(line) <= indent):
            break
        stop += 1
    return stop


def _drop_item(lines: list[str], index: int, start: int, end: int) -> int:
    """Delete the list item at ``index`` and each enclosing `###`/`####` heading it leaves empty.

    ``start``/``end`` bound the Unreleased body; returns how many lines were deleted.
    """

    stop = _item_end(lines, index, end)
    if stop < end and not lines[stop].strip() and not lines[index - 1].strip():
        stop += 1  # the item was a paragraph of its own: keep one separating blank, not two
    del lines[index:stop]
    removed = stop - index
    end -= removed
    emptied_level = 7
    while True:
        heading = next((i for i in range(index - 1, start - 1, -1) if _heading_level(lines[i])), None)
        if heading is None:
            break
        level = _heading_level(lines[heading])
        if level < 3 or level >= emptied_level:
            break
        body_end = next((i for i in range(heading + 1, end) if 0 < _heading_level(lines[i]) <= level), end)
        if any(lines[i].strip() for i in range(heading + 1, body_end)):
            break
        del lines[heading:body_end]
        removed += body_end - heading
        end -= body_end - heading
        index, emptied_level = heading, level
    return removed


def stage_changelog_blob(
    rel_path: str | os.PathLike[str],
    content: str,
    dir: str | os.PathLike[str] = ".",
    index_file: str | os.PathLike[str] | None = None,
) -> str:
    """Stage exact changelog content as an index blob without git-adding the worktree copy."""

    path_text = os.fspath(rel_path)
    oid = run_git(
        ["hash-object", "-w", "--stdin"],
        cwd=dir,
        input_text=content,
        index_file=index_file,
    ).stdout.strip()
    if not oid:
        raise GitError(f"git hash-object returned no oid for {path_text}")
    mode = _staged_changelog_mode(path_text, dir, index_file=index_file)
    run_git(
        ["update-index", "--add", "--cacheinfo", f"{mode},{oid},{path_text}"],
        cwd=dir,
        index_file=index_file,
    )
    return oid


def detect_boundaries(
    files: Sequence[str],
    changelogs: Sequence[str | os.PathLike[str]],
    dir: str | os.PathLike[str] = ".",
) -> list[ChangelogBoundary]:
    """Group staged files under the nearest ancestor CHANGELOG.md."""

    repo_dir = Path(dir)
    dir_to_changelog: dict[str, Path] = {}
    root_changelog: Path | None = None
    for changelog in changelogs:
        path = Path(changelog)
        rel = _relative_to(path, repo_dir)
        parent_key = str(Path(rel).parent).replace("\\", "/")
        if parent_key in ("", "."):
            root_changelog = path
        else:
            dir_to_changelog[parent_key] = path

    grouped: dict[Path, list[str]] = {}
    for file in files:
        current = Path(file).parent
        selected: Path | None = None
        while True:
            key = "" if str(current) == "." else str(current).replace("\\", "/")
            if key in dir_to_changelog:
                selected = dir_to_changelog[key]
                break
            if key == "":
                break
            parent = current.parent
            if parent == current:
                break
            current = parent
        if selected is None:
            selected = root_changelog
        if selected is not None:
            grouped.setdefault(selected, []).append(file)

    return [
        ChangelogBoundary(path, tuple(paths)) for path, paths in sorted(grouped.items(), key=lambda item: str(item[0]))
    ]


async def generate_changelog_entries(
    changelog_path: str | os.PathLike[str],
    is_package_changelog: bool,
    stat: str,
    diff: str,
    existing_entries: str | None,
    authored_entries: str | None,
    can_revise: bool,
    config: CommitConfig,
    observations: str | None = None,
) -> tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]:
    """Ask the configured model for Keep a Changelog entries and revision operations.

    ``observations`` (per-file change-summary markdown) replaces the raw diff
    in the prompt when provided. With a judge configured and a change of at
    least :attr:`CommitConfig.changelog_gate_min_tokens`, the model is only
    called when the change is judged user-visible at
    :attr:`CommitConfig.changelog_gate_threshold` or above.
    """

    judge = Judge.from_config(config)
    threshold = float(config.changelog_gate_threshold)
    changes = observations or diff
    if judge is not None and threshold > 0 and len(changes) // 4 >= int(config.changelog_gate_min_tokens):
        probability = await judge_changelog_worthiness(judge, changelog_path, stat, changes)
        if probability is not None and probability < threshold:
            style.status(
                f"{style.dim('›')} {style.dim(f'changelog: {os.fspath(changelog_path)} skipped, change judged not user-visible (p={probability:.2f})')}"
            )
            return {}, []
    prompt = render_changelog_prompt(
        os.fspath(changelog_path),
        is_package_changelog,
        stat,
        diff,
        existing_entries=existing_entries,
        authored_entries=authored_entries,
        can_revise=can_revise,
        observations=observations,
    )
    from .api import OneShotSpec, run_oneshot

    response = await run_oneshot(
        config,
        OneShotSpec(
            operation="changelog",
            model=resolve_model_name(config.analysis_model),
            prompt_family="changelog",
            system_prompt=prompt.system,
            user_prompt=prompt.user,
            tool_name="create_changelog_entries",
            progress_label="changelog",
            cacheable=True,
            reasoning_effort=config.changelog_reasoning_effort,
        ),
    )
    output = response.output if hasattr(response, "output") else response
    payload = _parse_jsonish(output)
    entries = payload.get("entries", payload) if isinstance(payload, dict) else {}
    revisions = parse_changelog_revisions(response.text_content or "") if can_revise else []
    return _coerce_entries(entries if isinstance(entries, Mapping) else {}), revisions


async def judge_changelog_worthiness(
    judge: Judge, changelog_path: str | os.PathLike[str], stat: str, changes: str
) -> float | None:
    """Probability that ``changes`` (a diff or observation markdown) deserve a changelog entry; None on failure."""

    request = JudgmentRequest(
        {
            "changelog": os.fspath(changelog_path),
            "stat": stat,
            "changes": clip_text(changes, _WORTHINESS_HEAD_CHARS, _WORTHINESS_TAIL_CHARS),
        },
        {"worthy": _WORTHINESS_QUESTION},
    )
    (answers,) = await judge.judge_all("changelog-worthy", [request])
    if answers is None:
        return None
    answer = answers["worthy"]
    return answer.probability if isinstance(answer, NoulAnswer) else None


def normalize_changelog_entry(entry: str) -> str | None:
    """Normalize one model-emitted changelog line to a markdown bullet."""

    stripped = entry.strip()
    if stripped.startswith(("- ", "* ")):
        stripped = stripped[2:].strip()
    return f"- {stripped}" if stripped else None


def parse_changelog_revisions(text: str) -> list[ChangelogRevision]:
    """Parse revision pairs from the first ``<revise>`` block."""

    block = _REVISE_BLOCK_RE.search(text)
    if block is None:
        return []

    revisions: list[ChangelogRevision] = []
    pending_old: str | None = None
    for line in block.group(1).splitlines():
        match = _REVISION_LINE_RE.fullmatch(line)
        if match is None:
            continue
        prefix, entry = match.groups()
        if prefix.casefold() == "old":
            pending_old = normalize_changelog_entry(entry)
        elif pending_old is not None:
            revisions.append(ChangelogRevision(pending_old, normalize_changelog_entry(entry)))
            pending_old = None
    return revisions


def _staged_files(dir: Path) -> list[str]:
    stdout = run_git(["diff", "--cached", "--name-only"], cwd=dir).stdout
    return [line for line in stdout.splitlines() if line]


def _find_changelogs(dir: Path) -> list[Path]:
    """Locate tracked plus untracked-but-not-ignored CHANGELOG.md files via git.

    Avoids walking the worktree (node_modules and friends); gitignored
    changelogs are never changelog boundaries.
    """
    patterns = ["CHANGELOG.md", "**/CHANGELOG.md"]
    tracked = run_git(["ls-files", "--", *patterns], cwd=dir).stdout
    untracked = run_git(["ls-files", "--others", "--exclude-standard", "--", *patterns], cwd=dir).stdout
    paths = {
        dir / line for line in (*tracked.splitlines(), *untracked.splitlines()) if Path(line).name == "CHANGELOG.md"
    }
    return sorted(paths, key=lambda path: _relative_to(path, dir))


def _staged_changelog_content(rel_path: str, dir: str | os.PathLike[str]) -> str | None:
    result = run_git(["show", f":{rel_path}"], cwd=dir, check=False, allow_exit_codes=(128,))
    return result.stdout if result.returncode == 0 else None


def _staged_changelog_mode(
    rel_path: str,
    dir: str | os.PathLike[str],
    index_file: str | os.PathLike[str] | None = None,
) -> str:
    result = run_git(
        ["ls-files", "-s", "--", rel_path],
        cwd=dir,
        index_file=index_file,
    ).stdout.strip()
    return result.split(maxsplit=1)[0] if result else "100644"


def _staged_changelog_oid(rel_path: str, dir: str | os.PathLike[str]) -> str | None:
    result = run_git(["ls-files", "-s", "--", rel_path], cwd=dir).stdout.strip()
    fields = result.split(maxsplit=3)
    return fields[1] if len(fields) >= 2 else None


def _head_unreleased(
    rel_path: str,
    path: str | os.PathLike[str],
    dir: str | os.PathLike[str],
) -> UnreleasedSection | None:
    """Parse HEAD's copy of the changelog.

    Returns an empty section when the file is absent from HEAD (every current
    entry was authored in this change), or None when HEAD's copy is unparseable.
    """
    result = run_git(["show", f"HEAD:{rel_path}"], cwd=dir, check=False, allow_exit_codes=(128,))
    if result.returncode != 0:
        return UnreleasedSection(Path(path), (), 0, 0, {})
    try:
        return parse_unreleased_section(result.stdout, path)
    except ValidationFailure:
        return None


def _entries_added_since(head: UnreleasedSection, committed: UnreleasedSection) -> dict[ChangelogCategory, list[str]]:
    """Unreleased entries the commit's own changelog copy adds over HEAD — hand-written for this change.

    ``committed`` is the staged copy (compose: the target tree), never the worktree: unstaged entries
    may document other pending work, so they only reach the duplicate filter.
    """
    baseline = {entry.casefold() for values in head.entries.values() for entry in values}
    added: dict[ChangelogCategory, list[str]] = {}
    for category, values in committed.entries.items():
        fresh = [entry for entry in values if entry.casefold() not in baseline]
        if fresh:
            added[category] = list(dict.fromkeys(fresh))
    return added


def _report_applied_revisions(applied: Sequence[ChangelogRevision]) -> None:
    for revision in applied:
        if revision.new is None:
            style.status(f"{style.info('›')} {style.dim('changelog: dropped:')} {style.dim(revision.old)}")
        else:
            style.status(
                f"{style.info('›')} {style.dim('changelog: revised:')} "
                f"{style.dim(revision.old)} {style.dim('→')} {revision.new}"
            )


async def drop_duplicate_entries(
    entries: Mapping[ChangelogCategory, Sequence[str]],
    *existing: Mapping[ChangelogCategory, Sequence[str]] | None,
    judge: Judge | None,
    config: CommitConfig,
) -> dict[ChangelogCategory, list[str]]:
    """Drop generated entries that restate ones already in the Unreleased section.

    Verbatim (case-insensitive) repeats and empty entries are always dropped.
    Content-stem overlap with a single existing entry decides the clear cases
    locally: ≥70% is a duplicate, <15% is distinct. The band between is judged
    when a judge is configured — one request per call, one yes/no question per
    candidate against the full list of existing entries, dropping at
    :attr:`CommitConfig.changelog_duplicate_threshold` — and kept otherwise.
    """
    existing_entries = [
        entry for entry_map in existing if entry_map is not None for values in entry_map.values() for entry in values
    ]
    verbatim = {entry.casefold() for entry in existing_entries}
    candidates: list[tuple[ChangelogCategory, str]] = [
        (category, value)
        for category, values in entries.items()
        for value in values
        if value.casefold() not in verbatim and _entry_words(value)
    ]
    existing_keys = [_stem_keys(_entry_words(entry)) for entry in existing_entries]
    duplicates: set[str] = set()
    uncertain: list[str] = []
    for _, value in candidates:
        keys = _stem_keys(_entry_words(value))
        overlap = max((len(keys & seen) / len(keys) for seen in existing_keys if keys), default=0.0)
        if overlap >= 0.7:
            duplicates.add(value)
        elif overlap >= 0.15:
            uncertain.append(value)
    if judge is not None and uncertain:
        judged = await _judge_duplicates(judge, existing_entries, uncertain, config)
        if judged is not None:
            duplicates |= judged
    deduped: dict[ChangelogCategory, list[str]] = {}
    for category, value in candidates:
        if value not in duplicates:
            deduped.setdefault(category, []).append(value)
    return deduped


async def _judge_duplicates(
    judge: Judge, existing_entries: Sequence[str], candidates: Sequence[str], config: CommitConfig
) -> set[str] | None:
    """Candidates judged to restate an existing entry, or None when the judgment failed."""
    keys = {f"C{index}": value for index, value in enumerate(candidates, start=1)}
    request = JudgmentRequest(
        {
            "existing_entries": "\n".join(existing_entries),
            "candidates": "\n".join(f"{key}: {value}" for key, value in keys.items()),
        },
        {
            key: Noul(
                f"Does candidate {key} restate a change already covered by one of the existing entries, "
                "rather than describing a distinct change?",
                yes=_DUPLICATE_YES,
                no=_DUPLICATE_NO,
            )
            for key in keys
        },
    )
    (answers,) = await judge.judge_all("changelog-duplicate", [request])
    if answers is None:
        return None
    threshold = float(config.changelog_duplicate_threshold)
    return {
        keys[key]
        for key, answer in answers.items()
        if isinstance(answer, NoulAnswer) and answer.probability >= threshold
    }


_ENTRY_STOPWORDS = frozenset(
    "the and for was were now not that this with from are has had have been its when then than into also".split()
)
_BOILERPLATE_WORDS = frozenset(
    "added add fixed fix changed change removed remove updated update improved improve support new".split()
)


def _stem_keys(words: set[str]) -> set[str]:
    """Four-letter stems of an entry's content words, minus the verbs nearly every bullet shares.

    Two entries about the same change share such stems (``retry``/``retries`` both give
    ``retr``) while the shared verbs (``added``, ``fixed``) never count as evidence.
    """
    return {word[:4] for word in words if word not in _BOILERPLATE_WORDS}


def _entry_words(entry: str) -> set[str]:
    """Significant, lightly-stemmed content words of a changelog bullet."""
    words = re.findall(r"[a-z0-9]+", entry.casefold())
    return {word.removesuffix("s") for word in words if len(word) > 2 and word not in _ENTRY_STOPWORDS}


def _diff_for_files(files: Sequence[str], dir: Path, max_len: int) -> str:
    if not files:
        return ""
    diff = run_git(["diff", "--cached", "--", *files], cwd=dir).stdout
    if len(diff) > max_len:
        return run_git(["diff", "--cached", "-U1", "--", *files], cwd=dir).stdout
    return diff


def _stat_for_files(files: Sequence[str], dir: Path) -> str:
    if not files:
        return ""
    return run_git(["diff", "--cached", "--stat", "--", *files], cwd=dir).stdout


def _category_or_none(name: str) -> ChangelogCategory | None:
    normalized = name.strip().lower()
    valid = {category.value.lower() for category in ChangelogCategory} | {
        category.name.lower() for category in ChangelogCategory
    }
    if normalized == "breaking":
        return ChangelogCategory.BREAKING
    return ChangelogCategory.from_name(name) if normalized in valid else None


def _coerce_entries(
    entries: Mapping[ChangelogCategory, Sequence[str]] | Mapping[str, Sequence[str]],
) -> dict[ChangelogCategory, list[str]]:
    coerced: dict[ChangelogCategory, list[str]] = {}
    for key, values in entries.items():
        category = key if isinstance(key, ChangelogCategory) else ChangelogCategory.from_name(str(key))
        iterable = [values] if isinstance(values, str) else values
        normalized = [line for value in iterable if (line := normalize_changelog_entry(str(value))) is not None]
        if normalized:
            coerced.setdefault(category, []).extend(normalized)
    return coerced


def _format_entry_map(entries: Mapping[ChangelogCategory, Sequence[str]]) -> str | None:
    lines: list[str] = []
    for category in ChangelogCategory.render_order():
        values = entries.get(category, [])
        if not values:
            continue
        lines.append(f"### {category.value}")
        lines.extend(values)
        lines.append("")
    return "\n".join(lines).strip() or None


def _format_existing_entries(unreleased: UnreleasedSection) -> str | None:
    return _format_entry_map(unreleased.entries)


def _relative_to(path: Path, dir: Path | str | os.PathLike[str]) -> str:
    repo_dir = Path(dir)
    try:
        return path.relative_to(repo_dir).as_posix()
    except ValueError:
        return path.as_posix()


def _is_package_changelog(path: Path, dir: Path) -> bool:
    parent = path.parent
    try:
        return parent.resolve() != dir.resolve()
    except OSError:
        return parent != dir


def _parse_jsonish(value: Any) -> Any:
    """Coerce a model changelog response into a mapping.

    Markdown is the canonical changelog format, so a raw string is parsed with
    ``parse_changelog_response``. A string that looks like a JSON object/array
    (or a ```json fenced block) is decoded with ``json.loads`` first, since the
    rare provider that still emits JSON has bullet lines like ``"Added": [...]``
    that the markdown parser would misread as headings.
    """
    if isinstance(value, str):
        text = value.strip()
        if text.startswith("```"):
            text = text.strip("`").removeprefix("json").strip()
        if text.startswith(("{", "[")):
            try:
                return json.loads(text)
            except json.JSONDecodeError:
                pass
        return parse_changelog_response(text)
    if hasattr(value, "model_dump"):
        return value.model_dump()
    if hasattr(value, "__dict__"):
        return vars(value)
    return value


__all__ = [
    "ChangelogBoundary",
    "ChangelogWeaver",
    "PreparedChangelogFlow",
    "UnreleasedSection",
    "apply_changelog_updates",
    "apply_revisions",
    "drop_duplicate_entries",
    "detect_boundaries",
    "generate_changelog_entries",
    "generate_changelog_updates",
    "normalize_changelog_entry",
    "parse_changelog_revisions",
    "parse_unreleased_section",
    "judge_changelog_worthiness",
    "prepare_changelog_flow",
    "prepare_synthesized_updates",
    "prepared_changelog_contexts",
    "run_changelog_flow",
    "stage_changelog_blob",
    "write_entries",
]

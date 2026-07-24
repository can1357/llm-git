"""Changelog maintenance for staged commits."""

from __future__ import annotations

import asyncio
import json
import os
import re
from collections.abc import Collection, Mapping, Sequence
from dataclasses import dataclass, replace
from pathlib import Path
from typing import TYPE_CHECKING, Any

from . import style
from .diffing import scrub_diff_for_prompt, smart_truncate_diff
from .errors import GitError, ValidationFailure
from .git import run_git
from .markdown_output import parse_changelog_response
from .models import ChangelogCategory, resolve_model_name
from .templates import render_changelog_prompt

if TYPE_CHECKING:
    from .config import CommitConfig
    from .map_reduce import FileObservation

_REVISE_BLOCK_RE = re.compile(r"<revise\b[^>]*>(.*?)(?:</revise>|$)", re.IGNORECASE | re.DOTALL)
_REVISION_LINE_RE = re.compile(r"^\s*(OLD|NEW)\s*:(.*)$", re.IGNORECASE)


@dataclass(frozen=True, slots=True)
class UnreleasedSection:
    """Parsed bounds and entries for a changelog's Unreleased section."""

    path: Path
    header_line: int
    end_line: int
    entries: dict[ChangelogCategory, list[str]]


@dataclass(frozen=True, slots=True)
class ChangelogRevision:
    """One reconcile operation that replaces or drops an existing Unreleased entry."""

    old: str
    new: str | None


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
    changelog_content: str
    worktree_content: str
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
            authored_entries = _format_entry_map(_entries_added_since(head_unreleased, unreleased, worktree_unreleased))

        can_revise = bool(config.changelog_revise) and head_unreleased is not None and bool(existing_entries)
        prepared.preps.append(
            _BoundaryPrep(
                boundary,
                diff,
                stat,
                rel_path,
                is_tracked,
                changelog_content,
                worktree_content,
                unreleased,
                worktree_unreleased,
                head_unreleased,
                existing_entries,
                authored_entries,
                can_revise,
            )
        )
    return prepared


async def generate_changelog_updates(
    prepared: PreparedChangelogFlow,
    config: CommitConfig,
    observations: Sequence[FileObservation] | None = None,
) -> list[tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]]:
    """Run the changelog model concurrently for every prepared boundary.

    When map-phase ``observations`` are given, boundaries covered by them are
    prompted with per-file change summaries instead of their raw diff.
    """

    return list(
        await asyncio.gather(
            *(
                generate_changelog_entries(
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
                for prep in prepared.preps
            )
        )
    )


def apply_changelog_updates(
    prepared: PreparedChangelogFlow,
    generated: Sequence[tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]],
) -> list[ChangelogBoundary]:
    """Write generated entries into worktree changelogs and stage exact blobs."""

    repo_dir = prepared.repo_dir
    updated: list[ChangelogBoundary] = []
    untracked_to_stage: list[str] = []
    for prep, (entries, revisions) in zip(prepared.preps, generated, strict=True):
        staged_entries = prep.unreleased.entries
        worktree_entries = prep.worktree_unreleased.entries if prep.worktree_unreleased is not None else None
        applied: list[ChangelogRevision] = []
        if prep.can_revise:
            assert prep.head_unreleased is not None
            revisable = {entry.casefold() for values in prep.head_unreleased.entries.values() for entry in values}
            staged_entries, applied = apply_revisions(staged_entries, revisions, revisable)
            if worktree_entries is not None:
                worktree_entries, _ = apply_revisions(worktree_entries, revisions, revisable)
        entries = _drop_duplicate_entries(entries, staged_entries, worktree_entries)
        if not entries and not applied:
            continue

        updated_staged = write_entries(
            prep.changelog_content,
            replace(prep.unreleased, entries=staged_entries),
            entries,
        )
        updated_worktree = (
            write_entries(
                prep.worktree_content,
                replace(prep.worktree_unreleased, entries=worktree_entries),
                entries,
            )
            if prep.worktree_unreleased is not None and worktree_entries is not None
            else updated_staged
        )
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
    sections = [
        "# {}\n{}".format(item.file, "\n".join(f"- {text}" for text in item.observations))
        for item in observations
        if item.file in wanted and item.observations
    ]
    return "\n\n".join(sections) if sections else None


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
                worktree_unreleased = parse_unreleased_section(worktree_content, path)
            except OSError, UnicodeError, ValidationFailure:
                continue

            authored = _entries_added_since(
                head_unreleased,
                target_unreleased,
                worktree_unreleased if worktree_content != target_content else None,
            )
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

    async def weave_group(
        self,
        files: Sequence[str],
        diff: str,
        stat: str,
        repo_dir: str | os.PathLike[str],
        index_file: str | os.PathLike[str],
    ) -> None:
        """Generate, reconcile, and stage changelog entries for one compose group."""

        matched = detect_boundaries(
            files,
            [state.changelog_path for state in self.boundaries],
            repo_dir,
        )
        states = {state.changelog_path: state for state in self.boundaries}
        group_diff = scrub_diff_for_prompt(diff)
        if len(group_diff) > self.config.max_diff_length:
            group_diff = smart_truncate_diff(group_diff, self.config.max_diff_length, self.config)
        for boundary in matched:
            state = states[boundary.changelog_path]
            try:
                parent_unreleased = parse_unreleased_section(state.chain_content, state.changelog_path)
            except ValidationFailure:
                continue

            authored_casefolds = {entry.casefold() for values in state.authored.values() for entry in values}
            existing_map: dict[ChangelogCategory, list[str]] = {}
            for category, values in parent_unreleased.entries.items():
                existing = [entry for entry in values if entry.casefold() not in authored_casefolds]
                if existing:
                    existing_map[category] = existing
            existing_entries = _format_entry_map(existing_map)
            authored_entries = _format_entry_map(state.authored)
            can_revise = bool(self.config.changelog_revise) and bool(existing_entries)

            entries, revisions = await generate_changelog_entries(
                state.changelog_path,
                state.is_package,
                stat,
                group_diff,
                existing_entries,
                authored_entries,
                can_revise,
                self.config,
            )
            revisable = {entry.casefold() for values in existing_map.values() for entry in values}
            if can_revise:
                revised_map, applied = apply_revisions(
                    parent_unreleased.entries,
                    revisions,
                    revisable,
                )
            else:
                revised_map = {category: list(values) for category, values in parent_unreleased.entries.items()}
                applied = []

            shadow_unreleased = parse_unreleased_section(
                state.worktree_content,
                state.changelog_path,
            )
            if can_revise:
                shadow_revised_map, _ = apply_revisions(
                    shadow_unreleased.entries,
                    revisions,
                    revisable,
                )
            else:
                shadow_revised_map = {category: list(values) for category, values in shadow_unreleased.entries.items()}

            entries = _drop_duplicate_entries(entries, revised_map, shadow_revised_map)
            if not entries and not applied:
                continue

            state.chain_content = write_entries(
                state.chain_content,
                replace(parent_unreleased, entries=revised_map),
                entries,
            )
            state.worktree_content = write_entries(
                state.worktree_content,
                replace(shadow_unreleased, entries=shadow_revised_map),
                entries,
            )
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

    lines = content.splitlines()
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
    current_category: ChangelogCategory | None = None
    for line in lines[header_line + 1 : end_line]:
        trimmed = line.strip()
        if trimmed.startswith("### "):
            category_name = trimmed.removeprefix("### ").strip()
            current_category = _category_or_none(category_name)
        elif current_category is not None and trimmed.startswith(("- ", "* ")):
            normalized = normalize_changelog_entry(trimmed)
            if normalized is not None:
                entries.setdefault(current_category, []).append(normalized)
    return UnreleasedSection(Path(path), header_line, end_line, entries)


def write_entries(
    content: str,
    unreleased: UnreleasedSection,
    new_entries: Mapping[ChangelogCategory, Sequence[str]] | Mapping[str, Sequence[str]],
) -> str:
    """Rebuild the Unreleased section with new entries before existing entries."""

    normalized_new = _coerce_entries(new_entries)

    lines = content.split("\n")
    result: list[str] = list(lines[: unreleased.header_line + 1])
    result.append("")

    existing = unreleased.entries
    for category in ChangelogCategory.render_order():
        fresh = normalized_new.get(category, [])
        old = existing.get(category, [])
        if not fresh and not old:
            continue

        result.append(f"### {category.value}")
        result.append("")
        result.extend(fresh)
        result.extend(old)
        result.append("")

    while result and result[-1] == "":
        result.pop()
    result.append("")
    if unreleased.end_line < len(lines):
        result.extend(lines[unreleased.end_line :])

    return "\n".join(result)


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
    in the prompt when provided.
    """

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
        return UnreleasedSection(Path(path), 0, 0, {})
    try:
        return parse_unreleased_section(result.stdout, path)
    except ValidationFailure:
        return None


def _entries_added_since(
    head: UnreleasedSection,
    *sections: UnreleasedSection | None,
) -> dict[ChangelogCategory, list[str]]:
    """Unreleased entries present now but absent from HEAD — hand-written for this change."""
    baseline = {entry.casefold() for values in head.entries.values() for entry in values}
    added: dict[ChangelogCategory, list[str]] = {}
    for section in sections:
        if section is None:
            continue
        for category, values in section.entries.items():
            bucket = added.setdefault(category, [])
            for entry in values:
                if entry.casefold() not in baseline and entry not in bucket:
                    bucket.append(entry)
    return {category: values for category, values in added.items() if values}


def apply_revisions(
    entries: Mapping[ChangelogCategory, Sequence[str]],
    revisions: Sequence[ChangelogRevision],
    revisable: Collection[str],
) -> tuple[dict[ChangelogCategory, list[str]], list[ChangelogRevision]]:
    """Apply safe reconciliation operations and return the operations that matched."""

    revised = {category: list(values) for category, values in entries.items()}
    applied: list[ChangelogRevision] = []
    for revision in revisions:
        target = revision.old.casefold()
        if target not in revisable:
            continue
        matched = False
        for values in revised.values():
            for index, entry in enumerate(values):
                if entry.casefold() != target:
                    continue
                if revision.new is None:
                    values.pop(index)
                else:
                    values[index] = revision.new
                applied.append(revision)
                matched = True
                break
            if matched:
                break
    return revised, applied


def _report_applied_revisions(applied: Sequence[ChangelogRevision]) -> None:
    for revision in applied:
        if revision.new is None:
            style.status(f"{style.info('›')} {style.dim('changelog: dropped:')} {style.dim(revision.old)}")
        else:
            style.status(
                f"{style.info('›')} {style.dim('changelog: revised:')} "
                f"{style.dim(revision.old)} {style.dim('→')} {revision.new}"
            )


def _drop_duplicate_entries(
    entries: Mapping[ChangelogCategory, Sequence[str]],
    *existing: Mapping[ChangelogCategory, Sequence[str]] | None,
) -> dict[ChangelogCategory, list[str]]:
    """Drop generated entries that restate ones already in the Unreleased section.

    Catches verbatim repeats and near-duplicates (most of the entry's content
    words already appear in a single existing entry, e.g. a reworded or
    recategorized copy of a hand-written line).
    """
    existing_words = [
        _entry_words(entry)
        for entry_map in existing
        if entry_map is not None
        for values in entry_map.values()
        for entry in values
    ]

    def is_duplicate(entry: str) -> bool:
        words = _entry_words(entry)
        if not words:
            return True
        return any(len(words & seen) / len(words) >= 0.7 for seen in existing_words)

    deduped: dict[ChangelogCategory, list[str]] = {}
    for category, values in entries.items():
        fresh = [value for value in values if not is_duplicate(value)]
        if fresh:
            deduped[category] = fresh
    return deduped


_ENTRY_STOPWORDS = frozenset(
    "the and for was were now not that this with from are has had have been its when then than into also".split()
)


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
    "ChangelogRevision",
    "PreparedChangelogFlow",
    "UnreleasedSection",
    "apply_changelog_updates",
    "apply_revisions",
    "detect_boundaries",
    "generate_changelog_entries",
    "generate_changelog_updates",
    "normalize_changelog_entry",
    "parse_changelog_revisions",
    "parse_unreleased_section",
    "prepare_changelog_flow",
    "run_changelog_flow",
    "stage_changelog_blob",
    "write_entries",
]

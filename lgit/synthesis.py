"""Single-pass conventional analysis and boundary-specific changelog generation."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from pathlib import Path, PurePosixPath
from typing import TYPE_CHECKING, Any

from . import api, templates
from .diffing import condense_stat, parse_diff, scrub_diff_for_prompt, smart_truncate_diff
from .map_reduce import observe_diff_files, render_observations_markdown, should_use_map_reduce
from .markdown_output import analysis_from_mapping
from .models import (
    ChangelogCategory,
    ChangelogContext,
    ChangelogRevision,
    ChangelogUpdate,
    CommitEvidence,
    resolve_model_name,
)
from .templates import SynthesisParams
from .tokens import create_token_counter

if TYPE_CHECKING:
    from .config import CommitConfig


def can_synthesize(config: CommitConfig, *, fast: bool = False) -> bool:
    """Return whether fusion preserves the active legacy prompt-override policy.

    A user-supplied ``synthesis.md`` opts into fusion. Without it, any override
    for a prompt family that fusion replaces keeps the existing separate calls.
    """

    configured_dir = Path(config.prompts_dir).expanduser() if config.prompts_dir else None
    if templates.has_prompt_override("synthesis", configured_dir):
        return True
    legacy_families = ("fast", "changelog") if fast else ("analysis", "map", "reduce", "changelog")
    return not any(templates.has_prompt_override(family, configured_dir) for family in legacy_families)


async def generate_commit_evidence(
    config: CommitConfig,
    stat: str,
    diff: str,
    boundaries: Sequence[ChangelogContext],
    *,
    model_name: str | None = None,
    fast: bool = False,
    scope_candidates: str = "",
    user_context: str | None = None,
    recent_commits: str | None = None,
    common_scopes: str | None = None,
    project_context: str | None = None,
    debug_output: str | Path | None = None,
    debug_prefix: str | None = None,
) -> CommitEvidence:
    """Generate commit analysis and changelog updates from one shared evidence set.

    Small inputs are scrubbed and sent directly. Standard-mode inputs that meet
    the existing map-reduce threshold reuse the configured map phase, then send
    its complete per-file observations to the synthesis call without a separate
    reduce call. This function only generates data; it never mutates a repository.
    """

    ordered_boundaries = tuple(boundaries)
    _validate_boundary_inputs(ordered_boundaries)

    prompt_diff = scrub_diff_for_prompt(diff)
    changed_files = tuple(file.filename for file in parse_diff(prompt_diff))
    observations = ""
    prompt_stat = stat
    counter = create_token_counter(config)
    if not fast and should_use_map_reduce(prompt_diff, config, counter):
        mapped = await observe_diff_files(
            prompt_diff,
            resolve_model_name(config.effective_map_model),
            config,
            counter,
        )
        observations = render_observations_markdown(mapped)
        prompt_diff = ""
        prompt_stat = condense_stat(stat)
    elif len(prompt_diff) > int(config.max_diff_length):
        prompt_diff = smart_truncate_diff(prompt_diff, int(config.max_diff_length), config, counter)

    prompts_dir = Path(config.prompts_dir).expanduser() if config.prompts_dir else None
    prompt = templates.render_synthesis_prompt(
        SynthesisParams(
            stat=prompt_stat,
            diff=prompt_diff,
            observations=observations,
            changed_files=changed_files,
            evidence_limits=_evidence_limits(changed_files),
            boundaries=ordered_boundaries,
            scope_candidates=scope_candidates,
            user_context=user_context,
            recent_commits=recent_commits,
            common_scopes=common_scopes,
            types_description=api.format_types_description(config),
            project_context=project_context,
            fast=fast,
        ),
        prompts_dir=prompts_dir,
    )
    response = await api.run_oneshot(
        config,
        api.OneShotSpec(
            operation="synthesis",
            model=resolve_model_name(model_name or config.analysis_model),
            prompt_family="synthesis",
            system_prompt=prompt.system,
            user_prompt=prompt.user,
            tool_name="create_commit_evidence",
            progress_label="synthesize commit and changelogs",
            debug=(api.OneShotDebug(debug_output, debug_prefix, "synthesis") if debug_output is not None else None),
            cacheable=True,
        ),
    )
    output = response.output if hasattr(response, "output") else response
    return _parse_commit_evidence(output, ordered_boundaries, config)


def _evidence_limits(paths: Sequence[str]) -> str:
    """Distinguish reference-document edits from the runtime behavior they describe."""
    if not paths:
        return ""
    for path in paths:
        parsed = PurePosixPath(path)
        parts = {part.casefold() for part in parsed.parts[:-1]}
        name = parsed.name.casefold()
        if parts & {"prompts", "templates"}:
            return ""
        if not (name.startswith("readme.") or name in {"readme", "agents.md", "contributing.md"}):
            return ""
    return (
        "Only reference/developer documentation files changed. The diff describes documentation edits, "
        "not the implementation of the runtime behavior mentioned inside those documents. "
        "No production source or runtime prompt/template assets changed in this evidence. "
        "Do not announce documented existing functionality as newly delivered functionality; "
        "product changelog entries will normally be empty."
    )


def _validate_boundary_inputs(boundaries: Sequence[ChangelogContext]) -> None:
    seen: set[str] = set()
    for boundary in boundaries:
        if not isinstance(boundary.path, str) or not boundary.path:
            raise ValueError("changelog boundary path must be a non-empty string")
        if boundary.path in seen:
            raise ValueError(f"duplicate changelog boundary path: {boundary.path}")
        seen.add(boundary.path)


def _parse_commit_evidence(
    output: Any,
    boundaries: Sequence[ChangelogContext],
    config: CommitConfig,
) -> CommitEvidence:
    if not isinstance(output, Mapping):
        raise ValueError("synthesis response must be a JSON object")

    commit = output.get("commit")
    if not isinstance(commit, Mapping):
        raise ValueError("synthesis response must contain a commit object")
    if not isinstance(commit.get("type"), str):
        raise ValueError("synthesis commit must contain a conventional type")
    if commit.get("summary") is not None and not isinstance(commit["summary"], str):
        raise ValueError("synthesis commit summary must be a string or null")
    raw_details = commit.get("details", [])
    if not isinstance(raw_details, list):
        raise ValueError("synthesis commit details must be a JSON array")
    if len(raw_details) > 6:
        raise ValueError("synthesis commit details must contain at most 6 items")
    for detail in raw_details:
        if isinstance(detail, str):
            if not detail.strip():
                raise ValueError("synthesis commit details must not be empty")
        elif not isinstance(detail, Mapping) or not isinstance(detail.get("text"), str) or not detail["text"].strip():
            raise ValueError("synthesis commit details must contain text")
    default_type = next(iter(config.types), "chore")
    analysis = analysis_from_mapping(commit, default_type=default_type)

    raw_updates = output.get("changelogs")
    if not isinstance(raw_updates, list):
        raise ValueError("synthesis response changelogs must be a JSON array")

    by_path = {boundary.path: boundary for boundary in boundaries}
    parsed: dict[str, ChangelogUpdate] = {}
    for raw_update in raw_updates:
        update = _parse_changelog_update(raw_update, by_path)
        if update.path in parsed:
            raise ValueError(f"duplicate changelog update path: {update.path}")
        parsed[update.path] = update

    missing = [boundary.path for boundary in boundaries if boundary.path not in parsed]
    if missing:
        raise ValueError(f"missing changelog update paths: {', '.join(missing)}")
    return CommitEvidence(analysis, tuple(parsed[boundary.path] for boundary in boundaries))


def _parse_changelog_update(
    value: Any,
    boundaries: Mapping[str, ChangelogContext],
) -> ChangelogUpdate:
    if not isinstance(value, Mapping):
        raise ValueError("each changelog update must be a JSON object")
    path = value.get("path")
    if not isinstance(path, str) or path not in boundaries:
        raise ValueError(f"unknown changelog update path: {path!r}")
    boundary = boundaries[path]
    entries = _parse_entries(value.get("entries"))
    revisions = _parse_revisions(value.get("revisions"), boundary)
    return ChangelogUpdate(path, entries, revisions)


def _parse_entries(value: Any) -> dict[ChangelogCategory, list[str]]:
    if not isinstance(value, Mapping):
        raise ValueError("changelog entries must be a JSON object")
    entries: dict[ChangelogCategory, list[str]] = {}
    for raw_category, raw_values in value.items():
        if not isinstance(raw_category, str):
            raise ValueError("changelog category names must be strings")
        category = _strict_category(raw_category)
        if category in entries:
            raise ValueError(f"duplicate changelog category: {raw_category}")
        if not isinstance(raw_values, list):
            raise ValueError(f"changelog category {raw_category!r} must contain a JSON array")
        normalized = [_normalize_entry(item) for item in raw_values]
        if normalized:
            entries[category] = normalized
    return entries


def _parse_revisions(value: Any, boundary: ChangelogContext) -> list[ChangelogRevision]:
    if not isinstance(value, list):
        raise ValueError("changelog revisions must be a JSON array")
    if value and not boundary.can_revise:
        raise ValueError(f"changelog revisions are disabled for {boundary.path}")

    existing_lines = set((boundary.existing_entries or "").splitlines())
    revisions: list[ChangelogRevision] = []
    targets: set[str] = set()
    for raw_revision in value:
        if not isinstance(raw_revision, Mapping):
            raise ValueError("each changelog revision must be a JSON object")
        if "old" not in raw_revision or "new" not in raw_revision:
            raise ValueError("each changelog revision must contain old and new")
        old = raw_revision["old"]
        new = raw_revision["new"]
        if not isinstance(old, str) or old not in existing_lines or not old.startswith("- "):
            raise ValueError(f"revision target is not an exact existing entry in {boundary.path}: {old!r}")
        if old in targets:
            raise ValueError(f"duplicate revision target in {boundary.path}: {old}")
        if new is not None and not isinstance(new, str):
            raise ValueError("revision replacement must be a string or null")
        targets.add(old)
        revisions.append(ChangelogRevision(old, None if new is None else _normalize_entry(new)))
    return revisions


def _strict_category(value: str) -> ChangelogCategory:
    normalized = value.strip().casefold()
    for category in ChangelogCategory:
        if normalized in {category.name.casefold(), category.value.casefold()}:
            return category
    if normalized == "breaking":
        return ChangelogCategory.BREAKING
    raise ValueError(f"unknown changelog category: {value!r}")


def _normalize_entry(value: Any) -> str:
    if not isinstance(value, str):
        raise ValueError("changelog entries must be strings")
    stripped = value.strip()
    if stripped.startswith(("- ", "* ")):
        stripped = stripped[2:].strip()
    if not stripped:
        raise ValueError("changelog entries must not be empty")
    if "\n" in stripped or "\r" in stripped:
        raise ValueError("changelog entries must be single lines")
    return f"- {stripped}"


__all__ = ["can_synthesize", "generate_commit_evidence"]

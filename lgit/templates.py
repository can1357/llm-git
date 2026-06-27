"""Prompt template loading and rendering helpers."""

from __future__ import annotations

import os
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import Any

from jinja2 import Environment

from .errors import ConfigError

USER_SEPARATOR_MARKER = "<!-- USER -->"
PROMPT_CATEGORIES = (
    "analysis",
    "summary",
    "changelog",
    "map",
    "reduce",
    "fast",
    "compose-intent",
    "compose-bind",
)

_ENV = Environment(autoescape=False, keep_trailing_newline=True)


@dataclass(frozen=True, slots=True)
class PromptParts:
    """Rendered prompt split into static system text and rendered user text."""

    system: str
    user: str


@dataclass(frozen=True, slots=True)
class AnalysisParams:
    """Parameters for rendering the analysis prompt."""

    stat: str = ""
    diff: str = ""
    scope_candidates: str = ""
    recent_commits: str | None = None
    common_scopes: str | None = None
    types_description: str | None = None
    project_context: str | None = None


@dataclass(frozen=True, slots=True)
class MapFile:
    """One file diff passed to the map prompt."""

    path: str
    diff: str


@dataclass(frozen=True, slots=True)
class ComposeIntentPromptParams:
    """Parameters for rendering the compose-intent prompt."""

    max_commits: int = 1
    stat: str = ""
    snapshot_summary: str = ""
    planning_targets: str = ""
    planning_notes: str = ""
    split_bias: str = ""
    types_description: str | None = None


@dataclass(frozen=True, slots=True)
class ComposeBindPromptParams:
    """Parameters for rendering the compose-bind prompt."""

    groups: str = ""
    ambiguous_files: str = ""


@dataclass(frozen=True, slots=True)
class FastPromptParams:
    """Parameters for rendering the fast-mode prompt."""

    stat: str = ""
    diff: str = ""
    scope_candidates: str = ""
    user_context: str | None = None
    types_description: str | None = None


def get_user_prompts_dir() -> Path | None:
    """Return the user prompt override directory, if a home directory is known."""
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE")
    if not home:
        return None
    return Path(home).joinpath(".llm-git", "prompts")


def ensure_prompts_dir() -> Path | None:
    """Create the user prompt directory and unpack package prompt files."""
    user_prompts_dir = get_user_prompts_dir()
    if user_prompts_dir is None:
        return None
    try:
        user_prompts_dir.mkdir(parents=True, exist_ok=True)
    except OSError as exc:
        raise ConfigError(f"Failed to create prompts directory {user_prompts_dir}: {exc}") from exc

    for relative_path, content in _iter_package_prompt_files():
        destination = user_prompts_dir.joinpath(relative_path)
        try:
            destination.parent.mkdir(parents=True, exist_ok=True)
            if not destination.exists():
                destination.write_text(content, encoding="utf-8")
        except OSError as exc:
            raise ConfigError(f"Failed to write prompt file {destination}: {exc}") from exc
    return user_prompts_dir


def split_prompt_template(template_content: str) -> tuple[str | None, str]:
    """Split a prompt into optional system text and templated user text."""
    separator = _find_user_separator(template_content)
    if separator is None:
        return None, template_content
    system_end, user_start = separator
    return template_content[:system_end], template_content[user_start:]


def render_prompt_parts(
    template_name: str,
    template_content: str,
    context: Mapping[str, Any],
) -> PromptParts:
    """Render one prompt while enforcing a static system section."""
    system_template, user_template = split_prompt_template(template_content)
    system = ""
    if system_template is not None:
        _ensure_static_system_prompt(system_template, template_name)
        system = system_template.strip()
    try:
        rendered_user = _ENV.from_string(user_template).render(**context)
    except Exception as exc:  # jinja2 has several template-specific subclasses.
        raise ConfigError(f"Failed to render {template_name} prompt template: {exc}") from exc
    return PromptParts(system=system, user=rendered_user.strip())


def render_analysis_prompt(params: AnalysisParams | None = None, **kwargs: Any) -> PromptParts:
    """Render the analysis prompt."""
    p = params if params is not None else AnalysisParams(**kwargs)
    template_content = load_template_file("analysis")
    context = {
        "stat": p.stat,
        "diff": p.diff,
        "scope_candidates": p.scope_candidates,
        "recent_commits": p.recent_commits,
        "common_scopes": p.common_scopes,
        "types_description": p.types_description,
        "project_context": p.project_context,
    }
    return render_prompt_parts("analysis.md", template_content, context)


def render_summary_prompt(
    commit_type: str,
    scope: str,
    chars: str,
    details: str,
    stat: str,
    user_context: str | None = None,
) -> PromptParts:
    """Render the summary prompt."""
    template_content = load_template_file("summary")
    context = {
        "commit_type": commit_type,
        "scope": scope,
        "chars": chars,
        "details": details,
        "stat": stat,
        "user_context": user_context,
    }
    return render_prompt_parts("summary.md", template_content, context)


def render_changelog_prompt(
    changelog_path: str,
    is_package_changelog: bool,
    stat: str,
    diff: str,
    existing_entries: str | None = None,
) -> PromptParts:
    """Render the changelog prompt."""
    template_content = load_template_file("changelog")
    context = {
        "changelog_path": changelog_path,
        "is_package_changelog": is_package_changelog,
        "stat": stat,
        "diff": diff,
        "existing_entries": existing_entries,
    }
    return render_prompt_parts("changelog.md", template_content, context)


def render_map_prompt(
    files: Iterable[MapFile | Mapping[str, str]],
    context_header: str = "",
) -> PromptParts:
    """Render the map prompt for batched file-observation extraction."""
    template_content = load_template_file("map")
    context = {
        "files": [_mapping_for_file(file) for file in files],
        "context_header": context_header,
    }
    return render_prompt_parts("map.md", template_content, context)


def render_reduce_prompt(
    observations: str,
    stat: str,
    scope_candidates: str,
    types_description: str | None = None,
) -> PromptParts:
    """Render the reduce prompt for synthesizing map observations."""
    template_content = load_template_file("reduce")
    context = {
        "observations": observations,
        "stat": stat,
        "scope_candidates": scope_candidates,
        "types_description": types_description,
    }
    return render_prompt_parts("reduce.md", template_content, context)


def render_fast_prompt(params: FastPromptParams | None = None, **kwargs: Any) -> PromptParts:
    """Render the fast-mode single-call prompt."""
    p = params if params is not None else FastPromptParams(**kwargs)
    template_content = load_template_file("fast")
    context = {
        "stat": p.stat,
        "diff": p.diff,
        "scope_candidates": p.scope_candidates,
        "user_context": p.user_context,
        "types_description": p.types_description,
    }
    return render_prompt_parts("fast.md", template_content, context)


def render_compose_intent_prompt(
    params: ComposeIntentPromptParams | None = None,
    **kwargs: Any,
) -> PromptParts:
    """Render the compose-intent planning prompt."""
    p = params if params is not None else ComposeIntentPromptParams(**kwargs)
    template_content = load_template_file("compose-intent")
    context = {
        "max_commits": p.max_commits,
        "stat": p.stat,
        "snapshot_summary": p.snapshot_summary,
        "planning_targets": p.planning_targets,
        "planning_notes": p.planning_notes,
        "split_bias": p.split_bias,
        "types_description": p.types_description,
    }
    return render_prompt_parts("compose-intent.md", template_content, context)


def render_compose_bind_prompt(
    params: ComposeBindPromptParams | None = None,
    **kwargs: Any,
) -> PromptParts:
    """Render the compose-bind hunk-assignment prompt."""
    p = params if params is not None else ComposeBindPromptParams(**kwargs)
    template_content = load_template_file("compose-bind")
    context = {"groups": p.groups, "ambiguous_files": p.ambiguous_files}
    return render_prompt_parts("compose-bind.md", template_content, context)


def load_template_file(category: str) -> str:
    """Load a user override prompt first, then the packaged prompt resource."""
    if category not in PROMPT_CATEGORIES:
        raise ConfigError(f"Unknown prompt category {category!r}")
    if prompts_dir := get_user_prompts_dir():
        user_template = prompts_dir.joinpath(f"{category}.md")
        if user_template.exists():
            try:
                return user_template.read_text(encoding="utf-8")
            except OSError as exc:
                raise ConfigError(f"Failed to read template file {user_template}: {exc}") from exc

    resource = resources.files("lgit.resources").joinpath("prompts", f"{category}.md")
    if resource.is_file():
        try:
            return resource.read_text(encoding="utf-8")
        except OSError as exc:
            raise ConfigError(f"Failed to read package template {category}.md: {exc}") from exc
    raise ConfigError(f"Template {category!r} not found as user override or package resource")


def _find_user_separator(content: str) -> tuple[int, int] | None:
    marker_pos = content.find(USER_SEPARATOR_MARKER)
    if marker_pos == -1:
        return None
    if marker_pos >= 2 and content[marker_pos - 2 : marker_pos] == "\r\n":
        system_end = marker_pos - 2
    elif marker_pos >= 1 and content[marker_pos - 1 : marker_pos] == "\n":
        system_end = marker_pos - 1
    else:
        system_end = marker_pos
    after_marker = marker_pos + len(USER_SEPARATOR_MARKER)
    if content[after_marker : after_marker + 2] == "\r\n":
        user_start = after_marker + 2
    elif content[after_marker : after_marker + 1] == "\n":
        user_start = after_marker + 1
    else:
        user_start = after_marker
    return system_end, user_start


def _ensure_static_system_prompt(system_template: str, template_name: str) -> None:
    if "{{" in system_template or "{%" in system_template or "{#" in system_template:
        raise ConfigError(
            f"Template {template_name!r} contains dynamic tags in system section. "
            f"Move interpolated content below {USER_SEPARATOR_MARKER}."
        )


def _mapping_for_file(file: MapFile | Mapping[str, str]) -> Mapping[str, str]:
    if isinstance(file, MapFile):
        return {"path": file.path, "diff": file.diff}
    return file


def _iter_package_prompt_files() -> Iterable[tuple[Path, str]]:
    prompts_root = resources.files("lgit.resources").joinpath("prompts")
    yield from _walk_prompt_resources(prompts_root, Path())


def _walk_prompt_resources(root: Any, prefix: Path) -> Iterable[tuple[Path, str]]:
    for child in sorted(root.iterdir(), key=lambda item: item.name):
        child_prefix = prefix / child.name
        if child.is_dir():
            yield from _walk_prompt_resources(child, child_prefix)
        elif child.name.endswith(".md"):
            yield child_prefix, child.read_text(encoding="utf-8")


__all__ = [
    "USER_SEPARATOR_MARKER",
    "PROMPT_CATEGORIES",
    "PromptParts",
    "AnalysisParams",
    "MapFile",
    "ComposeIntentPromptParams",
    "ComposeBindPromptParams",
    "FastPromptParams",
    "get_user_prompts_dir",
    "ensure_prompts_dir",
    "split_prompt_template",
    "render_prompt_parts",
    "load_template_file",
    "render_analysis_prompt",
    "render_summary_prompt",
    "render_changelog_prompt",
    "render_map_prompt",
    "render_reduce_prompt",
    "render_fast_prompt",
    "render_compose_intent_prompt",
    "render_compose_bind_prompt",
]

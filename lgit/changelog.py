"""Changelog maintenance for staged commits."""

from __future__ import annotations

import json
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import TYPE_CHECKING, Any

from .diffing import smart_truncate_diff
from .errors import GitError, ValidationFailure
from .git import run_git
from .markdown_output import parse_changelog_response
from .models import ChangelogCategory, resolve_model_name

if TYPE_CHECKING:
    from .config import CommitConfig


@dataclass(frozen=True, slots=True)
class UnreleasedSection:
    """Parsed bounds and entries for a changelog's Unreleased section."""

    path: Path
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


async def run_changelog_flow(args: Any, config: CommitConfig) -> list[ChangelogBoundary]:
    """Generate and stage changelog entries for currently staged files."""

    repo_dir = Path(getattr(args, "dir", "."))
    staged_files = _staged_files(repo_dir)
    candidate_files = [path for path in staged_files if not path.lower().endswith("changelog.md")]
    if not candidate_files:
        return []

    changelogs = _find_changelogs(repo_dir)
    if not changelogs:
        return []

    boundaries = detect_boundaries(candidate_files, changelogs, repo_dir)
    updated: list[ChangelogBoundary] = []
    untracked_to_stage: list[str] = []

    max_diff_length = config.max_diff_length
    for boundary in boundaries:
        diff = _diff_for_files(boundary.files, repo_dir, max_diff_length)
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

        entries = await generate_changelog_entries(
            boundary.changelog_path,
            _is_package_changelog(boundary.changelog_path, repo_dir),
            stat,
            diff,
            _format_existing_entries(unreleased),
            config,
        )
        if not entries:
            continue

        updated_staged = write_entries(changelog_content, unreleased, entries)
        updated_worktree = (
            write_entries(worktree_content, worktree_unreleased, entries)
            if worktree_unreleased is not None
            else updated_staged
        )
        boundary.changelog_path.write_text(updated_worktree, encoding="utf-8")

        if is_tracked:
            stage_changelog_blob(rel_path, updated_staged, repo_dir)
        else:
            untracked_to_stage.append(rel_path)
        updated.append(ChangelogBoundary(boundary.changelog_path, boundary.files, diff, stat))

    if untracked_to_stage:
        run_git(["add", "--", *untracked_to_stage], cwd=repo_dir)
    return updated


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
    """Insert new entries at the top of the Unreleased section."""

    normalized_new = _coerce_entries(new_entries)
    if not normalized_new:
        return content

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
) -> str:
    """Stage exact changelog content as an index blob without git-adding the worktree copy."""

    path_text = os.fspath(rel_path)
    oid = run_git(["hash-object", "-w", "--stdin"], cwd=dir, input_text=content).stdout.strip()
    if not oid:
        raise GitError(f"git hash-object returned no oid for {path_text}")
    mode = _staged_changelog_mode(path_text, dir)
    run_git(["update-index", "--add", "--cacheinfo", f"{mode},{oid},{path_text}"], cwd=dir)
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
    config: CommitConfig,
) -> dict[ChangelogCategory, list[str]]:
    """Ask the configured model for Keep a Changelog entries."""

    system_prompt, user_prompt = _render_prompt(
        "changelog",
        {
            "changelog_path": os.fspath(changelog_path),
            "is_package_changelog": is_package_changelog,
            "stat": stat,
            "diff": diff,
            "existing_entries": existing_entries or "",
        },
    )
    from .api import OneShotSpec, run_oneshot

    response = await run_oneshot(
        config,
        OneShotSpec(
            operation="changelog",
            model=resolve_model_name(config.analysis_model),
            prompt_family="changelog",
            system_prompt=system_prompt,
            user_prompt=user_prompt,
            tool_name="create_changelog_entries",
            progress_label="changelog",
            cacheable=True,
        ),
    )
    output = response.output if hasattr(response, "output") else response
    payload = _parse_jsonish(output)
    entries = payload.get("entries", payload) if isinstance(payload, dict) else {}
    return _coerce_entries(entries if isinstance(entries, Mapping) else {})


def normalize_changelog_entry(entry: str) -> str | None:
    """Normalize one model-emitted changelog line to a markdown bullet."""

    stripped = entry.strip()
    if stripped.startswith(("- ", "* ")):
        stripped = stripped[2:].strip()
    return f"- {stripped}" if stripped else None


def _staged_files(dir: Path) -> list[str]:
    stdout = run_git(["diff", "--cached", "--name-only"], cwd=dir).stdout
    return [line for line in stdout.splitlines() if line]


def _find_changelogs(dir: Path) -> list[Path]:
    paths = {
        dir / path
        for path in run_git(["ls-files", "--", "CHANGELOG.md", "**/CHANGELOG.md"], cwd=dir).stdout.splitlines()
    }
    for path in dir.rglob("CHANGELOG.md"):
        if ".git" not in path.parts:
            paths.add(path)
    return sorted(paths, key=lambda path: _relative_to(path, dir))


def _staged_changelog_content(rel_path: str, dir: str | os.PathLike[str]) -> str | None:
    result = run_git(["show", f":{rel_path}"], cwd=dir, check=False, allow_exit_codes=(128,))
    return result.stdout if result.returncode == 0 else None


def _staged_changelog_mode(rel_path: str, dir: str | os.PathLike[str]) -> str:
    result = run_git(["ls-files", "-s", "--", rel_path], cwd=dir).stdout.strip()
    return result.split(maxsplit=1)[0] if result else "100644"


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


def _format_existing_entries(unreleased: UnreleasedSection) -> str | None:
    lines: list[str] = []
    for category in ChangelogCategory.render_order():
        entries = unreleased.entries.get(category, [])
        if not entries:
            continue
        lines.append(f"### {category.value}")
        lines.extend(entries)
        lines.append("")
    return "\n".join(lines).strip() or None


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


def _render_prompt(family: str, values: Mapping[str, Any]) -> tuple[str, str]:
    text = (resources.files("lgit.resources") / "prompts" / f"{family}.md").read_text(encoding="utf-8")
    try:
        from jinja2 import Template

        text = Template(text).render(**values)
    except Exception:
        for key, value in values.items():
            text = text.replace("{{ " + key + " }}", str(value))
            text = text.replace("{{" + key + "}}", str(value))
    marker = "<!-- USER -->"
    if marker in text:
        system, user = text.split(marker, 1)
    else:
        system, user = text, ""
    return system.strip(), user.strip()


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
    "UnreleasedSection",
    "detect_boundaries",
    "generate_changelog_entries",
    "normalize_changelog_entry",
    "parse_unreleased_section",
    "run_changelog_flow",
    "stage_changelog_blob",
    "write_entries",
]

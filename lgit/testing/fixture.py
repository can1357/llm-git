"""Fixture manifests, on-disk fixture loading, and golden-file I/O."""

from __future__ import annotations

import json
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any, Self

from lgit.models import AnalysisDetail, ChangelogCategory, ConventionalAnalysis

FIXTURES_DIR = "tests/fixtures"


@dataclass(slots=True)
class FixtureEntry:
    """Manifest entry for one fixture."""

    description: str
    tags: list[str] = field(default_factory=list)

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Self:
        return cls(description=str(data.get("description", "")), tags=_string_list(data.get("tags", [])))


@dataclass(slots=True)
class Manifest:
    """Fixture manifest loaded from ``manifest.toml``."""

    fixtures: dict[str, FixtureEntry] = field(default_factory=dict)

    @classmethod
    def load(cls, fixtures_dir: str | Path) -> Self:
        path = Path(fixtures_dir) / "manifest.toml"
        if not path.exists():
            return cls()
        with path.open("rb") as handle:
            data = tomllib.load(handle)
        raw = data.get("fixtures", {})
        if not isinstance(raw, Mapping):
            raise ValueError(f"invalid fixture manifest: {path}")
        return cls(
            {
                str(name): FixtureEntry.from_mapping(entry if isinstance(entry, Mapping) else {})
                for name, entry in raw.items()
            }
        )

    def save(self, fixtures_dir: str | Path) -> None:
        path = Path(fixtures_dir) / "manifest.toml"
        path.parent.mkdir(parents=True, exist_ok=True)
        lines: list[str] = []
        for name in sorted(self.fixtures):
            entry = self.fixtures[name]
            lines.append(f"[fixtures.{name}]")
            lines.append(f"description = {_toml_value(entry.description)}")
            lines.append(f"tags = {_toml_value(entry.tags)}")
            lines.append("")
        path.write_text("\n".join(lines), encoding="utf-8")

    def add(self, name: str, entry: FixtureEntry) -> None:
        self.fixtures[name] = entry


@dataclass(slots=True)
class FixtureMeta:
    """Metadata captured with a fixture."""

    source_repo: str
    source_commit: str
    description: str
    captured_at: str
    tags: list[str] = field(default_factory=list)

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Self:
        return cls(
            source_repo=str(data.get("source_repo", "")),
            source_commit=str(data.get("source_commit", "")),
            description=str(data.get("description", "")),
            captured_at=str(data.get("captured_at", "")),
            tags=_string_list(data.get("tags", [])),
        )


@dataclass(slots=True)
class FixtureContext:
    """Analysis context captured for deterministic fixture runs."""

    recent_commits: str | None = None
    common_scopes: str | None = None
    project_context: str | None = None
    user_context: str | None = None

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Self:
        return cls(
            recent_commits=_optional_str(data.get("recent_commits")),
            common_scopes=_optional_str(data.get("common_scopes")),
            project_context=_optional_str(data.get("project_context")),
            user_context=_optional_str(data.get("user_context")),
        )

    def to_mapping(self) -> dict[str, str]:
        return {
            key: value
            for key, value in {
                "recent_commits": self.recent_commits,
                "common_scopes": self.common_scopes,
                "project_context": self.project_context,
                "user_context": self.user_context,
            }.items()
            if value is not None
        }


@dataclass(slots=True)
class FixtureInput:
    """Frozen inputs used by the analysis harness."""

    diff: str
    stat: str
    scope_candidates: str = ""
    context: FixtureContext = field(default_factory=FixtureContext)


@dataclass(slots=True)
class Golden:
    """Expected fixture output."""

    analysis: ConventionalAnalysis
    final_message: str = ""


@dataclass(slots=True)
class Fixture:
    """A complete fixture loaded from or saved to disk."""

    name: str
    meta: FixtureMeta
    input: FixtureInput
    golden: Golden | None = None

    @classmethod
    def load(cls, fixtures_dir: str | Path, name: str) -> Self:
        fixture_dir = Path(fixtures_dir) / name
        if not fixture_dir.exists():
            raise FileNotFoundError(f"fixture {name!r} not found at {fixture_dir}")

        meta_path = fixture_dir / "meta.toml"
        if not meta_path.exists():
            raise FileNotFoundError(f"fixture {name!r} missing meta.toml")
        meta = FixtureMeta.from_mapping(_read_toml(meta_path))

        input_dir = fixture_dir / "input"
        diff = _read_text(input_dir / "diff.patch")
        stat = _read_text(input_dir / "stat.txt")
        scope_candidates = _read_text(input_dir / "scope_candidates.txt", missing_ok=True)
        context_path = input_dir / "context.toml"
        context = FixtureContext.from_mapping(_read_toml(context_path)) if context_path.exists() else FixtureContext()

        golden = None
        golden_dir = fixture_dir / "golden"
        analysis_path = golden_dir / "analysis.json"
        if analysis_path.exists():
            analysis = analysis_from_json(analysis_path.read_text(encoding="utf-8"))
            final_message = _read_text(golden_dir / "final.txt", missing_ok=True)
            golden = Golden(analysis=analysis, final_message=final_message)

        return cls(name=name, meta=meta, input=FixtureInput(diff, stat, scope_candidates, context), golden=golden)

    def save(self, fixtures_dir: str | Path) -> None:
        fixture_dir = Path(fixtures_dir) / self.name
        input_dir = fixture_dir / "input"
        golden_dir = fixture_dir / "golden"
        input_dir.mkdir(parents=True, exist_ok=True)

        _write_toml(
            fixture_dir / "meta.toml",
            {
                "source_repo": self.meta.source_repo,
                "source_commit": self.meta.source_commit,
                "description": self.meta.description,
                "captured_at": self.meta.captured_at,
                "tags": self.meta.tags,
            },
        )
        (input_dir / "diff.patch").write_text(self.input.diff, encoding="utf-8")
        (input_dir / "stat.txt").write_text(self.input.stat, encoding="utf-8")
        (input_dir / "scope_candidates.txt").write_text(self.input.scope_candidates, encoding="utf-8")
        _write_toml(input_dir / "context.toml", self.input.context.to_mapping())

        if self.golden is not None:
            golden_dir.mkdir(parents=True, exist_ok=True)
            (golden_dir / "analysis.json").write_text(analysis_to_json(self.golden.analysis), encoding="utf-8")
            (golden_dir / "final.txt").write_text(self.golden.final_message, encoding="utf-8")

    def update_golden(self, analysis: ConventionalAnalysis, final_message: str) -> None:
        self.golden = Golden(analysis=analysis, final_message=final_message)


async def add_fixture(
    fixtures_dir: str | Path, commit_hash: str, name: str, repo_dir: str | Path = ".", config: Any | None = None
) -> Fixture:
    """Create a fixture from a commit and add it to the manifest."""

    from lgit.analysis import extract_scope_candidates
    from lgit.git import extract_style_patterns, get_common_scopes, get_git_diff, get_git_stat, get_recent_commits
    from lgit.repo import RepoMetadata

    diff = get_git_diff("commit", commit_hash, repo_dir, config)
    stat = get_git_stat("commit", commit_hash, repo_dir, config)
    scope_candidates, _ = extract_scope_candidates("commit", commit_hash, str(repo_dir), config)

    recent_commits = None
    common_scopes = None
    try:
        commits = get_recent_commits(repo_dir, 20)
        patterns = extract_style_patterns(commits)
        recent_commits = None if patterns is None else patterns.format_for_prompt()
    except Exception:
        recent_commits = None
    try:
        scopes = get_common_scopes(repo_dir, 100)
        common_scopes = ", ".join(f"{scope} ({count})" for scope, count in scopes[:10]) or None
    except Exception:
        common_scopes = None
    project_context = RepoMetadata.detect(repo_dir).format_for_prompt()

    fixture = Fixture(
        name=name,
        meta=FixtureMeta(
            source_repo=str(repo_dir),
            source_commit=commit_hash,
            description=f"Fixture from commit {commit_hash}",
            captured_at=datetime.now(UTC).isoformat(),
            tags=[],
        ),
        input=FixtureInput(
            diff=diff,
            stat=stat,
            scope_candidates=scope_candidates,
            context=FixtureContext(
                recent_commits=recent_commits, common_scopes=common_scopes, project_context=project_context
            ),
        ),
    )
    root = Path(fixtures_dir)
    root.mkdir(parents=True, exist_ok=True)
    fixture.save(root)
    manifest = Manifest.load(root)
    manifest.add(name, FixtureEntry(description=f"From commit {commit_hash}", tags=[]))
    manifest.save(root)
    return fixture


def discover_fixtures(fixtures_dir: str | Path) -> list[str]:
    """Return sorted fixture directory names under ``fixtures_dir``."""

    root = Path(fixtures_dir)
    if not root.exists():
        return []
    return sorted(path.name for path in root.iterdir() if path.is_dir() and (path / "meta.toml").exists())


def load_fixtures(fixtures_dir: str | Path, names: list[str] | None = None) -> list[Fixture]:
    selected = discover_fixtures(fixtures_dir) if names is None else names
    return [Fixture.load(fixtures_dir, name) for name in selected]


def analysis_from_json(content: str) -> ConventionalAnalysis:
    data = json.loads(content)
    if not isinstance(data, Mapping):
        raise ValueError("analysis JSON must be an object")
    details = tuple(_detail_from_json(item) for item in (data.get("details") or ()))
    issue_refs = tuple(str(item) for item in (data.get("issue_refs") or ()))
    return ConventionalAnalysis(
        commit_type=str(data.get("type", data.get("commit_type", "chore"))),
        scope=_optional_str(data.get("scope")),
        summary=_optional_str(data.get("summary")),
        details=details,
        issue_refs=issue_refs,
    )


def analysis_to_json(analysis: ConventionalAnalysis) -> str:
    return json.dumps(analysis_to_mapping(analysis), indent=2, ensure_ascii=False) + "\n"


def analysis_to_mapping(analysis: ConventionalAnalysis) -> dict[str, Any]:
    data: dict[str, Any] = {"type": str(analysis.commit_type)}
    if analysis.scope is not None:
        data["scope"] = str(analysis.scope)
    if analysis.summary:
        data["summary"] = analysis.summary
    data["details"] = [_detail_to_json(detail) for detail in analysis.details]
    data["issue_refs"] = list(analysis.issue_refs)
    return data


def _detail_from_json(item: Any) -> AnalysisDetail:
    if isinstance(item, Mapping):
        raw_category = item.get("changelog_category")
        category = ChangelogCategory.from_name(str(raw_category)) if raw_category not in (None, "") else None
        return AnalysisDetail(
            text=str(item.get("text", "")),
            changelog_category=category,
            user_visible=bool(item.get("user_visible", False)),
        )
    return AnalysisDetail.simple(str(item))


def _detail_to_json(detail: AnalysisDetail) -> dict[str, Any]:
    data: dict[str, Any] = {"text": detail.text}
    if detail.changelog_category is not None:
        data["changelog_category"] = detail.changelog_category.value
    data["user_visible"] = bool(detail.user_visible)
    return data


def _read_toml(path: Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    return dict(data)


def _read_text(path: Path, *, missing_ok: bool = False) -> str:
    if missing_ok and not path.exists():
        return ""
    return path.read_text(encoding="utf-8")


def _write_toml(path: Path, data: Mapping[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    lines = [f"{key} = {_toml_value(value)}" for key, value in data.items() if value is not None]
    path.write_text("\n".join(lines) + ("\n" if lines else ""), encoding="utf-8")


def _toml_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int | float):
        return str(value)
    if isinstance(value, list | tuple):
        return "[" + ", ".join(_toml_value(item) for item in value) + "]"
    text = str(value)
    if "\n" in text:
        return '"""' + text.replace('"""', '\\"\\"\\"') + '"""'
    return json.dumps(text, ensure_ascii=False)


def _optional_str(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value)
    return text if text else None


def _string_list(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    return [str(item) for item in value]


__all__ = [
    "FIXTURES_DIR",
    "Fixture",
    "FixtureContext",
    "FixtureEntry",
    "FixtureInput",
    "FixtureMeta",
    "Golden",
    "Manifest",
    "add_fixture",
    "analysis_from_json",
    "analysis_to_json",
    "analysis_to_mapping",
    "discover_fixtures",
    "load_fixtures",
]

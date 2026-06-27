"""Scope candidate extraction from git numstat output."""

from __future__ import annotations

from collections import Counter
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Protocol

from .models import ScopeCandidate

PLACEHOLDER_DIRS = {
    "src",
    "lib",
    "bin",
    "crates",
    "benches",
    "examples",
    "internal",
    "pkg",
    "include",
    "tests",
    "test",
    "docs",
    "packages",
    "modules",
}

SKIP_DIRS = {".test", "tests", "benches", "examples", "target", "build", "node_modules", ".github"}


class _ScopeConfig(Protocol):
    """Config attributes read during scope analysis; satisfied by ``CommitConfig``."""

    excluded_files: list[str]
    wide_change_threshold: float
    wide_change_abstract: bool


@dataclass(slots=True)
class ScopeAnalyzer:
    """Accumulates changed-line totals per meaningful path component."""

    component_lines: Counter[str] = field(default_factory=Counter)
    total_lines: int = 0

    @classmethod
    def from_numstat(cls, numstat: str, config: _ScopeConfig | None = None) -> ScopeAnalyzer:
        """Build an analyzer from git diff --numstat output."""

        analyzer = cls()
        for line in numstat.splitlines():
            analyzer.process_numstat_line(line, config)
        return analyzer

    def process_numstat_line(self, line: str, config: _ScopeConfig | None = None) -> None:
        """Process one added/deleted/path numstat row."""

        parts = line.split("\t")
        if len(parts) < 3:
            return
        added = _parse_count(parts[0])
        deleted = _parse_count(parts[1])
        lines_changed = added + deleted
        if lines_changed == 0:
            return

        raw_path = "\t".join(parts[2:])
        path = extract_path_from_rename(raw_path)
        if _is_excluded(path, config):
            return

        self.total_lines += lines_changed
        for component in extract_components_from_path(path):
            if any("." in segment for segment in component.split("/")):
                continue
            self.component_lines[component] += lines_changed

    def build_scope_candidates(self) -> list[ScopeCandidate]:
        """Return sorted candidates with percentage and confidence scores."""

        if self.total_lines == 0:
            return []
        candidates: list[ScopeCandidate] = []
        for path, lines in self.component_lines.items():
            if "/" not in path and path in PLACEHOLDER_DIRS:
                continue
            percentage = lines / self.total_lines * 100.0
            is_two_segment = "/" in path
            if is_two_segment:
                confidence = percentage * 1.2 if percentage > 60.0 else percentage * 0.8
            else:
                confidence = percentage
            candidates.append(ScopeCandidate(path=path, percentage=percentage, confidence=confidence))
        candidates.sort(key=lambda item: item.confidence, reverse=True)
        return candidates

    @staticmethod
    def is_wide_change(candidates: Sequence[ScopeCandidate], config: _ScopeConfig | None = None) -> bool:
        """Return true when no scope dominates or many roots are touched."""

        threshold = float(getattr(config, "wide_change_threshold", 0.5))
        if candidates and candidates[0].percentage / 100.0 < threshold:
            return True
        roots = {candidate.path.split("/", 1)[0] for candidate in candidates}
        return len(roots) >= 3

    @staticmethod
    def extract_scope(numstat: str, config: _ScopeConfig | None = None) -> tuple[list[ScopeCandidate], int]:
        """Return candidates plus total changed lines from numstat."""

        analyzer = ScopeAnalyzer.from_numstat(numstat, config)
        return analyzer.build_scope_candidates(), analyzer.total_lines

    @staticmethod
    def count_changed_lines(numstat: str, config: _ScopeConfig | None = None) -> int:
        """Count changed non-binary, non-excluded lines in numstat."""

        return ScopeAnalyzer.from_numstat(numstat, config).total_lines

    @staticmethod
    def analyze_wide_change(numstat: str) -> str | None:
        """Detect an abstract category for cross-cutting changes."""

        raw_paths = (_path_from_numstat_line(line) for line in numstat.splitlines())
        paths = [path for path in raw_paths if path]
        if not paths:
            return None

        total = len(paths)
        md_count = 0
        test_count = 0
        config_count = 0
        has_cargo_toml = False
        has_package_json = False
        error_keywords = 0
        type_keywords = 0

        for path in paths:
            lower = path.lower()
            suffix = Path(path).suffix.lower()
            if suffix == ".md":
                md_count += 1
            if "/test" in path or "test_" in path or "_test." in path or ".test." in path:
                test_count += 1
            if suffix in {".toml", ".yaml", ".yml", ".json"}:
                config_count += 1
            if "Cargo.toml" in path:
                has_cargo_toml = True
            if "package.json" in path:
                has_package_json = True
            if any(keyword in lower for keyword in ("error", "exception", "fail")):
                error_keywords += 1
            if any(keyword in lower for keyword in ("type", "struct", "enum")):
                type_keywords += 1

        if has_cargo_toml or has_package_json:
            return "deps"
        if md_count * 100 / total > 70:
            return "docs"
        if test_count * 100 / total > 60:
            return "tests"
        if error_keywords * 100 / total > 40:
            return "error-handling"
        if type_keywords * 100 / total > 40:
            return "type-refactor"
        if config_count * 100 / total > 50:
            return "config"
        return None


def extract_scope_candidates(
    source: object,
    target: str | None = None,
    dir: str = ".",
    config: _ScopeConfig | None = None,
) -> tuple[str, bool]:
    """Extract a scope prompt string and wide-change flag.

    `source` may be raw numstat text or a mode value. When a mode is passed,
    this function imports `lgit.git.get_git_numstat` lazily to avoid a module
    cycle and fetches numstat for that mode.
    """

    if isinstance(source, str) and _looks_like_numstat(source):
        numstat = source
    else:
        from .git import get_git_numstat

        numstat = get_git_numstat(source, target, dir, config)

    candidates, total_lines = ScopeAnalyzer.extract_scope(numstat, config)
    if total_lines == 0:
        return "(none - no meaningful scopes)", False

    is_wide = ScopeAnalyzer.is_wide_change(candidates, config)
    if is_wide:
        if bool(getattr(config, "wide_change_abstract", False)):
            pattern = ScopeAnalyzer.analyze_wide_change(numstat)
            scope_str = f"(cross-cutting: {pattern})" if pattern else "(none - multi-component change)"
        else:
            scope_str = "(none - multi-component change)"
    else:
        parts: list[str] = []
        for candidate in candidates[:5]:
            if candidate.percentage < 10.0:
                continue
            if "/" in candidate.path and candidate.percentage > 60.0:
                confidence_label = "high confidence"
            else:
                confidence_label = "moderate confidence"
            parts.append(f"{candidate.path} ({candidate.percentage:.0f}%, {confidence_label})")
        if parts:
            scope_str = ", ".join(parts)
            if any("/" in candidate.path and candidate.percentage > 60.0 for candidate in candidates[:5]):
                scope_str += "\nPrefer 2-segment scopes marked 'high confidence'."
        else:
            scope_str = "(none - unclear component)"

    return scope_str, is_wide


def extract_path_from_rename(path_part: str) -> str:
    """Return the destination path from git numstat rename syntax."""

    path_part = path_part.strip()
    brace_start = path_part.find("{")
    if brace_start != -1:
        arrow_pos = path_part.find(" => ", brace_start)
        if arrow_pos != -1:
            brace_end = path_part.find("}", arrow_pos)
            if brace_end != -1:
                prefix = path_part[:brace_start]
                new_name = path_part[arrow_pos + 4 : brace_end].strip()
                suffix = path_part[brace_end + 1 :]
                return f"{prefix}{new_name}{suffix}".strip()

        return path_part
    if " => " in path_part:
        return path_part.split(" => ", 1)[1].strip()
    return path_part


def extract_components_from_path(path: str) -> list[str]:
    """Extract single- and two-segment meaningful components from a path."""

    segments = [segment for segment in path.replace("\\", "/").split("/") if segment]
    meaningful: list[str] = []

    for index, segment in enumerate(segments):
        if segment in PLACEHOLDER_DIRS:
            if len(segments) > index + 1:
                continue
            break
        if _is_file_segment(segment):
            continue
        if segment in SKIP_DIRS:
            continue
        stripped = _strip_extension(segment)
        if stripped and not stripped.startswith("."):
            meaningful.append(stripped)

    if not meaningful:
        return []
    components = [meaningful[0]]
    if len(meaningful) >= 2:
        components.append(f"{meaningful[0]}/{meaningful[1]}")
    return components


def _path_from_numstat_line(line: str) -> str | None:
    parts = line.split("\t")
    if len(parts) < 3:
        return None
    return extract_path_from_rename("\t".join(parts[2:]))


def _parse_count(raw: str) -> int:
    try:
        return int(raw)
    except ValueError:
        return 0


def _strip_extension(segment: str) -> str:
    if "." not in segment:
        return segment
    stem, _ = segment.rsplit(".", 1)
    return stem


def _is_file_segment(segment: str) -> bool:
    return "." in segment and not segment.startswith(".") and segment.rfind(".") > 0


def _is_excluded(path: str, config: _ScopeConfig | None) -> bool:
    excluded = getattr(config, "excluded_files", ())
    return any(path.endswith(str(pattern)) for pattern in excluded)


def _looks_like_numstat(text: str) -> bool:
    if "\n" in text or "\t" in text:
        first = next((line for line in text.splitlines() if line.strip()), "")
        parts = first.split("\t")
        return len(parts) >= 3 and (_is_numstat_count(parts[0]) or parts[0] == "-")
    return False


def _is_numstat_count(value: str) -> bool:
    try:
        int(value)
    except ValueError:
        return False
    return True

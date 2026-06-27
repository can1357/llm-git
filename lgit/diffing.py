"""Unified diff parsing, truncation, and whitespace classification."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol


class _TokenCounter(Protocol):
    def count_sync(self, text: str) -> int: ...


_DEFAULT_LOW_PRIORITY_EXTENSIONS = {
    "lock",
    "log",
    "md",
    "txt",
    "json",
    "yaml",
    "yml",
    "toml",
    "sum",
    "tmp",
    "bak",
}


@dataclass(slots=True)
class FileDiff:
    """A single file section from a unified git diff."""

    filename: str
    header: str
    content: str = ""
    additions: int = 0
    deletions: int = 0
    is_binary: bool = False

    @property
    def size(self) -> int:
        """Return the UTF-8 byte size used for budgeting."""

        return _byte_len(self.header) + _byte_len(self.content)

    def token_estimate(self, counter: _TokenCounter | None = None) -> int:
        """Estimate token count using a provided counter or a 4-char heuristic."""

        if counter is None:
            return max(1, (len(self.header) + len(self.content)) // 4)
        count = getattr(counter, "count_sync", None)
        if callable(count):
            return int(count(self.header)) + int(count(self.content))
        if callable(counter):
            return int(counter(self.header)) + int(counter(self.content))
        return max(1, (len(self.header) + len(self.content)) // 4)

    def priority(self, config: object | None = None) -> int:
        """Rank this file for context retention; higher values are kept first."""

        if self.is_binary:
            return -100

        filename_lower = self.filename.lower()
        if filename_lower.endswith(("cargo.toml", "package.json", "go.mod", "requirements.txt", "pyproject.toml")):
            return 70
        if "prompt" in filename_lower or "system" in filename_lower:
            return 100
        if (
            "/test" in self.filename
            or "test_" in self.filename
            or "_test." in self.filename
            or ".test." in self.filename
        ):
            return 10

        low_priority = getattr(config, "low_priority_extensions", _DEFAULT_LOW_PRIORITY_EXTENSIONS)
        ext = self.filename.rsplit(".", 1)[-1] if "." in self.filename else ""
        if any(str(item).lstrip(".") == ext for item in low_priority):
            return 20

        match ext:
            case "rs" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "java" | "c" | "cpp" | "h" | "hpp":
                return 100
            case "sql" | "sh" | "bash":
                return 80
            case _:
                return 50

    def truncate(self, max_size: int) -> None:
        """Truncate content in place while preserving headers and useful edges."""

        if self.size <= max_size:
            return

        truncation_suffix = "\n... (truncated)"
        available = max_size - _byte_len(self.header) - _byte_len(truncation_suffix)
        if available < 50:
            self.content = "... (truncated)"
            return

        lines = self.content.splitlines()
        if len(lines) > 30:
            keep_start = 15
            keep_end = 10
            omitted = len(lines) - keep_start - keep_end
            self.content = "\n".join([*lines[:keep_start], f"... (truncated {omitted} lines) ...", *lines[-keep_end:]])
            return

        self.content = _truncate_utf8(self.content, available) + truncation_suffix


def _byte_len(text: str) -> int:
    return len(text.encode("utf-8"))


def _truncate_utf8(text: str, max_bytes: int) -> str:
    data = text.encode("utf-8")
    if len(data) <= max_bytes:
        return text
    return data[:max_bytes].decode("utf-8", errors="ignore")


@dataclass(slots=True)
class WhitespaceReport:
    """Classification of a diff by whitespace-only and substantive files."""

    whitespace_only_files: list[str] = field(default_factory=list)
    has_substantive: bool = False

    @property
    def all_whitespace(self) -> bool:
        """Return true when every changed file only changes whitespace."""

        return bool(self.whitespace_only_files) and not self.has_substantive

    @property
    def is_whitespace_only(self) -> bool:
        """Return true when every changed file only changes whitespace."""

        return self.all_whitespace


def parse_diff(diff: str) -> list[FileDiff]:
    """Parse a unified git diff into file-level sections."""

    file_diffs: list[FileDiff] = []
    current: FileDiff | None = None
    in_diff_header = False

    for line in diff.splitlines():
        if line.startswith("diff --git"):
            if current is not None:
                file_diffs.append(current)
            parts = line.split()
            filename = parts[3].removeprefix("b/") if len(parts) > 3 else "unknown"
            current = FileDiff(filename=filename, header=line)
            in_diff_header = True
            continue

        if current is None:
            continue

        if line.startswith("Binary files"):
            current.is_binary = True
            current.header += "\n" + line
        elif line.startswith(
            (
                "index ",
                "new file",
                "deleted file",
                "rename ",
                "copy ",
                "similarity index",
                "dissimilarity index",
                "old mode",
                "new mode",
                "+++",
                "---",
            )
        ):
            current.header += "\n" + line
        elif line.startswith("@@"):
            in_diff_header = False
            current.header += "\n" + line
        elif not in_diff_header:
            if current.content:
                current.content += "\n"
            current.content += line
            if line.startswith("+") and not line.startswith("+++"):
                current.additions += 1
            elif line.startswith("-") and not line.startswith("---"):
                current.deletions += 1
        else:
            current.header += "\n" + line

    if current is not None:
        file_diffs.append(current)
    return file_diffs


def reconstruct_diff(files: list[FileDiff] | tuple[FileDiff, ...]) -> str:
    """Reconstruct a unified diff from parsed file objects."""

    sections: list[str] = []
    for file in files:
        if file.content:
            sections.append(f"{file.header}\n{file.content}")
        else:
            sections.append(file.header)
    return "\n".join(sections)


def smart_truncate_diff(
    diff: str,
    max_length: int,
    config: object | None = None,
    counter: _TokenCounter | None = None,
) -> str:
    """Truncate a diff by file priority while retaining whole-file scope."""

    file_diffs = [file for file in parse_diff(diff) if not _is_excluded(file.filename, config)]
    if not file_diffs:
        return "No relevant files to analyze (only lock files or excluded files were changed)"

    file_diffs.sort(key=lambda file: file.priority(config), reverse=True)
    total_size = sum(file.size for file in file_diffs)
    total_tokens = sum(file.token_estimate(counter) for file in file_diffs)
    max_diff_tokens = int(getattr(config, "max_diff_tokens", 16_000))
    effective_max = max_diff_tokens * 4 if total_tokens > max_diff_tokens else max_length

    if total_size <= effective_max:
        return reconstruct_diff(file_diffs)

    included: list[FileDiff] = []
    header_only_size = sum(_byte_len(file.header) + 20 for file in file_diffs)
    total_files = len(file_diffs)

    if header_only_size <= effective_max:
        remaining_space = max(0, effective_max - header_only_size)
        space_per_file = remaining_space // len(file_diffs) if file_diffs else 0
        for file in file_diffs:
            if file.is_binary:
                included.append(FileDiff(file.filename, file.header, "", file.additions, file.deletions, True))
                continue
            target_size = _byte_len(file.header) + space_per_file
            if file.size > target_size:
                file.truncate(target_size)
            included.append(file)
    else:
        current_size = 0
        for file in file_diffs:
            if file.is_binary:
                continue
            if current_size + file.size <= effective_max:
                current_size += file.size
                included.append(file)
            elif current_size < effective_max // 2 and file.priority(config) >= 50:
                file.truncate(max(0, effective_max - current_size - 100))
                included.append(file)
                break

    if not included:
        return "Error: Could not include any files in the diff"

    result = reconstruct_diff(included)
    excluded_count = total_files - len(included)
    if excluded_count > 0:
        result += f"\n\n... ({excluded_count} files omitted) ..."
    return result


def truncate_diff_by_lines(diff: str, max_lines: int, config: object | None = None) -> str:
    """Truncate a diff to a line budget, distributing lines by file priority."""

    files = parse_diff(diff)
    total_lines = sum(len(file.header.splitlines()) + len(file.content.splitlines()) for file in files)
    if total_lines <= max_lines:
        return diff

    total_priority = sum(max(1, file.priority(config)) for file in files) or 1
    result: list[str] = []
    for file in files:
        result.extend(file.header.splitlines())
        content_lines = file.content.splitlines()
        priority = max(1, file.priority(config))
        allocated = max(5, int(max_lines * priority / total_priority))
        if len(content_lines) <= allocated:
            result.extend(content_lines)
            if not content_lines:
                result.append("")
            continue
        keep_start = allocated // 2
        keep_end = allocated - keep_start
        omitted = len(content_lines) - keep_start - keep_end
        result.extend(content_lines[:keep_start])
        result.append(f"[... {omitted} lines omitted ...]")
        result.extend(content_lines[-keep_end:])
    return "\n".join(result) + ("\n" if result else "")


def classify_diff_whitespace(diff: str) -> WhitespaceReport:
    """Classify a unified diff by whitespace-only versus substantive files."""

    _, sections = _file_sections(diff)
    report = WhitespaceReport()
    for path, section in sections:
        if _section_is_whitespace_only(section):
            report.whitespace_only_files.append(path)
        else:
            report.has_substantive = True
    return report


def strip_whitespace_only_files(diff: str) -> str | None:
    """Return diff without whitespace-only file sections, or None if unchanged."""

    preamble, sections = _file_sections(diff)
    if not sections:
        return None

    kept: list[str] = []
    stripped_any = False
    for _, section in sections:
        if _section_is_whitespace_only(section):
            stripped_any = True
        else:
            kept.append(section)

    if not stripped_any or not kept:
        return None
    return preamble + "".join(kept)


def _is_excluded(filename: str, config: object | None) -> bool:
    excluded = getattr(config, "excluded_files", ())
    return any(filename.endswith(str(pattern)) for pattern in excluded)


def _file_section_starts(diff: str) -> list[int]:
    starts: list[int] = []
    search_from = 0
    while True:
        idx = diff.find("diff --git", search_from)
        if idx == -1:
            return starts
        if idx == 0 or diff[idx - 1] == "\n":
            starts.append(idx)
        search_from = idx + len("diff --git")


def _file_sections(diff: str) -> tuple[str, list[tuple[str, str]]]:
    starts = _file_section_starts(diff)
    if not starts:
        return diff, []
    preamble = diff[: starts[0]]
    sections: list[tuple[str, str]] = []
    for index, start in enumerate(starts):
        end = starts[index + 1] if index + 1 < len(starts) else len(diff)
        section = diff[start:end]
        first_line = section.splitlines()[0] if section else ""
        parts = first_line.split()
        path = parts[3].removeprefix("b/") if len(parts) > 3 else "unknown"
        sections.append((path, section))
    return preamble, sections


def _section_is_whitespace_only(section: str) -> bool:
    added: list[str] = []
    removed: list[str] = []
    has_change = False

    for line in section.splitlines():
        if line.startswith(("Binary files", "rename from", "rename to", "copy from", "copy to")):
            return False
        if line.startswith(("+++", "---")):
            continue
        if line.startswith("+"):
            has_change = True
            added.extend(ch for ch in line[1:] if not ch.isspace())
        elif line.startswith("-"):
            has_change = True
            removed.extend(ch for ch in line[1:] if not ch.isspace())

    return has_change and added == removed

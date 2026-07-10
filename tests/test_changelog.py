from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from subprocess import CompletedProcess

from lgit.api import _extract_json_from_content
from lgit.changelog import (
    UnreleasedSection,
    _format_existing_entries,
    _parse_jsonish,
    _staged_changelog_content,
    parse_unreleased_section,
    stage_changelog_blob,
    write_entries,
)
from lgit.models import ChangelogCategory

type RunGit = Callable[..., CompletedProcess[str]]

BASE_CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n## [1.0.0] - 2020-01-01\n\n### Added\n\n- Old entry.\n"


def test_changelog_staging_keeps_unrelated_unstaged_edits_out(repo: Path, run_git: RunGit) -> None:
    changelog_path = repo / "CHANGELOG.md"
    changelog_path.write_text(BASE_CHANGELOG, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "docs: add changelog")

    worktree_content = f"{BASE_CHANGELOG}\nUNRELATED DRAFT NOTES\n"
    changelog_path.write_text(worktree_content, encoding="utf-8")

    staged_content = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged_content == BASE_CHANGELOG

    unreleased = parse_unreleased_section(staged_content, changelog_path)
    worktree_unreleased = parse_unreleased_section(worktree_content, changelog_path)
    new_entries = {ChangelogCategory.ADDED: ["New pinned entry."]}

    updated_staged = write_entries(staged_content, unreleased, new_entries)
    updated_worktree = write_entries(worktree_content, worktree_unreleased, new_entries)
    changelog_path.write_text(updated_worktree, encoding="utf-8")
    stage_changelog_blob("CHANGELOG.md", updated_staged, repo)

    staged_now = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged_now is not None
    assert "New pinned entry." in staged_now
    assert "UNRELATED DRAFT NOTES" not in staged_now

    on_disk = changelog_path.read_text(encoding="utf-8")
    assert "New pinned entry." in on_disk
    assert "UNRELATED DRAFT NOTES" in on_disk


def test_extract_json_from_content_raw() -> None:
    content = '{"entries": {"Added": ["entry 1"]}}'

    assert _extract_json_from_content(content) == '{"entries": {"Added": ["entry 1"]}}'


def test_extract_json_from_content_code_block() -> None:
    content = """Here's the changelog:

```json
{"entries": {"Added": ["entry 1"]}}
```

That's all!"""

    assert _extract_json_from_content(content) == '{"entries": {"Added": ["entry 1"]}}'


def test_extract_json_from_content_generic_block() -> None:
    content = """```
{"entries": {"Fixed": ["bug fix"]}}
```"""

    assert _extract_json_from_content(content) == '{"entries": {"Fixed": ["bug fix"]}}'


def test_parse_jsonish_parses_markdown_changelog() -> None:
    content = "# Added\n- Added websocket reconnects.\n\n# Fixed\n- Fixed retry loop."

    assert _parse_jsonish(content) == {
        "entries": {"Added": ["Added websocket reconnects."], "Fixed": ["Fixed retry loop."]}
    }


def test_parse_jsonish_falls_back_to_json_object() -> None:
    content = '```json\n{"entries": {"Added": ["entry 1"]}}\n```'

    assert _parse_jsonish(content) == {"entries": {"Added": ["entry 1"]}}


def test_parse_jsonish_decodes_pretty_printed_json_over_markdown() -> None:
    content = '{\n  "entries": {\n    "Added": ["Added websocket reconnects."]\n  }\n}'

    assert _parse_jsonish(content) == {"entries": {"Added": ["Added websocket reconnects."]}}


def test_parse_unreleased_section() -> None:
    content = """# Changelog

## [Unreleased]

### Added

- Feature one
- Feature two

### Fixed

- Bug fix

## [1.0.0] - 2024-01-01

### Added

- Initial release
"""

    section = parse_unreleased_section(content, Path("CHANGELOG.md"))

    assert section.header_line == 2
    assert section.end_line == 13
    assert section.entries[ChangelogCategory.ADDED] == ["- Feature one", "- Feature two"]
    assert section.entries[ChangelogCategory.FIXED] == ["- Bug fix"]


def test_format_existing_entries() -> None:
    unreleased = UnreleasedSection(
        Path("CHANGELOG.md"),
        header_line=0,
        end_line=10,
        entries={
            ChangelogCategory.ADDED: ["- Feature one", "- Feature two"],
            ChangelogCategory.FIXED: ["- Bug fix"],
        },
    )

    formatted = _format_existing_entries(unreleased)

    assert formatted is not None
    assert "### Added" in formatted
    assert "- Feature one" in formatted
    assert "### Fixed" in formatted
    assert "- Bug fix" in formatted


def test_write_entries_trims_and_skips_empty_bullets() -> None:
    content = """# Changelog

## [Unreleased]

## [1.0.0] - 2024-01-01
"""
    unreleased = parse_unreleased_section(content, Path("CHANGELOG.md"))
    new_entries = {
        ChangelogCategory.ADDED: [
            "  Added configurable power assertions  ",
            " -   ",
            "",
            "* Fixed prompt cancellation cleanup ",
        ]
    }

    updated = write_entries(content, unreleased, new_entries)

    assert "- Added configurable power assertions\n" in updated
    assert "- Fixed prompt cancellation cleanup\n" in updated
    assert "- \n" not in updated
    assert "* Fixed" not in updated


def test_write_entries_keeps_category_bullets_contiguous() -> None:
    content = """# Changelog

## [Unreleased]

### Changed

- Standardized reasoning effort levels.
- Updated costs and context windows.

## [1.0.0] - 2024-01-01
"""
    unreleased = parse_unreleased_section(content, Path("CHANGELOG.md"))
    new_entries = {ChangelogCategory.CHANGED: ["Enabled reasoning effort controls."]}

    updated = write_entries(content, unreleased, new_entries)

    assert (
        "- Enabled reasoning effort controls.\n- Standardized reasoning effort levels.\n- Updated costs and context windows.\n"
        in updated
    )


def test_parse_unreleased_section_skips_empty_bullets() -> None:
    content = """# Changelog

## [Unreleased]

### Fixed

- 
- Fixed cancellation cleanup
*    

## [1.0.0] - 2024-01-01
"""

    section = parse_unreleased_section(content, Path("CHANGELOG.md"))

    assert section.entries[ChangelogCategory.FIXED] == ["- Fixed cancellation cleanup"]


def test_format_existing_entries_empty() -> None:
    unreleased = UnreleasedSection(Path("CHANGELOG.md"), header_line=0, end_line=10, entries={})

    assert _format_existing_entries(unreleased) is None

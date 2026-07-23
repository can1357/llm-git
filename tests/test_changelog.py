from __future__ import annotations

import asyncio
from collections.abc import Callable
from pathlib import Path
from subprocess import CompletedProcess
from types import SimpleNamespace

from lgit.api import _extract_json_from_content
from lgit.changelog import (
    ChangelogRevision,
    UnreleasedSection,
    _drop_duplicate_entries,
    _entries_added_since,
    _format_existing_entries,
    _head_unreleased,
    _parse_jsonish,
    _staged_changelog_content,
    apply_revisions,
    parse_changelog_revisions,
    parse_unreleased_section,
    run_changelog_flow,
    stage_changelog_blob,
    write_entries,
)
from lgit.config import CommitConfig
from lgit.map_reduce import FileObservation
from lgit.markdown_output import parse_changelog_response
from lgit.models import ChangelogCategory

type RunGit = Callable[..., CompletedProcess[str]]

BASE_CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n## [1.0.0] - 2020-01-01\n\n### Added\n\n- Old entry.\n"

BATCH_ENTRY = "- Added widget frobnication."
BATCH_CHANGELOG = f"# Changelog\n\n## [Unreleased]\n\n### Added\n\n{BATCH_ENTRY}\n\n## [1.0.0] - 2020-01-01\n"


def _commit_batch_changelog(repo: Path, run_git: RunGit) -> Path:
    changelog_path = repo / "CHANGELOG.md"
    changelog_path.write_text(BATCH_CHANGELOG, encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: added batch changelog")
    return changelog_path


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


def test_parse_changelog_revisions_normalizes_replacements_and_drops() -> None:
    text = """<revise>
OLD: Added task labels
NEW: * Added task labels with automatic retries
OLD: - Added experimental command
NEW:
</revise>"""

    assert parse_changelog_revisions(text) == [
        ChangelogRevision("- Added task labels", "- Added task labels with automatic retries"),
        ChangelogRevision("- Added experimental command", None),
    ]


def test_parse_changelog_revisions_without_block_returns_empty() -> None:
    assert parse_changelog_revisions("OLD: - Outside the block\nNEW: - Ignored") == []


def test_parse_changelog_revisions_ignores_unpaired_lines_case_insensitively() -> None:
    text = """<ReViSe>
nEw: orphan
oLd: first unpaired entry
OLD: * second entry
ignored prose
new: replacement entry
OLD:
NEW: ignored empty old
</rEvIsE>"""

    assert parse_changelog_revisions(text) == [ChangelogRevision("- second entry", "- replacement entry")]


def test_parse_changelog_response_strips_revision_block_from_sections() -> None:
    text = """<revise>
OLD: - Added task labels
NEW: - Added task labels with retries
</revise>

# Fixed
- Fixed a released retry bug
"""

    assert parse_changelog_response(text) == {"entries": {"Fixed": ["Fixed a released retry bug"]}}


def test_parse_changelog_response_accepts_revisions_only_without_exception() -> None:
    text = """<revise>
OLD: - Added canceled command
NEW:
</revise>"""

    assert parse_changelog_response(text) == {"entries": {}}


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


def test_entries_added_since_isolates_hand_written_entries() -> None:
    head = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Old entry.\n")
    staged = parse_unreleased_section(
        "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- New staged entry.\n- Old entry.\n\n### Fixed\n\n- New fix.\n"
    )
    worktree = parse_unreleased_section(
        "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- New staged entry.\n- Worktree only entry.\n- Old entry.\n"
    )

    added = _entries_added_since(head, staged, worktree)

    assert added == {
        ChangelogCategory.ADDED: ["- New staged entry.", "- Worktree only entry."],
        ChangelogCategory.FIXED: ["- New fix."],
    }


def test_entries_added_since_empty_when_unchanged() -> None:
    head = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Old entry.\n")

    assert _entries_added_since(head, head, None) == {}


def test_apply_revisions_replaces_drops_and_skips_unsafe_ops() -> None:
    entries = {
        ChangelogCategory.ADDED: ["- Before", "- Target entry", "- After", "- Authored entry"],
        ChangelogCategory.FIXED: ["- Drop entry"],
    }
    replace_op = ChangelogRevision("- TARGET ENTRY", "- Replacement entry")
    drop_op = ChangelogRevision("- Drop entry", None)
    authored_op = ChangelogRevision("- Authored entry", None)
    unmatched_op = ChangelogRevision("- Missing entry", None)

    revised, applied = apply_revisions(
        entries,
        [replace_op, drop_op, authored_op, unmatched_op],
        {"- target entry", "- drop entry", "- missing entry"},
    )

    assert revised == {
        ChangelogCategory.ADDED: ["- Before", "- Replacement entry", "- After", "- Authored entry"],
        ChangelogCategory.FIXED: [],
    }
    assert applied == [replace_op, drop_op]
    assert entries[ChangelogCategory.ADDED][1] == "- Target entry"


def test_drop_duplicate_entries_filters_case_insensitively() -> None:
    section = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Added retry logic.\n")
    entries = {
        ChangelogCategory.ADDED: ["- added retry logic.", "- Added new thing."],
        ChangelogCategory.FIXED: ["- Added retry logic."],
    }

    assert _drop_duplicate_entries(entries, section.entries, None) == {ChangelogCategory.ADDED: ["- Added new thing."]}


def test_drop_duplicate_entries_catches_reworded_recategorized_copies() -> None:
    section = parse_unreleased_section(
        "# Changelog\n\n## [Unreleased]\n\n### Changed\n\n"
        "- Task rendering now keeps the agent type badge on live progress and finished result rows.\n"
    )
    entries = {
        ChangelogCategory.FIXED: [
            "- Fixed task rendering to keep the agent type badge visible on progress and result rows",
            "- Fixed crash when parsing empty diffs",
        ],
    }

    assert _drop_duplicate_entries(entries, section.entries, None) == {
        ChangelogCategory.FIXED: ["- Fixed crash when parsing empty diffs"]
    }


def test_head_unreleased_missing_file_marks_all_entries_authored(repo: Path) -> None:
    section = _head_unreleased("CHANGELOG.md", repo / "CHANGELOG.md", repo)

    assert section is not None
    assert section.entries == {}


def test_head_unreleased_unparseable_returns_none(repo: Path, run_git: RunGit) -> None:
    (repo / "CHANGELOG.md").write_text("# Changelog\n\nNo unreleased section here.\n", encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: changelog without unreleased")

    assert _head_unreleased("CHANGELOG.md", repo / "CHANGELOG.md", repo) is None


def test_flow_passes_authored_entries_and_drops_duplicates(repo: Path, run_git: RunGit, monkeypatch: object) -> None:
    import lgit.api as api

    changelog_path = repo / "CHANGELOG.md"
    changelog_path.write_text(BASE_CHANGELOG, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n\n\ndef extra():\n    return 3\n", encoding="utf-8")
    edited = BASE_CHANGELOG.replace(
        "## [Unreleased]\n",
        "## [Unreleased]\n\n### Changed\n\n- Changed value to return 2.\n",
    )
    changelog_path.write_text(edited, encoding="utf-8")
    run_git(repo, "add", ".")

    captured: list[object] = []

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        captured.append(spec)
        return SimpleNamespace(output="# Changed\n- changed value to return 2.\n\n# Added\n- Added extra helper.")

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    args = SimpleNamespace(dir=str(repo))
    updated = asyncio.run(run_changelog_flow(args, CommitConfig()))

    assert len(updated) == 1
    (spec,) = captured
    prompt = spec.user_prompt  # type: ignore[attr-defined]
    assert "<authored_entries>" in prompt
    assert "- Changed value to return 2." in prompt

    staged = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged is not None
    assert staged.count("Changed value to return 2.") == 1
    assert "- Added extra helper." in staged


def test_flow_prompts_with_observations_instead_of_diff(repo: Path, run_git: RunGit, monkeypatch: object) -> None:
    import lgit.api as api

    changelog_path = repo / "CHANGELOG.md"
    changelog_path.write_text(BASE_CHANGELOG, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", ".")

    captured: list[object] = []

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        captured.append(spec)
        return SimpleNamespace(output="# Changed\n- Changed value to return 2")

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    observations = [
        FileObservation("app.py", ("changed value() to return 2",)),
        FileObservation("unrelated.py", ("touched an unstaged file",)),
    ]
    updated = asyncio.run(run_changelog_flow(SimpleNamespace(dir=str(repo)), CommitConfig(), observations))

    assert len(updated) == 1
    (spec,) = captured
    prompt = spec.user_prompt  # type: ignore[attr-defined]
    assert "<file_change_summaries>" in prompt
    assert "- changed value() to return 2" in prompt
    # The raw diff is replaced, and observations for files outside the boundary are filtered out.
    assert "diff --git" not in prompt
    assert "unstaged file" not in prompt


def test_flow_falls_back_to_diff_when_observations_miss_boundary(
    repo: Path,
    run_git: RunGit,
    monkeypatch: object,
) -> None:
    import lgit.api as api

    changelog_path = repo / "CHANGELOG.md"
    changelog_path.write_text(BASE_CHANGELOG, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", ".")

    captured: list[object] = []

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        captured.append(spec)
        return SimpleNamespace(output="<exception>internal only</exception>")

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    observations = [FileObservation("elsewhere.py", ("changed something else",))]
    asyncio.run(run_changelog_flow(SimpleNamespace(dir=str(repo)), CommitConfig(), observations))

    (spec,) = captured
    prompt = spec.user_prompt  # type: ignore[attr-defined]
    assert "<file_change_summaries>" not in prompt
    assert "diff --git" in prompt


def test_flow_revises_head_entry_without_adding_sections(
    repo: Path,
    run_git: RunGit,
    monkeypatch: object,
) -> None:
    import lgit.api as api

    changelog_path = _commit_batch_changelog(repo, run_git)
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    replacement = "- Added reliable widget frobnication with automatic retries."
    captured: list[object] = []

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        captured.append(spec)
        return SimpleNamespace(
            output={"entries": {}},
            text_content=(
                f"<revise>\nOLD: {BATCH_ENTRY}\nNEW: {replacement}\n</revise>\n"
                "<exception>consolidated into the existing entry</exception>"
            ),
        )

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    updated = asyncio.run(run_changelog_flow(SimpleNamespace(dir=str(repo)), CommitConfig()))

    assert len(updated) == 1
    (spec,) = captured
    assert "<revise>" in spec.system_prompt  # type: ignore[attr-defined]
    staged = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged is not None
    assert BATCH_ENTRY not in staged
    assert staged.count(replacement) == 1
    assert staged.count("### Added") == 1
    assert "### Fixed" not in staged
    assert changelog_path.read_text(encoding="utf-8") == staged


def test_flow_protects_authored_entry_from_revision(
    repo: Path,
    run_git: RunGit,
    monkeypatch: object,
) -> None:
    import lgit.api as api

    changelog_path = _commit_batch_changelog(repo, run_git)
    authored_entry = "- Added author-approved widget telemetry."
    edited = BATCH_CHANGELOG.replace(BATCH_ENTRY, f"{authored_entry}\n{BATCH_ENTRY}")
    changelog_path.write_text(edited, encoding="utf-8")
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", ".")

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        return SimpleNamespace(
            output={"entries": {}},
            text_content=(
                f"<revise>\nOLD: {authored_entry}\nNEW:\n</revise>\n"
                "<exception>entry already covered the change</exception>"
            ),
        )

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    updated = asyncio.run(run_changelog_flow(SimpleNamespace(dir=str(repo)), CommitConfig()))

    assert updated == []
    assert _staged_changelog_content("CHANGELOG.md", repo) == edited
    assert changelog_path.read_text(encoding="utf-8") == edited


def test_flow_revision_switch_disables_reconciliation(
    repo: Path,
    run_git: RunGit,
    monkeypatch: object,
) -> None:
    import lgit.api as api

    changelog_path = _commit_batch_changelog(repo, run_git)
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")
    captured: list[object] = []

    async def fake_run_oneshot(config: CommitConfig, spec: object) -> object:
        captured.append(spec)
        return SimpleNamespace(
            output={"entries": {}},
            text_content=f"<revise>\nOLD: {BATCH_ENTRY}\nNEW: - Replaced entry.\n</revise>",
        )

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)  # type: ignore[attr-defined]

    updated = asyncio.run(
        run_changelog_flow(
            SimpleNamespace(dir=str(repo)),
            CommitConfig(changelog_revise=False),
        )
    )

    assert updated == []
    (spec,) = captured
    assert "<revise>" not in spec.system_prompt  # type: ignore[attr-defined]
    assert _staged_changelog_content("CHANGELOG.md", repo) == BATCH_CHANGELOG
    assert changelog_path.read_text(encoding="utf-8") == BATCH_CHANGELOG

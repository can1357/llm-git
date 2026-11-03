from __future__ import annotations

import asyncio
from collections.abc import Callable, Sequence
from dataclasses import replace
from pathlib import Path
from subprocess import CompletedProcess
from types import SimpleNamespace

import pytest
from lgit import api, changelog, git
from lgit.api import _extract_json_from_content
from lgit.changelog import (
    ChangelogWeaver,
    UnreleasedSection,
    _entries_added_since,
    _format_existing_entries,
    _head_unreleased,
    _parse_jsonish,
    _staged_changelog_content,
    apply_changelog_updates,
    apply_revisions,
    drop_duplicate_entries,
    generate_changelog_entries,
    parse_changelog_revisions,
    parse_unreleased_section,
    prepare_changelog_flow,
    prepare_synthesized_updates,
    prepared_changelog_contexts,
    run_changelog_flow,
    stage_changelog_blob,
    write_entries,
)
from lgit.config import CommitConfig
from lgit.errors import ValidationFailure
from lgit.judgment import Answers, Judge, JudgmentRequest, NoulAnswer
from lgit.map_reduce import FileObservation
from lgit.markdown_output import parse_changelog_response
from lgit.models import ChangelogCategory, ChangelogRevision, ChangelogUpdate

type RunGit = Callable[..., CompletedProcess[str]]

BASE_CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n## [1.0.0] - 2020-01-01\n\n### Added\n\n- Old entry.\n"

BATCH_ENTRY = "- Added widget frobnication."
BATCH_CHANGELOG = f"# Changelog\n\n## [Unreleased]\n\n### Added\n\n{BATCH_ENTRY}\n\n## [1.0.0] - 2020-01-01\n"


def _added_changelog(*entries: str) -> str:
    bullets = "\n".join(f"- {entry}" for entry in entries)
    section = f"\n\n### Added\n\n{bullets}" if bullets else ""
    return f"# Changelog\n\n## [Unreleased]{section}\n"


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

    updated_staged = write_entries(unreleased, new_entries)
    updated_worktree = write_entries(worktree_unreleased, new_entries)
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
        lines=(),
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

    updated = write_entries(unreleased, new_entries)

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

    updated = write_entries(unreleased, new_entries)

    assert (
        "- Enabled reasoning effort controls.\n- Standardized reasoning effort levels.\n- Updated costs and context windows.\n"
        in updated
    )


def test_write_entries_preserves_subsections_prose_and_unknown_categories() -> None:
    content = """# Changelog

## [Unreleased]

### Highlights

Long chats got faster.

### Changed

#### Tabs

- Tabs scroll with a mouse wheel.

### Fixed

* Fixed a crash.

#### macOS

- Threads run in QoS classes.

## [1.0.0] - 2024-01-01
"""
    unreleased = parse_unreleased_section(content, Path("CHANGELOG.md"))
    new_entries = {
        ChangelogCategory.CHANGED: ["Switching tabs is faster."],
        ChangelogCategory.FIXED: ["Fixed a hang."],
        ChangelogCategory.ADDED: ["Added a tab peek."],
    }

    assert (
        write_entries(unreleased, new_entries)
        == """# Changelog

## [Unreleased]

### Highlights

Long chats got faster.

### Added

- Added a tab peek.

### Changed

- Switching tabs is faster.

#### Tabs

- Tabs scroll with a mouse wheel.

### Fixed

* Fixed a hang.
* Fixed a crash.

#### macOS

- Threads run in QoS classes.

## [1.0.0] - 2024-01-01
"""
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
    unreleased = UnreleasedSection(Path("CHANGELOG.md"), lines=(), header_line=0, end_line=10, entries={})

    assert _format_existing_entries(unreleased) is None


def test_flow_authored_entries_exclude_unstaged_worktree_entries(repo: Path, run_git: RunGit) -> None:
    # Unstaged entries may document other pending work; presenting them as authored for this
    # commit made the model describe that work in the message.
    old = "Documented the original behavior."
    staged_authored = "Documented a staged author change."
    unrelated = "Added an unrelated feature still being written."
    path = repo / "CHANGELOG.md"
    path.write_text(_added_changelog(old), encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    path.write_text(_added_changelog(staged_authored, old), encoding="utf-8")
    run_git(repo, "add", ".")
    path.write_text(_added_changelog(unrelated, staged_authored, old), encoding="utf-8")

    prepared = prepare_changelog_flow(SimpleNamespace(dir=str(repo)), CommitConfig())
    (context,) = prepared_changelog_contexts(prepared)

    assert context.authored_entries is not None
    assert f"- {staged_authored}" in context.authored_entries
    assert unrelated not in context.authored_entries


def test_entries_added_since_empty_when_unchanged() -> None:
    head = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Old entry.\n")

    assert _entries_added_since(head, head) == {}


def test_apply_revisions_replaces_drops_and_skips_unsafe_ops() -> None:
    content = """# Changelog

## [Unreleased]

### Added

#### CLI

* Before
* Target entry
* Authored entry

### Fixed

#### macOS

- Drop entry
  wrapped detail of the dropped entry

#### Windows

- Kept fix

## [1.0.0] - 2024-01-01
"""
    replace_op = ChangelogRevision("- TARGET ENTRY", "- Replacement entry")
    drop_op = ChangelogRevision("- Drop entry", None)
    authored_op = ChangelogRevision("- Authored entry", None)
    unmatched_op = ChangelogRevision("- Missing entry", None)

    revised, applied = apply_revisions(
        parse_unreleased_section(content),
        [replace_op, drop_op, authored_op, unmatched_op],
        {"- target entry", "- drop entry", "- missing entry"},
    )

    assert applied == [replace_op, drop_op]
    assert revised.entries == {
        ChangelogCategory.ADDED: ["- Before", "- Replacement entry", "- Authored entry"],
        ChangelogCategory.FIXED: ["- Kept fix"],
    }
    assert (
        "\n".join(revised.lines)
        == """# Changelog

## [Unreleased]

### Added

#### CLI

* Before
* Replacement entry
* Authored entry

### Fixed

#### Windows

- Kept fix

## [1.0.0] - 2024-01-01
"""
    )


def test_drop_duplicate_entries_filters_case_insensitively() -> None:
    section = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Added retry logic.\n")
    entries = {
        ChangelogCategory.ADDED: ["- added retry logic.", "- Added new thing."],
        ChangelogCategory.FIXED: ["- Added retry logic."],
    }

    assert _dedup(entries, section.entries) == {ChangelogCategory.ADDED: ["- Added new thing."]}


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

    assert _dedup(entries, section.entries) == {ChangelogCategory.FIXED: ["- Fixed crash when parsing empty diffs"]}


class _NoulJudge(Judge):
    """Answers every yes/no question from a key→probability table, recording requests."""

    def __init__(self, table: dict[str, float]) -> None:
        super().__init__(base_url="https://judge.test", model="fake", api_key="k")
        self.table = table
        self.requests: list[JudgmentRequest] = []

    async def judge_all(self, operation: str, requests: Sequence[JudgmentRequest]) -> list[Answers | None]:
        self.requests.extend(requests)
        if self.table.get("__fail__"):
            return [None for _ in requests]
        return [{key: NoulAnswer(self.table.get(key, 0.0)) for key in request.questions} for request in requests]


def _dedup(entries, *existing, judge: Judge | None = None):
    return asyncio.run(drop_duplicate_entries(entries, *existing, judge=judge, config=CommitConfig()))


def test_drop_duplicate_entries_judges_rewordings_and_skips_verbatim_repeats() -> None:
    section = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Added retry logic.\n")
    entries = {
        ChangelogCategory.ADDED: ["- added retry logic.", "- Retries failed requests automatically", "- Added colours"],
    }
    judge = _NoulJudge({"C1": 0.9, "C2": 0.1})

    assert _dedup(entries, section.entries, judge=judge) == {ChangelogCategory.ADDED: ["- Added colours"]}
    # The verbatim repeat never reaches the judge, nor does "Added colours" (no content stem in
    # common with any existing entry); only the reworded retry line is judged.
    (request,) = judge.requests
    assert list(request.questions) == ["C1"]
    assert "C1: - Retries failed requests automatically" in request.state["candidates"]
    assert request.state["existing_entries"] == "- Added retry logic."


def test_drop_duplicate_entries_falls_back_to_overlap_when_judgment_fails() -> None:
    section = parse_unreleased_section("# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Added retry logic.\n")
    entries = {ChangelogCategory.FIXED: ["- Added retry logic", "- Fixed crash"]}

    assert _dedup(entries, section.entries, judge=_NoulJudge({"__fail__": 1})) == {
        ChangelogCategory.FIXED: ["- Fixed crash"]
    }
    # Nothing existing → nothing to judge, no request made.
    judge = _NoulJudge({})
    assert _dedup(entries, {}, judge=judge) == entries and judge.requests == []


def test_generate_changelog_entries_skips_model_when_judged_not_user_visible(monkeypatch: pytest.MonkeyPatch) -> None:
    calls: list[str] = []

    async def fake_run_oneshot(*args: object, **kwargs: object) -> SimpleNamespace:
        calls.append("model")
        return SimpleNamespace(output={"entries": {"Added": ["- Added thing"]}}, text_content="")

    monkeypatch.setattr(api, "run_oneshot", fake_run_oneshot)
    config = CommitConfig(judgment_api_key="k", changelog_gate_min_tokens=0)
    judge = _NoulJudge({"worthy": 0.1})
    monkeypatch.setattr(changelog.Judge, "from_config", classmethod(lambda cls, _config: judge))

    result = asyncio.run(
        generate_changelog_entries("CHANGELOG.md", False, " a.py | 1 +\n", "diff", None, None, False, config)
    )
    assert result == ({}, []) and calls == []
    assert judge.requests[0].state["stat"] == " a.py | 1 +\n"

    judge.table["worthy"] = 0.95
    result = asyncio.run(
        generate_changelog_entries("CHANGELOG.md", False, " a.py | 1 +\n", "diff", None, None, False, config)
    )
    assert calls == ["model"] and result[0] == {ChangelogCategory.ADDED: ["- Added thing"]}

    # Gate disabled: the model always runs.
    judge.table["worthy"] = 0.0
    asyncio.run(
        generate_changelog_entries(
            "CHANGELOG.md", False, "", "diff", None, None, False, replace(config, changelog_gate_threshold=0)
        )
    )
    assert calls == ["model", "model"]

    # Below the size floor the gate never runs: the round trip costs more than the call it could save.
    judge.requests.clear()
    asyncio.run(
        generate_changelog_entries(
            "CHANGELOG.md", False, "", "diff", None, None, False, replace(config, changelog_gate_min_tokens=2000)
        )
    )
    assert calls == ["model", "model", "model"] and judge.requests == []


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


def test_synthesized_updates_reconcile_multiple_boundaries_and_preserve_all_entries(
    repo: Path,
    run_git: RunGit,
) -> None:
    root_old = "Documented the original root behavior."
    package_old = "Documented the original package behavior."
    authored = "Documented an author-approved root change."
    (repo / "CHANGELOG.md").write_text(_added_changelog(root_old), encoding="utf-8")
    package = repo / "packages" / "widget"
    package.mkdir(parents=True)
    (package / "CHANGELOG.md").write_text(_added_changelog(package_old), encoding="utf-8")
    (package / "widget.py").write_text("value = 1\n", encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "docs: add changelog boundaries")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    (package / "widget.py").write_text("value = 2\n", encoding="utf-8")
    (repo / "CHANGELOG.md").write_text(_added_changelog(authored, root_old), encoding="utf-8")
    run_git(repo, "add", ".")

    config = CommitConfig()
    prepared = prepare_changelog_flow(SimpleNamespace(dir=str(repo)), config)
    contexts = prepared_changelog_contexts(prepared)
    paths = {context.path: context for context in contexts}
    updates = [
        ChangelogUpdate(
            path="packages/widget/CHANGELOG.md",
            entries={
                ChangelogCategory.ADDED: [
                    "- Added package alpha support.",
                    "- Added package beta support.",
                    "- Added package gamma support.",
                ]
            },
            revisions=[ChangelogRevision(f"- {package_old}", "- Clarified the package behavior.")],
        ),
        ChangelogUpdate(
            path="CHANGELOG.md",
            entries={
                ChangelogCategory.ADDED: [
                    f"- {authored}",
                    "- Added a distinct synthesized root change.",
                ]
            },
            revisions=[
                ChangelogRevision(f"- {root_old}", "- Clarified the root behavior."),
                ChangelogRevision(f"- {authored}", None),
            ],
        ),
    ]

    assert set(paths) == {"CHANGELOG.md", "packages/widget/CHANGELOG.md"}
    generated = asyncio.run(prepare_synthesized_updates(prepared, updates, config))
    applied = apply_changelog_updates(prepared, generated)

    assert len(applied) == 2
    root_staged = _staged_changelog_content("CHANGELOG.md", repo)
    package_staged = _staged_changelog_content("packages/widget/CHANGELOG.md", repo)
    assert root_staged is not None and package_staged is not None
    assert root_staged.count(f"- {authored}") == 1
    assert f"- {root_old}" not in root_staged
    assert "- Clarified the root behavior." in root_staged
    assert "- Added a distinct synthesized root change." in root_staged
    assert f"- {package_old}" not in package_staged
    assert "- Clarified the package behavior." in package_staged
    assert "- Added package alpha support." in package_staged
    assert "- Added package beta support." in package_staged
    assert "- Added package gamma support." in package_staged


def test_synthesized_updates_preserve_staged_and_worktree_authored_divergence(
    repo: Path,
    run_git: RunGit,
) -> None:
    old = "Documented the original behavior."
    staged_authored = "Documented a staged author change."
    worktree_authored = "Documented an unstaged author change."
    path = repo / "CHANGELOG.md"
    path.write_text(_added_changelog(old), encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    path.write_text(_added_changelog(staged_authored, old), encoding="utf-8")
    run_git(repo, "add", ".")
    path.write_text(_added_changelog(worktree_authored, staged_authored, old), encoding="utf-8")

    config = CommitConfig()
    prepared = prepare_changelog_flow(SimpleNamespace(dir=str(repo)), config)
    (context,) = prepared_changelog_contexts(prepared)
    updates = [
        ChangelogUpdate(
            path=context.path,
            entries={
                ChangelogCategory.ADDED: [
                    f"- {worktree_authored}",
                    "- Added a synthesized behavior.",
                ]
            },
            revisions=[
                ChangelogRevision(f"- {old}", "- Clarified the original behavior."),
                ChangelogRevision(f"- {staged_authored}", None),
            ],
        )
    ]

    generated = asyncio.run(prepare_synthesized_updates(prepared, updates, config))
    apply_changelog_updates(prepared, generated)

    staged = _staged_changelog_content("CHANGELOG.md", repo)
    worktree = path.read_text(encoding="utf-8")
    assert staged is not None
    assert f"- {worktree_authored}" not in staged
    assert f"- {staged_authored}" in staged
    assert "- Clarified the original behavior." in staged
    assert "- Added a synthesized behavior." in staged
    assert f"- {worktree_authored}" in worktree
    assert f"- {staged_authored}" in worktree
    assert "- Clarified the original behavior." in worktree
    assert worktree.count("- Added a synthesized behavior.") == 1


def test_synthesized_updates_honor_disabled_revisions(repo: Path, run_git: RunGit) -> None:
    old = "Documented stable behavior."
    path = repo / "CHANGELOG.md"
    path.write_text(_added_changelog(old), encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: add changelog")
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")

    config = CommitConfig(changelog_revise=False)
    prepared = prepare_changelog_flow(SimpleNamespace(dir=str(repo)), config)
    (context,) = prepared_changelog_contexts(prepared)
    update = ChangelogUpdate(
        path=context.path,
        entries={ChangelogCategory.FIXED: ["- Fixed a separate behavior."]},
        revisions=[ChangelogRevision(f"- {old}", "- Replaced despite disabled revisions.")],
    )

    generated = asyncio.run(prepare_synthesized_updates(prepared, [update], config))
    apply_changelog_updates(prepared, generated)

    staged = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged is not None
    assert f"- {old}" in staged
    assert "Replaced despite disabled revisions." not in staged
    assert "- Fixed a separate behavior." in staged


def test_synthesized_updates_reject_malformed_boundary_coverage(repo: Path, run_git: RunGit) -> None:
    path = repo / "CHANGELOG.md"
    path.write_text(_added_changelog(), encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: add changelog")
    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    run_git(repo, "add", "app.py")

    config = CommitConfig()
    prepared = prepare_changelog_flow(SimpleNamespace(dir=str(repo)), config)
    valid = ChangelogUpdate("CHANGELOG.md", {}, [])

    with pytest.raises(ValidationFailure, match="missing changelog updates"):
        asyncio.run(prepare_synthesized_updates(prepared, [], config))
    with pytest.raises(ValidationFailure, match="unknown changelog update path"):
        asyncio.run(
            prepare_synthesized_updates(
                prepared,
                [ChangelogUpdate("unknown/CHANGELOG.md", {}, [])],
                config,
            )
        )
    with pytest.raises(ValidationFailure, match="duplicate changelog update path"):
        asyncio.run(prepare_synthesized_updates(prepared, [valid, valid], config))


def test_weaver_accepts_seeded_synthesized_context_and_preserves_authored_entries(
    repo: Path,
    run_git: RunGit,
) -> None:
    old = "Documented the original compose behavior."
    authored = "Documented an authored compose behavior."
    path = repo / "CHANGELOG.md"
    path.write_text(_added_changelog(old), encoding="utf-8")
    run_git(repo, "add", "CHANGELOG.md")
    run_git(repo, "commit", "-m", "docs: add changelog")

    (repo / "app.py").write_text("def value():\n    return 2\n", encoding="utf-8")
    path.write_text(_added_changelog(authored, old), encoding="utf-8")
    run_git(repo, "add", ".")
    target_tree = git.write_real_index_tree(repo)
    weaver = ChangelogWeaver.create(repo, CommitConfig(), target_tree)
    assert weaver is not None

    with git.TempGitIndex(repo) as index:
        git.read_tree_into_index(index.path, git.get_head_hash(repo), repo)
        weaver.seed_temp_index(repo, index.path)
        (context,) = weaver.contexts_for_group(["app.py"], repo)
        converted = weaver.updates_for_group(
            [
                ChangelogUpdate(
                    path=context.path,
                    entries={ChangelogCategory.ADDED: ["- Added synthesized compose behavior."]},
                    revisions=[
                        ChangelogRevision(f"- {old}", "- Clarified the compose behavior."),
                        ChangelogRevision(f"- {authored}", None),
                    ],
                )
            ],
            repo,
        )
        asyncio.run(weaver.weave_group(["app.py"], "", "", repo, index.path, converted))
        temporary = git.run_git(["show", ":CHANGELOG.md"], cwd=repo, index_file=index.path).stdout

    weaver.flush(repo)
    staged = _staged_changelog_content("CHANGELOG.md", repo)
    assert staged == temporary
    assert f"- {old}" not in staged
    assert "- Clarified the compose behavior." in staged
    assert staged.count(f"- {authored}") == 1
    assert "- Added synthesized compose behavior." in staged
    assert path.read_text(encoding="utf-8") == staged

    with pytest.raises(ValidationFailure, match="unknown changelog update path"):
        weaver.updates_for_group([ChangelogUpdate("unknown/CHANGELOG.md", {}, [])], repo)

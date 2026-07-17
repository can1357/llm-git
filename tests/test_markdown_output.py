from __future__ import annotations

import pytest
from lgit.markdown_output import (
    parse_changelog_response,
    parse_conventional_analysis_markdown,
    parse_fast_commit_markdown,
    parse_summary_markdown,
)


def _entries(markdown: str) -> dict[str, list[str]]:
    return parse_changelog_response(markdown)["entries"]


# ===== conventional analysis =====


def test_conventional_analysis() -> None:
    md = (
        "# feat(api): add user authentication endpoint\n\n"
        "- Added POST /auth/login endpoint\n"
        "- Implemented bcrypt password hashing\n\n"
        "Fixes: #123"
    )

    analysis = parse_conventional_analysis_markdown(md)

    assert str(analysis.commit_type) == "feat"
    assert str(analysis.scope) == "api"
    assert analysis.summary == "add user authentication endpoint"
    assert [detail.text for detail in analysis.details] == [
        "Added POST /auth/login endpoint.",
        "Implemented bcrypt password hashing.",
    ]
    assert analysis.issue_refs == ("#123",)


def test_analysis_lenient_variations() -> None:
    md = "```md\n**fix(core): corrected null deref**\n\n* fixed a crash\n* guarded the pointer\n\nCloses: #7, #8\n```"

    analysis = parse_conventional_analysis_markdown(md)

    assert str(analysis.commit_type) == "fix"
    assert str(analysis.scope) == "core"
    assert [detail.text for detail in analysis.details] == [
        "fixed a crash.",
        "guarded the pointer.",
    ]
    assert analysis.issue_refs == ("#7", "#8")


def test_analysis_no_scope_and_leading_blank_lines() -> None:
    analysis = parse_conventional_analysis_markdown("\n\n\n# chore: bumped version\n")

    assert str(analysis.commit_type) == "chore"
    assert analysis.scope is None
    assert analysis.summary == "bumped version"


def test_heading_requires_known_type_not_json_key() -> None:
    json_ish = '{\n  "type": "refactor",\n  "summary": "did things"\n}'

    analysis = parse_conventional_analysis_markdown(json_ish)

    assert str(analysis.commit_type) == "refactor"
    assert analysis.summary == "did things"
    with pytest.raises(ValueError, match="heading not found"):
        parse_conventional_analysis_markdown("summary: did a thing\nscope: core")


def test_issue_footer_not_misread_as_heading() -> None:
    with pytest.raises(ValueError, match="heading not found"):
        parse_conventional_analysis_markdown("Fixes: #123\n- did a thing")

    analysis = parse_conventional_analysis_markdown("# fix(api): corrected thing\n- patched it\nFixes: #123")

    assert str(analysis.commit_type) == "fix"
    assert analysis.issue_refs == ("#123",)


def test_noncanonical_heading_type_is_coerced() -> None:
    md = (
        "# ui: implement file management and detail view enhancements\n\n"
        "- Expanded file management capabilities within FilesPanel to support broader operations.\n"
        "- Updated user interface components to improve data presentation and interaction flow in DetailView and MetricsPanel.\n"
        "- Enhanced API communication logic to support new state requirements for modal components.\n"
        "- Adjusted kernel argument help documentation for improved clarity."
    )

    analysis = parse_conventional_analysis_markdown(md)

    assert str(analysis.commit_type) == "ux"
    assert analysis.summary == "implement file management and detail view enhancements"
    assert len(analysis.details) == 4


def test_unknown_heading_type_falls_back_to_chore() -> None:
    analysis = parse_conventional_analysis_markdown("# wibble: tweaked the knobs\n\n- Adjusted a knob.")

    assert str(analysis.commit_type) == "chore"
    assert analysis.summary == "tweaked the knobs"


def test_noncanonical_type_only_coerced_for_markdown_heading() -> None:
    with pytest.raises(ValueError, match="heading not found"):
        parse_conventional_analysis_markdown("wibble: did a thing")
    with pytest.raises(ValueError, match="heading not found"):
        parse_conventional_analysis_markdown("note: see below\nmore prose")


# ===== fast commit =====


def test_fast_commit_details_are_plain_strings() -> None:
    md = (
        "# refactor(web): derive provider order from options\n\n"
        "- Derived the metadata dynamically.\n"
        "- Reprioritized the default sequence."
    )

    commit = parse_fast_commit_markdown(md)

    assert str(commit.commit_type) == "refactor"
    assert str(commit.scope) == "web"
    assert commit.body == ("Derived the metadata dynamically.", "Reprioritized the default sequence.")
    assert all(isinstance(detail, str) for detail in commit.body)


# ===== summary: all the wrapping variations =====


@pytest.mark.parametrize(
    "markdown",
    [
        "<summary>Added JWT auth</summary>",
        "Added JWT auth",
        '"Added JWT auth"',
        '<summary>"Added JWT auth"</title>',
        "```md\n<summary>\nAdded JWT auth\n</summary>\n```",
        "Title: Added JWT auth",
        "# Added JWT auth",
        "\n\n  Added JWT auth  \n\n",
    ],
)
def test_summary_variations(markdown: str) -> None:
    assert parse_summary_markdown(markdown) == "Added JWT auth"


# ===== changelog: header + item variations =====


def test_changelog_hash_and_dash() -> None:
    entries = _entries("# Added\n- POST /auth/login endpoint\n\n# Fixed\n- Race condition")

    assert entries["Added"] == ["POST /auth/login endpoint"]
    assert entries["Fixed"] == ["Race condition"]


def test_changelog_lenient_mixed() -> None:
    md = "## Added\n- one\n* two\n\n\nFixed:\nthree\n- four\n\n# Security\n\n  five  "

    entries = _entries(md)

    assert entries["Added"] == ["one", "two"]
    assert entries["Fixed"] == ["three", "four"]
    assert entries["Security"] == ["five"]


def test_changelog_bare_category_not_confused_with_item() -> None:
    entries = _entries("# Security\n- Added rate limiting on auth endpoints")

    assert "Security" in entries
    assert "Added" not in entries
    assert entries["Security"] == ["Added rate limiting on auth endpoints"]


def test_changelog_emphasized_headers() -> None:
    md = "**Added**\n- new endpoint\n*Fixed*\n- a bug\n__Security__\n- hardening"

    entries = _entries(md)

    assert entries["Added"] == ["new endpoint"]
    assert entries["Fixed"] == ["a bug"]
    assert entries["Security"] == ["hardening"]


def test_changelog_quoted_and_hash_emphasized_headers() -> None:
    entries = _entries('"Added"\n- one\n## **Changed**\n- two')

    assert entries["Added"] == ["one"]
    assert entries["Changed"] == ["two"]


def test_changelog_inline_category_entries() -> None:
    md = "Added: a feature\n**Fixed:** a crash\n**Removed**: an old flag"

    entries = _entries(md)

    assert entries["Added"] == ["a feature"]
    assert entries["Fixed"] == ["a crash"]
    assert entries["Removed"] == ["an old flag"]


def test_changelog_breaking_changes_alias() -> None:
    md = "Breaking Changes:\n- dropped v1 API\n**Breaking Changes:** changed default"

    entries = _entries(md)

    assert entries["Breaking Changes"] == ["dropped v1 API", "changed default"]


def test_changelog_inline_does_not_eat_multiword_item() -> None:
    entries = _entries("# Changed\n- Updated behavior: now retries on 5xx")

    assert entries["Changed"] == ["Updated behavior: now retries on 5xx"]
    assert "Added" not in entries


def test_normalize_escaped_whitespace_direct() -> None:
    from lgit.markdown_output import _normalize_escaped_whitespace

    # 1. Normalizes escaped newlines and tabs
    assert _normalize_escaped_whitespace("hello \\n world") == "hello \n world"
    assert _normalize_escaped_whitespace("hello \\r\\n world") == "hello \n world"
    assert _normalize_escaped_whitespace("hello \\t world") == "hello \t world"

    # 2. Preserves literal escapes inside inline backticks
    assert _normalize_escaped_whitespace("use `\\n` as separator") == "use `\\n` as separator"
    assert _normalize_escaped_whitespace("hello \\n world `\\n` test \\n") == "hello \n world `\\n` test \n"

    # 3. Preserves literal escapes inside triple backticks (blocks)
    code_block = "```\nprint(\"\\n\")\n```"
    assert _normalize_escaped_whitespace(code_block) == code_block


def test_normalize_escaped_whitespace_indirect() -> None:
    # Test that parsing an analysis with literal \n as line separators converts them correctly
    md = (
        "# feat(catalog): increased maxTokens to 65,536 for Kimi\\n\\n"
        "- Updated models.json\\n"
        "- Adjusted compat logic\\n"
    )
    analysis = parse_conventional_analysis_markdown(md)
    assert str(analysis.commit_type) == "feat"
    assert str(analysis.scope) == "catalog"
    assert analysis.summary == "increased maxTokens to 65,536 for Kimi"
    assert [detail.text for detail in analysis.details] == [
        "Updated models.json.",
        "Adjusted compat logic.",
    ]

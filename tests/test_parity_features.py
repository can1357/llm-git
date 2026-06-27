"""Regression tests for Rust→Python parity features ported into the CLI.

Each test pins a behavior that was missing or divergent in the Python port:
extended commit-type acceptance, lossy analysis-scope coercion, the required
``user_visible`` analysis field, compose markdown fallbacks reachable in tool
mode, real shell completions, and phase-timing artifacts.
"""

from __future__ import annotations

import json

from lgit import api, cli, profile
from lgit import markdown_output as md
from lgit.config import CommitConfig
from lgit.models import ConventionalAnalysis


def test_analysis_schema_requires_user_visible() -> None:
    schema = api.build_analysis_schema(["feat", "fix"], CommitConfig())
    detail = schema["properties"]["details"]["items"]
    assert "user_visible" in detail["required"]


def test_heading_parser_accepts_extended_commit_types() -> None:
    # These registered types were previously coerced to ``chore``.
    for commit_type in ("deps", "security", "config", "hotfix", "ux", "infra"):
        parsed = md._parse_heading_line(f"{commit_type}(api): did a thing", coerce=False)
        assert parsed is not None and parsed[0] == commit_type


def test_analysis_scope_is_sanitized_not_raised() -> None:
    # Lossy coercion (Rust deserialize_optional_scope): messy scope sanitizes,
    # unusable scope drops to None — neither raises.
    assert str(ConventionalAnalysis(commit_type="feat", scope="Weird Scope!!").scope) == "weird-scope"
    assert ConventionalAnalysis(commit_type="feat", scope="!!!").scope is None


def test_compose_intent_markdown_fallback_parses_groups() -> None:
    plan = md.parse_compose_intent_markdown("G1 := feat(api): add login\n\nfiles:\n- G1: src/auth.py")
    groups = plan.get("groups")
    assert groups and groups[0]["file_ids"] == ["src/auth.py"]


def test_compose_markdown_fallback_reachable_in_tool_mode() -> None:
    # markdown_mode=False must still fall back to markdown parsing when the
    # model emits prose instead of a tool call.
    parsed = api._parse_plain_text(
        "create_compose_intent_plan", "G1 := feat(api): add login\n\nfiles:\n- G1: src/auth.py", False
    )
    groups = parsed.get("groups") if isinstance(parsed, dict) else None
    assert groups and groups[0]["file_ids"] == ["src/auth.py"]


def test_completion_scripts_are_real() -> None:
    for shell in ("bash", "zsh", "fish"):
        script = cli._completion_script(shell)
        assert "lgit" in script
        assert "stub" not in script.lower() and "placeholder" not in script.lower()


def test_timing_collector_writes_timings_json(tmp_path) -> None:
    collector = profile.create_timing_collector(True)
    profile.record_timing(collector, "analysis", 0.5)
    profile.record_timing(collector, "summary", 0.25)
    assert profile.format_timing_report(collector).strip()
    out = tmp_path / "timings.json"
    profile.write_timings_json(out, collector)
    data = json.loads(out.read_text())
    phases = data.get("phases", data if isinstance(data, list) else [])
    assert len(phases) == 2

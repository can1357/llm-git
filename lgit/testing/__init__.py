"""Fixture-based testing harness for lgit."""

from __future__ import annotations

from pathlib import Path

from .compare import CompareResult, compare_analysis
from .fixture import (
    FIXTURES_DIR,
    Fixture,
    FixtureContext,
    FixtureEntry,
    FixtureInput,
    FixtureMeta,
    Golden,
    Manifest,
    add_fixture,
    discover_fixtures,
    load_fixtures,
)
from .report import generate_html_report
from .runner import RunResult, TestRunner, TestSummary, run_test_mode


def fixtures_dir() -> Path:
    """Return the default fixtures directory path."""

    return Path(FIXTURES_DIR)


def list_fixtures(path: str | Path | None = None) -> list[str]:
    """List available fixtures from the manifest when present, else discover directories."""

    root = fixtures_dir() if path is None else Path(path)
    manifest = Manifest.load(root)
    if manifest.fixtures:
        return sorted(manifest.fixtures)
    return discover_fixtures(root)


__all__ = [
    "CompareResult",
    "FIXTURES_DIR",
    "Fixture",
    "FixtureContext",
    "FixtureEntry",
    "FixtureInput",
    "FixtureMeta",
    "Golden",
    "Manifest",
    "RunResult",
    "TestRunner",
    "TestSummary",
    "add_fixture",
    "compare_analysis",
    "discover_fixtures",
    "fixtures_dir",
    "generate_html_report",
    "list_fixtures",
    "load_fixtures",
    "run_test_mode",
]

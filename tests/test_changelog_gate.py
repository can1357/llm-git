"""Live quality gate: baseline (diff-prompt) vs new (observation-prompt) changelog generation.

Opt-in: requires a reachable LLM proxy and an explicit env flag. Runs a matrix of
fixture changesets x scenarios x models against two lgit versions:

- OLD: the baseline git worktree (``LLM_GIT_GATE_BASELINE``, default
  ``/work/.tree/llm-git-gate``) whose changelog prompt always receives the raw diff.
- NEW: this working tree, whose changelog prompt receives map-phase per-file
  observations for large-diff reuse.

Both sides execute in subprocesses through ``tests/gate/changelog_driver.py`` with
``PYTHONPATH`` selecting the version, so the baseline runs its own verbatim code.
Responses are scored against the changelog policies (categories, entry format,
past tense, exception discipline, revise/authored rules) and the gate asserts the
new pipeline does not regress against the old one per model.

Run:
    LLM_GIT_QUALITY_GATE=1 uv run pytest tests/test_changelog_gate.py -s
"""

from __future__ import annotations

import asyncio
import json
import os
import pwd
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import pytest
from lgit.changelog import _entry_words, _observations_markdown, parse_changelog_revisions
from lgit.config import CommitConfig
from lgit.diffing import parse_diff, reconstruct_diff
from lgit.map_reduce import FileObservation, observe_diff_files
from lgit.validation import is_past_tense_first_word

pytestmark = pytest.mark.skipif(
    not os.environ.get("LLM_GIT_QUALITY_GATE"),
    reason="live LLM quality gate; set LLM_GIT_QUALITY_GATE=1 to run",
)

PROJECT_ROOT = Path(__file__).resolve().parents[1]
FIXTURES_DIR = PROJECT_ROOT / "tests" / "fixtures"
DRIVER = PROJECT_ROOT / "tests" / "gate" / "changelog_driver.py"
BASELINE_TREE = Path(os.environ.get("LLM_GIT_GATE_BASELINE", "/work/.tree/llm-git-gate"))

MODELS = (
    "openrouter/openai/gpt-oss-120b",
    "openrouter/google/gemini-3.5-flash-lite",
    "openrouter/minimax/minimax-m2.7",
)
MAP_MODEL = "openrouter/google/gemini-3.5-flash-lite"

KNOWN_CATEGORIES = {"added", "changed", "fixed", "deprecated", "removed", "security", "breaking"}

EDIT_MODE_FIXTURE = "pi-cae5aaa3c-feat-coding-agent-changed-default-edit-m"
EDIT_MODE_AUTHORED = (
    "# Changed\n"
    "- Changed default edit mode from `patch` to `hashline` for more precise code modifications\n"
    "- Changed `readHashLines` setting default from false to true to enable hash line reading by default"
)
EDIT_MODE_EXISTING = (
    "# Added\n"
    "- Added `getAllServerNames()` method to MCPManager for enumerating all known servers\n"
    "# Changed\n"
    "- Changed default edit mode from `patch` to `replace` for safer code modifications"
)


@dataclass(frozen=True, slots=True)
class GateCase:
    """One fixture/scenario cell of the gate matrix."""

    fixture: str
    scenario: str
    changelog_path: str
    is_package: bool
    expect: str  # "entries" | "exception"
    existing_entries: str | None = None
    authored_entries: str | None = None
    can_revise: bool = False


CASES = (
    GateCase(
        "pi-030b81262-feat-ai-implemented-oauth-authentication", "plain", "packages/ai/CHANGELOG.md", True, "entries"
    ),
    GateCase(
        "pi-f8a4c4021-fix-ai-corrected-retry-logic-to-detect-c", "plain", "packages/ai/CHANGELOG.md", True, "entries"
    ),
    GateCase(EDIT_MODE_FIXTURE, "plain", "packages/coding-agent/CHANGELOG.md", True, "entries"),
    GateCase(
        "pi-96d676a5e-refactor-coding-agent-migrated-tool-rend",
        "plain",
        "packages/coding-agent/CHANGELOG.md",
        True,
        "exception",
    ),
    GateCase("pi-a13d20b5a-chore-bump-version-to-3-9-1337", "plain", "CHANGELOG.md", False, "exception"),
    GateCase(
        EDIT_MODE_FIXTURE,
        "authored-covered",
        "packages/coding-agent/CHANGELOG.md",
        True,
        "exception",  # The authored entries fully describe the diff; policy demands the exception tag.
        authored_entries=EDIT_MODE_AUTHORED,
    ),
    GateCase(
        EDIT_MODE_FIXTURE,
        "revise-stale",
        "packages/coding-agent/CHANGELOG.md",
        True,
        "entries",
        existing_entries=EDIT_MODE_EXISTING,
        can_revise=True,
    ),
)


@dataclass(slots=True)
class Scored:
    """Scored response for one job on one side."""

    case: GateCase
    model: str
    error: str | None
    violations: list[str] = field(default_factory=list)


def _real_home() -> Path:
    return Path(pwd.getpwuid(os.getuid()).pw_dir)


def _load_real_config() -> CommitConfig:
    explicit = os.environ.get("LLM_GIT_CONFIG")
    if explicit:
        return CommitConfig.from_file(explicit)
    default = _real_home() / ".config" / "llm-git" / "config.toml"
    return CommitConfig.from_file(default) if default.exists() else CommitConfig.load()


def _strip_changelogs(diff: str) -> str:
    files = [f for f in parse_diff(diff) if Path(f.filename).name.lower() != "changelog.md"]
    return reconstruct_diff(files)


def _fixture_inputs(name: str) -> tuple[str, str]:
    base = FIXTURES_DIR / name / "input"
    diff = _strip_changelogs((base / "diff.patch").read_text(encoding="utf-8"))
    stat = "\n".join(
        line for line in (base / "stat.txt").read_text(encoding="utf-8").splitlines() if "CHANGELOG.md" not in line
    )
    return diff, stat


def _job_id(case: GateCase, model: str) -> str:
    return f"{case.fixture}|{case.scenario}|{model}"


def _first_token(entry: str) -> str:
    match = re.match(r"[A-Za-z]+", entry)
    return match.group(0) if match else ""


def _overlap(entry: str, reference: str) -> float:
    words = _entry_words(entry)
    ref_words = _entry_words(reference)
    if not words or not ref_words:
        return 0.0
    return len(words & ref_words) / len(words)


def _reference_bullets(block: str | None) -> list[str]:
    if not block:
        return []
    return [line.strip()[2:].strip() for line in block.splitlines() if line.strip().startswith("- ")]


def _score(case: GateCase, text: str) -> list[str]:
    violations: list[str] = []
    revise_match = re.search(r"<revise\b[^>]*>(.*?)(?:</revise>|$)", text, re.IGNORECASE | re.DOTALL)
    body = text[: revise_match.start()] + text[revise_match.end() :] if revise_match else text
    has_exception = re.search(r"<exception>", body, re.IGNORECASE) is not None
    body = re.sub(r"<exception>.*?(?:</exception>|$)", "", body, flags=re.IGNORECASE | re.DOTALL)

    if revise_match and not case.can_revise:
        violations.append("emitted <revise> without permission")

    entries: list[str] = []
    current: str | None = None
    for raw_line in body.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        heading = re.match(r"^#{1,6}\s+(.+?)\s*:?\s*$", line)
        if heading:
            name = heading.group(1).strip().strip("`")
            current = name.lower()
            if current not in KNOWN_CATEGORIES:
                violations.append(f"unknown section heading {name!r}")
                current = None
            continue
        if line.startswith(("- ", "* ")):
            entry = line[2:].strip()
            if current is None:
                violations.append(f"bullet outside a known category: {entry[:60]!r}")
                continue
            entries.append(entry)
            if len(entry) > 100:
                violations.append(f"entry over 100 chars ({len(entry)}): {entry[:60]!r}")
            if entry.endswith("."):
                violations.append(f"entry has trailing period: {entry[:60]!r}")
            token = _first_token(entry)
            if token and not is_past_tense_first_word(token.lower()):
                violations.append(f"entry not past-tense ({token!r}): {entry[:60]!r}")

    if not entries and not has_exception and not (revise_match and case.can_revise):
        violations.append("neither category entries nor <exception> returned")
    if case.expect == "exception" and entries:
        violations.append(f"returned {len(entries)} entries for a change that is not user-visible")
    if case.expect == "entries" and not entries and not (revise_match and case.can_revise):
        violations.append("missed user-visible changes (exception-only response)")

    for reference in _reference_bullets(case.authored_entries):
        for entry in entries:
            if _overlap(entry, reference) >= 0.7:
                violations.append(f"restated authored entry: {entry[:60]!r}")

    existing = _reference_bullets(case.existing_entries)
    for reference in existing:
        for entry in entries:
            if _overlap(entry, reference) >= 0.7:
                violations.append(f"restated existing entry instead of revising: {entry[:60]!r}")
    if revise_match:
        for revision in parse_changelog_revisions(text):
            if revision.old.removeprefix("- ").strip() not in {e.strip() for e in existing}:
                violations.append(f"revise OLD not verbatim from existing entries: {revision.old[:60]!r}")

    return violations


def _config_path() -> Path:
    explicit = os.environ.get("LLM_GIT_CONFIG")
    if explicit:
        return Path(explicit)
    return _real_home() / ".config" / "llm-git" / "config.toml"


def _run_driver(tree: Path, jobs: list[dict[str, Any]], tmp_path: Path, tag: str) -> dict[str, Any]:
    jobs_file = tmp_path / f"jobs-{tag}.json"
    jobs_file.write_text(json.dumps({"jobs": jobs}), encoding="utf-8")
    # Isolated HOME: user prompt overrides in ~/.llm-git/prompts/ would silently
    # pin BOTH sides to the same template and make the comparison vacuous.
    fake_home = tmp_path / f"home-{tag}"
    fake_home.mkdir(exist_ok=True)
    env = {
        **os.environ,
        "HOME": str(fake_home),
        "LLM_GIT_CONFIG": str(_config_path()),
        "PYTHONPATH": str(tree),
        "LLM_GIT_CACHE_DISABLED": "1",
    }
    proc = subprocess.run(
        [sys.executable, str(DRIVER), str(jobs_file)],
        capture_output=True,
        text=True,
        env=env,
        timeout=1800,
    )
    assert proc.returncode == 0, f"{tag} driver failed:\n{proc.stdout}\n{proc.stderr}"
    return json.loads(proc.stdout)


async def _collect_observations(config: CommitConfig, diffs: dict[str, str]) -> dict[str, list[FileObservation]]:
    async def observe(name: str, diff: str) -> tuple[str, list[FileObservation]]:
        try:
            return name, await observe_diff_files(diff, MAP_MODEL, config)
        except ValueError:
            # Nothing mappable (lockfile/binary-only change): production would not
            # run map-reduce either, so the new side falls back to the diff prompt.
            return name, []

    return dict(await asyncio.gather(*(observe(name, diff) for name, diff in diffs.items())))


def test_changelog_quality_gate(tmp_path: Path) -> None:
    assert BASELINE_TREE.is_dir(), f"baseline worktree missing: {BASELINE_TREE}"
    config = _load_real_config()

    fixture_inputs = {case.fixture: _fixture_inputs(case.fixture) for case in CASES}
    observations = asyncio.run(
        _collect_observations(config, {name: diff for name, (diff, _stat) in fixture_inputs.items()})
    )

    jobs: list[dict[str, Any]] = []
    for case in CASES:
        diff, stat = fixture_inputs[case.fixture]
        fixture_observations = observations[case.fixture]
        files = {item.file for item in fixture_observations}
        for model in MODELS:
            jobs.append(
                {
                    "id": _job_id(case, model),
                    "model": model,
                    "changelog_path": case.changelog_path,
                    "is_package": case.is_package,
                    "stat": stat,
                    "diff": diff,
                    "existing_entries": case.existing_entries,
                    "authored_entries": case.authored_entries,
                    "can_revise": case.can_revise,
                    "observations": _observations_markdown(fixture_observations, files),
                }
            )

    old_payload = _run_driver(BASELINE_TREE, jobs, tmp_path, "old")
    new_payload = _run_driver(PROJECT_ROOT, jobs, tmp_path, "new")
    assert not old_payload["supports_observations"], f"baseline unexpectedly new: {old_payload['lgit_file']}"
    assert new_payload["supports_observations"], f"new side resolved wrong tree: {new_payload['lgit_file']}"

    # Loud vacuity guards: the two sides MUST have prompted differently.
    observed_jobs = {job["id"] for job in jobs if job["observations"]}
    assert len(observed_jobs) >= len(MODELS) * 3, "map phase produced observations for too few fixtures"
    for row in old_payload["results"]:
        assert "<diff>" in row["user_prompt"], f"old prompt missing diff: {row['id']}"
        assert "<file_change_summaries>" not in row["user_prompt"], f"old prompt got observations: {row['id']}"
    for row in new_payload["results"]:
        if row["id"] in observed_jobs:
            assert "<file_change_summaries>" in row["user_prompt"], f"new prompt missing observations: {row['id']}"
            assert "<diff>" not in row["user_prompt"], f"new prompt still carries diff: {row['id']}"

    case_by_id = {_job_id(case, model): (case, model) for case in CASES for model in MODELS}

    def scored(payload: dict[str, Any]) -> list[Scored]:
        results = []
        for row in payload["results"]:
            case, model = case_by_id[row["id"]]
            if row["error"]:
                results.append(Scored(case, model, row["error"]))
            else:
                results.append(Scored(case, model, None, _score(case, row["text"])))
        return results

    old_scores, new_scores = scored(old_payload), scored(new_payload)

    print("\n=== changelog quality gate (old=diff prompt, new=observation prompt) ===")
    failures: list[str] = []
    for model in MODELS:
        rows = {
            "old": [s for s in old_scores if s.model == model],
            "new": [s for s in new_scores if s.model == model],
        }
        totals = {}
        for side, side_scores in rows.items():
            errors = sum(1 for s in side_scores if s.error)
            violations = sum(len(s.violations) for s in side_scores)
            totals[side] = (errors, violations)
            print(f"\n[{model}] {side}: {len(side_scores)} calls, {errors} errors, {violations} violations")
            for s in side_scores:
                label = f"  {s.case.fixture.split('-', 2)[-1][:36]}/{s.case.scenario}"
                if s.error:
                    print(f"{label}: ERROR {s.error}")
                for violation in s.violations:
                    print(f"{label}: {violation}")
        breakdown = Counter(v.split(":")[0].split("(")[0] for s in rows["new"] for v in s.violations)
        if breakdown:
            print(f"  new-side breakdown: {dict(breakdown)}")
        old_errors, old_violations = totals["old"]
        new_errors, new_violations = totals["new"]
        if new_errors > old_errors:
            failures.append(f"{model}: transport/parse errors regressed {old_errors} -> {new_errors}")
        if new_violations > old_violations:
            failures.append(f"{model}: policy violations regressed {old_violations} -> {new_violations}")

    assert not failures, "quality gate regressions:\n" + "\n".join(failures)

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import lgit.synthesis as synthesis
import pytest
from lgit.config import CommitConfig
from lgit.map_reduce import FileObservation
from lgit.models import ChangelogContext


def _boundary(
    path: str = "CHANGELOG.md",
    *,
    files: tuple[str, ...] = ("src/api.py",),
    can_revise: bool = False,
    existing_entries: str | None = None,
) -> ChangelogContext:
    return ChangelogContext(
        path=path,
        files=files,
        is_package=path != "CHANGELOG.md",
        existing_entries=existing_entries,
        authored_entries=None,
        can_revise=can_revise,
    )


def _payload(*paths: str, summary: str = "added shared evidence") -> dict[str, Any]:
    return {
        "commit": {
            "type": "feat",
            "scope": "api",
            "summary": summary,
            "details": ["Added shared evidence."],
            "issue_refs": [],
        },
        "changelogs": [
            {"path": path, "entries": {"Added": [f"Added support for {path}"]}, "revisions": []} for path in paths
        ],
    }


def _generate(
    monkeypatch: pytest.MonkeyPatch,
    payload: Any,
    boundaries: tuple[ChangelogContext, ...],
    *,
    config: CommitConfig | None = None,
    model_name: str | None = None,
) -> tuple[Any, list[Any]]:
    calls: list[Any] = []

    async def fake_run_oneshot(call_config: CommitConfig, spec: Any) -> Any:
        del call_config
        calls.append(spec)
        return payload

    monkeypatch.setattr(synthesis.api, "run_oneshot", fake_run_oneshot)
    effective_config = config or CommitConfig(map_reduce_enabled=False, cache_enabled=False)
    result = asyncio.run(
        synthesis.generate_commit_evidence(
            effective_config,
            "src/api.py | 2 ++",
            "diff --git a/src/api.py b/src/api.py\n+feature\n",
            boundaries,
            model_name=model_name,
        )
    )
    return result, calls


@pytest.mark.parametrize(
    "updates,match",
    [
        ([], "missing changelog update paths"),
        (
            [
                {"path": "CHANGELOG.md", "entries": {}, "revisions": []},
                {"path": "CHANGELOG.md", "entries": {}, "revisions": []},
            ],
            "duplicate changelog update path",
        ),
        ([{"path": "elsewhere.md", "entries": {}, "revisions": []}], "unknown changelog update path"),
    ],
)
def test_generate_rejects_missing_duplicate_and_outside_boundaries(
    monkeypatch: pytest.MonkeyPatch,
    updates: list[dict[str, Any]],
    match: str,
) -> None:
    payload = _payload()
    payload["changelogs"] = updates

    with pytest.raises(ValueError, match=match):
        _generate(monkeypatch, payload, (_boundary(),))


@pytest.mark.parametrize(
    "boundary,update,match",
    [
        (
            _boundary(),
            {"path": "CHANGELOG.md", "entries": {"Unexpected": ["Changed behavior"]}, "revisions": []},
            "unknown changelog category",
        ),
        (
            _boundary(),
            {
                "path": "CHANGELOG.md",
                "entries": {},
                "revisions": [{"old": "- Added old behavior", "new": None}],
            },
            "revisions are disabled",
        ),
        (
            _boundary(can_revise=True, existing_entries="### Added\n- Added old behavior"),
            {
                "path": "CHANGELOG.md",
                "entries": {},
                "revisions": [{"old": "Added old behavior", "new": "- Added final behavior"}],
            },
            "not an exact existing entry",
        ),
        (
            _boundary(can_revise=True, existing_entries="### Added\n- Added old behavior"),
            {
                "path": "CHANGELOG.md",
                "entries": {},
                "revisions": [{"old": "- Added old behavior", "new": 3}],
            },
            "replacement must be a string or null",
        ),
    ],
)
def test_generate_rejects_invalid_categories_and_revisions(
    monkeypatch: pytest.MonkeyPatch,
    boundary: ChangelogContext,
    update: dict[str, Any],
    match: str,
) -> None:
    payload = _payload()
    payload["changelogs"] = [update]

    with pytest.raises(ValueError, match=match):
        _generate(monkeypatch, payload, (boundary,))


def test_model_and_prompt_override_precedence(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    prompt_dir = tmp_path / "prompts"
    prompt_dir.mkdir()
    (prompt_dir / "analysis.md").write_text("legacy analysis", encoding="utf-8")
    config = CommitConfig(prompts_dir=str(prompt_dir), map_reduce_enabled=False, cache_enabled=False)

    assert synthesis.can_synthesize(config) is False
    assert synthesis.can_synthesize(config, fast=True) is True

    (prompt_dir / "fast.md").write_text("legacy fast", encoding="utf-8")
    assert synthesis.can_synthesize(config, fast=True) is False

    (prompt_dir / "synthesis.md").write_text(
        "custom synthesis system\n<!-- USER -->\ncustom {{ stat }}",
        encoding="utf-8",
    )
    assert synthesis.can_synthesize(config) is True
    assert synthesis.can_synthesize(config, fast=True) is True

    _, calls = _generate(
        monkeypatch,
        _payload("CHANGELOG.md"),
        (_boundary(),),
        config=config,
        model_name="lite",
    )
    (spec,) = calls
    assert spec.model == "gemini-3.1-flash-lite"
    assert spec.system_prompt == "custom synthesis system"
    assert "custom src/api.py | 2 ++" in spec.user_prompt


def test_map_strategy_sends_complete_observations_without_raw_diff(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    observed_models: list[str] = []

    monkeypatch.setattr(synthesis, "should_use_map_reduce", lambda diff, config, counter: True)

    async def fake_observe(diff: str, model: str, config: CommitConfig, counter: Any) -> list[FileObservation]:
        del diff, config, counter
        observed_models.append(model)
        return [
            FileObservation("src/api.py", ("Added retry fallback.",), 8, 2),
            FileObservation("tests/test_api.py", ("Asserted retry precedence.",), 5, 0),
        ]

    monkeypatch.setattr(synthesis, "observe_diff_files", fake_observe)
    config = CommitConfig(
        analysis_model="claude-opus-4.8",
        map_model="haiku",
        map_reduce_enabled=True,
        cache_enabled=False,
    )
    _, calls = _generate(
        monkeypatch,
        _payload("CHANGELOG.md"),
        (_boundary(files=("src/api.py", "tests/test_api.py")),),
        config=config,
    )

    assert observed_models == ["claude-haiku-4-5"]
    (spec,) = calls
    assert spec.model == "claude-opus-4.8"
    assert "# src/api.py (+8/-2)" in spec.user_prompt
    assert "# tests/test_api.py (+5/-0)" in spec.user_prompt
    assert "Added retry fallback." in spec.user_prompt
    assert "Asserted retry precedence." in spec.user_prompt
    assert "diff --git" not in spec.user_prompt

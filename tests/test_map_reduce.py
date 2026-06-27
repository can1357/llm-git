from __future__ import annotations

import asyncio
from typing import Any

import lgit.map_reduce as map_reduce_module
import pytest
from lgit.config import CommitConfig
from lgit.diffing import FileDiff
from lgit.models import ConventionalAnalysis
from lgit.tokens import TokenCounter


def _test_counter() -> TokenCounter:
    return TokenCounter.new("http://localhost:4000", None, "claude-sonnet-4.6")


def _file_with_tokens(filename: str, token_estimate: int) -> FileDiff:
    return FileDiff(
        filename=filename,
        header="",
        content="x" * (token_estimate * 4),
        additions=0,
        deletions=0,
        is_binary=False,
    )


def _diff_for_files(paths: list[str], payload: str = "a") -> str:
    return "".join(f"diff --git a/{path} b/{path}\n@@ -0,0 +1 @@\n+{payload}\n" for path in paths)


def test_map_phase_model_uses_summary_model(monkeypatch: pytest.MonkeyPatch) -> None:
    config = CommitConfig(
        summary_model="claude-haiku-4-5",
        analysis_model="claude-opus-4.1",
        cache_enabled=False,
    )
    captured: dict[str, str] = {}

    async def fake_observe_diff_files(
        diff: str,
        map_model_name: str,
        config: CommitConfig,
        counter: Any | None = None,
    ) -> list[map_reduce_module.FileObservation]:
        del diff, config, counter
        captured["map_model_name"] = map_model_name
        return [map_reduce_module.FileObservation("src/lib.rs", ("updated library",))]

    async def fake_reduce_phase(
        observations: list[map_reduce_module.FileObservation],
        stat: str,
        scope_candidates: str,
        model_name: str,
        config: CommitConfig,
    ) -> ConventionalAnalysis:
        del observations, stat, scope_candidates, config
        captured["reduce_model_name"] = model_name
        return ConventionalAnalysis(commit_type="chore", summary="updated library")

    monkeypatch.setattr(map_reduce_module, "observe_diff_files", fake_observe_diff_files)
    monkeypatch.setattr(map_reduce_module, "reduce_phase", fake_reduce_phase)

    result = asyncio.run(map_reduce_module.run_map_reduce(config, "stat", "diff"))

    assert result.commit_type == "chore"
    assert captured == {
        "map_model_name": "claude-haiku-4-5",
        "reduce_model_name": "claude-opus-4.1",
    }
    assert map_reduce_module.MAP_PHASE_CONCURRENCY == 16


def test_build_file_batches_single_batch_when_under_budget() -> None:
    files = [_file_with_tokens("a.rs", 4), _file_with_tokens("b.rs", 4), _file_with_tokens("c.rs", 1)]

    assert map_reduce_module.build_file_batches(files, _test_counter(), 10) == [[0, 1, 2]]


def test_build_file_batches_splits_when_budget_exceeded() -> None:
    files = [_file_with_tokens("a.rs", 4), _file_with_tokens("b.rs", 4), _file_with_tokens("c.rs", 4)]

    assert map_reduce_module.build_file_batches(files, _test_counter(), 10) == [[0, 1], [2]]


def test_build_file_batches_preserves_order_and_every_file_once() -> None:
    files = [
        _file_with_tokens("a.rs", 3),
        _file_with_tokens("b.rs", 8),
        _file_with_tokens("c.rs", 2),
        _file_with_tokens("d.rs", 9),
        _file_with_tokens("e.rs", 1),
    ]

    batches = map_reduce_module.build_file_batches(files, _test_counter(), 10)

    assert [idx for batch in batches for idx in batch] == [0, 1, 2, 3, 4]


def test_build_file_batches_isolates_oversized_files() -> None:
    files = [
        _file_with_tokens("a.rs", 2),
        _file_with_tokens("b.rs", 2),
        _file_with_tokens("huge.rs", 12),
        _file_with_tokens("c.rs", 2),
    ]

    assert map_reduce_module.build_file_batches(files, _test_counter(), 10) == [[0, 1], [2], [3]]


def test_batch_response_mapping_matches_paths_and_falls_back_for_omissions() -> None:
    files = [
        _file_with_tokens("src/lib.rs", 1),
        _file_with_tokens("src/main.rs", 1),
        _file_with_tokens("crates/core/mod.rs", 1),
    ]
    response = {
        "files": [
            {"path": "src/lib.rs", "observations": ["updated library entrypoint"]},
            {"path": "main.rs", "observations": ["changed CLI wiring"]},
        ]
    }

    result = map_reduce_module._map_batch_response_to_observations(files, response, None, None)

    assert result[0].file == "src/lib.rs"
    assert result[0].observations == ("updated library entrypoint",)
    assert result[1].file == "src/main.rs"
    assert result[1].observations == ("changed CLI wiring",)
    assert result[2].file == "crates/core/mod.rs"
    assert result[2].observations == ("Updated mod.rs.",)


def test_batch_response_mapping_falls_back_for_text_only_response() -> None:
    files = [_file_with_tokens("src/lib.rs", 1), _file_with_tokens("src/main.rs", 1)]

    result = map_reduce_module._map_batch_response_to_observations(
        files,
        {"files": []},
        "- unstructured observation",
        None,
    )

    assert result[0].observations == ("Updated lib.rs.",)
    assert result[1].observations == ("Updated main.rs.",)


def test_should_use_map_reduce_disabled() -> None:
    config = CommitConfig(map_reduce_enabled=False)
    diff = _diff_for_files(["a.rs", "b.rs", "c.rs", "d.rs"])

    assert not map_reduce_module.should_use_map_reduce(diff, config, _test_counter())


def test_should_use_map_reduce_few_files() -> None:
    diff = _diff_for_files(["a.rs", "b.rs"])

    assert not map_reduce_module.should_use_map_reduce(diff, CommitConfig(), _test_counter())


def test_should_use_map_reduce_many_tiny_files_below_threshold() -> None:
    config = CommitConfig(map_reduce_threshold=1_000)
    diff = _diff_for_files(["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"])

    assert not map_reduce_module.should_use_map_reduce(diff, config, _test_counter())


def test_should_use_map_reduce_large_total_diff() -> None:
    config = CommitConfig(map_reduce_threshold=20)
    diff = _diff_for_files(["a.rs"], "a" * 200)

    assert map_reduce_module.should_use_map_reduce(diff, config, _test_counter())


def test_should_use_map_reduce_single_oversized_file() -> None:
    config = CommitConfig(map_reduce_threshold=10**12)
    diff = _diff_for_files(["a.rs"], "a" * ((map_reduce_module.MAX_FILE_TOKENS + 1) * 4))

    assert map_reduce_module.should_use_map_reduce(diff, config, _test_counter())


def test_generate_context_header_empty() -> None:
    files = [FileDiff("only.rs", "", "", additions=10, deletions=5, is_binary=False)]
    context_headers = map_reduce_module._ContextHeaders(files)

    assert context_headers.header_for_files(["only.rs"]) == ""


def test_generate_context_header_multiple() -> None:
    files = [
        FileDiff("src/main.rs", "", "fn main() {}", additions=10, deletions=5, is_binary=False),
        FileDiff("src/lib.rs", "", "mod test;", additions=3, deletions=1, is_binary=False),
        FileDiff("tests/test.rs", "", "#[test]", additions=20, deletions=0, is_binary=False),
    ]
    context_headers = map_reduce_module._ContextHeaders(files)

    header = context_headers.header_for_files(["src/main.rs"])

    assert "OTHER FILES IN THIS CHANGE:" in header
    assert "src/lib.rs" in header
    assert "tests/test.rs" in header
    assert "src/main.rs" not in header


def test_infer_file_description() -> None:
    assert map_reduce_module._infer_file_description("src/test_utils.rs", "") == "test file"
    assert map_reduce_module._infer_file_description("README.md", "") == "documentation"
    assert map_reduce_module._infer_file_description("prompts/analysis/default.md", "") == "prompt template"
    assert map_reduce_module._infer_file_description("system/analysis/default.md", "") == "prompt template"
    assert map_reduce_module._infer_file_description("config.toml", "") == "configuration"
    assert map_reduce_module._infer_file_description("src/error.rs", "") == "error definitions"
    assert map_reduce_module._infer_file_description("src/types.rs", "") == "type definitions"
    assert map_reduce_module._infer_file_description("src/mod.rs", "") == "module exports"
    assert map_reduce_module._infer_file_description("src/main.rs", "") == "entry point"
    assert map_reduce_module._infer_file_description("src/api.rs", "fn call()") == "implementation"
    assert map_reduce_module._infer_file_description("src/models.rs", "struct Foo") == "type definitions"
    assert map_reduce_module._infer_file_description("src/unknown.xyz", "") == "source code"


def test_parse_string_to_observations_json_array() -> None:
    assert map_reduce_module._parse_observations('["item one", "item two", "item three"]') == [
        "item one",
        "item two",
        "item three",
    ]


def test_parse_string_to_observations_bullet_points() -> None:
    input_text = "- added new function\n- fixed bug in parser\n- updated tests"

    assert map_reduce_module._parse_observations(input_text) == [
        "added new function",
        "fixed bug in parser",
        "updated tests",
    ]

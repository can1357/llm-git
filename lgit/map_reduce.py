"""Map-reduce analysis for large git diffs."""

from __future__ import annotations

import asyncio
import json
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .api import (
    OneShotSpec,
    build_analysis_schema,
    format_types_description,
    render_prompt,
    run_oneshot,
    strict_json_schema,
)
from .diffing import FileDiff, parse_diff, reconstruct_diff
from .markdown_output import analysis_from_mapping, fallback_summary, parse_conventional_analysis_markdown
from .models import AnalysisDetail, ConventionalAnalysis, resolve_model_name
from .tokens import create_token_counter

MAX_FILE_TOKENS = 50_000
MAP_PHASE_CONCURRENCY = 16
MAX_CONTEXT_FILES = 20


@dataclass(frozen=True, slots=True)
class FileObservation:
    """Factual observations extracted for one changed file."""

    file: str
    observations: tuple[str, ...]
    additions: int = 0
    deletions: int = 0


def should_use_map_reduce(diff: str, config: Any, counter: Any | None = None) -> bool:
    """Return whether ``diff`` should be analyzed with map-reduce."""

    if not bool(getattr(config, "map_reduce_enabled", True)):
        return False
    counter = counter or create_token_counter(config)
    total_tokens = 0
    has_included_file = False
    for file in _included_files(parse_diff(diff), config):
        has_included_file = True
        file_tokens = file.token_estimate(counter)
        if file_tokens > MAX_FILE_TOKENS:
            return True
        total_tokens += file_tokens
        if total_tokens >= int(getattr(config, "map_reduce_threshold", 5000)):
            return True
    return has_included_file and total_tokens >= int(getattr(config, "map_reduce_threshold", 5000))


def build_file_batches(files: Sequence[FileDiff], counter: Any, budget: int) -> list[list[int]]:
    """Group file indices into token-budgeted map batches."""

    return _build_file_batches_for_indices(files, range(len(files)), counter, budget)


def build_llm_file_batches(files: Sequence[FileDiff], counter: Any, budget: int) -> list[list[int]]:
    """Group non-binary files into token-budgeted LLM batches."""

    indices = [idx for idx, file in enumerate(files) if not file.is_binary]
    return _build_file_batches_for_indices(files, indices, counter, budget)


async def observe_diff_files(
    diff: str, map_model_name: str, config: Any, counter: Any | None = None
) -> list[FileObservation]:
    """Run the map phase and return per-file observations."""

    counter = counter or create_token_counter(config)
    files = _included_files(parse_diff(diff), config)
    if not files:
        raise ValueError("No relevant files to summarize after filtering")
    return await _map_phase(files, map_model_name, config, counter)


async def reduce_phase(
    observations: Sequence[FileObservation], stat: str, scope_candidates: str, model_name: str, config: Any
) -> ConventionalAnalysis:
    """Synthesize map observations into final conventional analysis."""

    type_enum = list(getattr(config, "types", {}) or {"chore": None})
    observations_json = json.dumps(
        [_observation_to_mapping(item) for item in observations], ensure_ascii=False, indent=2
    )
    variant = "markdown" if bool(getattr(config, "markdown_output", True)) else "default"
    system_prompt, user_prompt = _render_reduce_prompt(
        variant,
        observations_json,
        stat,
        scope_candidates,
        format_types_description(config),
    )
    response = await run_oneshot(
        config,
        OneShotSpec(
            operation="map-reduce/reduce",
            model=resolve_model_name(model_name),
            prompt_family="reduce",
            prompt_variant=variant,
            system_prompt=system_prompt,
            user_prompt=user_prompt,
            tool_name="create_conventional_analysis",
            tool_description="Analyze file observations and classify as a conventional commit",
            schema=build_analysis_schema(type_enum, config),
            progress_label="reduce file observations",
            cacheable=True,
        ),
    )
    output = response.output if hasattr(response, "output") else response
    default_type = type_enum[0] if type_enum else "chore"
    if isinstance(output, ConventionalAnalysis):
        return output
    if isinstance(output, Mapping):
        return analysis_from_mapping(output, default_type=default_type)
    text_content = getattr(response, "text_content", None)
    if text_content:
        try:
            return parse_conventional_analysis_markdown(text_content, default_type=default_type)
        except ValueError:
            pass
    return _fallback_reduce_analysis(observations, config)


async def run_map_reduce(*args: Any, **kwargs: Any) -> ConventionalAnalysis:
    """Run map and reduce phases for a large diff.

    Accepts Python order ``(config, stat, diff, scope_candidates=...)`` and the
    Rust-port order ``(diff, stat, scope_candidates, model_name, config, counter)``.
    """

    if args and isinstance(args[0], str):
        diff = args[0]
        stat = args[1] if len(args) > 1 else kwargs.get("stat", "")
        scope_candidates = args[2] if len(args) > 2 else kwargs.get("scope_candidates", "")
        model_name = args[3] if len(args) > 3 else kwargs.get("model_name")
        config = args[4] if len(args) > 4 else kwargs["config"]
        counter = args[5] if len(args) > 5 else kwargs.get("counter")
    else:
        config = args[0] if args else kwargs["config"]
        stat = args[1] if len(args) > 1 else kwargs.get("stat", "")
        diff = args[2] if len(args) > 2 else kwargs.get("diff", "")
        scope_candidates = args[3] if len(args) > 3 else kwargs.get("scope_candidates", "")
        model_name = kwargs.get("model_name")
        counter = kwargs.get("counter")

    counter = counter or create_token_counter(config)
    reduce_model = resolve_model_name(
        str(model_name or getattr(config, "analysis_model", getattr(config, "model", "claude-opus-4.8")))
    )
    map_model = resolve_model_name(str(getattr(config, "summary_model", getattr(config, "model", reduce_model))))
    observations = await observe_diff_files(str(diff), map_model, config, counter)
    return await reduce_phase(observations, str(stat), str(scope_candidates), reduce_model, config)


async def _map_phase(
    files: Sequence[FileDiff], map_model_name: str, config: Any, counter: Any
) -> list[FileObservation]:
    context_headers = _ContextHeaders(files)
    batches = build_llm_file_batches(files, counter, int(getattr(config, "map_batch_token_budget", 16000)))
    observations_by_index: list[FileObservation | None] = [None] * len(files)
    for idx, file in enumerate(files):
        if file.is_binary:
            observations_by_index[idx] = FileObservation(
                file.filename, ("Binary file changed.",), file.additions, file.deletions
            )

    semaphore = asyncio.Semaphore(MAP_PHASE_CONCURRENCY)

    async def run_batch(batch_idx: int, batch_indices: list[int]) -> list[tuple[int, FileObservation]]:
        async with semaphore:
            batch_files = [files[idx] for idx in batch_indices]
            paths = [file.filename for file in batch_files]
            context_header = context_headers.header_for_files(paths)
            observations = await _map_file_batch(
                batch_files,
                context_header,
                map_model_name,
                config,
                counter,
                f"map batch {batch_idx + 1}/{len(batches)} ({len(batch_files)} files)",
            )
            return list(zip(batch_indices, observations, strict=True))

    results = await asyncio.gather(*(run_batch(idx, batch) for idx, batch in enumerate(batches)))
    for batch_result in results:
        for idx, observation in batch_result:
            observations_by_index[idx] = observation
    observations: list[FileObservation] = []
    for idx, observation in enumerate(observations_by_index):
        if observation is None:
            raise RuntimeError(f"Missing map observation for {files[idx].filename}")
        observations.append(observation)
    return observations


async def _map_file_batch(
    files: Sequence[FileDiff], context_header: str, model_name: str, config: Any, counter: Any, progress_label: str
) -> list[FileObservation]:
    rendered = [_render_file_diff_for_batch(file, counter) for file in files]
    prompt_files = [{"path": file.filename, "diff": diff} for file, diff in zip(files, rendered, strict=True)]
    variant = "markdown" if bool(getattr(config, "markdown_output", True)) else "default"
    system_prompt, user_prompt = _render_map_prompt(variant, prompt_files, context_header)
    response = await run_oneshot(
        config,
        OneShotSpec(
            operation="map-reduce/map",
            model=resolve_model_name(model_name),
            prompt_family="map",
            prompt_variant=variant,
            system_prompt=system_prompt,
            user_prompt=user_prompt,
            tool_name="create_file_observations",
            tool_description="Extract observations from a batch of file changes",
            schema=_batch_observation_schema(),
            progress_label=progress_label,
            cacheable=True,
        ),
    )
    output = response.output if hasattr(response, "output") else response
    text_content = getattr(response, "text_content", None)
    stop_reason = getattr(response, "stop_reason", None)
    return _map_batch_response_to_observations(files, output, text_content, stop_reason)


def _map_batch_response_to_observations(
    files: Sequence[FileDiff], output: Any, text_content: str | None, stop_reason: str | None
) -> list[FileObservation]:
    entries = _observation_entries(output)
    if not entries and text_content and text_content.strip():
        return [_fallback_file_observation(file) for file in files]
    used = [False] * len(entries)
    observations: list[FileObservation] = []
    stopped_at_max_tokens = stop_reason == "max_tokens"
    for file in files:
        entry_idx = _find_observation_entry(file.filename, entries, used, files)
        if entry_idx is None:
            observations.append(_fallback_file_observation(file))
            continue
        used[entry_idx] = True
        entry = entries[entry_idx]
        raw_observations = _parse_observations(entry.get("observations", []))
        if not raw_observations and stopped_at_max_tokens:
            raw_observations = [_fallback_observation_text(file.filename)]
        observations.append(FileObservation(file.filename, tuple(raw_observations), file.additions, file.deletions))
    return observations


def _observation_entries(output: Any) -> list[dict[str, Any]]:
    if isinstance(output, Mapping):
        raw = output.get("files", [])
    elif isinstance(output, list):
        raw = output
    else:
        raw = []
    return [dict(item) for item in raw if isinstance(item, Mapping)]


def _find_observation_entry(
    filename: str, entries: Sequence[Mapping[str, Any]], used: Sequence[bool], batch_files: Sequence[FileDiff]
) -> int | None:
    basename = _path_basename(filename)
    basename_unique = sum(1 for file in batch_files if _path_basename(file.filename) == basename) == 1
    matchers = (
        lambda entry: str(entry.get("path", "")) == filename,
        lambda entry: basename_unique and _path_basename(str(entry.get("path", ""))) == basename,
        lambda entry: _path_suffix_matches(str(entry.get("path", "")), filename),
    )
    for matcher in matchers:
        for idx, entry in enumerate(entries):
            if not used[idx] and matcher(entry):
                return idx
    return None


def _parse_observations(value: Any) -> list[str]:
    if isinstance(value, str):
        stripped = value.strip()
        if stripped.startswith("["):
            try:
                decoded = json.loads(stripped)
                if isinstance(decoded, list):
                    return [str(item).strip() for item in decoded if str(item).strip()]
            except json.JSONDecodeError:
                pass
        return [line.lstrip("-*• ").strip() for line in stripped.splitlines() if line.lstrip("-*• ").strip()]
    if isinstance(value, Iterable):
        return [str(item).strip() for item in value if str(item).strip()]
    return []


def _build_file_batches_for_indices(
    files: Sequence[FileDiff], indices: Iterable[int], counter: Any, budget: int
) -> list[list[int]]:
    token_budget = max(1, int(budget))
    batches: list[list[int]] = []
    current: list[int] = []
    current_tokens = 0
    for idx in indices:
        file_tokens = files[idx].token_estimate(counter)
        if file_tokens > token_budget:
            if current:
                batches.append(current)
                current = []
                current_tokens = 0
            batches.append([idx])
            continue
        if current and current_tokens + file_tokens > token_budget:
            batches.append(current)
            current = []
            current_tokens = 0
        current.append(idx)
        current_tokens += file_tokens
    if current:
        batches.append(current)
    return batches


def _included_files(files: Sequence[FileDiff], config: Any) -> list[FileDiff]:
    excluded = tuple(str(item) for item in getattr(config, "excluded_files", ()))
    return [file for file in files if not any(file.filename.endswith(pattern) for pattern in excluded)]


def _render_file_diff_for_batch(file: FileDiff, counter: Any) -> str:
    if file.token_estimate(counter) <= MAX_FILE_TOKENS:
        return _reconstruct_single_file_diff(file)
    clone = FileDiff(file.filename, file.header, file.content, file.additions, file.deletions, file.is_binary)
    clone.truncate(MAX_FILE_TOKENS * 4)
    return reconstruct_diff([clone])


def _reconstruct_single_file_diff(file: FileDiff) -> str:
    return f"{file.header}\n{file.content}" if file.content else file.header


def _fallback_file_observation(file: FileDiff) -> FileObservation:
    return FileObservation(file.filename, (_fallback_observation_text(file.filename),), file.additions, file.deletions)


def _fallback_observation_text(filename: str) -> str:
    return f"Updated {_path_basename(filename)}."


def _fallback_reduce_analysis(
    observations: Sequence[FileObservation], config: Any, stat: str = ""
) -> ConventionalAnalysis:
    details = [obs for item in observations for obs in item.observations if obs]
    summary = fallback_summary(stat=stat, details=details, limit=int(getattr(config, "summary_hard_limit", 128)))
    return ConventionalAnalysis(
        commit_type="chore",
        summary=summary,
        details=tuple(AnalysisDetail.simple(_ensure_sentence(detail)) for detail in details[:6]),
        issue_refs=(),
    )


def _ensure_sentence(text: str) -> str:
    stripped = text.strip()
    return stripped if not stripped or stripped.endswith((".", "!", "?")) else f"{stripped}."


def _batch_observation_schema() -> dict[str, Any]:
    return strict_json_schema(
        {
            "files": {
                "type": "array",
                "description": "Per-file observations for every file in the map batch.",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Exact input file path."},
                        "observations": {"type": "array", "items": {"type": "string"}},
                    },
                    "required": ["path", "observations"],
                    "additionalProperties": False,
                },
            }
        },
        ["files"],
    )


def _render_map_prompt(variant: str, files: Sequence[Mapping[str, str]], context_header: str) -> tuple[str, str]:
    try:
        from .templates import render_map_prompt

        parts = render_map_prompt(variant, files, context_header)
        return parts.system, parts.user
    except Exception:
        return render_prompt("map", variant, {"files": files, "context_header": context_header})


def _render_reduce_prompt(
    variant: str, observations: str, stat: str, scope_candidates: str, types_description: str
) -> tuple[str, str]:
    try:
        from .templates import render_reduce_prompt

        parts = render_reduce_prompt(variant, observations, stat, scope_candidates, types_description)
        return parts.system, parts.user
    except Exception:
        return render_prompt(
            "reduce",
            variant,
            {
                "observations": observations,
                "stat": stat,
                "scope_candidates": scope_candidates,
                "types_description": types_description,
            },
        )


def _observation_to_mapping(item: FileObservation) -> dict[str, Any]:
    return {
        "file": item.file,
        "observations": list(item.observations),
        "additions": item.additions,
        "deletions": item.deletions,
    }


def _path_basename(path: str) -> str:
    return Path(path).name or path


def _path_suffix_matches(left: str, right: str) -> bool:
    return _path_has_suffix(left, right) or _path_has_suffix(right, left)


def _path_has_suffix(path: str, suffix: str) -> bool:
    return path == suffix or path.endswith(f"/{suffix}") or path.endswith(f"\\{suffix}")


class _ContextHeaders:
    def __init__(self, files: Sequence[FileDiff]) -> None:
        self.large_commit_header = f"(Large commit with {len(files)} total files)" if len(files) > 100 else None
        self.files = (
            [
                (
                    _file.filename,
                    _file.additions + _file.deletions,
                    _infer_file_description(_file.filename, _file.content),
                )
                for _file in files
            ]
            if self.large_commit_header is None
            else []
        )

    def header_for_files(self, current_files: Sequence[str]) -> str:
        if self.large_commit_header:
            return self.large_commit_header
        current = set(current_files)
        others = [item for item in self.files if item[0] not in current]
        if not others:
            return ""
        shown = sorted(others, key=lambda item: item[1], reverse=True)[:MAX_CONTEXT_FILES]
        lines = ["OTHER FILES IN THIS CHANGE:", *(f"- {path} ({size} lines): {desc}" for path, size, desc in shown)]
        if len(shown) < len(others):
            lines.append(f"... and {len(others) - len(shown)} more files")
        return "\n".join(lines)


def _infer_file_description(filename: str, content: str) -> str:
    lower = filename.lower()
    suffix = Path(filename).suffix.lower()
    if "test" in lower:
        return "test file"
    if "prompt" in lower or "system" in lower:
        return "prompt template"
    if suffix == ".md":
        return "documentation"
    if "config" in lower or suffix in {".toml", ".yaml", ".yml"}:
        return "configuration"
    if "error" in lower:
        return "error definitions"
    if "type" in lower:
        return "type definitions"
    if lower.endswith(("mod.rs", "lib.rs")):
        return "module exports"
    if lower.endswith(("main.rs", "main.go", "main.py")):
        return "entry point"
    if "class " in content or "def " in content or "fn " in content:
        return "implementation"
    if "struct " in content or "enum " in content:
        return "type definitions"
    if "async " in content or "await" in content:
        return "async code"
    return "source code"


__all__ = [
    "FileObservation",
    "build_file_batches",
    "build_llm_file_batches",
    "observe_diff_files",
    "reduce_phase",
    "run_map_reduce",
    "should_use_map_reduce",
]

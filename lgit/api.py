"""Async LLM API client and conventional-commit generation helpers."""

from __future__ import annotations

import asyncio
import json
import re
import time
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, replace
from enum import StrEnum
from importlib import resources
from pathlib import Path
from typing import Any
from urllib.parse import urljoin

import httpx
from jinja2 import Template

from . import cache as llm_cache
from . import profile
from .errors import ApiContextLengthExceeded, ApiError, LgitError
from .markdown_output import (
    analysis_from_mapping,
    fallback_summary,
    parse_changelog_response,
    parse_compose_binding_markdown,
    parse_compose_intent_markdown,
    parse_conventional_analysis_markdown,
    parse_fast_commit_markdown,
    parse_summary_markdown,
)
from .markdown_output import (
    strip_type_prefix as strip_markdown_type_prefix,
)
from .models import CommitSummary, ConventionalAnalysis, ConventionalCommit, ResolvedApiMode, resolve_model_name
from .normalization import post_process_commit_message
from .validation import is_past_tense_first_word, validate_summary_quality

ANTHROPIC_REQUIRED_MAX_TOKENS = 16_384
_CONTEXT_LENGTH_MARKERS = (
    "context_length_exceeded",
    "context window",
    "maximum context length",
    "exceeds the context",
    "input exceeds",
    "prompt is too long",
    "too many tokens",
)
_JSON_CACHE_PREFIX = "\x00json:"


class OneShotSource(StrEnum):
    """Origin of a parsed one-shot response."""

    TOOL_CALL = "tool_call"
    OUTPUT_JSON_PARSE = "output_json_parse"
    PLAIN_TEXT_CONTENT = "plain_text_content"
    CACHE = "cache"


@dataclass(frozen=True, slots=True)
class OneShotDebug:
    """Optional raw request/response debug output target."""

    dir: str | Path | None = None
    prefix: str | None = None
    name: str = "oneshot"


@dataclass(frozen=True, slots=True)
class OneShotSpec:
    """Complete description of a single LLM tool or markdown request."""

    operation: str
    model: str | None = None
    prompt_family: str = "custom"
    prompt_variant: str = "default"
    system_prompt: str = ""
    user_prompt: str = ""
    tool_name: str = "create_response"
    tool_description: str = "Return the requested structured response"
    schema: Mapping[str, Any] | None = None
    progress_label: str | None = None
    debug: OneShotDebug | Mapping[str, Any] | str | Path | None = None
    cacheable: bool = True


@dataclass(frozen=True, slots=True)
class OneShotResponse:
    """Parsed output and metadata from a one-shot LLM request."""

    output: Any
    source: OneShotSource
    text_content: str | None = None
    stop_reason: str | None = None


def strict_json_schema(properties: Mapping[str, Any], required: list[str] | tuple[str, ...]) -> dict[str, Any]:
    """Build a strict object JSON schema with no additional properties."""

    return {"type": "object", "properties": dict(properties), "required": list(required), "additionalProperties": False}


async def run_oneshot(
    config: Any,
    prompt: str | OneShotSpec | Mapping[str, Any] | None = None,
    *,
    spec: OneShotSpec | Mapping[str, Any] | None = None,
    system_prompt: str | None = None,
    model: str | None = None,
    schema: Mapping[str, Any] | None = None,
    schema_name: str = "response",
    tool_name: str | None = None,
    tool_description: str | None = None,
    operation: str | None = None,
    prompt_family: str = "custom",
    prompt_variant: str = "default",
    temperature: float | None = None,
    debug_label: str | None = None,
    debug: OneShotDebug | Mapping[str, Any] | str | Path | None = None,
    markdown_output: bool | None = None,
    cache: bool = True,
    cacheable: bool | None = None,
    **_: Any,
) -> Any:
    """Run one LLM request, returning parsed output or a ``OneShotResponse`` for specs."""

    del temperature
    return_response = isinstance(prompt, OneShotSpec) or spec is not None or isinstance(prompt, Mapping)
    built = _coerce_spec(
        prompt,
        spec=spec,
        system_prompt=system_prompt,
        model=model,
        schema=schema,
        schema_name=schema_name,
        tool_name=tool_name,
        tool_description=tool_description,
        operation=operation,
        prompt_family=prompt_family,
        prompt_variant=prompt_variant,
        debug_label=debug_label,
        debug=debug,
        cacheable=cache if cacheable is None else cacheable,
    )
    if not built.model:
        built = replace(
            built,
            model=resolve_model_name(
                str(getattr(config, "analysis_model", getattr(config, "model", "claude-opus-4.8")))
            ),
        )
    response = await _run_oneshot_response(config, built, markdown_output=markdown_output)
    return response if return_response else response.output


async def generate_conventional_analysis(
    config: Any,
    stat: str,
    diff: str,
    scope_candidates: str = "",
    *,
    user_context: str | None = None,
    recent_commits: str | None = None,
    common_scopes: str | None = None,
    project_context: str | None = None,
    debug_output: str | Path | None = None,
    debug_prefix: str | None = None,
) -> ConventionalAnalysis:
    """Generate a structured conventional-commit analysis for a diff."""

    variant = (
        "markdown"
        if bool(getattr(config, "markdown_output", True))
        else str(getattr(config, "analysis_prompt_variant", "default"))
    )
    system_prompt, user_prompt = render_prompt(
        "analysis",
        variant,
        {
            "project_context": project_context or "",
            "types_description": format_types_description(config),
            "stat": stat,
            "scope_candidates": scope_candidates,
            "common_scopes": common_scopes or "",
            "recent_commits": recent_commits or "",
            "diff": diff,
        },
    )
    if user_context:
        user_prompt = f"{user_prompt}\n\n<user_context>\n{user_context}\n</user_context>"
    type_enum = list(getattr(config, "types", {}) or {"chore": None})
    spec = OneShotSpec(
        operation="analysis",
        model=resolve_model_name(str(getattr(config, "analysis_model", getattr(config, "model", "claude-opus-4.8")))),
        prompt_family="analysis",
        prompt_variant=variant,
        system_prompt=system_prompt,
        user_prompt=user_prompt,
        tool_name="create_conventional_analysis",
        tool_description="Create conventional commit analysis from a git diff",
        schema=build_analysis_schema(type_enum, config),
        progress_label="analysis",
        debug=OneShotDebug(debug_output, debug_prefix, "analysis") if debug_output else None,
        cacheable=True,
    )
    response = await _run_oneshot_response(config, spec)
    return _coerce_analysis(response.output, response.text_content, default_type=type_enum[0] if type_enum else "chore")


def strip_type_prefix(summary: str, commit_type: str | None = None, scope: str | None = None) -> str:
    """Strip Rust-equivalent conventional type prefixes from a summary."""

    if commit_type is None:
        return strip_markdown_type_prefix(summary)
    text = str(summary).strip()
    commit_type_lower = commit_type.lower()
    prefixes = []
    if scope:
        prefixes.append(f"{commit_type}({scope}): ")
    prefixes.append(f"{commit_type}: ")
    for prefix in prefixes:
        if text.lower().startswith(prefix.lower()):
            return text[len(prefix) :].strip()
    match = re.match(r"^([a-z][a-z0-9-]*)(?:\(([^)]*)\))?:\s+(.*)$", text, re.IGNORECASE)
    if match and match.group(1).lower() == commit_type_lower:
        return match.group(3).strip()
    return text


def summary_from_holistic_analysis(analysis: ConventionalAnalysis, config: Any, stat: str = "") -> str | None:
    """Return a hard-limit-validated holistic summary from analysis, or None."""

    del stat
    if not analysis.summary or not str(analysis.summary).strip():
        return None
    summary = strip_type_prefix(
        str(analysis.summary).strip(),
        str(analysis.commit_type),
        None if analysis.scope is None else str(analysis.scope),
    ).rstrip(" .")
    if not summary:
        return None
    return str(CommitSummary.from_raw(summary, max_length=int(getattr(config, "summary_hard_limit", 128))))


async def generate_summary_from_analysis(
    config: Any,
    analysis: ConventionalAnalysis,
    stat: str = "",
    *,
    user_context: str | None = None,
    debug_output: str | Path | None = None,
    debug_prefix: str | None = None,
) -> str:
    """Generate a concise summary from structured analysis details."""

    commit_type = str(analysis.commit_type)
    scope = None if analysis.scope is None else str(analysis.scope)
    prefix_len = len(commit_type) + 2 + (len(scope) + 2 if scope else 0)
    chars = max(20, int(getattr(config, "summary_guideline", 72)) - prefix_len)
    variant = (
        "markdown"
        if bool(getattr(config, "markdown_output", True))
        else str(getattr(config, "summary_prompt_variant", "default"))
    )
    details = "\n".join(f"- {detail}" for detail in analysis.body_texts()) or f"- {analysis.summary or ''}"
    system_prompt, user_prompt = render_prompt(
        "summary",
        variant,
        {
            "commit_type": commit_type,
            "scope": scope,
            "chars": chars,
            "user_context": user_context or "",
            "details": details,
            "stat": stat,
        },
    )
    spec = OneShotSpec(
        operation="summary",
        model=resolve_model_name(str(getattr(config, "summary_model", getattr(config, "model", "claude-haiku-4-5")))),
        prompt_family="summary",
        prompt_variant=variant,
        system_prompt=system_prompt,
        user_prompt=user_prompt,
        tool_name="create_commit_summary",
        tool_description="Compose a git commit summary line from detail statements",
        schema=strict_json_schema(
            {
                "summary": {
                    "type": "string",
                    "description": f"Single line summary, target {getattr(config, 'summary_guideline', 72)} chars, hard limit {getattr(config, 'summary_hard_limit', 128)}.",
                    "maxLength": int(getattr(config, "summary_hard_limit", 128)),
                }
            },
            ["summary"],
        ),
        progress_label="summary",
        debug=OneShotDebug(debug_output, debug_prefix, "summary") if debug_output else None,
        cacheable=True,
    )
    try:
        response = await _run_oneshot_response(config, spec)
        summary = _summary_from_output(response.output, response.text_content)
    except Exception:
        summary = ""
    summary = strip_type_prefix(
        summary
        or analysis.summary
        or fallback_summary(
            stat, analysis.body_texts(), limit=int(getattr(config, "summary_hard_limit", 128)), commit_type=commit_type
        )
    )
    if not validate_summary_quality(summary, commit_type, stat).ok:
        summary = _fallback_summary_for_commit(
            stat, analysis.body_texts(), commit_type, int(getattr(config, "summary_hard_limit", 128))
        )
    return summary[: int(getattr(config, "summary_hard_limit", 128))].rstrip(" .")


async def generate_analysis_with_map_reduce(
    config: Any, stat: str, diff: str, scope_candidates: str = "", **kwargs: Any
) -> ConventionalAnalysis:
    """Generate analysis directly or through map-reduce for large diffs."""

    from . import style
    from .map_reduce import run_map_reduce, should_use_map_reduce
    from .tokens import create_token_counter

    counter = kwargs.pop("counter", None) or create_token_counter(config)
    if should_use_map_reduce(diff, config, counter):
        count_sync = getattr(counter, "count_sync", None)
        token_count = int(count_sync(diff)) if callable(count_sync) else max(1, len(diff) // 4)
        style.print_info(f"Large diff detected ({token_count} tokens), using map-reduce...")
        return await run_map_reduce(config, stat, diff, scope_candidates, counter=counter, **kwargs)
    return await generate_conventional_analysis(config, stat, diff, scope_candidates, **kwargs)


async def generate_fast_commit(
    config: Any,
    stat: str,
    diff: str,
    scope_candidates: str = "",
    *,
    user_context: str | None = None,
    debug_output: str | Path | None = None,
    debug_prefix: str | None = None,
) -> ConventionalCommit:
    """Generate a complete conventional commit in one model call."""

    variant = "markdown" if bool(getattr(config, "markdown_output", True)) else "default"
    system_prompt, user_prompt = render_prompt(
        "fast",
        variant,
        {
            "stat": stat,
            "diff": diff,
            "scope_candidates": scope_candidates,
            "user_context": user_context or "",
            "types_description": format_types_description(config),
        },
    )
    type_enum = list(getattr(config, "types", {}) or {"chore": None})
    spec = OneShotSpec(
        operation="fast",
        model=resolve_model_name(str(getattr(config, "analysis_model", getattr(config, "model", "claude-opus-4.8")))),
        prompt_family="fast",
        prompt_variant=variant,
        system_prompt=system_prompt,
        user_prompt=user_prompt,
        tool_name="create_fast_commit",
        tool_description="Create a compact conventional commit message",
        schema=strict_json_schema(
            {
                "type": {"type": "string", "enum": type_enum, "description": "Conventional commit type"},
                "scope": {"type": "string", "description": "Optional scope. Omit if unclear."},
                "summary": {"type": "string", "description": "Compact past-tense summary, no prefix or period"},
                "details": {"type": "array", "items": {"type": "string"}, "description": "0-3 detail sentences"},
            },
            ["type", "summary", "details"],
        ),
        progress_label="fast",
        debug=OneShotDebug(debug_output, debug_prefix, "fast") if debug_output else None,
        cacheable=True,
    )
    response = await _run_oneshot_response(config, spec)
    commit = _coerce_fast_commit(
        response.output, response.text_content, default_type=type_enum[0] if type_enum else "chore"
    )
    return post_process_commit_message(commit, config)


def _fallback_summary_for_commit(stat: str, details: Iterable[str], commit_type: str, limit: int) -> str:
    details_list = [str(detail) for detail in details]
    candidate = fallback_summary(stat, details_list, limit=limit, commit_type=commit_type)
    if validate_summary_quality(candidate, commit_type, stat).ok:
        return candidate
    first_detail = details_list[0].strip().rstrip(".") if details_list else ""
    cleaned = first_detail or strip_type_prefix(candidate).strip()
    for variant in (commit_type, f"{commit_type}ed", f"{commit_type}d"):
        prefix = f"{variant.lower()} "
        if cleaned.lower().startswith(prefix):
            cleaned = cleaned[len(variant) :].strip()
            break
    verb = {
        "feat": "added",
        "fix": "fixed",
        "refactor": "restructured",
        "docs": "documented",
        "test": "tested",
        "perf": "optimized",
        "build": "updated",
        "ci": "updated",
        "chore": "updated",
        "style": "formatted",
        "revert": "reverted",
    }.get(commit_type, "changed")
    first_word = cleaned.split(maxsplit=1)[0] if cleaned else ""
    prefixed = cleaned if first_word and is_past_tense_first_word(first_word) else f"{verb} {cleaned or 'files'}"
    try:
        return str(CommitSummary.from_raw(prefixed, max_length=limit))
    except LgitError:
        return fallback_summary("", details_list, limit=limit, commit_type=commit_type)


def render_prompt(family: str, variant: str, context: Mapping[str, Any]) -> tuple[str, str]:
    """Render a prompt through ``lgit.templates`` with resource fallback."""

    try:
        from . import templates

        rendered = _render_with_template_helper(templates, family, variant, context)
        if rendered is not None:
            return rendered
    except ImportError, AttributeError:
        pass
    template_text = (
        resources.files("lgit.resources").joinpath("prompts", family, f"{variant}.md").read_text(encoding="utf-8")
    )
    system, user = _split_prompt(Template(template_text).render(**context))
    return system, user


def format_types_description(config: Any) -> str:
    """Format configured commit-type guidance for prompts."""

    lines: list[str] = []
    for name, type_config in (getattr(config, "types", {}) or {}).items():
        description = getattr(type_config, "description", "")
        hint = getattr(type_config, "hint", "")
        line = f"- {name}: {description}".rstrip()
        if hint:
            line += f" ({hint})"
        lines.append(line)
    classifier_hint = str(getattr(config, "classifier_hint", "") or "").strip()
    if classifier_hint:
        lines.append(classifier_hint)
    return "\n".join(lines)


def build_analysis_schema(type_enum: list[str], config: Any) -> dict[str, Any]:
    """Return the strict schema used for conventional analysis calls."""

    return strict_json_schema(
        {
            "type": {"type": "string", "enum": type_enum, "description": "Conventional commit type"},
            "scope": {"type": "string", "description": "Optional scope. Omit if unclear."},
            "summary": {
                "type": "string",
                "description": "Umbrella commit summary without type/scope prefix or trailing period",
                "maxLength": int(getattr(config, "summary_hard_limit", 128)),
            },
            "details": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "changelog_category": {
                            "type": "string",
                            "enum": ["Added", "Changed", "Fixed", "Deprecated", "Removed", "Security"],
                        },
                        "user_visible": {"type": "boolean"},
                    },
                    "required": ["text", "user_visible"],
                    "additionalProperties": False,
                },
            },
            "issue_refs": {"type": "array", "items": {"type": "string"}},
        },
        ["type", "summary", "details", "issue_refs"],
    )


def encode_cache_payload(source: OneShotSource | str, output: Any, text_content: str | None = None) -> str | None:
    """Encode parsed output for stable cache storage."""

    if str(source) in {OneShotSource.PLAIN_TEXT_CONTENT.value, OneShotSource.OUTPUT_JSON_PARSE.value} and text_content:
        return text_content
    try:
        return _JSON_CACHE_PREFIX + json.dumps(output, ensure_ascii=False, separators=(",", ":"), default=_json_default)
    except TypeError:
        return None


def decode_cache_payload(
    tool_name: str, operation: str, stored: str, markdown_output: bool
) -> tuple[Any, str | None] | None:
    """Decode a cached payload using JSON first, then markdown/plain-text fallback."""

    is_raw = not stored.startswith(_JSON_CACHE_PREFIX)
    payload = stored if is_raw else stored.removeprefix(_JSON_CACHE_PREFIX)
    try:
        output = _parse_json_payload(payload)
    except json.JSONDecodeError, ValueError, TypeError:
        try:
            output = _parse_plain_text(tool_name, payload, markdown_output)
        except json.JSONDecodeError, ValueError, TypeError:
            return None
    if output is None:
        return None
    return output, payload if is_raw else None


async def _run_oneshot_response(
    config: Any, spec: OneShotSpec, *, markdown_output: bool | None = None
) -> OneShotResponse:
    markdown_mode = bool(getattr(config, "markdown_output", True) if markdown_output is None else markdown_output)
    mode = _resolved_mode(config, spec.model or "")
    cache_entry = _build_cache_entry(config, spec, markdown_mode)
    if cache_entry is not None:
        cache_obj, key = cache_entry
        stored = cache_obj.get(key)
        if stored is not None:
            decoded = decode_cache_payload(spec.tool_name, spec.operation, stored, markdown_mode)
            if decoded is not None:
                output, text = decoded
                profile.print_llm_progress(lambda: f"cache hit {spec.operation} ({spec.model})")
                return OneShotResponse(output=output, source=OneShotSource.CACHE, text_content=text)

    attempts = max(1, int(getattr(config, "max_retries", 3)))
    request_json = ""
    response_text = ""
    last_error: Exception | None = None
    last_retry_from_error = False
    for attempt in range(1, attempts + 1):
        try:
            request, response_text = await _send_oneshot(config, spec, mode, markdown_mode)
            request_json = json.dumps(request, ensure_ascii=False, default=_json_default)
            if not response_text.strip():
                raise _RetryableResponse("empty response body")
            response = _parse_oneshot_response(mode, spec.tool_name, spec.operation, response_text, markdown_mode)
            if cache_entry is not None:
                payload = encode_cache_payload(response.source, response.output, response.text_content)
                if payload is not None:
                    cache_entry[0].put(cache_entry[1], spec.model or "", spec.operation, request_json, payload)
            return response
        except ApiContextLengthExceeded:
            raise
        except _RetryableResponse as exc:
            last_error = exc
            last_retry_from_error = False
        except (
            httpx.TimeoutException,
            httpx.TransportError,
            LgitError,
            json.JSONDecodeError,
            ValueError,
            TypeError,
        ) as exc:
            _record_failure(config, cache_entry, spec, request_json, response_text, exc)
            last_error = exc
            last_retry_from_error = True
        if attempt < attempts:
            await asyncio.sleep(max(0, int(getattr(config, "initial_backoff_ms", 1000))) / 1000 * (2 ** (attempt - 1)))
    if last_retry_from_error and last_error is not None:
        raise last_error
    raise LgitError(f"Max retries exceeded for {spec.operation}: {last_error}")


async def _send_oneshot(
    config: Any, spec: OneShotSpec, mode: ResolvedApiMode, markdown_mode: bool
) -> tuple[dict[str, Any], str]:
    timeout = httpx.Timeout(
        float(getattr(config, "request_timeout_secs", 120)), connect=float(getattr(config, "connect_timeout_secs", 30))
    )
    headers = {"content-type": "application/json"}
    api_key = getattr(config, "api_key", None)
    if mode == ResolvedApiMode.CHAT_COMPLETIONS:
        if api_key:
            headers["authorization"] = f"Bearer {api_key}"
        request = _openai_request(config, spec, markdown_mode)
        url = urljoin(
            str(getattr(config, "api_base_url", "http://localhost:4000")).rstrip("/") + "/", "chat/completions"
        )
    else:
        headers["anthropic-version"] = "2023-06-01"
        if api_key:
            headers["x-api-key"] = str(api_key)
            headers["authorization"] = f"Bearer {api_key}"
        if _anthropic_prompt_caching_enabled(config):
            headers["anthropic-beta"] = "prompt-caching-2024-07-31"
        request = _anthropic_request(config, spec, markdown_mode)
        url = _anthropic_messages_url(str(getattr(config, "api_base_url", "")))
    _save_debug(spec.debug, "request", request)
    profile.print_llm_progress(lambda: f"query {spec.operation} model={spec.model}")
    start = time.monotonic()
    async with httpx.AsyncClient(timeout=timeout) as client:
        response = await client.post(url, headers=headers, json=request)
    text = response.text
    profile.print_llm_progress(
        lambda: (
            f"response {spec.operation} status={response.status_code} elapsed={time.monotonic() - start:.2f}s size={len(text)}B"
        )
    )
    _save_debug_text(spec.debug, "response", text)
    if not response.is_success and _is_context_length_error(text):
        raise ApiContextLengthExceeded(
            operation=spec.operation, model=spec.model or "", status=response.status_code, body=text
        )
    if 500 <= response.status_code <= 599:
        raise _RetryableResponse(f"server error {response.status_code}: {text}")
    if not response.is_success:
        raise ApiError(status=response.status_code, body=text)
    return request, text


def _openai_request(config: Any, spec: OneShotSpec, markdown_mode: bool) -> dict[str, Any]:
    messages = []
    if spec.system_prompt.strip():
        messages.append({"role": "system", "content": spec.system_prompt})
    messages.append({"role": "user", "content": spec.user_prompt})
    request: dict[str, Any] = {"model": spec.model, "messages": messages}
    if not markdown_mode:
        request["tools"] = [_openai_tool(spec.tool_name, spec.tool_description, spec.schema or {})]
        request["tool_choice"] = {"type": "function", "function": {"name": spec.tool_name}}
    prompt_cache_key = _openai_prompt_cache_key(config, spec)
    if prompt_cache_key:
        request["prompt_cache_key"] = prompt_cache_key
    return request


def _anthropic_request(config: Any, spec: OneShotSpec, markdown_mode: bool) -> dict[str, Any]:
    prompt_caching = _anthropic_prompt_caching_enabled(config)
    request: dict[str, Any] = {
        "model": spec.model,
        "max_tokens": ANTHROPIC_REQUIRED_MAX_TOKENS,
        "messages": [{"role": "user", "content": [_anthropic_text(spec.user_prompt, prompt_caching)]}],
    }
    if spec.system_prompt.strip():
        request["system"] = [_anthropic_text(spec.system_prompt, prompt_caching)]
    if not markdown_mode:
        request["tools"] = [_anthropic_tool(spec.tool_name, spec.tool_description, spec.schema or {}, prompt_caching)]
        request["tool_choice"] = {"type": "tool", "name": spec.tool_name}
    return request


def _parse_oneshot_response(
    mode: ResolvedApiMode, tool_name: str, operation: str, response_text: str, markdown_mode: bool
) -> OneShotResponse:
    if mode == ResolvedApiMode.CHAT_COMPLETIONS:
        body = json.loads(response_text)
        choices = body.get("choices") or []
        if not choices:
            raise LgitError(f"API returned empty response for {operation}")
        message = choices[0].get("message") or {}
        if refusal := message.get("refusal"):
            raise LgitError(f"Model refused {operation}: {refusal}")
        last_error: Exception | None = None
        for call in message.get("tool_calls") or []:
            function = (call or {}).get("function") or {}
            if str(function.get("name", "")).endswith(tool_name):
                args = str(function.get("arguments") or "").strip()
                if not args:
                    last_error = LgitError(f"Model returned empty function arguments for {operation}")
                else:
                    try:
                        return OneShotResponse(
                            _parse_tool_arguments(args, operation), OneShotSource.TOOL_CALL, message.get("content")
                        )
                    except LgitError as exc:
                        last_error = exc
        content = message.get("content")
        if content is not None:
            if not str(content).strip():
                raise _RetryableResponse("empty content")
            return _parse_content_fallback(tool_name, operation, str(content), markdown_mode)
        if last_error is not None:
            raise last_error
        raise LgitError(f"No {operation} found in API response")

    tool_input, text_content, stop_reason = _extract_anthropic_content(response_text, tool_name)
    if tool_input is not None:
        return OneShotResponse(tool_input, OneShotSource.TOOL_CALL, text_content or None, stop_reason)
    if not text_content.strip():
        raise _RetryableResponse("empty content")
    response = _parse_content_fallback(tool_name, operation, text_content, markdown_mode)
    return OneShotResponse(response.output, response.source, response.text_content, stop_reason)


def _parse_content_fallback(tool_name: str, operation: str, content: str, markdown_mode: bool) -> OneShotResponse:
    try:
        return OneShotResponse(_parse_json_payload(content), OneShotSource.OUTPUT_JSON_PARSE, content)
    except (json.JSONDecodeError, ValueError, TypeError) as json_error:
        try:
            parsed = _parse_plain_text(tool_name, content, markdown_mode)
        except (json.JSONDecodeError, ValueError, TypeError) as markdown_error:
            raise LgitError(f"Failed to parse {operation} plain-text fallback: {markdown_error}") from markdown_error
        if parsed is None:
            raise LgitError(f"Failed to parse {operation} content JSON: {json_error}") from json_error
        return OneShotResponse(parsed, OneShotSource.PLAIN_TEXT_CONTENT, content)


def _parse_plain_text(tool_name: str, content: str, markdown_mode: bool) -> Any:
    text = _normalize_plain_text(content)
    if not text:
        return None
    if tool_name == "create_conventional_analysis":
        return _analysis_to_mapping(parse_conventional_analysis_markdown(text))
    if tool_name == "create_fast_commit":
        commit = parse_fast_commit_markdown(text)
        return {
            "type": str(commit.commit_type),
            "scope": None if commit.scope is None else str(commit.scope),
            "summary": str(commit.summary),
            "details": list(commit.body),
        }
    if tool_name == "create_file_observations":
        from .markdown_output import parse_file_observations_markdown

        return {"files": parse_file_observations_markdown(text)}
    if tool_name == "create_changelog_entries":
        return parse_changelog_response(text)
    if tool_name == "create_compose_intent_plan":
        return parse_compose_intent_markdown(text)
    if tool_name == "bind_compose_hunks":
        return parse_compose_binding_markdown(text)
    if tool_name == "create_commit_summary":
        return {"summary": parse_summary_markdown(text) if markdown_mode else strip_type_prefix(text)}
    return None


def _parse_tool_arguments(args: str, operation: str) -> Any:
    try:
        return _parse_json_payload(args)
    except (json.JSONDecodeError, ValueError, TypeError) as exc:
        raise LgitError(f"Failed to parse {operation} tool arguments: {exc}") from exc


def _parse_json_payload(text: str) -> Any:
    candidate = _extract_json_from_content(text)
    return json.loads(candidate)


def _extract_json_from_content(content: str) -> str:
    trimmed = _normalize_plain_text(content)
    if not trimmed:
        return trimmed

    start = trimmed.find("{")
    end = trimmed.rfind("}")
    if start >= 0 and end >= start:
        return trimmed[start : end + 1]
    start = trimmed.find("[")
    end = trimmed.rfind("]")
    if start >= 0 and end >= start:
        return trimmed[start : end + 1]
    return trimmed


def _normalize_plain_text(content: str) -> str:
    trimmed = content.strip()
    fenced = re.search(r"```(?:json|markdown|md)?\s*(.*?)```", trimmed, re.IGNORECASE | re.DOTALL)
    return fenced.group(1).strip() if fenced else trimmed


def _extract_anthropic_content(response_text: str, tool_name: str) -> tuple[Any | None, str, str | None]:
    value = json.loads(response_text)
    stop_reason = value.get("stop_reason")
    tool_input = None
    text_parts: list[str] = []
    for item in value.get("content") or []:
        item_type = item.get("type", "")
        if item_type == "tool_use" and item.get("name") == tool_name:
            tool_input = item.get("input")
        elif item_type == "text" and isinstance(item.get("text"), str):
            text_parts.append(item["text"])
    return tool_input, "\n".join(text_parts), None if stop_reason is None else str(stop_reason)


def _coerce_spec(prompt: str | OneShotSpec | Mapping[str, Any] | None, **kwargs: Any) -> OneShotSpec:
    spec = kwargs.pop("spec", None)
    if isinstance(spec, OneShotSpec):
        return spec
    if isinstance(prompt, OneShotSpec):
        return prompt
    if spec is None and isinstance(prompt, Mapping):
        spec = prompt
    if isinstance(spec, Mapping):
        values = dict(spec)
        if "cache" in values and "cacheable" not in values:
            values["cacheable"] = values.pop("cache")
        return OneShotSpec(**{key: value for key, value in values.items() if key in OneShotSpec.__dataclass_fields__})
    schema = kwargs["schema"] or strict_json_schema({"response": {"type": "string"}}, ["response"])
    schema_name = kwargs["schema_name"]
    inferred_tool = kwargs["tool_name"] or (
        schema_name if schema_name.startswith(("create_", "bind_")) else f"create_{schema_name}"
    )
    return OneShotSpec(
        operation=kwargs["operation"] or schema_name,
        model=resolve_model_name(str(kwargs["model"] or "")) if kwargs["model"] else None,
        prompt_family=kwargs["prompt_family"],
        prompt_variant=kwargs["prompt_variant"],
        system_prompt=kwargs["system_prompt"] or "",
        user_prompt=str(prompt or ""),
        tool_name=inferred_tool,
        tool_description=kwargs["tool_description"] or f"Create {schema_name}",
        schema=schema,
        progress_label=kwargs["operation"] or schema_name,
        debug=_coerce_debug(kwargs["debug"], kwargs["debug_label"] or schema_name),
        cacheable=bool(kwargs["cacheable"]),
    )


def _coerce_debug(debug: OneShotDebug | Mapping[str, Any] | str | Path | None, name: str) -> OneShotDebug | None:
    if debug is None:
        return None
    if isinstance(debug, OneShotDebug):
        return debug
    if isinstance(debug, Mapping):
        return OneShotDebug(**debug)
    return OneShotDebug(debug, None, name)


def _resolved_mode(config: Any, model: str) -> ResolvedApiMode:
    resolver = getattr(config, "resolve_api_mode", None)
    if callable(resolver):
        return resolver(model)
    return config.resolved_api_mode


def _build_cache_entry(config: Any, spec: OneShotSpec, markdown_mode: bool) -> tuple[Any, str] | None:
    if not spec.cacheable:
        return None
    cache_obj = llm_cache.global_cache()
    if cache_obj is None:
        return None
    mode = str(_resolved_mode(config, spec.model or ""))
    key = llm_cache.compute_key(
        llm_cache.CacheMaterial(
            operation=spec.operation,
            model=spec.model or "",
            tool_name=spec.tool_name,
            tool_description=spec.tool_description,
            system_prompt=spec.system_prompt,
            user_prompt=spec.user_prompt,
            schema=spec.schema or {},
            api_mode=mode,
            markdown_output=markdown_mode,
        )
    )
    return cache_obj, key


def _record_failure(
    config: Any, cache_entry: tuple[Any, str] | None, spec: OneShotSpec, request: str, response: str, error: Exception
) -> None:
    sink = llm_cache.global_cache()
    if sink is None and cache_entry is not None:
        sink = cache_entry[0]
    if sink is not None:
        sink.put_failure(
            cache_entry[1] if cache_entry else "", spec.model or "", spec.operation, request, response, str(error)
        )


def _openai_tool(name: str, description: str, schema: Mapping[str, Any]) -> dict[str, Any]:
    if "properties" not in schema:
        raise LgitError("Schema must include top-level properties")
    required = schema.get("required")
    if not isinstance(required, (list, tuple)) or not all(isinstance(value, str) for value in required):
        raise LgitError("Schema must include top-level required array of strings")
    parameters = {
        "type": "object",
        "properties": dict(schema["properties"]),
        "required": list(required),
        "additionalProperties": False,
    }
    return {"type": "function", "function": {"name": name, "description": description, "parameters": parameters}}


def _anthropic_tool(name: str, description: str, schema: Mapping[str, Any], cache: bool) -> dict[str, Any]:
    tool = {"name": name, "description": description, "input_schema": dict(schema)}
    if cache:
        tool["cache_control"] = {"type": "ephemeral"}
    return tool


def _anthropic_text(text: str, cache: bool) -> dict[str, Any]:
    content = {"type": "text", "text": text}
    if cache:
        content["cache_control"] = {"type": "ephemeral"}
    return content


def _anthropic_prompt_caching_enabled(config: Any) -> bool:
    return "anthropic.com" in str(getattr(config, "api_base_url", "")).lower()


def _anthropic_messages_url(base_url: str) -> str:
    trimmed = base_url.rstrip("/")
    return f"{trimmed}/messages" if trimmed.endswith("/v1") else f"{trimmed}/v1/messages"


def _openai_prompt_cache_key(config: Any, spec: OneShotSpec) -> str | None:
    base_url = str(getattr(config, "api_base_url", "")).lower()
    if not spec.system_prompt.strip() or "api.openai.com" not in base_url:
        return None
    return f"llm-git:v1:{spec.model}:{spec.prompt_family}:{spec.prompt_variant}"


def _is_context_length_error(body: str) -> bool:
    lower = body.lower()
    return any(marker in lower for marker in _CONTEXT_LENGTH_MARKERS)


def _save_debug(debug: OneShotDebug | Mapping[str, Any] | str | Path | None, phase: str, value: Any) -> None:
    if debug is None:
        return
    _save_debug_text(debug, phase, json.dumps(value, ensure_ascii=False, indent=2, default=_json_default))


def _save_debug_text(debug: OneShotDebug | Mapping[str, Any] | str | Path | None, phase: str, text: str) -> None:
    debug_obj = _coerce_debug(debug, "oneshot")
    if debug_obj is None or debug_obj.dir is None:
        return
    directory = Path(debug_obj.dir)
    directory.mkdir(parents=True, exist_ok=True)
    prefix = f"{debug_obj.prefix}_" if debug_obj.prefix else ""
    path = directory / f"{prefix}{debug_obj.name}_{phase}.json"
    path.write_text(text, encoding="utf-8")


def _render_with_template_helper(
    templates: Any, family: str, variant: str, context: Mapping[str, Any]
) -> tuple[str, str] | None:
    helper = getattr(templates, f"render_{family.replace('-', '_')}_prompt", None)
    if not callable(helper):
        return None
    match family:
        case "analysis" | "fast":
            parts = helper(variant=variant, **dict(context))
        case "summary":
            parts = helper(
                variant,
                str(context.get("commit_type", "")),
                str(context.get("scope") or ""),
                str(context.get("chars", "")),
                str(context.get("details", "")),
                str(context.get("stat", "")),
                context.get("user_context"),
            )
        case "map":
            parts = helper(variant, context.get("files", ()), str(context.get("context_header", "")))
        case "reduce":
            parts = helper(
                variant,
                str(context.get("observations", "")),
                str(context.get("stat", "")),
                str(context.get("scope_candidates", "")),
                context.get("types_description"),
            )
        case _:
            return None
    return str(parts.system), str(parts.user)


def _split_prompt(text: str) -> tuple[str, str]:
    marker = "======USER======="
    if marker in text:
        system, user = text.split(marker, 1)
        return system.strip(), user.strip()
    return text.strip(), ""


def _coerce_analysis(output: Any, text_content: str | None, *, default_type: str) -> ConventionalAnalysis:
    if isinstance(output, ConventionalAnalysis):
        return output
    if isinstance(output, Mapping):
        return analysis_from_mapping(output, default_type=default_type)
    if text_content:
        return parse_conventional_analysis_markdown(text_content, default_type=default_type)
    return parse_conventional_analysis_markdown(str(output), default_type=default_type)


def _summary_from_output(output: Any, text_content: str | None) -> str:
    if isinstance(output, Mapping):
        value = output.get("summary")
        if value:
            return strip_type_prefix(str(value))
    if isinstance(output, str):
        return parse_summary_markdown(output)
    return parse_summary_markdown(text_content or "")


def _coerce_fast_commit(output: Any, text_content: str | None, *, default_type: str) -> ConventionalCommit:
    if isinstance(output, ConventionalCommit):
        return output
    if isinstance(output, Mapping):
        analysis = analysis_from_mapping(output, default_type=default_type)
        return ConventionalCommit.from_raw(
            commit_type=str(analysis.commit_type),
            scope=None if analysis.scope is None else str(analysis.scope),
            summary=analysis.summary
            or fallback_summary(details=analysis.body_texts(), commit_type=str(analysis.commit_type)),
            body=analysis.body_texts(),
        )
    if text_content:
        return parse_fast_commit_markdown(text_content, default_type=default_type)
    return parse_fast_commit_markdown(str(output), default_type=default_type)


def _analysis_to_mapping(analysis: ConventionalAnalysis) -> dict[str, Any]:
    return {
        "type": str(analysis.commit_type),
        "scope": None if analysis.scope is None else str(analysis.scope),
        "summary": analysis.summary or "",
        "details": [
            {
                "text": detail.text,
                **(
                    {"changelog_category": detail.changelog_category.value}
                    if detail.changelog_category is not None
                    else {}
                ),
                "user_visible": detail.user_visible,
            }
            for detail in analysis.details
        ],
        "issue_refs": list(analysis.issue_refs),
    }


def _json_default(value: Any) -> Any:
    if hasattr(value, "value"):
        return value.value
    if hasattr(value, "__dict__"):
        return vars(value)
    return str(value)


class _RetryableResponse(Exception):
    pass


__all__ = [
    "OneShotDebug",
    "OneShotResponse",
    "OneShotSource",
    "OneShotSpec",
    "build_analysis_schema",
    "decode_cache_payload",
    "encode_cache_payload",
    "fallback_summary",
    "format_types_description",
    "generate_analysis_with_map_reduce",
    "generate_conventional_analysis",
    "generate_fast_commit",
    "generate_summary_from_analysis",
    "summary_from_holistic_analysis",
    "render_prompt",
    "run_oneshot",
    "strict_json_schema",
    "strip_type_prefix",
]

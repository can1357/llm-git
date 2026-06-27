"""Lenient markdown and text parsers for model commit outputs."""

from __future__ import annotations

import json
import re
from collections.abc import Iterable, Mapping
from typing import Any

from .errors import InvalidCommitType
from .models import (
    AnalysisDetail,
    ChangelogCategory,
    CommitType,
    ConventionalAnalysis,
    ConventionalCommit,
    coerce_commit_type,
)

_PREFIX_RE = re.compile(
    r"^\s*(?:#+\s*)?(?P<type>[a-z][a-z0-9-]*)(?:\((?P<scope>[^)]+)\))?!?\s*:\s*(?P<summary>.+?)\s*$",
    re.IGNORECASE,
)
_SUMMARY_TAG_RE = re.compile(r"<summary\b[^>]*>\s*(.*?)(?:\s*</[^>]+>|$)", re.IGNORECASE | re.DOTALL)
_ISSUE_RE = re.compile(r"#\d+(?:\s*-\s*#?\d+)?")
_CATEGORY_RE = re.compile(
    r"^\s*(?:\[(?P<bracket>[^\]]+)\]|(?P<prefix>Added|Changed|Fixed|Deprecated|Removed|Security|Breaking Changes)\s*:)\s*(?P<text>.*)$",
    re.IGNORECASE,
)
_SUMMARY_VERBS = {
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
}
_SUMMARY_SAFE_DEFAULTS = {
    "refactor": "restructured change",
    "feat": "added functionality",
    "fix": "fixed issue",
    "docs": "documented updates",
    "test": "tested changes",
    "chore": "updated tooling",
    "build": "updated tooling",
    "ci": "updated tooling",
    "style": "updated tooling",
    "perf": "optimized performance",
    "revert": "reverted previous commit",
}


def strip_type_prefix(text: str) -> str:
    """Remove a conventional-commit prefix from ``text`` when present."""

    first_line = _clean_markdown_text(text).splitlines()[0].strip() if text.strip() else ""
    match = _PREFIX_RE.match(first_line)
    summary = match.group("summary") if match else first_line
    return _strip_trailing_period(_strip_wrapping_quotes(summary.strip()))


def fallback_summary(
    stat: str = "", details: Iterable[str] = (), diff: str = "", *, limit: int = 72, commit_type: str = "chore"
) -> str:
    """Return a deterministic, type-aware summary when model output cannot be parsed."""

    normalized_type = _normalize_commit_type(commit_type) or "chore"
    candidate = ""
    needs_verb = False
    for detail in details:
        candidate = strip_type_prefix(str(detail).lstrip("-*•–+ ").strip())
        if candidate:
            candidate, needs_verb = _strip_leading_type_word(candidate, normalized_type)
            break
    if not candidate:
        area = _primary_stat_subject(stat) or _primary_stat_subject(diff)
        candidate = "Updated files" if area is None or area.lower() == "files" else f"Updated {area}"
    candidate = " ".join(candidate.replace("\n", " ").replace("\r", " ").split()).strip().rstrip(".;:").strip()
    if not candidate:
        candidate = "Updated files"
    if needs_verb and not _starts_with_past_tense(candidate):
        candidate = f"{_summary_verb(normalized_type)} {candidate}"
    cap = max(1, min(limit, 50))
    candidate = _truncate_summary(candidate, cap).rstrip(".")
    first_word = candidate.split(maxsplit=1)[0] if candidate else ""
    if first_word.lower() == normalized_type.lower():
        candidate = _safe_summary_default(normalized_type)
    return _truncate_summary(candidate, cap)


def parse_summary_markdown(text: str) -> str:
    """Parse a summary from markdown, XML-ish tags, JSON, or plain text."""

    if not text.strip():
        return ""
    jsonish = _try_json(text)
    if isinstance(jsonish, Mapping):
        for key in ("summary", "title", "message"):
            value = jsonish.get(key)
            if isinstance(value, str) and value.strip():
                return strip_type_prefix(value)
    cleaned = _clean_markdown_text(text)
    tagged = _extract_tag_lenient(cleaned, "summary")
    raw = tagged if tagged is not None else cleaned
    stripped = _strip_heading_markers(raw)
    stripped = _strip_label_prefix(stripped)
    stripped = _strip_wrapping_quotes(stripped)
    summary = " ".join(stripped.split())
    if not summary:
        raise ValueError("markdown summary empty after normalization")
    return strip_type_prefix(summary)


def parse_conventional_analysis_markdown(text: str, *, default_type: str = "chore") -> ConventionalAnalysis:
    """Parse a conventional analysis from markdown, JSON, or plain text."""

    payload = _try_json(text)
    if isinstance(payload, Mapping):
        return analysis_from_mapping(payload, default_type=default_type)

    cleaned = _clean_markdown_text(text)
    lines = cleaned.splitlines()
    heading: tuple[int, str, str | None, str] | None = None
    coerced: tuple[int, str, str | None, str] | None = None
    for index, line in enumerate(lines[:5]):
        candidate = _strip_heading_markers(line)
        parsed = _parse_heading_line(candidate, coerce=False)
        if parsed is not None:
            heading = (index, *parsed)
            break
        if coerced is None and line.strip().startswith("#"):
            parsed = _parse_heading_line(candidate, coerce=True)
            if parsed is not None:
                coerced = (index, *parsed)
    if heading is None:
        heading = coerced
    if heading is None:
        raise ValueError("markdown analysis type(scope): summary heading not found")

    heading_index, commit_type, scope, summary = heading
    detail_texts: list[str] = []
    issue_refs: list[str] = []
    for line in lines[heading_index + 1 :]:
        stripped = line.strip()
        if not stripped:
            continue
        lower = stripped.lower()
        if lower.startswith(("fixes:", "closes:", "resolves:")):
            _, refs = stripped.split(":", 1)
            issue_refs.extend(ref.strip() for ref in refs.split(",") if ref.strip())
            continue
        bullet = _strip_bullet(stripped)
        if bullet:
            detail_texts.append(_ensure_sentence(bullet))
            issue_refs.extend(_ISSUE_RE.findall(bullet))

    details = tuple(AnalysisDetail.simple(detail) for detail in _dedupe(detail_texts))
    return ConventionalAnalysis(
        commit_type=commit_type, scope=scope, summary=summary, details=details, issue_refs=tuple(_dedupe(issue_refs))
    )


def parse_fast_commit_markdown(text: str, *, default_type: str = "chore") -> ConventionalCommit:
    """Parse a complete conventional commit from markdown or JSON text."""

    payload = _try_json(text)
    if isinstance(payload, Mapping):
        analysis = analysis_from_mapping(payload, default_type=default_type)
    else:
        analysis = parse_conventional_analysis_markdown(text, default_type=default_type)
    return ConventionalCommit.from_raw(
        commit_type=str(analysis.commit_type),
        scope=None if analysis.scope is None else str(analysis.scope),
        summary=analysis.summary or fallback_summary(details=analysis.body_texts()),
        body=analysis.body_texts(),
    )


def analysis_from_mapping(payload: Mapping[str, Any], *, default_type: str = "chore") -> ConventionalAnalysis:
    """Coerce a JSON-like mapping into ``ConventionalAnalysis``."""

    commit_type = str(payload.get("type") or payload.get("commit_type") or default_type).strip() or default_type
    raw_scope = payload.get("scope")
    scope_text = "" if raw_scope is None else str(raw_scope).strip()
    scope = None if scope_text.lower() in {"", "null", "none", "(none)"} else scope_text
    summary = strip_type_prefix(str(payload.get("summary") or "")) or None
    raw_details = payload.get("details") or payload.get("body") or []
    details = tuple(_coerce_detail(item) for item in _coerce_iterable(raw_details) if _detail_text(item))
    issue_refs = tuple(
        str(item).strip()
        for item in _coerce_iterable(payload.get("issue_refs") or payload.get("issues") or ())
        if str(item).strip()
    )
    return ConventionalAnalysis(
        commit_type=commit_type, scope=scope, summary=summary, details=details, issue_refs=issue_refs
    )


def parse_file_observations_markdown(text: str) -> list[dict[str, Any]]:
    """Parse map-phase file observations from JSON or lenient markdown."""

    payload = _try_json(text)
    if isinstance(payload, Mapping):
        files = payload.get("files", [])
        if isinstance(files, list):
            return [_coerce_file_observations(item) for item in files if isinstance(item, Mapping)]
    if isinstance(payload, list):
        return [_coerce_file_observations(item) for item in payload if isinstance(item, Mapping)]

    files: list[dict[str, Any]] = []
    current_path: str | None = None
    current_obs: list[str] = []
    for line in _clean_markdown_text(text).splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        heading = re.match(r"^(?:#+\s*)?(?:file\s*[:=-]\s*)?`?([^`]+?)`?\s*:??$", stripped, re.IGNORECASE)
        bullet = _strip_bullet(stripped)
        if bullet is None and heading and ("/" in heading.group(1) or "." in heading.group(1)):
            if current_path is not None:
                files.append({"path": current_path, "observations": current_obs})
            current_path = heading.group(1).strip()
            current_obs = []
        elif bullet is not None and current_path is not None:
            current_obs.append(_strip_trailing_period(bullet))
    if current_path is not None:
        files.append({"path": current_path, "observations": current_obs})
    return files


def parse_compose_intent_markdown(text: str) -> dict[str, Any]:
    """Parse markdown compose intent output into a JSON-like mapping."""

    trimmed = _clean_markdown_text(text)
    groups: list[dict[str, Any]] = []
    group_map: dict[str, int] = {}
    for line in trimmed.splitlines():
        trimmed_line = line.strip()
        if ":=" not in trimmed_line:
            continue
        gid, rest = trimmed_line.split(":=", 1)
        gid = gid.strip()
        rest = rest.strip()
        if not gid or ":" not in rest:
            continue
        type_scope, rationale = rest.split(":", 1)
        commit_type, scope = _parse_compose_type_scope(type_scope.strip())
        group_map[gid] = len(groups)
        groups.append(
            {
                "group_id": gid,
                "type": str(coerce_commit_type(commit_type)),
                "scope": scope,
                "rationale": rationale.strip(),
                "file_ids": [],
                "dependencies": [],
            }
        )

    for line in trimmed.splitlines():
        trimmed_line = line.strip()
        if "<-" not in trimmed_line:
            continue
        gid, deps_text = trimmed_line.split("<-", 1)
        idx = group_map.get(gid.strip())
        if idx is not None:
            groups[idx]["dependencies"] = [dep.strip() for dep in deps_text.strip().split(",") if dep.strip()]

    in_files_section = False
    for line in trimmed.splitlines():
        trimmed_line = line.strip()
        if trimmed_line.lower().startswith("files:"):
            in_files_section = True
            continue
        bullet = _bullet_content(trimmed_line)
        if not in_files_section or bullet is None or ":" not in bullet:
            continue
        gid, files_text = bullet.split(":", 1)
        idx = group_map.get(gid.strip())
        if idx is not None:
            groups[idx]["file_ids"] = [file_id.strip() for file_id in files_text.strip().split(",")]

    if not groups:
        raise ValueError("markdown compose intent: no groups found (format: G1 := type(scope): rationale)")
    return {"groups": groups}


def parse_compose_binding_markdown(text: str) -> dict[str, Any]:
    """Parse markdown compose hunk-binding output into a JSON-like mapping."""

    assignments: list[dict[str, Any]] = []
    current_group: str | None = None
    current_hunks: list[str] = []
    for line in _clean_markdown_text(text).splitlines():
        trimmed_line = line.strip()
        if trimmed_line.startswith("#"):
            if current_group is not None:
                assignments.append({"group_id": current_group, "hunk_ids": current_hunks})
                current_hunks = []
            current_group = trimmed_line.lstrip("#").strip().rstrip(":").strip()
            continue
        hunk_id = _bullet_content(trimmed_line)
        if hunk_id is not None:
            current_hunks.append(hunk_id)
    if current_group is not None:
        assignments.append({"group_id": current_group, "hunk_ids": current_hunks})
    if not assignments:
        raise ValueError("markdown compose binding: no assignments found (format: # group_id\\n- hunk_id)")
    return {"assignments": assignments}


def _parse_compose_type_scope(type_scope: str) -> tuple[str, str | None]:
    if "(" in type_scope:
        p_start = type_scope.find("(")
        p_end = type_scope.find(")", p_start + 1)
        if p_end >= 0:
            commit_type = type_scope[:p_start].strip()
            scope = type_scope[p_start + 1 : p_end].strip()
            return commit_type, scope or None
    return type_scope, None


def _coerce_detail(item: Any) -> AnalysisDetail:
    text = _detail_text(item)
    category: ChangelogCategory | None = None
    user_visible = False
    if isinstance(item, Mapping):
        raw_category = item.get("changelog_category") or item.get("category")
        if raw_category:
            category = ChangelogCategory.from_name(str(raw_category))
        user_visible = bool(item.get("user_visible", category is not None))
    else:
        text, category = _strip_category_prefix(text)
        user_visible = category is not None
    return AnalysisDetail(text=_ensure_sentence(text), changelog_category=category, user_visible=user_visible)


def _detail_text(item: Any) -> str:
    if isinstance(item, Mapping):
        return str(item.get("text") or item.get("summary") or item.get("detail") or "").strip()
    return str(item).strip()


def _coerce_file_observations(item: Mapping[str, Any]) -> dict[str, Any]:
    observations = _coerce_observation_strings(item.get("observations") or item.get("details") or [])
    return {"path": str(item.get("path") or item.get("file") or ""), "observations": observations}


def _coerce_observation_strings(value: Any) -> list[str]:
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
    return [str(item).strip() for item in _coerce_iterable(value) if str(item).strip()]


def _coerce_iterable(value: Any) -> Iterable[Any]:
    if value is None:
        return ()
    if isinstance(value, str):
        return (value,) if value.strip() else ()
    if isinstance(value, Iterable):
        return value
    return (value,)


def _normalize_commit_type(raw: str) -> str | None:
    try:
        return str(CommitType.from_raw(raw))
    except InvalidCommitType:
        return None


def _summary_verb(commit_type: str) -> str:
    return _SUMMARY_VERBS.get(commit_type, "changed")


def _safe_summary_default(commit_type: str) -> str:
    return _SUMMARY_SAFE_DEFAULTS.get(commit_type, "updated files")


def _strip_leading_type_word(text: str, commit_type: str) -> tuple[str, bool]:
    cleaned = text.strip().rstrip(".")
    variants = {commit_type, f"{commit_type}ed", f"{commit_type}d"}
    for variant in sorted(variants, key=len, reverse=True):
        prefix = f"{variant.lower()} "
        if cleaned.lower().startswith(prefix):
            return cleaned[len(variant) :].strip(), True
    return cleaned, False


def _starts_with_past_tense(text: str) -> bool:
    words = text.split()
    first = words[0].lower() if words else ""
    return first.endswith("ed") or first in {
        "built",
        "changed",
        "documented",
        "fixed",
        "optimized",
        "restructured",
        "updated",
    }


def _primary_stat_subject(text: str) -> str | None:
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        subject = stripped.split("|", 1)[0].strip()
        return subject or "files"
    return None


def _try_json(text: str) -> Any:
    cleaned = _clean_markdown_text(text).strip()
    candidates = [cleaned]
    fenced = re.search(r"```(?:json)?\s*(.*?)```", text, re.IGNORECASE | re.DOTALL)
    if fenced:
        candidates.insert(0, fenced.group(1).strip())
    for candidate in candidates:
        if not candidate or candidate[0] not in "[{":
            continue
        try:
            return json.loads(candidate)
        except json.JSONDecodeError:
            continue
    return None


def _clean_markdown_text(text: str) -> str:
    cleaned = _normalize_escaped_whitespace(text.strip())
    if cleaned.startswith("```"):
        after_open = cleaned[3:]
        content_start = after_open.find("\n")
        if content_start >= 0:
            body = after_open[content_start + 1 :]
            end = body.rfind("```")
            cleaned = (body[:end] if end >= 0 else body).strip()
    else:
        cleaned = "\n".join(
            line for line in cleaned.splitlines() if line.strip() != "```" and not line.lstrip().startswith("```")
        ).strip()
    return cleaned.replace("\r\n", "\n")


def _strip_bullet(line: str) -> str | None:
    bullet = _bullet_content(line)
    if bullet is not None and bullet:
        return bullet
    match = re.match(r"^\s*\d+[.)]\s+(?P<text>.+)$", line)
    return match.group("text").strip() if match else None


def _bullet_content(line: str) -> str | None:
    stripped = line.lstrip()
    for glyph in ("- ", "* ", "• ", "– ", "+ "):
        if stripped.startswith(glyph):
            return stripped[len(glyph) :].strip()
    return None


def _strip_category_prefix(text: str) -> tuple[str, ChangelogCategory | None]:
    match = _CATEGORY_RE.match(text)
    if not match:
        return text.strip(), None
    raw_category = match.group("bracket") or match.group("prefix") or ""
    return match.group("text").strip(), ChangelogCategory.from_name(raw_category)


def _strip_wrapping_quotes(text: str) -> str:
    pairs = {'"': '"', "'": "'", "`": "`", "“": "”", "‘": "’"}
    stripped = text.strip()
    if len(stripped) >= 2 and pairs.get(stripped[0]) == stripped[-1]:
        return stripped[1:-1].strip()
    return stripped


def _normalize_escaped_whitespace(text: str) -> str:
    real = text.count("\n")
    literal = text.count("\\n")
    if literal == 0 or literal < real:
        return text
    return text.replace("\\r\\n", "\n").replace("\\n", "\n").replace("\\r", "\n").replace("\\t", "\t")


def _extract_tag_lenient(text: str, tag: str) -> str | None:
    lower = text.lower()
    open_tag = f"<{tag}"
    open_pos = lower.find(open_tag)
    if open_pos < 0:
        return None
    open_end = text.find(">", open_pos)
    if open_end < 0:
        return None
    content_start = open_end + 1
    rest = text[content_start:]
    close_pos = rest.find("</")
    return (rest[:close_pos] if close_pos >= 0 else rest).strip()


def _strip_label_prefix(text: str) -> str:
    stripped = text.strip()
    if ":" not in stripped:
        return stripped
    label, remainder = stripped.split(":", 1)
    if label.strip().lower() in {"title", "summary", "description", "result"}:
        return remainder.strip()
    return stripped


def _strip_heading_markers(text: str) -> str:
    stripped = text.strip().lstrip("#").strip()
    for marker in ("**", "*", "__", "_"):
        if stripped.startswith(marker) and stripped.endswith(marker) and len(stripped) > 2 * len(marker):
            stripped = stripped[len(marker) : -len(marker)].strip()
    return stripped


def _parse_heading_line(line: str, *, coerce: bool) -> tuple[str, str | None, str] | None:
    split = _split_heading(line)
    if split is None:
        return None
    commit_type, scope, summary = split
    canonical = _normalize_commit_type(commit_type)
    if canonical is not None:
        return canonical, scope, summary
    if coerce and _is_bare_word(commit_type) and not summary.startswith(('"', "{", "[")):
        return str(coerce_commit_type(commit_type)), scope, summary
    return None


def _split_heading(line: str) -> tuple[str, str | None, str] | None:
    if ":" not in line:
        return None
    type_scope, summary = line.split(":", 1)
    type_scope = type_scope.strip()
    summary = summary.strip()
    if not type_scope or not summary:
        return None
    scope: str | None = None
    if "(" in type_scope:
        p_start = type_scope.find("(")
        p_end = type_scope.find(")", p_start + 1)
        if p_end < 0:
            return None
        commit_type = type_scope[:p_start].strip()
        scope_text = type_scope[p_start + 1 : p_end].strip()
        type_scope = commit_type
        scope = scope_text or None
    if not type_scope:
        return None
    return type_scope, scope, summary


def _is_bare_word(text: str) -> bool:
    return bool(text) and text[0].isalpha() and all(ch.isalpha() or ch == "-" for ch in text)


def parse_changelog_response(text: str) -> dict[str, dict[str, list[str]]]:
    """Parse markdown changelog output into an ``entries`` mapping."""

    cleaned = _clean_markdown_text(text)
    if _has_exception_tag(cleaned):
        return {"entries": {}}
    known = {"Added", "Changed", "Fixed", "Deprecated", "Removed", "Security", "Breaking Changes"}
    entries: dict[str, list[str]] = {}
    current: str | None = None
    for raw_line in cleaned.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        candidate, inline = _changelog_heading(line)
        if candidate in known:
            current = candidate
            entries.setdefault(current, [])
            if inline:
                entries[current].append(inline)
            continue
        if candidate is None and line.startswith("#"):
            current = _strip_heading_markers(line).rstrip(":").strip()
            entries.setdefault(current, [])
            continue
        bullet = _strip_bullet(line)
        if bullet is not None:
            text_part, category = _strip_category_prefix(_strip_heading_markers(bullet))
            if category is not None:
                entries.setdefault(category.value, []).append(text_part)
                continue
            if current is None:
                maybe_category, inline_bullet = _changelog_heading(text_part)
                if maybe_category in known and inline_bullet:
                    entries.setdefault(maybe_category, []).append(inline_bullet)
                continue
            entries.setdefault(current, []).append(text_part)
        elif current is not None:
            entries.setdefault(current, []).append(line)
    if not any(values for values in entries.values()):
        raise ValueError("No changelog entries found in response")
    return {"entries": entries}


def _has_exception_tag(text: str) -> bool:
    return re.search(r"<exception(?:\s|/|>|$)", text, re.IGNORECASE) is not None


def _changelog_heading(line: str) -> tuple[str | None, str]:
    stripped = _strip_heading_markers(line).rstrip(":").strip()
    stripped = _strip_wrapping_quotes(stripped)
    if ":" in stripped:
        head, inline = stripped.split(":", 1)
        category = _known_category_name(_category_token(head))
        inline = inline.strip().lstrip("*_`").strip()
        return category, inline
    category = _known_category_name(_category_token(stripped))
    return category, ""


def _category_token(text: str) -> str:
    return _strip_heading_markers(text).strip("*_`\"'“”‘’ ")


def _known_category_name(text: str) -> str | None:
    normalized = text.strip().lower()
    if normalized == "breaking":
        return ChangelogCategory.BREAKING.value
    for category in ChangelogCategory:
        if category.value.lower() == normalized:
            return category.value
    return None


def _strip_trailing_period(text: str) -> str:
    return text[:-1].rstrip() if text.rstrip().endswith(".") else text.strip()


def _ensure_sentence(text: str) -> str:
    cleaned = text.strip()
    if not cleaned:
        return ""
    return cleaned if cleaned.endswith((".", "!", "?")) else f"{cleaned}."


def _truncate_summary(text: str, limit: int) -> str:
    cleaned = _strip_trailing_period(text.strip())
    if len(cleaned) <= limit:
        return cleaned
    return cleaned[: max(1, limit)].rsplit(" ", 1)[0].rstrip(" ,;:-") or cleaned[:limit].rstrip(" ,;:-")


def _dedupe(values: Iterable[str]) -> list[str]:
    seen: set[str] = set()
    result: list[str] = []
    for value in values:
        key = value.strip().lower()
        if key and key not in seen:
            seen.add(key)
            result.append(value.strip())
    return result


parse_conventional_analysis = parse_conventional_analysis_markdown
parse_summary_output = parse_summary_markdown
parse_fast_commit = parse_fast_commit_markdown
parse_batch_observations = parse_file_observations_markdown
parse_changelog_entries = parse_changelog_response
parse_compose_intent = parse_compose_intent_markdown
parse_compose_binding = parse_compose_binding_markdown


__all__ = [
    "analysis_from_mapping",
    "fallback_summary",
    "parse_batch_observations",
    "parse_changelog_entries",
    "parse_changelog_response",
    "parse_compose_binding",
    "parse_compose_binding_markdown",
    "parse_conventional_analysis",
    "parse_conventional_analysis_markdown",
    "parse_compose_intent",
    "parse_compose_intent_markdown",
    "parse_fast_commit",
    "parse_fast_commit_markdown",
    "parse_file_observations_markdown",
    "parse_summary_markdown",
    "parse_summary_output",
    "strip_type_prefix",
]

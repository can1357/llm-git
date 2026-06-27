"""Normalization utilities for conventional commit messages."""

from __future__ import annotations

import math
import unicodedata
from dataclasses import replace
from typing import Any

from .models import ConventionalCommit
from .validation import is_past_tense_verb, present_to_past, split_verb_token, verb_stem

_DEFAULT_MAX_DETAIL_TOKENS = 200
_DEFAULT_SUMMARY_HARD_LIMIT = 128

_PRE_NFKD_REPLACEMENTS = str.maketrans(
    {
        "≠": "!=",
        "½": "1/2",
        "¼": "1/4",
        "¾": "3/4",
        "⅓": "1/3",
        "⅔": "2/3",
        "⅕": "1/5",
        "⅖": "2/5",
        "⅗": "3/5",
        "⅘": "4/5",
        "⅙": "1/6",
        "⅚": "5/6",
        "⅛": "1/8",
        "⅜": "3/8",
        "⅝": "5/8",
        "⅞": "7/8",
        "⁰": "^0",
        "¹": "^1",
        "²": "^2",
        "³": "^3",
        "⁴": "^4",
        "⁵": "^5",
        "⁶": "^6",
        "⁷": "^7",
        "⁸": "^8",
        "⁹": "^9",
        "₀": "_0",
        "₁": "_1",
        "₂": "_2",
        "₃": "_3",
        "₄": "_4",
        "₅": "_5",
        "₆": "_6",
        "₇": "_7",
        "₈": "_8",
        "₉": "_9",
    }
)

_POST_NFKD_REPLACEMENTS = str.maketrans(
    {
        "‘": "'",
        "’": "'",
        "‚": "'",
        "‹": "'",
        "›": "'",
        "“": '"',
        "”": '"',
        "„": '"',
        "«": '"',
        "»": '"',
        "‐": "-",
        "‑": "-",
        "‒": "-",
        "–": "--",
        "—": "--",
        "―": "--",
        "−": "-",
        "→": "->",
        "←": "<-",
        "↔": "<->",
        "⇒": "=>",
        "⇐": "<=",
        "⇔": "<=>",
        "↑": "^",
        "↓": "v",
        "≤": "<=",
        "≥": ">=",
        "≈": "~=",
        "≡": "==",
        "×": "x",
        "÷": "/",
        "…": "...",
        "⋯": "...",
        "⋮": "...",
        "•": "-",
        "◦": "-",
        "▪": "-",
        "▫": "-",
        "◆": "-",
        "◇": "-",
        "✓": "v",
        "✔": "v",
        "✗": "x",
        "✘": "x",
        "λ": "lambda",
        "α": "alpha",
        "β": "beta",
        "γ": "gamma",
        "δ": "delta",
        "ε": "epsilon",
        "θ": "theta",
        "μ": "mu",
        "π": "pi",
        "σ": "sigma",
        "Σ": "Sigma",
        "Δ": "Delta",
        "Π": "Pi",
        "\u00a0": " ",
        "\u2000": " ",
        "\u2001": " ",
        "\u2002": " ",
        "\u2003": " ",
        "\u2004": " ",
        "\u2005": " ",
        "\u2006": " ",
        "\u2007": " ",
        "\u2008": " ",
        "\u2009": " ",
        "\u200a": " ",
        "\u202f": " ",
        "\u205f": " ",
        "\u3000": " ",
        "\u200b": "",
        "\u200c": "",
        "\u200d": "",
        "\ufeff": "",
    }
)


def normalize_unicode(text: str) -> str:
    """Normalize Unicode punctuation, symbols, fractions, arrows, and spaces."""

    pre_normalized = str(text).translate(_PRE_NFKD_REPLACEMENTS)
    normalized = unicodedata.normalize("NFKD", pre_normalized)
    return normalized.translate(_POST_NFKD_REPLACEMENTS)


def estimate_tokens(text: str) -> int:
    """Estimate token count using the Rust port's four-bytes-per-token rule."""

    return math.ceil(_byte_len(text) / 4)


def cap_details(details: list[str], max_tokens: int) -> None:
    """Keep highest-priority detail bullets within the approximate token budget."""

    if not details:
        return
    total = sum(estimate_tokens(detail) for detail in details)
    if total <= max_tokens:
        return

    scored: list[tuple[int, int, int]] = []
    for index, detail in enumerate(details):
        lower = detail.lower()
        score = 0
        if (
            "security" in lower
            or "vulnerability" in lower
            or "exploit" in lower
            or "critical" in lower
            or ("fix" in lower and "crash" in lower)
        ):
            score += 100
        if "breaking" in lower or "incompatible" in lower:
            score += 90
        if "performance" in lower or "faster" in lower or "optimization" in lower:
            score += 80
        if "fix" in lower or "bug" in lower:
            score += 70
        if "api" in lower or "interface" in lower or "public" in lower:
            score += 50
        if "user" in lower or "client" in lower:
            score += 40
        if "deprecated" in lower or "removed" in lower:
            score += 35
        score += min(_byte_len(detail) // 20, 10)
        scored.append((index, score, estimate_tokens(detail)))

    budget = max(0, int(max_tokens))
    keep: list[int] = []
    for index, _score, tokens in sorted(scored, key=lambda item: item[1], reverse=True):
        if tokens <= budget:
            keep.append(index)
            budget -= tokens
    keep.sort()
    details[:] = [details[index] for index in keep]


def normalize_summary_verb(summary: str, commit_type: str) -> str:
    """Convert the first present-tense summary verb to past tense when known."""

    stripped = str(summary).strip()
    if not stripped:
        return stripped

    parts = stripped.split()
    first_word = parts[0]
    rest = " ".join(parts[1:])
    first_word_lower = first_word.lower()

    if is_past_tense_verb(first_word_lower):
        if commit_type == "refactor" and first_word_lower == "refactored":
            return _join_first_rest("restructured", rest)
        return stripped

    split = split_verb_token(first_word)
    if split is None:
        return stripped
    stem_raw, suffix = split
    stem = stem_raw.lower()
    if verb_stem(first_word) is None:
        return stripped
    if suffix and not (suffix.startswith("-") or suffix.startswith("/")):
        return stripped

    if stem == "re" and suffix.startswith("-"):
        after_dash = suffix[1:]
        inner_length = 0
        for character in after_dash:
            if not character.isascii() or not character.isalpha():
                break
            inner_length += 1
        if inner_length == 0:
            return stripped
        inner = after_dash[:inner_length].lower()
        tail = after_dash[inner_length:]
        inner_past = _past_for_presentish(inner)
        if inner_past is None:
            return stripped
        if commit_type == "refactor" and inner_past == "refactored":
            inner_past = "restructured"
        return _join_first_rest(f"re-{inner_past}{tail}", rest)

    past = _past_for_presentish(stem)
    if past is None:
        return stripped
    if commit_type == "refactor" and past == "refactored":
        past = "restructured"
    return _join_first_rest(f"{past}{suffix}", rest)


def post_process_commit_message(msg: Any, config: Any | None = None) -> Any:
    """Return a normalized conventional commit, rebuilding frozen dataclasses."""

    summary = normalize_unicode(_summary_text(msg))
    body = [normalize_unicode(str(item)) for item in getattr(msg, "body", ())]
    footers = [normalize_unicode(str(item)) for item in getattr(msg, "footers", ())]

    summary = " ".join(summary.replace("\r", " ").replace("\n", " ").split())
    summary = _trim_summary_suffix(summary.strip()).strip()
    summary = _lowercase_first_token(summary)
    summary = normalize_summary_verb(summary, _commit_type_text(msg))
    summary = _lowercase_first_token(summary.strip()).rstrip(".").strip()

    normalized_summary = _coerce_summary(getattr(msg, "summary", ""), summary, _summary_hard_limit(config))

    cleaned_body: list[str] = []
    for item in body:
        cleaned = _strip_body_prefix(item.replace("\r", " ").replace("\n", " "))
        cleaned = _trim_body_suffix(" ".join(cleaned.split()).strip()).strip()
        if not cleaned:
            continue
        cleaned = _capitalize_first_letter(cleaned)
        if not cleaned.endswith("."):
            cleaned += "."
        cleaned_body.append(cleaned)
    cap_details(cleaned_body, int(getattr(config, "max_detail_tokens", _DEFAULT_MAX_DETAIL_TOKENS)))

    if isinstance(msg, ConventionalCommit):
        return replace(msg, summary=normalized_summary, body=tuple(cleaned_body), footers=tuple(footers))
    msg.summary = normalized_summary
    msg.body = cleaned_body
    msg.footers = footers
    return msg


def format_commit_message(msg: Any) -> str:
    """Format a conventional commit object as a commit message string."""

    commit_type = _commit_type_text(msg)
    scope = _scope_text(msg)
    scope_part = f"({scope})" if scope else ""
    result = f"{commit_type}{scope_part}: {_summary_text(msg)}"

    body = [str(item) for item in getattr(msg, "body", ()) if str(item).strip()]
    if body:
        result += "\n\n" + "\n".join(f"- {item}" for item in body)

    footers = [str(item) for item in getattr(msg, "footers", ()) if str(item).strip()]
    if footers:
        result += "\n\n" + "\n".join(footers)
    return result


def _past_for_presentish(stem: str) -> str | None:
    direct = present_to_past(stem)
    if direct is not None:
        return direct
    if stem.endswith("s"):
        singular = present_to_past(stem[:-1])
        if singular is not None:
            return singular
    if stem.endswith("es"):
        singular = present_to_past(stem[:-2])
        if singular is not None:
            return singular
    if stem.endswith("ies"):
        singular = present_to_past(f"{stem[:-3]}y")
        if singular is not None:
            return singular
    return None


def _byte_len(value: str) -> int:
    return len(value.encode("utf-8"))


def _join_first_rest(first: str, rest: str) -> str:
    return first if not rest else f"{first} {rest}"


def _lowercase_first_token(text: str) -> str:
    if not text or _first_token_is_all_caps(text):
        return text
    first = text[0]
    if first.isupper():
        return f"{first.lower()}{text[1:]}"
    return text


def _first_token_is_all_caps(text: str) -> bool:
    parts = text.split(maxsplit=1)
    if not parts:
        return False
    token = parts[0]
    letters = [character for character in token if character.isalpha()]
    return bool(letters) and all(character.isupper() for character in letters)


def _capitalize_first_letter(text: str) -> str:
    if text and text[0].islower():
        return f"{text[0].upper()}{text[1:]}"
    return text


def _trim_summary_suffix(text: str) -> str:
    return text.rstrip(".;:")


def _strip_body_prefix(text: str) -> str:
    stripped = text.strip()
    return stripped.lstrip("•-*+").strip()


def _trim_body_suffix(text: str) -> str:
    return text.rstrip(".;,")


def _coerce_summary(current: Any, value: str, max_length: int) -> Any:
    if isinstance(current, str):
        return value
    factory = getattr(type(current), "from_raw", None)
    if callable(factory):
        return factory(value, max_length=max_length)
    try:
        from .models import CommitSummary
    except ImportError:
        return value
    return CommitSummary.from_raw(value, max_length=max_length)


def _summary_hard_limit(config: Any | None) -> int:
    return int(getattr(config, "summary_hard_limit", _DEFAULT_SUMMARY_HARD_LIMIT))


def _commit_type_text(msg: Any) -> str:
    return str(getattr(msg, "commit_type", getattr(msg, "type", ""))).strip().lower()


def _scope_text(msg: Any) -> str | None:
    scope = getattr(msg, "scope", None)
    if scope is None:
        return None
    return str(scope).strip().lower()


def _summary_text(msg: Any) -> str:
    summary = getattr(msg, "summary", "")
    return str(getattr(summary, "value", summary))


__all__ = [
    "cap_details",
    "estimate_tokens",
    "format_commit_message",
    "normalize_summary_verb",
    "normalize_unicode",
    "post_process_commit_message",
]

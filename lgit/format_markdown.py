"""Markdown formatting compares rendered structure and delegates labeled fences to source formatters."""

from __future__ import annotations

import re
from collections.abc import Callable, Iterator, Sequence
from dataclasses import dataclass
from difflib import SequenceMatcher

from markdown_it import MarkdownIt
from markdown_it.rules_block.table import escapedSplit
from markdown_it.token import Token

from .format_data import Event, FormatDocument, Key

_MARKDOWN = MarkdownIt("commonmark", {"inline_definitions": True}).enable(["table", "strikethrough"])
_PROSE_SPACE = re.compile(r"[ \t\n]+")
_TASK = re.compile(r"^\[[ xX]\](?=[ \t\n])")
_ALERT = re.compile(r"^\[!(?:NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][ \t]*(?:\n|$)")
_FRONT_MATTER = re.compile(r"\A---[ \t]*\r?\n.*?^(?:---|\.\.\.)[ \t]*(?:\r?\n|\Z)", re.MULTILINE | re.DOTALL)


@dataclass(slots=True)
class MarkdownDocument:
    """A parsed comparison key and the fenced blocks, in document order."""

    document: FormatDocument
    fences: list[Token]


def _inline(tokens: Sequence[Token]) -> Iterator[Key]:
    prose: list[str] = []
    for token in tokens:
        if token.type in ("text", "softbreak"):
            prose.append(token.content if token.type == "text" else " ")
            continue
        if prose:
            yield ("text", _PROSE_SPACE.sub(" ", "".join(prose)))
            prose.clear()
        yield _markdown_key(token)
    if prose:
        yield ("text", _PROSE_SPACE.sub(" ", "".join(prose)))


def _markdown_key(token: Token) -> Key:
    if token.type == "definition":
        return ("definition", token.meta["id"], token.meta["url"], token.meta["title"])
    if token.type == "fence":
        return ("fence", token.info.strip(), token.content)
    if token.type == "inline":
        task = _TASK.match(token.content)
        alert = _ALERT.match(token.content)
        return (
            "inline",
            task[0].lower() if task else "",
            bool(alert),
            tuple(_inline(token.children or ())),
        )
    return (
        token.type,
        token.tag,
        token.hidden,
        tuple(token.attrs.items()),
        tuple(_inline(token.children)) if token.children is not None else token.content,
    )


def _table_cells(line: str) -> list[str]:
    cells = escapedSplit(line.strip())
    start = 0 if cells[0] else 1
    end = len(cells) if cells[-1] else len(cells) - 1
    return cells[start:end]


def markdown_document(text: str) -> MarkdownDocument | None:
    """Parse CommonMark and GFM tables/strikethrough without normalizing literal code or HTML."""
    events: list[Event] = []
    fences: list[Token] = []
    source = text
    if front_matter := _FRONT_MATTER.match(text):
        # Treat metadata, which CommonMark would interpret as prose, as literal.
        raw = front_matter[0]
        events.append((0, ("front-matter", raw)))
        source = "\n" * raw.count("\n") + text[front_matter.end() :]
    lines = text.split("\n")
    ends: list[int | None] = []
    row = table_width = 0
    header = False
    try:
        for token in _MARKDOWN.parse(source):
            if token.map is not None:
                row = token.map[0]
            if token.nesting == 1:
                ends.append(token.map[1] - 1 if token.map is not None else None)
            elif token.nesting == -1:
                end = ends.pop()
                if end is not None:
                    row = max(row, end)
            if token.type == "fence":
                fences.append(token)
            if token.type == "thead_open":
                header = True
            elif token.type == "thead_close":
                header = False
            if token.type == "tr_open":
                cells = _table_cells(lines[row])
                if header:
                    table_width = len(cells)
                elif len(cells) > table_width:
                    # The renderer drops surplus cells; their text still belongs to the document.
                    events.append((row, ("surplus-cells", *cells[table_width:])))
            events.append((row, _markdown_key(token)))
    except RecursionError:
        return None
    return MarkdownDocument(FormatDocument.from_events(text, events), fences)


type _Replacement = tuple[int, int, int, int]


def _fence_replacements(before: Token, after: Token, formatted: str) -> list[_Replacement]:
    assert before.map is not None and after.map is not None
    old_start, new_start = before.map[0] + 1, after.map[0] + 1
    old, new, result = (
        text.removesuffix("\n").split("\n") if text else [] for text in (before.content, after.content, formatted)
    )
    # Map selected lines back to the original Markdown, keeping blockquote/list prefixes intact.
    positions = {
        i: j
        for block in SequenceMatcher(None, result, new, autojunk=False).get_matching_blocks()
        for i, j in zip(range(block.a, block.a + block.size), range(block.b, block.b + block.size), strict=True)
    }
    replacements: list[_Replacement] = []
    for op, i1, i2, j1, j2 in SequenceMatcher(None, old, result, autojunk=False).get_opcodes():
        if op == "equal":
            continue
        start = positions.get(j1, 0) if j1 != j2 else 0
        if any(positions.get(j) != start + j - j1 for j in range(j1, j2)):
            return []
        replacements.append((old_start + i1, old_start + i2, new_start + start, new_start + start + j2 - j1))
    return replacements


def format_fences(
    old: str,
    new: str,
    before: MarkdownDocument,
    after: MarkdownDocument,
    format_code: Callable[[str, str, str], str],
) -> tuple[str, MarkdownDocument]:
    """Apply proven embedded changes only where the Markdown wrapper still parses identically."""
    old_lines, new_lines = old.split("\n"), new.split("\n")
    old_keys, new_keys = ([(f.info.strip(), f.content) for f in doc.fences] for doc in (before, after))
    bodies: list[_Replacement] = []
    wrappers: list[_Replacement] = []
    formatted_fences: dict[int, str] = {}
    for op, i1, i2, j1, j2 in SequenceMatcher(None, old_keys, new_keys, autojunk=False).get_opcodes():
        if op != "replace" or i2 - i1 != j2 - j1:
            continue
        for i, j in zip(range(i1, i2), range(j1, j2), strict=True):
            old_fence, new_fence = before.fences[i], after.fences[j]
            info = old_keys[i][0]
            if not info or info != new_keys[j][0]:
                continue
            formatted = format_code(info.split(maxsplit=1)[0].lower(), old_fence.content, new_fence.content)
            if formatted == old_fence.content:
                continue
            replacements = _fence_replacements(old_fence, new_fence, formatted)
            if not replacements:
                continue
            bodies.extend(replacements)
            formatted_fences[i] = formatted
            if formatted == new_fence.content:
                assert old_fence.map is not None and new_fence.map is not None
                old_start, old_end = old_fence.map
                new_start, new_end = new_fence.map
                wrappers.append((old_start, old_end, new_start, new_end))
            else:
                wrappers.extend(replacements)

    if not bodies:
        return old, before

    expected: list[Key] = []
    index = 0
    for key in before.document.tokens:
        if key[0] == "fence":
            if index in formatted_fences:
                key = (*key[:-1], formatted_fences[index])
            index += 1
        expected.append(key)
    expected_tokens = tuple(expected)

    # A reindented list fence may need its new opener/closer along with the formatted body.
    for replacements in (bodies, wrappers) if bodies != wrappers else (bodies,):
        lines = old_lines.copy()
        for i1, i2, j1, j2 in reversed(replacements):
            lines[i1:i2] = new_lines[j1:j2]
        candidate = "\n".join(lines)
        parsed = markdown_document(candidate)
        if parsed is not None and parsed.document.tokens == expected_tokens:
            return candidate, parsed
    return old, before

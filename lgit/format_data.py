"""Canonical source tokens for JSON-family files, TOML, YAML, and CSS."""

from __future__ import annotations

import json
import re
import tomllib
from collections.abc import Iterable, Iterator, Sequence
from dataclasses import dataclass
from itertools import chain, pairwise

import tinycss2
from ruamel.yaml import YAML, YAMLError
from ruamel.yaml.events import AliasEvent, CollectionStartEvent, DocumentEndEvent, DocumentStartEvent, ScalarEvent
from ruamel.yaml.tokens import ScalarToken
from tinycss2.ast import (
    CurlyBracketsBlock,
    FunctionBlock,
    IdentToken,
    LiteralToken,
    Node,
    ParenthesesBlock,
    SquareBracketsBlock,
)
from tree_sitter import Node as SyntaxNode

from .syntax_tree import parse_packed

# Tagged keys keep a literal from impersonating a structural marker.
type Key = tuple[object, ...]
type Event = tuple[int, Key]


@dataclass(slots=True)
class FormatDocument:
    """Comparable tokens in parser order, with a source-line index for finding mixed edits."""

    tokens: tuple[Key, ...]
    rows: list[list[Key]]

    @classmethod
    def from_events(cls, text: str, events: Iterable[Event]) -> FormatDocument:
        """Index events by their zero-based source line, including positions at EOF."""
        rows: list[list[Key]] = [[] for _ in range(text.count("\n") + 1)]
        tokens: list[Key] = []
        for row, key in events:
            rows[min(row, len(rows) - 1)].append(key)
            tokens.append(key)
        return cls(tuple(tokens), rows)


_JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*"|[^\s{}\[\]:,"]+|[{}\[\]:,]|\n')


def _invalid_json_constant(value: str) -> None:
    raise ValueError(value)


def json_document(text: str) -> FormatDocument | None:
    """Compare strict JSON, preserving object member order, duplicates, and number spellings."""
    try:
        json.loads(text, parse_int=str, parse_float=str, parse_constant=_invalid_json_constant)
    except ValueError, RecursionError:
        return None
    row = 0
    events: list[Event] = []
    for match in _JSON_TOKEN.finditer(text):
        raw = match[0]
        if raw == "\n":
            row += 1
        elif raw.startswith('"'):
            events.append((row, ("string", json.loads(raw))))
        else:
            events.append((row, ("token", raw)))
    return FormatDocument.from_events(text, events)


_JSONC_TOKEN = re.compile(r'//[^\r\n]*|/\*[\s\S]*?\*/|"(?:[^"\\]|\\.)*"|[ \t\r\n]+|[{}\[\]:,]|[^ \t\r\n{}\[\]:,"/]+|.')


def jsonc_document(text: str) -> FormatDocument | None:
    """Compare JSON with comments and optional trailing commas, preserving comments and member order."""
    row = 0
    events: list[tuple[int, tuple[str, str]]] = []
    significant: list[str] = []
    comment_count = 0
    for match in _JSONC_TOKEN.finditer(text):
        raw = match[0]
        if raw.startswith(("//", "/*")):
            events.append((row, ("comment", raw)))
            comment_count += 1
        elif raw.strip(" \t\r\n"):
            if raw == "/" or raw == '"':
                return None
            if raw in ("}", "]") and significant[-1:] == [","]:
                if len(significant) < 2 or significant[-2] in "{[,:":
                    return None
                significant.pop()
                del events[-1 - comment_count]
            significant.append(raw)
            key = ("string", raw) if raw.startswith('"') else ("token", raw)
            events.append((row, key))
            comment_count = 0
        row += raw.count("\n")
    try:
        json.loads(" ".join(significant), parse_int=str, parse_float=str, parse_constant=_invalid_json_constant)
    except ValueError, RecursionError:
        return None
    return FormatDocument.from_events(
        text, ((row, (kind, json.loads(raw) if kind == "string" else raw)) for row, (kind, raw) in events)
    )


def ndjson_document(text: str) -> FormatDocument | None:
    """Compare a JSON Lines stream without allowing records to change lines or boundaries."""

    def records() -> Iterator[Event]:
        for row, line in enumerate(text.removesuffix("\n").split("\n")):
            record = json_document(line)
            if record is None:
                raise ValueError("invalid JSON Lines record")
            yield row, ("record", *record.tokens)

    if text == "":
        return FormatDocument.from_events(text, ())
    try:
        return FormatDocument.from_events(text, records())
    except ValueError:
        return None


_TOML_LITERALS = frozenset({"string", "quoted_key", "offset_date_time", "local_date_time"})


def _toml_events(node: SyntaxNode) -> Iterator[Event]:
    row = node.start_point.row
    kind = node.type
    if kind in _TOML_LITERALS or (kind != "document" and not node.child_count):
        yield row, (kind, node.text)
        return
    yield row, (kind,)
    children = node.children
    trailing = next((i for i in range(len(children) - 2, -1, -1) if children[i].type != "comment"), -1)
    for i, child in enumerate(children):
        if kind != "array" or i != trailing or child.type != ",":
            yield from _toml_events(child)
            row = child.end_point.row
    yield row, ("end", kind)


def toml_document(text: str) -> FormatDocument | None:
    """Compare valid TOML, keeping ordered keys, comments, and literal values."""
    try:
        tomllib.loads(text)
        root = parse_packed(text, "toml")
        return FormatDocument.from_events(text, _toml_events(root)) if root is not None else None
    except ValueError, RecursionError:
        return None


_YAML_COMMENT = re.compile(r"(?<!\S)#")


def _yaml_comments(parser: YAML, text: str) -> Iterator[tuple[int, int, Key]]:
    # The scanner knows which hashes belong to a quoted, plain, or block scalar. A block scalar's
    # header can still carry a comment; only the body is literal.
    spans = (
        (start, token.end_mark.index)
        for token in parser.scan(text)
        if isinstance(token, ScalarToken)
        and (
            start := token.start_mark.index
            if token.style not in ("|", ">")
            else text.find("\n", token.start_mark.index)
        )
        != -1
    )
    start, end = next(spans, (len(text), len(text)))
    row = offset = comment_end = 0
    for match in _YAML_COMMENT.finditer(text):
        pos = match.start()
        if pos < comment_end:
            continue
        while end <= pos:
            start, end = next(spans, (len(text), len(text)))
        row += text.count("\n", offset, pos)
        offset = pos
        if pos < start:
            inline = bool(text[text.rfind("\n", 0, pos) + 1 : pos].strip())
            newline = text.find("\n", pos)
            comment_end = len(text) if newline < 0 else newline
            yield pos, row, ("comment", inline, text[pos + 1 : comment_end].strip())


def yaml_document(text: str) -> FormatDocument | None:
    """Compare YAML parser events without resolving aliases, constructing objects, or losing comments."""
    parser = YAML(typ="safe", pure=True)
    events: list[tuple[int, int, Key]] = []
    try:
        for event in parser.parse(text):
            kind = type(event).__name__
            key: Key = (kind,)
            if isinstance(event, ScalarEvent):
                # Plain vs. quoted values can resolve differently under different YAML schemas.
                key += (event.anchor, event.tag, event.implicit[:2], event.value)
            elif isinstance(event, CollectionStartEvent):
                key += (event.anchor, event.tag)
            elif isinstance(event, AliasEvent):
                key += (event.anchor,)
            elif isinstance(event, DocumentStartEvent):
                key += (event.explicit, event.version, tuple(sorted((event.tags or {}).items())))
            elif isinstance(event, DocumentEndEvent):
                key += (event.explicit,)
            events.append((event.start_mark.index, event.start_mark.line, key))
        if "#" in text:
            events.extend(_yaml_comments(parser, text))
            events.sort(key=lambda event: event[0])
    except YAMLError, RecursionError, ValueError:
        return None
    return FormatDocument.from_events(text, ((row, key) for _, row, key in events))


_CHARSET = re.compile(r'\A@charset "[^"\r\n]*";')
_COMMA = frozenset({","})
_COMBINATORS = _COMMA | {">", "+", "~", "||"}
_CONDITIONS = frozenset({"@media", "@container", "@supports"})
_RULE_BLOCKS = _CONDITIONS | {
    "@keyframes",
    "@-webkit-keyframes",
    "@layer",
    "@scope",
    "@starting-style",
    "@font-face",
    "@page",
    "@property",
}
_COLOR_FUNCTIONS = frozenset({"rgb", "rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch", "color"})
_TRIM_FUNCTIONS = _COLOR_FUNCTIONS | {"calc", "min", "max", "clamp"}


def _css_event(node: Node, kind: str, *parts: object) -> Event:
    return node.source_line - 1, (kind, *parts)


def _css_trim(nodes: Sequence[Node]) -> Sequence[Node]:
    start, end = 0, len(nodes)
    if start < end and nodes[start].type == "whitespace":
        start += 1
    if start < end and nodes[end - 1].type == "whitespace":
        end -= 1
    return nodes[start:end]


def _css_values(
    nodes: Sequence[Node], separators: frozenset[str] = frozenset(), *, opaque: bool = False, condition: bool = False
) -> Iterator[Event]:
    prev = ""
    for node, following in pairwise([*nodes, None]):
        assert node is not None
        kind = node.type
        if kind == "error":
            raise ValueError("invalid CSS")
        if kind == "whitespace":
            next_literal = following.value if isinstance(following, LiteralToken) else ""
            if opaque or (prev not in separators and next_literal not in separators):
                yield _css_event(node, "space", node.serialize() if opaque else " ")
        elif isinstance(node, FunctionBlock):
            yield _css_event(node, kind, node.name)
            args = _css_trim(node.arguments) if not opaque and node.lower_name in _TRIM_FUNCTIONS else node.arguments
            yield from _css_values(args, _COMMA if node.lower_name in _COLOR_FUNCTIONS else frozenset(), opaque=opaque)
            yield _css_event(args[-1] if args else node, "end", kind)
        elif isinstance(node, (ParenthesesBlock, SquareBracketsBlock, CurlyBracketsBlock)):
            yield _css_event(node, kind)
            contents = _css_trim(node.content)
            padding = frozenset({":"}) if condition and isinstance(node, ParenthesesBlock) else frozenset()
            declaration = _css_declaration(contents) if padding else None
            literal = opaque or (declaration is not None and declaration[0].value.startswith("--"))
            if literal:
                contents = node.content
            yield from _css_values(contents, padding, opaque=literal, condition=condition)
            yield _css_event(contents[-1] if contents else node, "end", kind)
        else:
            yield _css_event(node, kind, node.serialize())
        prev = node.value if isinstance(node, LiteralToken) else ""


def _css_declaration(head: Sequence[Node]) -> tuple[IdentToken, int] | None:
    if not head or not isinstance(head[0], IdentToken):
        return None
    index = 2 if len(head) > 1 and head[1].type == "whitespace" else 1
    return (head[0], index) if index < len(head) and head[index] == ":" else None


def _css_head(nodes: Sequence[Node]) -> Iterator[Event]:
    for node in nodes:
        if node.type == "comment":
            yield _css_event(node, "comment", node.serialize())


def _css_items(nodes: Sequence[Node], *, block: bool = False) -> Iterator[Event]:
    start = 0
    for index, node in enumerate(nodes):
        if node != ";" and not isinstance(node, CurlyBracketsBlock):
            continue
        first = start
        while first < index and nodes[first].type in ("whitespace", "comment"):
            first += 1
        head = nodes[first:index]
        declaration = _css_declaration(head) if block else None
        if isinstance(node, CurlyBracketsBlock):
            if declaration is not None and declaration[0].value.startswith("--"):
                continue
            if not head:
                raise ValueError("missing CSS rule prelude")
            yield from _css_head(nodes[start:first])
            at_rule = head[0].serialize().lower() if head[0].type == "at-keyword" else ""
            yield from _css_values(
                _css_trim(head), _COMBINATORS if not at_rule else _COMMA, condition=at_rule in _CONDITIONS
            )
            yield _css_event(node, "rule")
            if not at_rule or at_rule in _RULE_BLOCKS:
                yield from _css_items(node.content, block=True)
            else:
                yield from _css_values(node.content, opaque=True)
            yield _css_event(node.content[-1] if node.content else node, "rule-end")
        else:
            yield from _css_head(nodes[start:first])
            yield from _css_statement(head, declaration)
            if declaration is None:
                yield _css_event(node, "semicolon")
        start = index + 1
    first = start
    while first < len(nodes) and nodes[first].type in ("whitespace", "comment"):
        first += 1
    yield from _css_head(nodes[start:first])
    head = nodes[first:]
    yield from _css_statement(head, _css_declaration(head) if block else None)


def _css_statement(head: Sequence[Node], declaration: tuple[IdentToken, int] | None) -> Iterator[Event]:
    if declaration is None:
        yield from _css_values(_css_trim(head))
        return
    name, colon = declaration
    yield _css_event(name, "property", name.value)
    if name.value.startswith("--"):
        yield from _css_values(head[colon + 1 :], opaque=True)
    else:
        yield from _css_values(_css_trim(head[colon + 1 :]), _COMMA)
    yield _css_event(_css_trim(head)[-1], "property-end")


def css_document(text: str) -> FormatDocument | None:
    """Compare CSS token structure without erasing selector or value whitespace that can change behavior."""
    try:
        # The CSS tokenizer normally recovers at EOF. A top-level sentinel detects unclosed blocks
        # and comments so adding their terminators cannot be mistaken for formatting.
        tokens = tinycss2.parse_component_value_list(text + "\n;--lgit-css-end")
        if len(tokens) < 2 or tokens[-2] != ";" or not isinstance(tokens[-1], IdentToken):
            return None
        if tokens[-1].value != "--lgit-css-end":
            return None
        # The inserted newline can be part of a trailing whitespace token; only the stylesheet
        # structure is compared at this outer boundary.
        events = _css_items(tokens[:-2])
        if charset := _CHARSET.match(text):
            # Only an exact byte-prefix @charset can control stylesheet decoding.
            events = chain(((0, ("encoding", charset[0])),), events)
        return FormatDocument.from_events(text, events)
    except ValueError, RecursionError:
        return None

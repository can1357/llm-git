"""Ordered formatting comparisons for infrastructure and line-based configuration files."""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Iterator
from itertools import pairwise

from tree_sitter import Node

from .format_data import Event, FormatDocument, Key
from .syntax_tree import parse_packed

_TREE_LITERALS = {
    "hcl": frozenset({"string_lit", "quoted_template", "heredoc_template"}),
    "nix": frozenset({"string_expression", "indented_string_expression", "path_expression"}),
    "proto": frozenset({"string"}),
    "graphql": frozenset({"string_value"}),
    "sql": frozenset({"literal", "comment", "marginalia"}),
}
_SPACE = " \t"
_WORD = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_$"
_SQL_OPER = b"~!@#%^&*+-=|:<>/?\\"
_SQL_LINE_WORDS = {b"go", b"delimiter", b"prompt", b"rem", b"spool"}
_HCL_TRIVIA = re.compile(rb"/\*[\s\S]*?\*/|(?://|#)[^\r\n]*|\n")
_HCL_COMMA_CLOSERS = {"tuple": "tuple_end", "function_arguments": ""}


def _word(byte: int) -> bool:
    return byte >= 128 or byte in _WORD


def _hcl_newline(source: bytes, start: int, end: int) -> bool:
    # A block comment represents a space, not a sequence of newline tokens.
    return any(match[0] == b"\n" for match in _HCL_TRIVIA.finditer(source, start, end))


def _hcl_children(node: Node, children: list[Node], source: bytes, line_bound: bool) -> list[tuple[Node, bool]]:
    children_without_comments = [child for child in children if not child.is_extra]

    def separates(left: Node, right: Node) -> bool:
        return _hcl_newline(source, left.end_byte, right.start_byte)

    kind = node.type
    if kind == "body" and any(not separates(a, b) for a, b in pairwise(children_without_comments)):
        raise ValueError("missing HCL body newline")
    if kind == "block":
        opening = next((i for i, child in enumerate(children_without_comments) if child.type == "block_start"), -1)
        if opening < 0 or len(children_without_comments) < opening + 2:
            raise ValueError("invalid HCL block")
        start, *body, end = children_without_comments[opening:]
        if any(separates(a, b) for a, b in pairwise(children_without_comments[: opening + 1])):
            raise ValueError("newline in block header")
        if body:
            items = [child for child in body[0].children if not child.is_extra]
            if items:
                if separates(start, items[0]):
                    valid = separates(items[-1], end)
                else:
                    valid = len(items) == 1 and items[0].type == "attribute" and not separates(items[0], end)
                if not valid:
                    raise ValueError("invalid HCL block body layout")
    if kind == "object" and any(
        a.type == b.type == "object_elem" and not separates(a, b) for a, b in pairwise(children_without_comments)
    ):
        raise ValueError("missing HCL object element separator")
    if kind in {"attribute", "object_elem"}:
        line_bound = True
    if kind in {"tuple", "object", "function_arguments", "for_tuple_expr"} or (
        kind == "expression" and children_without_comments and children_without_comments[0].type == "("
    ):
        line_bound = False
    if line_bound and any(
        separates(a, b) and not (kind == "function_call" and (a.type == "(" or b.type == ")"))
        for a, b in pairwise(children_without_comments)
    ):
        raise ValueError("newline in HCL line-bound expression")
    return [(child, line_bound) for child in children]


def _tree_children(node: Node, source: bytes, language: str, line_bound: bool) -> list[tuple[Node, bool]]:
    children = node.children
    if language == "hcl":
        checked = _hcl_children(node, children, source, line_bound)
        if node.type == "object":
            return [(child, bound) for child, bound in checked if child.type != ","]
        closer = _HCL_COMMA_CLOSERS.get(node.type)
    elif language == "nix" and node.type == "formals":
        checked = [(child, False) for child in children]
        closer = "}"
    else:
        return [(child, False) for child in children]
    if closer is not None:
        for i in range(len(children) - 1, -1, -1):
            if children[i].is_extra or children[i].type == closer:
                continue
            if children[i].type == ",":
                del checked[i]
            break
    return checked


def _sql_gap(previous: bytes, raw: bytes, gap: bytes, in_brackets: bool) -> Key | None:
    left, right = previous[-1], raw[0]
    if in_brackets:
        # A bracketed expression may be a quoted identifier in another SQL dialect.
        return ("bracket-gap", gap)
    if previous.lower() in _SQL_LINE_WORDS or raw.lower() in _SQL_LINE_WORDS or previous == b"/" or raw == b"/":
        return ("line-gap", b"\n" in gap, bool(gap))
    if raw.startswith(b"--"):
        return ("comment-gap", b"\n" in gap, bool(gap))
    if (
        (left in _SQL_OPER and right in _SQL_OPER)
        or left == ord(".")
        or right == ord(".")
        or (_word(left) and right in b"('`\"[")
        or (left in b"'`\"" and (_word(right) or right in b"'`\""))
        or left in b"@:$?\\"
        or right in b"@:$?\\"
    ):
        return ("sql-gap", bool(gap), b"\n" in gap if left in b"'`\"" and right in b"'`\"" else False)
    return None


def _tree_events(root: Node, source: bytes, language: str) -> Iterator[Event]:
    allowed_gap = b" \t\n\r"
    if language in {"graphql", "hcl", "nix"}:
        allowed_gap += b","

    def trivia(gap: bytes) -> bool:
        if language == "graphql" and b"\xef" in gap:
            gap = gap.replace(b"\xef\xbb\xbf", b"")
        if language == "hcl" and b"\r" in gap and b"\r" in gap.replace(b"\r\n", b""):
            return False
        return not gap.strip(allowed_gap)

    cursor = 0
    last = b""
    bracket_depth = 0
    stack = [(node, False, bound) for node, bound in reversed(_tree_children(root, source, language, False))]
    while stack:
        node, closing, line_bound = stack.pop()
        kind = node.type
        if closing:
            yield node.end_point.row, ("end", kind)
            continue
        if language == "graphql" and kind == "comma":
            continue
        if node.child_count and kind not in _TREE_LITERALS[language]:
            yield node.start_point.row, ("start", kind)
            stack.append((node, True, False))
            stack.extend(
                (child, False, bound) for child, bound in reversed(_tree_children(node, source, language, line_bound))
            )
            continue
        raw = source[node.start_byte : node.end_byte]
        gap = source[cursor : node.start_byte]
        row = node.start_point.row
        if not raw or not trivia(gap):
            raise ValueError("unexpected text between parsed tokens")
        if last:
            if _word(last[-1]) and _word(raw[0]):
                yield row, ("word-gap", bool(gap))
            if language == "sql" and (sql_gap := _sql_gap(last, raw, gap, bool(bracket_depth))) is not None:
                yield row, sql_gap
        key_raw = raw
        if language == "sql":
            if not bracket_depth and kind.startswith("keyword_"):
                key_raw = raw.lower()
            if raw == b"[":
                bracket_depth += 1
            elif raw == b"]":
                bracket_depth = max(0, bracket_depth - 1)
        yield row, (kind, key_raw)
        cursor, last = node.end_byte, raw
    if not trivia(source[cursor:]):
        raise ValueError("unparsed text after last token")


# Text Format's lexer reserves adjacent identifiers after numbers, except for float suffixes.
_HEX = "[0-9a-fA-F]"
_TEXTPROTO_ESCAPE = (
    rf"""\\(?:[abfnrtv?\\'"]|[0-7]{{1,3}}|x{_HEX}{{1,2}}|u{_HEX}{{4}}|U(?:000{_HEX}{{5}}|0010{_HEX}{{4}}))"""
)
_TEXTPROTO_TOKEN = re.compile(
    r"(?P<space>[ \t\v\f\r\n]+)|(?P<comment>\#[^\n]*)|"
    rf"""(?P<string>"(?:[^"\\\r\n\x00-\x1f]|{_TEXTPROTO_ESCAPE})*"|'(?:[^'\\\r\n\x00-\x1f]|{_TEXTPROTO_ESCAPE})*')|"""
    rf"(?P<number>0[xX]{_HEX}+|0[0-7]+|(?:(?:[1-9][0-9]*|0)(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?[fF]?)|"
    r"(?P<word>[A-Za-z_][A-Za-z_0-9]*)|(?P<symbol>[-.<>/{}\[\]:,;])"
)


def _textproto_tokens(text: str, events: list[Event]) -> Iterator[tuple[str, str]]:
    offset = row = 0
    last_word = ended = False
    while offset < len(text):
        match = _TEXTPROTO_TOKEN.match(text, offset)
        if match is None:
            raise ValueError("invalid textproto token")
        raw = match[0]
        kind = match.lastgroup or ""
        offset = match.end()
        if kind == "space":
            row += raw.count("\n")
            ended = True
        else:
            if kind == "comment":
                events.append((row, (kind, raw.removesuffix("\r") if text.startswith("\n", offset) else raw)))
                ended = True
                continue
            events.append((row, (kind, raw)))
            word = kind in {"number", "word"}
            if last_word and word and not ended:
                raise ValueError("missing textproto token separator")
            last_word, ended = word, False
            yield kind, raw


class _TextProto:
    def __init__(self, tokens: Iterator[tuple[str, str]]) -> None:
        self.tokens = tokens
        self.kind, self.raw = "", ""
        self.advance()

    def advance(self) -> None:
        self.kind, self.raw = next(self.tokens, ("", ""))

    def take(self, raw: str) -> bool:
        if self.raw != raw:
            return False
        self.advance()
        return True

    def require(self, raw: str) -> None:
        if not self.take(raw):
            raise ValueError("missing textproto punctuation")

    def name(self) -> None:
        if self.kind != "word":
            raise ValueError("missing textproto field name")
        self.advance()

    def field_name(self) -> None:
        if not self.take("["):
            self.name()
            return
        # Extensions and expanded Any fields: [package.name] or [domain/path.Name].
        self.name()
        while self.take(".") or self.take("/"):
            self.name()
        self.require("]")

    def value(self, colon: bool) -> bool:
        if self.take("{"):
            self.message("}")
            return True
        if self.take("<"):
            self.message(">")
            return True
        if not colon:
            raise ValueError("missing textproto scalar colon")
        if self.kind == "string":
            while self.kind == "string":
                self.advance()
        else:
            self.take("-")
            if self.kind not in {"number", "word"}:
                raise ValueError("invalid textproto scalar")
            self.advance()
        return False

    def message(self, closer: str = "") -> None:
        while self.raw != closer:
            self.field_name()
            colon = self.take(":")
            if self.take("["):
                if not self.take("]"):
                    messages = self.value(colon)
                    while self.take(","):
                        if self.value(colon) != messages:
                            raise ValueError("mixed textproto scalar and message list")
                    self.require("]")
            else:
                self.value(colon)
            if not self.take(","):
                self.take(";")
        if closer:
            self.advance()


def _lines(text: str) -> Iterator[tuple[int, str, str]]:
    offset = 0
    for row, match in enumerate(re.finditer(r"\r?\n|\Z", text)):
        line = text[offset : match.start()]
        if "\r" in line:
            raise ValueError("unsupported bare CR")
        if line or match[0]:
            yield row, line, match[0]
        offset = match.end()


_INI_ENTRY = re.compile(r"([^:=\r\n]+?)[ \t]*([:=])(.*)")


def _ini_events(text: str, *, editorconfig: bool = False) -> Iterator[Event]:
    previous_indent = None
    for row, raw, _ in _lines(text):
        line = raw.lstrip(_SPACE)
        if not line or line.startswith(("#", ";")):
            if line:
                yield row, ("comment", line)
            continue
        indent = len(raw) - len(line)
        if previous_indent is not None and indent > previous_indent:
            raise ValueError("ambiguous INI continuation")
        previous_indent = None
        section = line.rstrip(_SPACE)
        if section.startswith("[") and section.endswith("]") and len(section) > 2:
            yield row, ("section", section[1:-1])
            continue
        match = _INI_ENTRY.fullmatch(line)
        if match is None or line.startswith("[") or line.rstrip(_SPACE).endswith("\\"):
            raise ValueError("unsupported INI record")
        name, separator, value = match.groups()
        name = name.rstrip(_SPACE)
        if not name or (editorconfig and separator != "="):
            raise ValueError("invalid INI key")
        # A padding space before a leading '#' or ';' can decide whether this value is a comment.
        padded_value = value.lstrip(_SPACE)
        if padded_value.startswith(("#", ";")) and padded_value != value:
            padded_value = " " + padded_value
        if editorconfig and not any(char in padded_value for char in "#;"):
            padded_value = padded_value.rstrip(_SPACE)
        yield row, ("entry", name, separator, padded_value)
        previous_indent = indent


def _gitignore_events(text: str) -> Iterator[Event]:
    for row, line, _ in _lines(text):
        if line.startswith("#"):
            yield row, ("comment", line)
            continue
        end = len(line.rstrip(" "))
        if end < len(line) and _odd_backslash(line[:end]):
            end += 1  # The first trailing space is part of an escape; only later spaces are padding.
        if end:
            yield row, ("pattern", line[:end])


_GIT_C_ESCAPE = r'\\(?:[abfnrtv\\"]|[0-3][0-7]{2})'
_GIT_ATTR_PATTERN = re.compile(rf""""(?:[^"\\]|{_GIT_C_ESCAPE})*"|[^ \t"]+""")
_GIT_ATTR = re.compile(r"(?:[-!])?[A-Za-z0-9_.][A-Za-z0-9_.-]*(?:=[^ \t]*)?")


def _gitattributes_events(text: str) -> Iterator[Event]:
    for row, raw, _ in _lines(text):
        line = raw.lstrip(_SPACE)
        if not line:
            continue
        if line.startswith("#"):
            yield row, ("comment", line)
            continue
        match = _GIT_ATTR_PATTERN.match(line)
        if match is None or (match.end() < len(line) and line[match.end()] not in _SPACE):
            raise ValueError("invalid Git attribute pattern")
        yield row, ("pattern", match[0])
        for attr in re.findall(r"[^ \t]+", line[match.end() :]):
            if not _GIT_ATTR.fullmatch(attr):
                raise ValueError("invalid Git attribute")
            yield row, ("attribute", attr)
        yield row, ("record-end",)


_GIT_SECTION = re.compile(r"""\[([A-Za-z0-9.-]+)(?:[ \t]+("(?:[^"\\]|\\.)*"))?\][ \t]*([#;].*)?""")
_GIT_SETTING = re.compile(r"([A-Za-z][A-Za-z0-9-]*)[ \t]*(.*)")


def _git_value(raw: str, quoted: bool) -> tuple[str, str, bool, bool]:
    index = 0
    while index < len(raw):
        char = raw[index]
        if char == "\\":
            index += 1
            if index == len(raw):
                return raw, "", quoted, True
            if raw[index] not in 'ntb\\"':
                raise ValueError("invalid Git config escape")
        elif char == '"':
            quoted = not quoted
        elif not quoted and char in "#;":
            return raw[:index].rstrip(_SPACE), raw[index:], False, False
        index += 1
    if quoted:
        raise ValueError("unterminated Git config quote")
    return raw.rstrip(_SPACE), "", False, False


def _gitconfig_events(text: str) -> Iterator[Event]:
    section = continued = quoted = False
    for row, raw, ending in _lines(text):
        if continued:
            value, comment, quoted, continued = _git_value(raw, quoted)
            yield row, ("value-continuation", value)
        else:
            line = raw.lstrip(_SPACE)
            if not line or line.startswith(("#", ";")):
                if line:
                    yield row, ("comment", line)
                continue
            if match := _GIT_SECTION.fullmatch(line):
                name, subsection, comment = match.groups()
                yield row, ("section", name, subsection)
                section = True
            else:
                match = _GIT_SETTING.fullmatch(line)
                if not section or match is None:
                    raise ValueError("invalid Git config record")
                name, rest = match.groups()
                if rest.startswith("="):
                    value, comment, quoted, continued = _git_value(rest[1:].lstrip(_SPACE), False)
                    yield row, ("setting", name, value)
                elif not rest or rest.startswith(("#", ";")):
                    yield row, ("setting", name)
                    comment = rest
                else:
                    raise ValueError("invalid Git config assignment")
        if comment:
            yield row, ("comment", comment)
        if continued and not ending:
            raise ValueError("missing Git config continuation newline")
    if continued:
        raise ValueError("unterminated Git config continuation")


def _odd_backslash(line: str) -> bool:
    return bool((len(line) - len(line.rstrip("\\"))) % 2)


_DOTENV_ENTRY = re.compile(r"(?:(export)[ \t]+)?([A-Za-z_][A-Za-z0-9_.-]*)[ \t]*(?:=(.*)|(#.*)?)")
_DOTENV_QUOTE = {quote: re.compile(rf"(?:[^\\{quote}]|\\.)*{quote}") for quote in ("'", '"')}


def _dotenv_events(text: str) -> Iterator[Event]:
    quote = ""
    for row, raw, ending in _lines(text):
        if quote:
            if match := _DOTENV_QUOTE[quote].match(raw):
                if (tail := raw[match.end() :].lstrip(_SPACE)) and not tail.startswith("#"):
                    raise ValueError("unexpected text after dotenv quote")
                quote = ""
            yield row, ("value-continuation", raw, ending if quote else "")
            continue
        line = raw.lstrip(_SPACE)
        if not line or line.startswith("#"):
            if line:
                yield row, ("comment", line)
            continue
        match = _DOTENV_ENTRY.fullmatch(line)
        if match is None:
            raise ValueError("invalid dotenv binding")
        exported, name, value, comment = match.groups()
        if value is not None:
            stripped = value.lstrip(_SPACE)
            if stripped[:1] in _DOTENV_QUOTE:
                mark = stripped[0]
                if (end := _DOTENV_QUOTE[mark].match(stripped, 1)) is None:
                    quote = mark
                elif (tail := stripped[end.end() :].lstrip(_SPACE)) and not tail.startswith("#"):
                    raise ValueError("unexpected text after dotenv quote")
            elif _odd_backslash(value):
                raise ValueError("ambiguous unquoted dotenv continuation")
        yield row, ("binding", exported, name, value, comment, ending if quote else "")
    if quote:
        raise ValueError("unterminated dotenv quote")


_PROPERTY_KEY = re.compile(r"(?:[^\\=: \t\f]|\\.)*")
_PROPERTY_SPACE = " \t\f"


def _properties_events(text: str) -> Iterator[Event]:
    continued = False
    for row, raw, ending in _lines(text):
        line = raw.lstrip(_PROPERTY_SPACE)
        if continued:
            yield row, ("value-continuation", line)
        elif not line:
            continue
        elif line.startswith(("#", "!")):
            yield row, ("comment", line)
            continue
        elif _odd_backslash(line):
            # Split keys and delimiters only on complete logical lines: an '=' can live on the next line.
            yield row, ("continued-entry", line)
        else:
            match = _PROPERTY_KEY.match(line)
            assert match is not None
            name = match[0]
            rest = line[match.end() :].lstrip(_PROPERTY_SPACE)
            if rest.startswith(("=", ":")):
                rest = rest[1:].lstrip(_PROPERTY_SPACE)
            yield row, ("entry", name, rest)
        continued = _odd_backslash(line)
        if continued and not ending:
            raise ValueError("missing properties continuation newline")
    if continued:
        raise ValueError("unterminated properties continuation")


_LINE_PARSERS: dict[str, Callable[[str], Iterable[Event]]] = {
    "ini": _ini_events,
    "editorconfig": lambda text: _ini_events(text, editorconfig=True),
    "gitconfig": _gitconfig_events,
    "gitignore": _gitignore_events,
    "gitattributes": _gitattributes_events,
    "dotenv": _dotenv_events,
    "properties": _properties_events,
}


def infra_document(text: str, format_name: str) -> FormatDocument | None:
    """Parse ordered source events, or decline unsupported syntax without evaluating input."""
    if "\0" in text:
        return None
    try:
        if parser := _LINE_PARSERS.get(format_name):
            return FormatDocument.from_events(text, parser(text))
        if format_name in _TREE_LITERALS:
            root = parse_packed(text, format_name)
            if root is not None:
                return FormatDocument.from_events(text, _tree_events(root, text.encode(), format_name))
        if format_name == "textproto":
            events: list[Event] = []
            _TextProto(_textproto_tokens(text, events)).message()
            return FormatDocument.from_events(text, events)
    except ValueError, RecursionError:
        return None
    return None

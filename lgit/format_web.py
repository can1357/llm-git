"""Conservative source-positioned formatting comparisons for web and technical documents."""

from __future__ import annotations

import re
from collections.abc import Iterable, Iterator
from dataclasses import dataclass, field
from itertools import pairwise
from xml.parsers import expat

from markdown_it import MarkdownIt
from tree_sitter import Node

from .format_data import Event, FormatDocument, Key
from .syntax_tree import parse_packed

_XML_SPACE = b" \t\r\n"
_WEB_SPACE = _XML_SPACE + b"\f"


@dataclass(slots=True)
class _Span:
    start: int
    end: int
    keys: tuple[tuple[int, Key], ...] = ()


def _events(source: bytes, spans: Iterable[_Span]) -> Iterator[Event]:
    """Emit proven syntax keys; preserve all intervening bytes on their original lines."""
    cursor = row = 0
    for span in sorted(spans, key=lambda part: (part.start, part.end)):
        while cursor < span.start:
            newline = source.find(b"\n", cursor, span.start)
            end = span.start if newline < 0 else newline + 1
            yield row, ("raw", source[cursor:end])
            row += newline >= 0
            cursor = end
        for position, key in span.keys:
            row += source.count(b"\n", cursor, position)
            yield row, key
            cursor = position
        row += source.count(b"\n", cursor, span.end)
        cursor = span.end
    while cursor < len(source):
        newline = source.find(b"\n", cursor)
        end = len(source) if newline < 0 else newline + 1
        yield row, ("raw", source[cursor:end])
        row += 1
        cursor = end


_XML_TAG = re.compile(rb"(</?)([^ \t\r\n/>]+)")
_XML_PART = re.compile(rb"""[ \t\r\n]*(?:(/?>)|([^ \t\r\n=]+)[ \t\r\n]*=[ \t\r\n]*(?:"([^"]*)"|'([^']*)'))""")
_XML_CHILDREN = frozenset({expat.model.XML_CTYPE_CHOICE, expat.model.XML_CTYPE_SEQ, expat.model.XML_CTYPE_NAME})


def _xml_tag(source: bytes, start: int) -> tuple[_Span, bool]:
    opening = _XML_TAG.match(source, start)
    if opening is None:
        raise ValueError("unexpected XML tag")
    keys: list[tuple[int, Key]] = [(start, ("xml-tag", opening[1], opening[2]))]
    end = opening.end()
    while part := _XML_PART.match(source, end):
        end = part.end()
        if part[1] is not None:
            keys.append((part.start(1), ("tag-end", part[1])))
            return _Span(start, end, tuple(keys)), part[1] == b"/>"
        keys.append((part.start(2), ("attribute", part[2], part[3] if part[3] is not None else part[4])))
    raise ValueError("unexpected XML tag tail")


@dataclass(slots=True)
class _XmlElement:
    end: int
    empty: bool
    preserve: bool
    layout: bool
    gaps: list[_Span] = field(default_factory=list)

    def before_child(self, source: bytes, start: int) -> None:
        if not self.layout or self.end == start:
            return
        if source[self.end : start].strip(_XML_SPACE):
            self.layout = False
        else:
            self.gaps.append(_Span(self.end, start))


def _xml_spans(source: bytes) -> list[_Span]:
    # Expat supplies validated tag boundaries and the internal DTD model. Do not load any
    # external subset or parse entity replacement text; the original source stays authoritative.
    parser = expat.ParserCreate(encoding="UTF-8")
    parser.SetParamEntityParsing(expat.XML_PARAM_ENTITY_PARSING_NEVER)
    parser.ExternalEntityRefHandler = lambda *_args: 1
    element_only: set[str] = set()
    declared: set[str] = set()
    spans: list[_Span] = []
    stack: list[_XmlElement] = []
    complete_dtd = True

    def doctype(_name: str, system: str | None, _public: str | None, _internal: int) -> None:
        nonlocal complete_dtd
        complete_dtd = system is None

    def entity(_name: str, parameter: int, *_args: object) -> None:
        nonlocal complete_dtd
        if parameter:
            complete_dtd = False

    def default(raw: str) -> None:
        nonlocal complete_dtd
        if not stack and raw.startswith("%"):
            complete_dtd = False

    def declaration(name: str, model: tuple[int, int, str | None, tuple]) -> None:
        if name in declared:
            element_only.discard(name)
        elif model[0] in _XML_CHILDREN:
            element_only.add(name)
        declared.add(name)

    def start_element(name: str, attrs: dict[str, str]) -> None:
        start = parser.CurrentByteIndex
        tag, empty = _xml_tag(source, start)
        spans.append(tag)
        preserve = stack[-1].preserve if stack else False
        if stack:
            stack[-1].before_child(source, start)
        if "xml:space" in attrs:
            preserve = attrs["xml:space"] != "default"
        stack.append(_XmlElement(tag.end, empty, preserve, complete_dtd and not preserve and name in element_only))

    def end_element(_name: str) -> None:
        element = stack.pop()
        end = element.end
        if not element.empty:
            start = parser.CurrentByteIndex
            element.before_child(source, start)
            if element.layout:
                spans.extend(element.gaps)
            tag, _ = _xml_tag(source, start)
            spans.append(tag)
            end = tag.end
        if stack:
            stack[-1].end = end

    parser.StartDoctypeDeclHandler = doctype
    parser.EntityDeclHandler = entity
    parser.DefaultHandler = default
    parser.ElementDeclHandler = declaration
    parser.StartElementHandler = start_element
    parser.EndElementHandler = end_element
    parser.Parse(source, True)
    return spans


_COMPONENT_TAGS = frozenset({"start_tag", "end_tag", "self_closing_tag"})
_JSX_TAGS = frozenset({"jsx_opening_element", "jsx_closing_element", "jsx_self_closing_element"})
_TAG_ATTRIBUTES = frozenset({"attribute", "directive_attribute", "jsx_attribute", "jsx_expression"})
_COMPONENT_OPAQUE = frozenset({"frontmatter", "html_interpolation", "expression", "interpolation"})
_COMPONENT_LITERAL = frozenset(
    b"iframe listing math noembed noframes noscript plaintext script style svg textarea title xmp".split()
)


def _tree_nodes(
    root: Node, containers: frozenset[str] | None = None, *, opaque: frozenset[str] = frozenset()
) -> Iterator[Node]:
    stack = [root]
    while stack:
        node = stack.pop()
        yield node
        if node.type not in opaque and (containers is None or node.type in containers):
            stack.extend(reversed(node.named_children))


def _component_tag(tag: Node, source: bytes, offset: int = 0) -> _Span | None:
    children = tag.children
    if len(children) < 3 or children[0].type not in ("<", "</") or children[-1].type not in (">", "/>"):
        return None
    opening, name, closing = children[0], children[1], children[-1]
    if name.start_byte != opening.end_byte or (name.type != "tag_name" and name != tag.child_by_field_name("name")):
        return None
    keys: list[tuple[int, Key]] = [(offset + tag.start_byte, ("tag", opening.type, name.text))]
    previous = name
    for attr in children[2:-1]:
        gap = source[previous.end_byte : attr.start_byte]
        if not gap or gap.strip(_WEB_SPACE) or attr.type not in _TAG_ATTRIBUTES:
            return None
        equals = next((child for child in attr.children if child.type == "="), None)
        parts: tuple[bytes | None, bytes | None]
        if equals is None:
            parts = (attr.text, None)
        else:
            before = source[attr.start_byte : equals.start_byte].rstrip(_WEB_SPACE)
            after = source[equals.end_byte : attr.end_byte].lstrip(_WEB_SPACE)
            if not before or not after:
                return None
            parts = (before, after)
        keys.append((offset + attr.start_byte, (attr.type, *parts)))
        previous = attr
    gap = source[previous.end_byte : closing.start_byte]
    if gap.strip(_WEB_SPACE):
        return None
    # Slash adjacency can change the value of an unquoted HTML attribute.
    if (
        closing.type == "/>"
        and previous != name
        and not gap
        and any(child.type == "attribute_value" for child in previous.children)
    ):
        return None
    keys.append((offset + closing.start_byte, ("tag-end", closing.type)))
    return _Span(offset + tag.start_byte, offset + tag.end_byte, tuple(keys))


def _component_spans(root: Node, source: bytes, format_name: str) -> Iterator[_Span]:
    stack = [root]
    while stack:
        node = stack.pop()
        if node.type in _COMPONENT_TAGS:
            if (span := _component_tag(node, source)) is not None:
                yield span
            continue
        if node.type in _COMPONENT_OPAQUE:
            continue
        children = node.named_children
        opening = children[0] if children and children[0].type == "start_tag" else None
        if opening is not None:
            name = opening.named_child(0)
            raw_name = name.text if name is not None else None
            literal = raw_name is not None and raw_name.lower() in _COMPONENT_LITERAL
            if format_name == "astro" or (format_name == "vue" and node.parent == root):
                attr_names = {
                    name_node.text
                    for attr in opening.named_children[1:]
                    if attr.named_child_count and (name_node := attr.named_child(0)) is not None
                }
                if format_name == "astro":
                    literal = literal or b"is:raw" in attr_names
                else:
                    literal = literal or node.type != "template_element" or b"lang" in attr_names
            if literal:
                children = [opening, children[-1]] if children[-1].type == "end_tag" else [opening]
        stack.extend(reversed(children))


_CSS_CONTAINER = frozenset({"stylesheet", "rule_set", "block"})
_CSS_COMBINATOR = {"child_selector": b">", "adjacent_sibling_selector": b"+", "sibling_selector": b"~"}
_PROPERTY = rb"(?:-?[a-zA-Z_][a-zA-Z0-9_\-]*)"
_CSS_NAME = re.compile(_PROPERTY + rb"|\$[a-zA-Z_][a-zA-Z0-9_\-]*")


def _css_gap(source: bytes, left: Node, right: Node) -> Iterator[_Span]:
    start, end = left.end_byte, right.start_byte
    if "comment" not in left.type and "comment" not in right.type and not source[start:end].strip(_WEB_SPACE):
        yield _Span(start, end)


def _scss_spans(root: Node, source: bytes) -> Iterator[_Span]:
    for node in _tree_nodes(root, opaque=frozenset({"declaration", "string_value", "interpolation"})):
        children = node.children
        if node.type in _CSS_CONTAINER:
            for left, right in pairwise(children):
                yield from _css_gap(source, left, right)
        elif node.type == "declaration" and len(children) >= 3 and children[1].type == ":":
            name, colon, first = children[:3]
            if name.type != "property_name" or not _CSS_NAME.fullmatch(name.text or b""):
                continue
            value = source[colon.end_byte : node.end_byte]
            if b"\\" in value or b"//" in value:
                continue
            yield from _css_gap(source, name, colon)
            yield from _css_gap(source, colon, first)
            if children[-1].type == ";":
                yield from _css_gap(source, children[-2], children[-1])
        elif (combinator := _CSS_COMBINATOR.get(node.type)) is not None and len(children) == 3:
            left, operator, right = children
            if operator.text == combinator:
                yield from _css_gap(source, left, operator)
                yield from _css_gap(source, operator, right)


# The indented Sass and Less subsets share only a lexer: declaration and block
# boundaries are different. Strings, comments, interpolation and escapes are opaque.
_STYLE_LEX = re.compile(
    rb"""(?P<opaque>/\*[\s\S]*?\*/|//[^\r\n]*|(?i:\burl)\((?:\\[\s\S]|[^()\r\n\\"'])*\)"""
    rb"""|"(?:\\[\s\S]|[^"\\])*"|'(?:\\[\s\S]|[^'\\])*'|`(?:\\[\s\S]|[^`\\])*`"""
    rb"""|[@#$]\{[\w-]+\}|\\(?:\r\n|[\s\S]))|(?P<punct>[{}()\[\];])|(?P<unclosed>/\*|["'`\\])"""
)
_STYLE_VALUE = rb"[-#.$@%_A-Za-z0-9]+(?:[ \t]+[-#.$@%_A-Za-z0-9]+)*"
_SASS_DECL = re.compile(
    rb"(?:(?P<variable>[ \t]*\$[\w-]+)[ \t]*:[ \t]*|(?P<property>[ \t]+"
    + _PROPERTY
    + rb"):[ \t]+)(?P<value>"
    + _STYLE_VALUE
    + rb")"
)
_LESS_DECL = re.compile(
    rb"[ \t\r\n\f]*(?P<name>@[\w-]+|"
    + _PROPERTY
    + rb")[ \t]*:[ \t\r\n\f]*(?P<value>"
    + _STYLE_VALUE
    + rb")[ \t\r\n\f]*"
)
_LESS_SELECTOR = re.compile(rb"[.#&*A-Za-z_](?:[#.&*A-Za-z_0-9\-,>+~ \t\r\n]|::?(?=[A-Za-z_-]))*")
_STYLE_PAIR = {b"(": b")", b"[": b"]", b"{": b"}"}


def _sass_spans(source: bytes) -> Iterator[_Span]:
    protected: list[tuple[int, int]] = []
    groups: list[tuple[bytes, int]] = []
    for token in _STYLE_LEX.finditer(source):
        raw = token[0]
        if token.lastgroup == "unclosed":
            protected.append((groups[0][1] if groups else token.start(), len(source)))
            groups.clear()
            break
        if token.lastgroup == "opaque" and not groups:
            protected.append(token.span())
        elif token.lastgroup == "punct":
            if raw in _STYLE_PAIR:
                groups.append((_STYLE_PAIR[raw], token.start()))
            elif groups and raw == groups[-1][0]:
                _, start = groups.pop()
                if not groups:
                    protected.append((start, token.end()))
    if groups:
        protected.append((groups[0][1], len(source)))

    offset = index = 0
    comment_indent: int | None = None
    for line in source.split(b"\n"):
        start, end = offset, offset + len(line)
        offset = end + 1
        while index < len(protected) and protected[index][1] <= start:
            index += 1
        stripped = line.lstrip(b" \t")
        indent = len(line) - len(stripped)
        if comment_indent is not None:
            if not stripped.strip(_XML_SPACE) or indent > comment_indent:
                continue
            comment_indent = None
        if stripped.startswith((b"//", b"/*")):
            comment_indent = indent
        if index < len(protected) and protected[index][0] < end:
            continue
        match = _SASS_DECL.fullmatch(line.removesuffix(b"\r"))
        if match is not None:
            name = match["variable"] or match["property"]
            yield _Span(start, start + match.end(), ((start, ("sass-declaration", name, match["value"])),))


def _less_spans(source: bytes) -> Iterator[_Span]:
    groups: list[bytes] = []
    depth = 0
    start = 0
    for token in _STYLE_LEX.finditer(source):
        if token.lastgroup == "unclosed":
            return
        raw = token[0]
        if token.lastgroup != "punct":
            if not groups and raw.startswith((b"//", b"/*")) and not source[start : token.start()].strip(_WEB_SPACE):
                start = token.end()
            continue
        if groups:
            if raw in _STYLE_PAIR:
                groups.append(_STYLE_PAIR[raw])
            elif raw == groups[-1]:
                groups.pop()
            continue
        if raw in (b"(", b"["):
            groups.append(_STYLE_PAIR[raw])
            continue
        if raw == b"{":
            prelude = source[start : token.start()]
            unindented = prelude.lstrip(_WEB_SPACE)
            selector = unindented.rstrip(_WEB_SPACE)
            if _LESS_SELECTOR.fullmatch(selector) is None:
                return
            depth += 1
            row_offset = start + len(prelude) - len(unindented)
            yield _Span(start, token.end(), ((row_offset, ("less-rule", selector)), (token.start(), ("{",))))
        elif raw in (b";", b"}"):
            if (match := _LESS_DECL.fullmatch(source, start, token.start())) is not None:
                if depth or match["name"].startswith(b"@"):
                    yield _Span(
                        start,
                        token.end(),
                        (
                            (match.start("name"), ("less-declaration", match["name"], match["value"])),
                            (token.start(), (raw,)),
                        ),
                    )
            elif raw == b"}" and not source[start : token.start()].strip(_WEB_SPACE):
                yield _Span(start, token.end(), ((token.start(), (raw,)),))
            if raw == b"}":
                if not depth:
                    return
                depth -= 1
        else:
            return
        start = token.end()


_MDX_MARKDOWN = MarkdownIt("commonmark")
_MDX_START = re.compile(rb"<[A-Za-z][A-Za-z0-9_.:-]*(?=[ \t\r\n/>])")
_MDX_IMPORT = re.compile(rb"""import[ \t]+[A-Za-z_$][\w$]*[ \t]+from[ \t]+(?:'[^'\\\r\n]*'|"[^"\\\r\n]*");?[ \t]*""")
_MDX_ESM = re.compile(rb"(?m)^(?:import|export)\b")


def _mdx_spans(text: str, source: bytes) -> Iterator[_Span]:
    if re.search(rb"\r(?!\n)|\0", source):
        return
    line_starts = [0, *(match.end() for match in re.finditer(b"\n", source)), len(source)]
    for block in _MDX_MARKDOWN.parse(text):
        if block.level or block.map is None or block.type in ("fence", "code_block"):
            continue
        start, end = (line_starts[row] for row in block.map)
        raw = source[start:end].rstrip(b"\r\n")
        if _MDX_START.match(raw) and block.type in ("html_block", "paragraph_open"):
            root = parse_packed(raw.decode(), "tsx")
            if root is not None and len(root.named_children) == 1:
                statement = root.named_child(0)
                if (
                    statement is not None
                    and statement.type == "expression_statement"
                    and statement.named_child_count == 1
                ):
                    element = statement.named_child(0)
                    if (
                        element is not None
                        and element.type in ("jsx_element", "jsx_self_closing_element")
                        and element.end_byte == len(raw)
                    ):
                        for node in _tree_nodes(element, frozenset({"jsx_element"})):
                            if node.type in _JSX_TAGS and (span := _component_tag(node, raw, start)) is not None:
                                yield span
                        continue
        # CommonMark does not understand these MDX contexts. Never use it to infer
        # JSX formatting after a construct whose boundary we could not prove.
        if _MDX_IMPORT.fullmatch(raw):
            continue
        if any(mark in raw for mark in (b"{", b"}", b"<")) or _MDX_ESM.search(raw):
            return


_ADOC_TITLE = re.compile(rb"(={1,6})[ \t]+([^ \t\r\n].*)")
_ADOC_RAW = re.compile(rb"(?:-{4,}|\.{4,}|\+{4,}|/{4,})")
_ADOC_UNCERTAIN = re.compile(rb"(?:\[.*\]|[|!,:]={3,}|={4,}|\*{4,}|_{4,}|--|\+|:[^:\s]+:.*)")
_ADOC_DIRECTIVE = re.compile(rb"(?:include|ifdef|ifndef|ifeval|endif)::")


def _asciidoc_spans(source: bytes) -> Iterator[_Span]:
    offset = 0
    blank = True
    fence = b""
    lines = iter(source.split(b"\n"))
    for raw in lines:
        start = offset
        offset += len(raw) + 1
        line = raw.removesuffix(b"\r")
        if _ADOC_DIRECTIVE.match(line):
            return
        marker = line.rstrip(b" \t")
        if fence:
            if marker == fence:
                fence = b""
            continue
        if _ADOC_RAW.fullmatch(marker):
            fence, blank = marker, False
            continue
        if _ADOC_UNCERTAIN.fullmatch(marker):
            # A source block with an immediate raw delimiter is still recognizable.
            if not marker.startswith(b"[source") or not marker.endswith(b"]"):
                return
            raw = next(lines, b"")
            offset += len(raw) + 1
            marker = raw.rstrip(b" \t\r")
            if not _ADOC_RAW.fullmatch(marker):
                return
            fence, blank = marker, False
            continue
        if blank and (title := _ADOC_TITLE.fullmatch(line)) is not None:
            yield _Span(start, start + title.start(2), ((start, ("asciidoc-heading", title[1])),))
        blank = not marker


def _rst_spans(root: Node, source: bytes) -> Iterator[_Span]:
    for section in root.named_children:
        if section.type != "section":
            continue
        titles = [child for child in section.children if child.type == "title"]
        marks = [child for child in section.children if child.type == "adornment"]
        if len(titles) != 1 or len(marks) not in (1, 2):
            continue
        start = titles[0].start_byte
        title = source[source.rfind(b"\n", 0, start) + 1 : titles[0].end_byte]
        marker = marks[0].text or b""
        if not title or not title.isascii() or b"\t" in title or len(marker) < len(title) or not marker:
            continue
        if marker != marker[:1] * len(marker) or any(mark.text != marker for mark in marks):
            continue
        for mark in marks:
            yield _Span(mark.start_byte, mark.end_byte, ((mark.start_byte, ("rst-adornment", marker[:1])),))


_LATEX_COMMANDS = frozenset(
    rb"""\documentclass \usepackage \title \author \date \part \chapter \section \subsection \subsubsection
    \paragraph \subparagraph \label \ref \emph \textbf \textit \texttt \cite \frac \includegraphics""".split()
)
_LATEX_LEXICAL = re.compile(
    rb"\\(?:catcode|obeyspaces|obeylines|ExplSyntaxOn|scantokens|Verb|lstinline|mintinline)(?![A-Za-z])|\^\^"
)
_LATEX_INLINE_VERBATIM = re.compile(rb"\\verb(?![A-Za-z])")
_LATEX_CONTAINERS = frozenset({"source_file", "text", "generic_environment"})
_LATEX_BODIES = _LATEX_CONTAINERS | {
    "part",
    "chapter",
    "section",
    "subsection",
    "subsubsection",
    "paragraph",
    "subparagraph",
}


def _latex_spans(root: Node, source: bytes) -> Iterator[_Span]:
    if _LATEX_LEXICAL.search(source):
        return
    verbatim_rows = {row for row, line in enumerate(source.split(b"\n")) if _LATEX_INLINE_VERBATIM.search(line)}
    stack = [root]
    while stack:
        node = stack.pop()
        if node.type == "generic_environment":
            begin = node.child_by_field_name("begin")
            name = begin.child_by_field_name("name") if begin is not None else None
            if name is None or name.text != b"{document}":
                continue
        if node.type in _LATEX_CONTAINERS:
            stack.extend(reversed(node.named_children))
            continue
        if node.child_count < 2:
            continue
        command, argument = node.children[:2]
        if command.text not in _LATEX_COMMANDS:
            continue
        delimiter = source[argument.start_byte : argument.start_byte + 1]
        gap = source[command.end_byte : argument.start_byte]
        if delimiter in (b"{", b"[") and not gap.strip(b" \t") and command.start_point.row not in verbatim_rows:
            yield _Span(
                command.start_byte, argument.start_byte, ((command.start_byte, ("latex-command", command.text)),)
            )
        # Section nodes can also own the following document body, but groups and
        # environments that we did not identify remain opaque.
        stack.extend(reversed([child for child in node.named_children if child.type in _LATEX_BODIES]))


def web_document(text: str, format_name: str) -> FormatDocument | None:
    """Compare known layout in a web/document format while retaining opaque source verbatim."""
    source = text.encode()
    spans: Iterable[_Span]
    try:
        match format_name:
            case "xml":
                spans = _xml_spans(source)
            case "sass":
                spans = _sass_spans(source)
            case "less":
                spans = _less_spans(source)
            case "mdx":
                spans = _mdx_spans(text, source)
            case "asciidoc":
                spans = _asciidoc_spans(source)
            case "scss" | "vue" | "svelte" | "astro" | "rst" | "latex":
                root = parse_packed(text, format_name)
                if root is None:
                    return None
                if format_name == "scss":
                    spans = _scss_spans(root, source)
                elif format_name == "rst":
                    spans = _rst_spans(root, source)
                elif format_name == "latex":
                    spans = _latex_spans(root, source)
                else:
                    spans = _component_spans(root, source, format_name)
            case _:
                return None
        return FormatDocument.from_events(text, _events(source, spans))
    except expat.ExpatError, ValueError, RecursionError:
        return None

"""Source-positioned formatting comparisons for KDL and HTML."""

from __future__ import annotations

from collections.abc import Iterator

from tree_sitter import Node

from .format_data import Event, FormatDocument
from .syntax_tree import parse

_KDL_LITERAL = frozenset({"string", "number", "single_line_comment", "multi_line_comment"})


def _kdl_events(node: Node) -> Iterator[Event]:
    kind = node.type
    row = node.start_point.row
    if kind in _KDL_LITERAL or (kind != "document" and not node.child_count):
        yield row, (kind, node.text)
        return
    yield row, (kind,)
    for child in node.children:
        # These can only appear outside a literal here. The tree still preserves node
        # boundaries, so dropping a terminator cannot turn one node into two.
        if child.type == "\\" or (kind == "node" and child.type == ";"):
            continue
        yield from _kdl_events(child)
        row = child.end_point.row
    yield row, ("end", kind)


def kdl_document(text: str) -> FormatDocument | None:
    """Compare KDL layout without changing node structure, values, or slashdash comments."""
    root = parse(text, "kdl")
    if root is None:
        return None
    try:
        return FormatDocument.from_events(text, _kdl_events(root))
    except RecursionError:
        return None


_HTML_SPACE = b"\t\n\f\r "
_HTML_LAYOUT = frozenset(
    b"""
    address article aside blockquote body dd details dialog div dl dt fieldset figcaption figure footer form
    h1 h2 h3 h4 h5 h6 head header hgroup hr html li main menu nav ol p section summary table tbody td tfoot th
    thead tr ul
""".split()
)
_HTML_VOID = frozenset(b"area base br col embed hr img input link meta param source track wbr".split())
_HTML_METADATA = frozenset(b"base link meta script style title".split())
_HTML_LITERAL = frozenset(
    b"""
    iframe listing math noembed noframes noscript plaintext pre script style svg template textarea title xmp
""".split()
)
_HTML_ELEMENTS = frozenset({"element", "style_element", "script_element"})


def _tag_name(tag: Node) -> Node:
    name = tag.named_child(0)
    if name is None or name.type != "tag_name":
        raise ValueError("missing HTML tag name")
    return name


def _html_attribute(node: Node, source: bytes) -> Event:
    match node.named_children:
        case [name]:
            raw_value = None
        case [name, value]:
            if source[name.end_byte : value.start_byte].strip(_HTML_SPACE) != b"=":
                raise ValueError("invalid HTML attribute separator")
            raw_value = source[value.start_byte : value.end_byte]
            if value.type == "quoted_attribute_value":
                raw_value = raw_value[1:-1]
            elif value.type != "attribute_value" or b"`" in raw_value:
                raise ValueError("invalid unquoted HTML attribute")
            elif source[value.end_byte : value.end_byte + 2] == b"/>":
                raise ValueError("ambiguous unquoted HTML attribute before />")
        case _:
            raise ValueError("invalid HTML attribute")
    return node.start_point.row, ("attribute", name.text, raw_value)


def _html_tag(tag: Node, name: Node, source: bytes) -> Iterator[Event]:
    raw_name = name.text or b""
    prefix = b"</" if tag.type == "end_tag" else b"<"
    if source[tag.start_byte : name.start_byte] != prefix:
        raise ValueError("invalid HTML tag opener")
    yield tag.start_point.row, (prefix, raw_name)
    end = name.end_byte
    for child in tag.named_children:
        if child == name:
            continue
        if child.type != "attribute":
            raise ValueError("invalid HTML attribute")
        gap = source[end : child.start_byte]
        if not gap or gap.strip(_HTML_SPACE):
            raise ValueError("missing separator between HTML attributes")
        yield _html_attribute(child, source)
        end = child.end_byte
    tail = source[end : tag.end_byte].lstrip(_HTML_SPACE)
    if tail not in (b">", b"/>"):
        raise ValueError("invalid HTML tag closer")
    yield tag.end_point.row, ("tag-end", b">" if raw_name.lower() in _HTML_VOID else tail)


def _html_gap(gap: bytes, row: int, *, block_layout: bool) -> Iterator[Event]:
    if gap:
        if gap.strip(_HTML_SPACE):
            yield row, ("text-gap", gap)
        elif not block_layout:
            yield row, ("text-gap", b" ")


def _html_children(
    children: list[Node], source: bytes, start: int, end: int, end_row: int, *, parent: bytes
) -> Iterator[Event]:
    # Keep whitespace presence next to inline elements and text. Only block-to-block gaps
    # (including the edges of a block container) are layout under default HTML rendering.
    container = not parent or parent in _HTML_LAYOUT
    previous_block = container
    for child in children:
        if child.type == "erroneous_end_tag":
            raise ValueError("unmatched HTML end tag")
        tag = child.named_child(0) if child.type in _HTML_ELEMENTS else None
        name = _tag_name(tag) if tag is not None else None
        name_lower = (name.text or b"").lower() if name is not None else b""
        styled = tag is not None and _inline_style(tag)
        block = not styled and (
            name_lower in _HTML_LAYOUT
            or child.type == "doctype"
            or (parent == b"head" and name_lower in _HTML_METADATA)
        )
        row = child.start_point.row
        yield from _html_gap(source[start : child.start_byte], row, block_layout=previous_block and block)
        if tag is not None and name is not None:
            yield from _html_element(
                child, source, tag, name, name_lower, literal=styled or name_lower in _HTML_LITERAL
            )
        else:
            yield row, (child.type, child.text)
        previous_block = block
        start = child.end_byte
    yield from _html_gap(source[start:end], end_row, block_layout=previous_block and container)


def _inline_style(tag: Node) -> bool:
    for attribute in tag.named_children:
        if attribute.type == "attribute" and (name := attribute.named_child(0)) is not None:
            if (name.text or b"").lower() == b"style":
                return True
    return False


def _html_element(
    node: Node, source: bytes, tag: Node, name: Node, name_lower: bytes, *, literal: bool
) -> Iterator[Event]:
    children = node.named_children
    yield from _html_tag(tag, name, source)
    if tag.type == "self_closing_tag" and name_lower not in _HTML_VOID:
        yield tag.end_point.row, ("element-end",)
        return
    close = children[-1] if children[-1].type == "end_tag" else None
    end = close.start_byte if close is not None else node.end_byte
    end_row = close.start_point.row if close is not None else node.end_point.row
    if literal:
        yield tag.end_point.row, ("literal-body", source[tag.end_byte : end])
    else:
        yield from _html_children(
            children[1 : -1 if close is not None else None],
            source,
            tag.end_byte,
            end,
            end_row,
            parent=name_lower,
        )
    if close is not None:
        yield from _html_tag(close, _tag_name(close), source)
    yield end_row, ("element-end",)


def html_document(text: str) -> FormatDocument | None:
    """Compare HTML tag layout while preserving text, inline spacing, and literal bodies."""
    source = text.encode()
    root = parse(source, "html")
    if root is None:
        return None
    try:
        events = _html_children(root.named_children, source, 0, len(source), text.count("\n"), parent=b"")
        return FormatDocument.from_events(text, events)
    except ValueError, RecursionError:
        return None

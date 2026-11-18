"""Source-positioned canonical tokens for syntax-sensitive formatting rules."""

from __future__ import annotations

import ast
import re
import warnings
from collections.abc import Iterator
from dataclasses import dataclass

from tree_sitter import Node

_JS = frozenset({"javascript", "typescript", "tsx"})
SUPPORTED = _JS | {"python", "rust", "go"}
_COMMENTS = frozenset({"comment", "line_comment", "block_comment"})
_JS_OBJECT_TYPES = frozenset({"interface_body", "object_type"})
_GO_SEPARATORS = frozenset(
    {
        "source_file",
        "statement_list",
        "import_spec_list",
        "const_declaration",
        "var_spec_list",
        "type_declaration",
        "field_declaration_list",
        "interface_type",
    }
)
_RUST_ARM_VALUES = frozenset(
    {
        "scoped_identifier",
        "integer_literal",
        "float_literal",
        "boolean_literal",
        "char_literal",
        "string_literal",
        "raw_string_literal",
        "unit_expression",
    }
)
_CLOSING = frozenset({")", "]", "}", ">"})
_GO_NUMBER = re.compile(r"^0([BOX])|([EP])(?=[+-]?[0-9_]+i?$)")
_JS_QUOTE = re.compile(rb"\\.|[\x22\x27]", re.DOTALL)

# Canonical tokens are tagged so literal text cannot impersonate syntax or a structural marker.
type Token = tuple[str, bytes]
type Rows = list[tuple[Token, ...]]
type Event = tuple[int, Token]


@dataclass(slots=True)
class FormatTree:
    """Comparable AST events, indexed by source line for extracting formatting from mixed edits."""

    rows: Rows
    tokens: tuple[Token, ...]
    python: str | None


def _python_ast(text: bytes | str, *, expression: bool = False) -> ast.AST | None:
    try:
        # Compiling a comparison key must not print escape warnings from source files.
        with warnings.catch_warnings(action="ignore", category=SyntaxWarning):
            return ast.parse(text, mode="eval" if expression else "exec", type_comments=not expression)
    except SyntaxError, ValueError, RecursionError:
        return None


def _python_structure(text: str) -> str | None:
    root = _python_ast(text)
    if root is None:
        return None
    for node in ast.walk(root):
        if isinstance(node, (ast.Import, ast.ImportFrom)) and getattr(node, "module", None) != "__future__":
            bindings = {alias.asname or alias.name.split(".", 1)[0] for alias in node.names}
            if len(bindings) == len(node.names):
                node.names.sort(key=lambda alias: (alias.name, alias.asname or ""))
    try:
        return ast.dump(root)
    except RecursionError:
        return None


def _text(node: Node) -> bytes:
    return node.text or b""


def _event(node: Node, kind: str, value: bytes = b"") -> Event:
    return node.start_point.row, (kind, value)


def _js_string(node: Node) -> bytes:
    text = _text(node)
    # JSX attributes don't interpret JavaScript escapes.
    if node.parent is not None and node.parent.type == "jsx_attribute":
        return text[1:-1]

    def unquote(match: re.Match[bytes]) -> bytes:
        part = match[0]
        return part[1:] if part in (b"\\'", b'\\"') else part

    return _JS_QUOTE.sub(unquote, text[1:-1])


def _jsx_text(text: bytes) -> bytes:
    # JSX's transform keeps spaces on the first/last physical lines next to inline siblings,
    # trims indentation next to line breaks, and joins the nonempty lines with one space.
    lines = text.replace(b"\r\n", b"\n").replace(b"\r", b"\n").replace(b"\t", b" ").split(b"\n")
    if len(lines) == 1:
        return lines[0]
    parts = (lines[0].rstrip(b" "), *(line.strip(b" ") for line in lines[1:-1]), lines[-1].lstrip(b" "))
    return b" ".join(filter(None, parts))


def _jsx_space(node: Node) -> bool:
    if node.type != "jsx_expression" or node.named_child_count != 1:
        return False
    child = node.named_child(0)
    return child is not None and child.type == "string" and _text(child) in (b"' '", b'" "')


def _jsx_children(node: Node, grammar: str) -> Iterator[Event]:
    opening = node.child_by_field_name("open_tag")
    name = opening.child_by_field_name("name") if opening is not None else None
    # Intrinsic elements render adjacent static text together. Component children must retain
    # their shape: two text children and one text child are not interchangeable props.
    host = name is not None and name.type == "identifier" and _text(name)[:1].islower()
    parts: list[bytes] = []
    row = 0
    for child in node.children:
        if child.type == "jsx_text" or _jsx_space(child):
            text = _jsx_text(_text(child)) if child.type == "jsx_text" else b" "
            if text:
                if not host:
                    yield _event(child, "jsx_text", text)
                    continue
                if not parts:
                    row = child.start_point.row
                parts.append(text)
            continue
        if parts:
            yield row, ("jsx_text", b"".join(parts))
            parts.clear()
        yield from _events(child, grammar)


def _sorted_items(node: Node, items: list[Node], grammar: str) -> Event:
    # Keep alias pairs and nested paths intact. Sorting full keys preserves multiplicity.
    keys = sorted(tuple(token for _, token in _events(item, grammar)) for item in items)
    return _event(node, "import_list", repr(keys).encode())


def _python_import(node: Node, children: list[Node]) -> Iterator[Event]:
    names = node.children_by_field_name("name")
    if names and not any(child.type in _COMMENTS for child in children):
        first = names[0].start_byte
        for child in children:
            if child.start_byte >= first or child.type == "(":
                break
            yield from _events(child, "python")
        yield _sorted_items(names[0], names, "python")
        return
    for child in children:
        if child.type not in {"(", ")"}:
            yield from _events(child, "python")


def _ordinary_children(node: Node, children: list[Node], grammar: str) -> Iterator[Event]:
    code = (i for i in range(len(children) - 1, -1, -1) if children[i].type not in _COMMENTS)
    last = next(code, -1)
    trailing_comma = next(code, -1) if last >= 0 and children[last].type in _CLOSING else -1
    for i, child in enumerate(children):
        kind = child.type
        if grammar in _JS:
            if kind == ";" and node.type != "for_statement" and node.type != "empty_statement":
                continue
            if kind == "," and node.type in _JS_OBJECT_TYPES:
                continue
        elif grammar == "go" and kind == ";" and node.type in _GO_SEPARATORS:
            continue
        if kind == "," and i == trailing_comma:
            tuple_like = node.type in {"tuple_expression", "tuple_type", "tuple_pattern", "tuple"}
            if not tuple_like and (node.type != "array" or i > 0 and children[i - 1].is_named):
                continue
        if grammar == "python" and node.type in {"parenthesized_expression", "with_clause"} and kind in {"(", ")"}:
            continue
        yield from _events(child, grammar)


def _drop_leading_semicolon(children: list[Node]) -> None:
    statements = ((i, child) for i, child in enumerate(children) if child.type not in _COMMENTS | {"hash_bang_line"})
    first = next(statements, None)
    if first is None or first[1].type != "empty_statement":
        return
    following = next(statements, None)
    if following is not None and following[1].type == "expression_statement":
        expr = next((child for child in following[1].named_children if child.type not in _COMMENTS), None)
        if expr is not None and expr.type == "string":
            return  # The semicolon keeps this string out of the directive prologue.
    del children[first[0]]


def _events(node: Node, grammar: str) -> Iterator[Event]:
    kind = node.type
    children = node.children
    if kind in _COMMENTS:
        comment = _text(node)
        if not (grammar == "go" and comment.startswith((b"//go:", b"//line "))):
            comment = b" ".join(comment.split())
        yield _event(node, kind, comment)
        return
    if grammar == "python":
        if kind in {"string", "concatenated_string", "type"}:
            parsed = _python_ast(_text(node), expression=True)
            if parsed is not None:
                yield _event(node, "type" if kind == "type" else "string", ast.dump(parsed).encode())
                for descendant in node.named_children:
                    yield from _comments(descendant)
                return
        if kind in {"import_statement", "import_from_statement"}:
            yield from _python_import(node, children)
            return
        if kind in {"parenthesized_expression", "with_clause"}:
            yield from _ordinary_children(node, children, grammar)
            return
        if kind == "line_continuation":
            return
    elif grammar in _JS:
        if kind == "string":
            yield _event(node, kind, _js_string(node))
            return
        if kind == "jsx_element":
            yield from _jsx_children(node, grammar)
            return
        if (
            kind == "parenthesized_expression"
            and len(children) == 3
            and children[1].type
            in {
                "jsx_element",
                "jsx_self_closing_element",
            }
        ):
            yield from _events(children[1], grammar)
            return
        if (
            kind == "formal_parameters"
            and len(children) in (3, 4)
            and node.parent is not None
            and node.parent.type == "arrow_function"
        ):
            param = children[1]
            if param.type == "required_parameter" and param.child_count == 1:
                param = param.children[0]
            if param.type == "identifier" and (len(children) == 3 or children[2].type == ","):
                yield from _events(param, grammar)
                return
    elif grammar == "go":
        if kind in {"int_literal", "float_literal", "imaginary_literal"}:
            number = _GO_NUMBER.sub(lambda match: match[0].lower(), _text(node).decode())
            yield _event(node, kind, number.encode())
            return
        if kind == "parenthesized_expression" and len(children) == 3:
            yield from _events(children[1], grammar)
            return

    if kind in {"named_imports", "use_list"} and not any(child.type in _COMMENTS for child in children):
        yield _sorted_items(node, node.named_children, grammar)
        return
    if grammar == "rust" and kind == "match_arm":
        yield _event(node, kind)
        value = node.child_by_field_name("value")
        for child in children:
            if child.type == ",":
                continue
            if child == value and child.type == "block" and child.child_count == 3:
                tail = child.children[1]
                if tail.type in _RUST_ARM_VALUES:
                    child = tail
            yield from _events(child, grammar)
        yield node.end_point.row, ("/" + kind, b"")
        return
    if not children:
        yield _event(node, kind, _text(node))
        return

    # Python has an independent whole-file AST check; its comparison stream can ignore wrapper
    # nodes to recognize comments moved outside parentheses without losing their token order.
    if grammar != "python":
        yield _event(node, kind)
    if grammar in _JS and kind == "program":
        _drop_leading_semicolon(children)
    yield from _ordinary_children(node, children, grammar)
    if grammar != "python":
        yield node.end_point.row, ("/" + kind, b"")


def _comments(node: Node) -> Iterator[Event]:
    if node.type in _COMMENTS:
        yield from _events(node, "python")
        return
    for child in node.named_children:
        yield from _comments(child)


def format_tree(root: Node, text: str, grammar: str) -> FormatTree | None:
    """Build canonical line events from a complete source tree."""
    python = _python_structure(text) if grammar == "python" else None
    if grammar == "python" and python is None:
        return None
    rows: list[list[Token]] = [[] for _ in range(text.count("\n") + 1)]
    tokens: list[Token] = []
    try:
        for row, token in _events(root, grammar):
            rows[row].append(token)
            tokens.append(token)
    except RecursionError:
        return None
    return FormatTree([tuple(row) for row in rows], tuple(tokens), python)

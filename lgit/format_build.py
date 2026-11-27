"""Conservative source-positioned comparisons for shell and build files."""

from __future__ import annotations

import ast
import json
import re
import warnings
from collections.abc import Iterator

from tree_sitter import Node

from .format_data import Event, FormatDocument, Key
from .syntax_tree import parse_packed

_GRAMMARS = {
    "shell": "bash",
    "fish": "fish",
    "powershell": "powershell",
    "make": "make",
    "cmake": "cmake",
    "starlark": "python",
}
_OPAQUE = {
    "bash": frozenset(
        {
            "string",
            "raw_string",
            "ansi_c_string",
            "translated_string",
            "regex",
            "extglob_pattern",
            "expansion",
            "heredoc_redirect",
        }
    ),
    "fish": frozenset(
        {"single_quote_string", "double_quote_string", "escape_sequence", "brace_expansion", "variable_expansion"}
    ),
    "powershell": frozenset({"string_literal", "generic_token", "stop_parsing", "verbatim_command_argument"}),
    "make": frozenset(
        {
            "recipe",
            "text",
            "raw_text",
            "ifeq_directive",
            "ifneq_directive",
            "define_directive",
            "function_call",
            "variable_reference",
        }
    ),
    "cmake": frozenset({"argument", "bracket_comment"}),
    "python": frozenset({"string", "concatenated_string"}),
}
_COMMENTS = frozenset({"comment", "line_comment", "block_comment", "bracket_comment"})
_SHELL_OPERATORS = frozenset({"|", "|&", "&&", "||", ";", "&", ";;", ";&", ";;&"})
_REDIRECT_OPERATORS = frozenset({"<", ">", ">>", "<&", ">&", ">|", "&>", "&>>", "<>"})
_MAKE_OPERATORS = frozenset({":", "::", "&:", "&::", "=", ":=", "::=", ":::=", "?=", "+=", "!=", "|"})
_PS_EXPRESSIONS = frozenset(
    {
        "assignment_expression",
        "additive_expression",
        "multiplicative_expression",
        "comparison_expression",
        "logical_expression",
        "bitwise_expression",
        "format_expression",
        "range_expression",
    }
)
_PS_BLOCKS = frozenset({"statement_block", "if_statement", "else_clause", "elseif_clause"})
_PYTHON_COMMA_LISTS = frozenset({"argument_list", "parameters", "list", "dictionary", "set"})
_LAYOUT = b" \t\r\n"
_DOCKER_HEAD = re.compile(r"[ \t]*(?P<command>[a-zA-Z]+)[ \t]+(?P<args>.*)")
_DOCKER_SPACE = re.compile(r"[ \t]+")
_DOCKER_DIRECTIVE = re.compile(r"[ \t]*#[ \t]*(?:escape|syntax|check)[ \t]*=", re.IGNORECASE)
_DOCKER_ESCAPE = re.compile(r"[ \t]*#[ \t]*escape[ \t]*=[ \t]*`", re.IGNORECASE)
_DOCKER_JSON = frozenset({"ADD", "CMD", "COPY", "ENTRYPOINT", "RUN", "SHELL", "VOLUME"})
_DOCKER_ARRAY_STRING = re.compile(r'"(?:[^"\\]|\\.)*"')
_DOCKER_BARE_ARGS = frozenset({"ADD", "COPY", "EXPOSE", "FROM"})
_DOCKER_BARE = re.compile(r"[^ \t\r\n'\"\\#$`]+(?:[ \t]+[^ \t\r\n'\"\\#$`]+)*")
_DOCKER_PAIRS = re.compile(r"[\w.-]+=[^ =\t\r\n'\"\\#$`]+(?:[ \t]+[\w.-]+=[^ =\t\r\n'\"\\#$`]+)*", re.ASCII)


def _docker_json(raw: str) -> Key | None:
    if not raw.startswith("["):
        return None
    try:
        value = json.loads(raw)
    except ValueError, RecursionError:
        return None
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        return None
    return ("json", *(match[0] for match in _DOCKER_ARRAY_STRING.finditer(raw)))


def _docker_document(text: str) -> FormatDocument | None:
    lines = text.split("\n")
    # The bundled grammar lacks the ADD/COPY JSON alternative. Substitute only independently
    # validated JSON arrays, then require every substitution to be a standalone instruction.
    json_paths: dict[int, Key] = {}
    grammar_lines: list[str] | None = None
    for row, line in enumerate(lines):
        if (match := _DOCKER_HEAD.fullmatch(line.removesuffix("\r"))) is None:
            continue
        if match["command"].upper() in {"ADD", "COPY"} and (key := _docker_json(match["args"])) is not None:
            json_paths[row] = key
            if grammar_lines is None:
                grammar_lines = lines.copy()
            grammar_lines[row] = line[: match.start("args")] + "lgit-src lgit-dst"
    root = parse_packed("\n".join(grammar_lines) if grammar_lines is not None else text, "dockerfile")
    if root is None:
        return None

    events: list[Event] = []
    row = prefix_start = 0
    in_prefix = True
    for node in root.named_children:
        start, end = node.start_point.row, node.end_point.row
        if end > start and node.end_point.column == 0:
            end -= 1
        if start < row or any(line.strip(" \t\r") for line in lines[row:start]):
            # A token on an already consumed physical line can be an inline hash that the
            # Docker grammar calls a comment even though Docker treats it as an argument.
            return None
        if node.type == "comment":
            line = lines[start]
            if in_prefix and _DOCKER_ESCAPE.match(line):
                # A non-default escape character changes instruction and continuation lexing.
                return None
            if in_prefix and _DOCKER_DIRECTIVE.match(line):
                events.append((start, ("parser-directive", *lines[prefix_start : start + 1])))
                prefix_start = start + 1
            else:
                events.append((start, ("comment", line.lstrip(" \t"))))
        else:
            in_prefix = False
            line = lines[start]
            match = _DOCKER_HEAD.fullmatch(line.removesuffix("\r"))
            if start == end and match is not None and node.type.endswith("_instruction"):
                command, args = match["command"].upper(), match["args"]
                if start in json_paths:
                    if node.type != command.lower() + "_instruction":
                        return None
                    events.append((start, (command, *json_paths.pop(start))))
                elif command in _DOCKER_JSON and (key := _docker_json(args)) is not None:
                    events.append((start, (command, *key)))
                elif command in _DOCKER_BARE_ARGS and _DOCKER_BARE.fullmatch(args.rstrip(" \t")):
                    events.append((start, (command, "words", *_DOCKER_SPACE.split(args.rstrip(" \t")))))
                elif command in {"ENV", "LABEL"} and _DOCKER_PAIRS.fullmatch(args.rstrip(" \t")):
                    events.append((start, (command, "values", *_DOCKER_SPACE.split(args.rstrip(" \t")))))
                else:
                    # The inherited SHELL may not be POSIX. Preserve command text, old-style
                    # ENV values, options using quotes, and other unclassified argument text.
                    events.append((start, (command, "raw", args, line.endswith("\r"))))
            else:
                events.extend((i, ("docker-raw", lines[i])) for i in range(start, end + 1))
        row = end + 1
    if json_paths or any(line.strip(" \t\r") for line in lines[row:]):
        return None
    return FormatDocument.from_events(text, events)


class _TreeEvents:
    def __init__(self, source: bytes, grammar: str) -> None:
        self.source = source
        self.grammar = grammar

    def _gap(self, parent: Node, left: Node | None, right: Node | None, start: int, end: int) -> Key | None:
        raw = self.source[start:end]
        if not raw:
            return None
        grammar = self.grammar
        if raw.strip(_LAYOUT) or (b"\r" in raw and (grammar in {"bash", "fish"} or b"\r" in raw.replace(b"\r\n", b""))):
            # External scanners may consume escapes and carriage returns that the shell sees.
            # Keep the seam, including spaces that separate words around an escaped newline.
            return "raw-gap", raw
        if right is not None and grammar in {"make", "python"}:
            line_start = end - right.start_point.column
            if line_start >= start:
                indent = self.source[line_start:end]
                if indent and (grammar == "make" or b"\t" in indent):
                    # A recipe prefix changes the interpretation of an entire Make line.
                    # Starlark implementations need not agree on the meaning of tabs.
                    return "indent", indent, b"\n" in raw if left is not None else False
        if left is None or right is None:
            return None
        line_break = b"\n" in raw
        if right.type in _COMMENTS or left.type in _COMMENTS:
            if line_break:
                return ("comment-break",)
            return None
        if grammar == "python":
            return None
        if grammar == "cmake":
            if parent.type in {"source_file", "body"}:
                return ("command-break",) if line_break else None
            if left.type == "(" or right.type == ")" or (right.type == "(" and parent.type.endswith("_command")):
                return None
            return ("argument-gap",)
        if line_break:
            return ("line-break",)
        if grammar == "make":
            if parent.type in {"variable_assignment", "rule"} and (
                left.type in _MAKE_OPERATORS or right.type in _MAKE_OPERATORS
            ):
                return None
        elif grammar == "powershell":
            if parent.type in _PS_EXPRESSIONS or parent.type in _PS_BLOCKS:
                return None
            if left.type in {"|", "&&", "||"} or right.type in {"|", "&&", "||"}:
                return None
            if left.type == "command_argument_sep" or right.type == "command_argument_sep":
                return None
        else:
            if left.type in _SHELL_OPERATORS or right.type in _SHELL_OPERATORS:
                return None
            if parent.type == "file_redirect" and left.type in _REDIRECT_OPERATORS:
                return None
            if grammar == "bash" and parent.type == "function_definition" and (left.type == ")" or right.type == "("):
                return None
        return ("space",)

    def _children(self, node: Node) -> list[Node]:
        children = node.children
        if self.grammar == "powershell" and children:
            tail = children[-1]
            if tail.type == "command_elements" or (
                node.type == "command_elements" and tail.type == "command_argument_sep"
            ):
                raw = self.source[tail.start_byte : tail.end_byte]
                if raw and not raw.strip(b" \t"):
                    children.pop()
        return children

    def events(self, node: Node, *, root: bool = False) -> Iterator[Event]:
        row, kind = node.start_point.row, node.type
        if not node.child_count or kind in _OPAQUE[self.grammar] or kind in _COMMENTS or kind == "command_argument_sep":
            raw = self.source[node.start_byte : node.end_byte]
            if root:
                padding = b" \t\n" if self.grammar in {"bash", "fish"} else _LAYOUT
                raw = self.source
                if not raw.strip(padding):
                    return
            if kind == "command_argument_sep" and raw and not raw.strip(b" \t"):
                raw = b" "
            elif self.grammar == "fish" and kind == "\n" and not raw.strip(b"\n"):
                raw = b"\n"
            yield row, ("token", kind, raw)
            return
        if not root:
            yield row, ("open", kind)
        left: Node | None = None
        start = 0 if root else node.start_byte
        children = self._children(node)
        ignored_comma = _trailing_comma(children) if self.grammar == "python" and kind in _PYTHON_COMMA_LISTS else None
        for child in children:
            if key := self._gap(node, left, child, start, child.start_byte):
                yield child.start_point.row, key
            start = child.end_byte
            if child == ignored_comma:
                continue
            yield from self.events(child)
            left = child
        if key := self._gap(node, left, None, start, len(self.source) if root else node.end_byte):
            yield node.end_point.row, key
        if not root:
            yield node.end_point.row, ("close", kind)


def _trailing_comma(children: list[Node]) -> Node | None:
    # The AST guard also checks that the enclosing list retains its meaning.
    at_end = False
    for child in reversed(children):
        if child.type in {")", "]", "}"}:
            at_end = True
        elif child.type not in _COMMENTS:
            return child if at_end and child.type == "," else None
    return None


def _ast_events(text: str) -> list[Event] | None:
    try:
        with warnings.catch_warnings(action="ignore", category=SyntaxWarning):
            module = ast.parse(text, type_comments=True)
        # AST dumps guard indentation and parser ambiguities without applying Python's rewrites.
        # Separate statement keys let unrelated formatting survive mixed edits in other rules.
        events: list[Event] = [(node.lineno - 1, ("python-statement", ast.dump(node))) for node in module.body]
        events.extend((node.lineno - 1, ("type-ignore", node.tag)) for node in module.type_ignores)
        return events
    except SyntaxError, ValueError, RecursionError:
        return None


def build_document(text: str, format_name: str) -> FormatDocument | None:
    """Compare shell and build layout without evaluating code, expanding variables, or running recipes."""
    if format_name == "dockerfile":
        return _docker_document(text)
    grammar = _GRAMMARS.get(format_name)
    if grammar is None or (root := parse_packed(text, grammar)) is None:
        return None
    python = _ast_events(text) if grammar == "python" else []
    if python is None:
        return None

    def events() -> Iterator[Event]:
        if grammar in {"bash", "fish"} and text.startswith("#!"):
            yield 0, ("shebang",)
        yield from _TreeEvents(text.encode(), grammar).events(root, root=True)
        yield from python

    try:
        return FormatDocument.from_events(text, events())
    except RecursionError:
        return None

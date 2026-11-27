"""Parse-tree views of source files for order-insensitive formatting detection.

Formatters and linters reorder runs of sibling import-like statements (`use`, `import`, `mod x;`).
The line lexer in [`reformat`](reformat.py) cannot see that such a reorder is formatting-only;
[`unordered_runs`] finds the runs so it can compare them as multisets instead of sequences.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from functools import cache
from typing import TypeIs, get_args

import tree_sitter_c
import tree_sitter_c_sharp
import tree_sitter_cpp
import tree_sitter_go
import tree_sitter_html
import tree_sitter_java
import tree_sitter_javascript
import tree_sitter_kdl
import tree_sitter_kotlin
import tree_sitter_python
import tree_sitter_rust
import tree_sitter_swift
import tree_sitter_typescript
from tree_sitter import Language, Node, Parser
from tree_sitter_language_pack import SupportedLanguage, get_parser

# A run item: `[start, end)` source lines, covering leading comments/attributes and a trailing comment.
type Item = tuple[int, int]


@dataclass(frozen=True, slots=True)
class _Grammar:
    language: Callable[[], object]
    # Node types of the statements whose order is insignificant.
    items: frozenset[str] = frozenset()
    # Whether a blank line ends a run (C include blocks are sorted per block, never across blocks).
    blank_breaks: bool = False


_JS_ITEMS = frozenset({"import_statement"})
_GRAMMARS = {
    "rust": _Grammar(tree_sitter_rust.language, frozenset({"use_declaration", "extern_crate_declaration", "mod_item"})),
    "javascript": _Grammar(tree_sitter_javascript.language, _JS_ITEMS),
    "typescript": _Grammar(tree_sitter_typescript.language_typescript, _JS_ITEMS),
    "tsx": _Grammar(tree_sitter_typescript.language_tsx, _JS_ITEMS),
    "python": _Grammar(tree_sitter_python.language, frozenset({"import_statement", "import_from_statement"})),
    "go": _Grammar(tree_sitter_go.language, frozenset({"import_spec"})),
    "java": _Grammar(tree_sitter_java.language, frozenset({"import_declaration"})),
    "kotlin": _Grammar(tree_sitter_kotlin.language, frozenset({"import"})),
    "swift": _Grammar(tree_sitter_swift.language, frozenset({"import_declaration"})),
    "c_sharp": _Grammar(tree_sitter_c_sharp.language, frozenset({"using_directive"})),
    "c": _Grammar(tree_sitter_c.language, frozenset({"preproc_include"}), blank_breaks=True),
    "cpp": _Grammar(tree_sitter_cpp.language, frozenset({"preproc_include"}), blank_breaks=True),
    "html": _Grammar(tree_sitter_html.language),
    "kdl": _Grammar(tree_sitter_kdl.language),
}
# Nodes that belong to the item they precede (or, on the same line, follow).
_LEADERS = frozenset({"comment", "line_comment", "block_comment", "multiline_comment", "attribute_item"})


@cache
def _parser(grammar: str) -> Parser:
    return Parser(Language(_GRAMMARS[grammar].language()))


@dataclass(slots=True)
class _Run:
    items: list[Item]
    # Last source line of the final item, for attaching a trailing comment.
    last_row: int = -1


def _last_row(node: Node) -> int:
    row, column = node.end_point
    return row - 1 if column == 0 and row > node.start_point.row else row


def _orders(node: Node, grammar: str) -> bool:
    """Whether the order of `node` among its siblings is insignificant."""

    if node.type not in _GRAMMARS[grammar].items:
        return False
    if grammar == "rust" and node.type == "mod_item":
        return node.child_by_field_name("body") is None
    if grammar in ("javascript", "typescript", "tsx"):
        # `import "x";` runs for its side effects, so its position matters.
        return any(child.type == "import_clause" for child in node.children)
    return True


def _runs_of(parent: Node, grammar: str) -> list[list[Item]]:
    runs: list[list[Item]] = []
    run: _Run | None = None
    leaders: list[Node] = []
    prev_row = -1  # last source line of the previous sibling, trailing comments included

    def close() -> None:
        nonlocal run
        if run is not None and len(run.items) > 1:
            runs.append(run.items)
        run = None

    for child in parent.children:
        if child.type in _LEADERS:
            if child.start_point.row == prev_row:
                # A trailing comment belongs to the statement it follows.
                if run is not None and run.last_row == prev_row:
                    run.items[-1] = (run.items[-1][0], _last_row(child) + 1)
                    run.last_row = _last_row(child)
                prev_row = _last_row(child)
            else:
                leaders.append(child)
            continue
        # The leaders that chain up to `child` travel with it; any other leader stands alone.
        first, next_row = len(leaders), child.start_point.row
        while first > 0 and _last_row(leaders[first - 1]) + 1 >= next_row:
            first -= 1
            next_row = leaders[first].start_point.row
        attached, stray = leaders[first:], leaders[:first]
        leaders = []
        prev_row = _last_row(child)
        macro_use = any(b"macro_use" in (leader.text or b"") for leader in attached)
        reorderable = _orders(child, grammar) and not macro_use
        if stray or not reorderable:
            close()
        if not reorderable:
            continue
        if run is not None and _GRAMMARS[grammar].blank_breaks and next_row > run.last_row + 1:
            close()
        if run is None:
            run = _Run([])
        run.items.append((next_row, prev_row + 1))
        run.last_row = prev_row
    close()
    return runs


def parse(text: str | bytes, grammar: str) -> Node | None:
    """Return the source tree only when the grammar parsed the whole file."""
    root = _parser(grammar).parse(text.encode() if isinstance(text, str) else text).root_node
    return None if root.has_error else root


_PACKED_LANGUAGES = frozenset(get_args(SupportedLanguage))


def _is_packed_language(grammar: str) -> TypeIs[SupportedLanguage]:
    return grammar in _PACKED_LANGUAGES


@cache
def _packed_parser(grammar: str) -> Parser:
    if not _is_packed_language(grammar):
        raise ValueError(f"unregistered bundled grammar: {grammar}")
    return get_parser(grammar)


def parse_packed(text: str, grammar: str) -> Node | None:
    """Parse with an offline bundled grammar; reject incomplete or malformed trees."""
    root = _packed_parser(grammar).parse(text.encode()).root_node
    return None if root.has_error else root


def unordered_runs(root: Node, grammar: str) -> list[list[Item]]:
    """Find groups of reorderable sibling statements in source order.

    An intervening statement or a detached comment ends a run; blank lines end some languages' runs.
    """
    runs: list[list[Item]] = []
    stack = [root]
    while stack:
        node = stack.pop()
        runs.extend(_runs_of(node, grammar))
        stack.extend(reversed(node.children))
    return sorted(runs)

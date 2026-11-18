"""Formatting-only change detection and the ``lgit --fmt`` style commit.

``--fmt`` commits the formatting-only part of the pending changes as one ``style:`` commit without any
model call; every other change stays pending exactly where it was (staged or unstaged).

Formatting regions match by normalized tokens or language-specific syntax rules. In Rust, Python,
Go, and JS/TS, parsed comparisons recognize formatter rewrites that change tokens (see
``format_tree``). Other rules cover whitespace, trailing commas, comment wrapping, and runs of
reordered imports. Mixed changes are spliced into HEAD and checked in whole-file context before
committing. Unknown languages, binaries, mode changes, renames, and partially staged files are skipped.

Example (Rust): ``fn f(a: u8, b: u8) {`` → ``fn f(\\n\\ta: u8,\\n\\tb: u8,\\n) {`` is formatting-only;
``"a b"`` → ``"ab"``, ``(x,)`` → ``(x)``, and ``foo();`` → ``// foo();`` are not.

The commit is built against a temporary index and attached with a checked ref update, so commit
hooks do not run (as in compose mode). Afterwards the real index entry of each unstaged file is
advanced to the committed blob, so the remaining changes show up exactly as before.
"""

from __future__ import annotations

import os
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field, replace
from difflib import SequenceMatcher
from itertools import chain
from pathlib import PurePosixPath

from tree_sitter import Node

from .errors import GitError
from .format_tree import SUPPORTED, format_tree
from .git import (
    TempGitIndex,
    commit_tree,
    current_head_ref,
    get_head_hash,
    hash_blob,
    read_tree_into_index,
    run_git,
    run_git_bytes,
    update_ref_checked,
    write_index_tree,
)
from .syntax_tree import parse, unordered_runs


@dataclass(frozen=True, slots=True)
class _Syntax:
    """Lexical rules the normalizer needs: comments, string delimiters, and indentation significance."""

    line_comments: tuple[str, ...] = ("//",)
    block_comment: tuple[str, str] | None = ("/*", "*/")
    # String delimiters, longest first.
    quotes: tuple[str, ...] = ('"', "'")
    # JS/TS backtick strings with `${…}` interpolation.
    template_literals: bool = False
    # Go backtick raw strings.
    raw_backticks: bool = False
    # Rust raw strings (`r#"…"#`) and `'a` lifetimes next to `'x'` char literals.
    rust: bool = False
    indent_sensitive: bool = False
    # Tree-sitter grammar for finding reorderable import runs; "" when order is never formatting.
    grammar: str = ""
    # Rust and Python: `(x,)` is a one-element tuple, so its trailing comma is not formatting.
    paren_tuples: bool = False


_C_LIKE = _Syntax()
_JS = _Syntax(template_literals=True, grammar="javascript")
_JVM = _Syntax(quotes=('"""', '"', "'"))
_SYNTAX_BY_SUFFIX: dict[str, _Syntax] = {
    **dict.fromkeys((".rs",), _Syntax(quotes=('"',), rust=True, grammar="rust", paren_tuples=True)),
    **dict.fromkeys((".js", ".jsx", ".mjs", ".cjs"), _JS),
    **dict.fromkeys((".ts", ".mts", ".cts"), replace(_JS, grammar="typescript")),
    **dict.fromkeys((".tsx",), replace(_JS, grammar="tsx")),
    **dict.fromkeys((".go",), _Syntax(raw_backticks=True, grammar="go")),
    **dict.fromkeys((".c",), replace(_C_LIKE, grammar="c")),
    **dict.fromkeys((".h", ".cc", ".cpp", ".cxx", ".hh", ".hpp"), replace(_C_LIKE, grammar="cpp")),
    **dict.fromkeys((".cs",), replace(_C_LIKE, grammar="c_sharp")),
    **dict.fromkeys((".zig",), _C_LIKE),
    **dict.fromkeys((".java",), replace(_JVM, grammar="java")),
    **dict.fromkeys((".kt", ".kts"), replace(_JVM, grammar="kotlin")),
    **dict.fromkeys((".swift",), replace(_JVM, grammar="swift")),
    **dict.fromkeys((".scala", ".dart"), _JVM),
    **dict.fromkeys((".json",), _Syntax(line_comments=(), block_comment=None, quotes=('"',))),
    **dict.fromkeys((".jsonc", ".json5"), _Syntax(quotes=('"', "'"))),
    **dict.fromkeys((".toml",), _Syntax(line_comments=("#",), block_comment=None, quotes=('"""', "'''", '"', "'"))),
    **dict.fromkeys(
        (".py", ".pyi"),
        _Syntax(
            line_comments=("#",),
            block_comment=None,
            quotes=('"""', "'''", '"', "'"),
            indent_sensitive=True,
            grammar="python",
            paren_tuples=True,
        ),
    ),
}

_CODE = "code"
_STRING = "str"
_COMMENT_END = "*/"
_SPACE = " \t\r\f\v"
_OPENERS = "([{"
_CLOSERS = ")]}"
# Operator characters whose adjacency changes tokenization (`+ +` vs `++`), so whitespace between two
# of them is kept, like whitespace between two word characters.
_OPERATORS = frozenset("+-*/%<>=!&|^~.:?@#\\")
# Equal lines tolerated between two changed regions that may be one reflow the line diff split apart.
_MERGE_GAP = 2
# Lines after a region searched for the token that follows it.
_LOOKAHEAD = 8
# Largest side (in lines) of a mixed region searched for a formatting-only head and tail.
_EDGE_LIMIT = 40
# Words after which `/` opens a JS regex literal.
_REGEX_KEYWORDS = frozenset("return typeof case do else in of new delete void throw instanceof yield await".split())
# Words after which `(` opens a parenthesized expression (where `(x,)` is a tuple), not a call.
_KEYWORDS = frozenset(
    "return yield in and or not is if elif else while match case await let for assert del lambda raise except "
    "typeof void instanceof of throw mut ref move".split()
)


@dataclass(slots=True)
class _Brackets:
    """Bracket groups open at a code position, for telling a trailing comma from a tuple's.

    Each group is `[commas so far, commas needed before a trailing one is formatting]`: 2 when `(`
    may open a tuple (`(x,)` differs from `(x)`), else 1 (calls, `[…]`, `{…}`).
    """

    groups: list[list[int]]
    tuples: bool  # whether `(x,)` can be a one-element tuple in this language
    prev: str = ""  # last code character; "" after a string or comment
    word: str = ""  # identifier ending at `prev`

    def feed(self, ch: str, *, joined: bool) -> None:
        """Advance past code character `ch`; `joined` when no whitespace precedes it."""

        if _is_word(ch):
            self.word = self.word + ch if joined and self.prev and _is_word(self.prev) else ch
        elif ch in _OPENERS:
            call = (
                ch != "("
                or not self.tuples
                or self.prev in ")]!"
                or (_is_word(self.prev) and self.word not in _KEYWORDS)
            )
            self.groups.append([0, 1 if call else 2])
        elif ch in _CLOSERS:
            if self.groups:
                self.groups.pop()
        elif ch == "," and self.groups:
            self.groups[-1][0] += 1
        self.prev = ch

    def trailing_comma(self) -> bool:
        """Whether the comma just fed is droppable when the group closes next."""

        return bool(self.groups) and self.groups[-1][0] >= self.groups[-1][1]

    def interrupt(self) -> None:
        self.prev = self.word = ""

    def snapshot(self) -> tuple[tuple[int, int], ...]:
        return tuple((commas, needed) for commas, needed in self.groups)


@dataclass(slots=True)
class _Line:
    """One source line as lexed in its file context."""

    # Bracket groups open at the start of the line (see `_Brackets`), outermost first.
    groups: tuple[tuple[int, int], ...]
    starts_in_code: bool
    indent: str
    # `(kind, text)`: kind is `_CODE`, `_STRING`, or the comment marker (`//`, `///`, `#`, `/*`, …).
    segments: list[tuple[str, str]] = field(default_factory=list)


def _line_comment_marker(raw: str, i: int, syntax: _Syntax) -> str | None:
    for marker in syntax.line_comments:
        if not raw.startswith(marker, i):
            continue
        if marker == "//" and raw.startswith("///", i) and not raw.startswith("////", i):
            return "///"
        if marker == "//" and raw.startswith("//!", i):
            return "//!"
        return marker
    return None


def _rust_raw_string_opener(raw: str, i: int) -> str | None:
    """Return the `r#"`/`br"` opener starting at `i`, if any."""

    if i > 0 and _is_word(raw[i - 1]):
        return None
    j = i + 1 if raw.startswith("br", i) else i
    if j >= len(raw) or raw[j] != "r":
        return None
    k = j + 1
    while k < len(raw) and raw[k] == "#":
        k += 1
    return raw[i : k + 1] if k < len(raw) and raw[k] == '"' else None


def _lex(text: str, syntax: _Syntax) -> list[_Line]:
    """Split `text` into lines of code, string, and comment segments, tracking bracket groups."""

    lines: list[_Line] = []
    brackets = _Brackets([], syntax.paren_tuples)
    joined = False
    templates: list[int] = []  # bracket depth at each open `${`
    kind = _CODE
    close = ""  # delimiter that ends the open string or block comment
    escapes = True
    buf: list[str] = []
    raw_lines = text.split("\n")

    def flush(line: _Line) -> None:
        if buf:
            line.segments.append((kind, "".join(buf)))
            buf.clear()

    for number, raw in enumerate(raw_lines):
        line = _Line(brackets.snapshot(), kind == _CODE, raw[: len(raw) - len(raw.lstrip(" \t"))])
        joined = False
        lines.append(line)
        i, n = 0, len(raw)
        if kind not in (_CODE, _STRING):
            # Block comment continuation: drop the decorative leading `*`.
            i = n - len(raw.lstrip())
            if raw.startswith("*", i) and not raw.startswith("*/", i):
                i += 1
        while i < n:
            ch = raw[i]
            if kind == _STRING:
                if escapes and ch == "\\":
                    buf.append(raw[i : i + 2])
                    i += 2
                elif raw.startswith(close, i):
                    buf.append(close)
                    i += len(close)
                    flush(line)
                    kind = _CODE
                elif close == "`" and syntax.template_literals and raw.startswith("${", i):
                    buf.append("${")
                    i += 2
                    flush(line)
                    kind = _CODE
                    templates.append(len(brackets.groups))
                else:
                    buf.append(ch)
                    i += 1
                continue
            if kind != _CODE:
                if raw.startswith(close, i):
                    flush(line)
                    line.segments.append((_COMMENT_END, ""))
                    kind = _CODE
                    i += len(close)
                else:
                    buf.append(ch)
                    i += 1
                continue
            if ch in _SPACE:
                joined = False
                buf.append(ch)
                i += 1
                continue
            if syntax.block_comment and raw.startswith(syntax.block_comment[0], i):
                flush(line)
                brackets.interrupt()
                kind = "/**" if raw.startswith("/**", i) and not raw.startswith("/**/", i) else "/*"
                close = syntax.block_comment[1]
                line.segments.append((kind, ""))  # an opener alone still changes what follows
                i += len(kind)
                continue
            marker = _line_comment_marker(raw, i, syntax)
            if marker is not None:
                flush(line)
                brackets.interrupt()
                line.segments.append((marker, raw[i + len(marker) :]))
                break
            if ch == "/" and syntax.template_literals and _regex_may_start(brackets):
                end = _regex_end(raw, i)
                if end is not None:
                    flush(line)
                    brackets.interrupt()
                    line.segments.append((_STRING, raw[i:end]))
                    i = end
                    continue
            opener = None
            if syntax.rust and ch in "rb":
                opener = _rust_raw_string_opener(raw, i)
                escapes = opener is None
                close = '"' + "#" * (opener.count("#") if opener else 0)
            elif syntax.rust and ch == "'":
                # `'\n'` and `'x'` are char literals; `'a` is a lifetime or label.
                if raw.startswith("\\", i + 1) or raw.startswith("'", i + 2):
                    opener, close, escapes = "'", "'", True
            elif ch == "`" and (syntax.template_literals or syntax.raw_backticks):
                opener, close, escapes = "`", "`", syntax.template_literals
            else:
                opener = next((quote for quote in syntax.quotes if raw.startswith(quote, i)), None)
                close, escapes = opener or "", True
            if opener is not None:
                flush(line)
                brackets.interrupt()
                kind = _STRING
                buf.append(opener)
                i += len(opener)
                continue
            if ch == "}" and templates and len(brackets.groups) == templates[-1]:
                templates.pop()
                flush(line)
                kind, close, escapes = _STRING, "`", True
                buf.append(ch)
                i += 1
                continue
            brackets.feed(ch, joined=joined)
            joined = True
            buf.append(ch)
            i += 1
        if kind == _STRING and number < len(raw_lines) - 1 and _spans_lines(close, syntax):
            buf.append("\n")
        flush(line)
        if kind == _STRING and not _spans_lines(close, syntax):
            kind = _CODE  # unterminated single-line literal (e.g. a JSX apostrophe)
    return lines


def _regex_may_start(brackets: _Brackets) -> bool:
    """Whether `/` here can open a JS regex literal rather than divide."""

    prev = brackets.prev
    return prev == "" or prev in "(,=:[!&|?{};+-*%<>~^}" or (_is_word(prev) and brackets.word in _REGEX_KEYWORDS)


def _regex_end(raw: str, i: int) -> int | None:
    """End (exclusive, flags included) of the JS regex literal opening at `i`, or None."""

    j = i + 1
    in_class = False
    while j < len(raw):
        ch = raw[j]
        if ch == "\\":
            j += 2
            continue
        if ch == "[":
            in_class = True
        elif ch == "]":
            in_class = False
        elif ch == "/" and not in_class:
            j += 1
            while j < len(raw) and raw[j].isalpha():
                j += 1
            return j
        j += 1
    return None


def _spans_lines(close: str, syntax: _Syntax) -> bool:
    return close in ('"""', "'''", "`") or (syntax.rust and close.startswith('"'))


def _is_word(ch: str) -> bool:
    return ch != "" and (ch.isalnum() or ch in "_$" or ord(ch) > 127)


def _joins(left: str, right: str) -> bool:
    """Whether whitespace between `left` and `right` separates tokens that would otherwise merge."""

    if left == right == ">":
        return False  # `Vec<Vec<T> >` and `Vec<Vec<T>>` close the same generics
    if right == "<" and left in "&|?:=":
        return False  # no operator ends in `&`, `|`, `?`, `:` or `=` and takes a following `<`
    return (_is_word(left) and _is_word(right)) or (left in _OPERATORS and right in _OPERATORS)


class _Stream:
    """Renders lexed lines as the stream that only formatting-equivalent text shares."""

    def __init__(self, lines: Sequence[_Line], syntax: _Syntax, after: Iterable[_Line]) -> None:
        self.lines = lines
        self.after = after
        self.syntax = syntax
        self.out: list[str] = []
        self.brackets = _Brackets([list(group) for group in lines[0].groups] if lines else [], syntax.paren_tuples)
        self.last = ""  # last emitted code character; "" after a string, comment, or indentation mark
        self.gap = False  # whitespace seen since the last code character
        self.deferred_comma = False
        self.comment_kind = ""
        self.words: list[str] = []
        # Rust strings: whether `\` escapes (a slice may start mid-string), and whether a `\`-newline
        # just swallowed the next line's leading whitespace.
        self.escapes = True
        self.continued = False

    def render(self) -> str:
        syntax = self.syntax
        for line in self.lines:
            if syntax.indent_sensitive and line.starts_in_code and not line.groups and _starts_statement(line):
                self.settle()
                self.out.append(f"\x02{line.indent}\x02")
                self.last = ""
            for position, (kind, text) in enumerate(line.segments):
                if kind == _CODE:
                    for ch in text:
                        self.code(ch)
                elif kind == _STRING:
                    self.string(text, resumed=position == 0 and not line.starts_in_code)
                else:
                    if self.comment_kind != kind:
                        if self.comment_kind:
                            self.settle()
                        self.comment_kind = kind
                    self.words.extend(text.split())
                    self.brackets.interrupt()
            self.gap = True
        if self.deferred_comma and self.brackets.trailing_comma() and self.following() in _CLOSERS:
            self.deferred_comma = False
        self.settle()
        return "".join(self.out)

    def following(self) -> str:
        """The first code character after the rendered lines, or an empty string."""

        for line in self.after:
            for kind, text in line.segments:
                if kind == _CODE or kind == _STRING:
                    stripped = text.lstrip(_SPACE)
                    if stripped:
                        return stripped[0]
        return ""

    def settle(self) -> None:
        if self.deferred_comma:
            self.out.append(",")
            self.last = ","
            self.deferred_comma = False
        if self.comment_kind:
            self.out.append(f"\x00{self.comment_kind}\x01{' '.join(self.words)}\x00")
            self.comment_kind = ""
            self.words.clear()

    def string(self, text: str, *, resumed: bool) -> None:
        self.settle()
        if self.syntax.rust:
            text = self.rust_string(text, resumed)
        self.out.append(text)
        self.last, self.gap = "", False
        self.brackets.interrupt()

    def rust_string(self, text: str, resumed: bool) -> str:
        """Fold Rust line continuations out of a string segment."""

        if not resumed:
            self.escapes, self.continued = not text.startswith(("r", "br")), False
        if not self.escapes:
            return text
        if self.continued:
            text = text.lstrip(" \t\r\n")
            self.continued = not text
        body = text.removesuffix("\n")
        if body != text and (len(body) - len(body.rstrip("\\"))) % 2:
            text, self.continued = body[:-1], True
        return text

    def code(self, ch: str) -> None:
        out, brackets = self.out, self.brackets
        if ch in _SPACE:
            self.gap = True
            return
        if self.deferred_comma and ch in _CLOSERS and brackets.trailing_comma():
            self.deferred_comma = False
        self.settle()
        if self.gap and self.last and _joins(self.last, ch):
            out.append(" ")
        brackets.feed(ch, joined=not self.gap)
        self.gap = False
        if ch == "," and brackets.groups:
            self.deferred_comma = True
            return
        out.append(ch)
        self.last = ch


def _normalize(lines: Sequence[_Line], syntax: _Syntax, after: Iterable[_Line] = ()) -> str:
    """Render `lines` as a stream that only formatting-equivalent text shares.

    `after` includes the following lines so a trailing comma is judged by the next token.
    """

    return _Stream(lines, syntax, after).render()


def _starts_statement(line: _Line) -> bool:
    for kind, text in line.segments:
        if kind == _CODE and not text.strip():
            continue
        return kind in (_CODE, _STRING)
    return False


def reformat_only(old: str, new: str, syntax: _Syntax) -> str:
    """Return `old` with only the formatting-only changes of `old` → `new` applied.

    A run of imports reordered into `new`'s order counts as formatting (see `_reorder_imports`);
    everything else is decided region by region (see `_reformat_regions`).
    """

    grammar = syntax.grammar
    old_root, new_root = (parse(old, grammar), parse(new, grammar)) if grammar else (None, None)
    if old_root is not None and new_root is not None:
        reordered = _reorder_imports(old, new, syntax, old_root, new_root)
        if reordered != old and (root := parse(reordered, grammar)) is not None:
            old, old_root = reordered, root
    return _reformat_regions(old, new, syntax, old_root, new_root)


def _reorder_imports(old: str, new: str, syntax: _Syntax, old_root: Node, new_root: Node) -> str:
    """Return `old` with each import run that `new` merely reorders replaced by `new`'s run.

    Runs pair up when they hold the same statements (each with its attached comments and attributes)
    in any order and any blank-line grouping; a run that gained, lost, or changed a statement is left
    to the region-by-region comparison.
    """

    old_runs, new_runs = unordered_runs(old_root, syntax.grammar), unordered_runs(new_root, syntax.grammar)
    if not old_runs or not new_runs:
        return old
    old_lines, new_lines = old.split("\n"), new.split("\n")

    def key(lines: list[str], run: list[tuple[int, int]]) -> tuple[str, ...]:
        return tuple(sorted(_normalize(_lex("\n".join(lines[a:b]), syntax), syntax) for a, b in run))

    candidates: dict[tuple[str, ...], list[list[tuple[int, int]]]] = {}
    for run in new_runs:
        candidates.setdefault(key(new_lines, run), []).append(run)
    for matches in candidates.values():
        matches.reverse()  # pop() takes them in source order
    swaps: list[tuple[int, int, list[str]]] = []
    for run in old_runs:
        paired = candidates.get(key(old_lines, run))
        if not paired:
            continue
        match = paired.pop()
        start, end = run[0][0], run[-1][1]
        replacement = new_lines[match[0][0] : match[-1][1]]
        if replacement != old_lines[start:end]:
            swaps.append((start, end, replacement))
    for start, end, replacement in reversed(swaps):
        old_lines[start:end] = replacement
    return "\n".join(old_lines)


def _reformat_regions(old: str, new: str, syntax: _Syntax, old_root: Node | None, new_root: Node | None) -> str:
    """Return `old` with only the formatting-only regions of `old` → `new` applied.

    Inside a change that also alters content, a formatting-only piece is taken only when it stands
    on its own: it keeps the indentation of its first line, does more than re-indent, and does more
    than add or drop blank lines. Otherwise its layout follows the content change (code wrapped in a
    new block, a blank line aligned into a new doc comment), and taking it alone would misplace it.
    """

    grammar = syntax.grammar
    if old == new:
        return old
    old_tree = format_tree(old_root, old, grammar) if old_root is not None and grammar in SUPPORTED else None
    new_tree = format_tree(new_root, new, grammar) if old_tree is not None and new_root is not None else None
    if old_tree is not None and new_tree is not None:
        if old_tree.tokens == new_tree.tokens and old_tree.python == new_tree.python:
            return new

    old_lines, new_lines = old.split("\n"), new.split("\n")
    old_lex, new_lex = _lex(old, syntax), _lex(new, syntax)

    def norm_old(i1: int, i2: int) -> str:
        return _normalize(old_lex[i1:i2], syntax, old_lex[i2 : i2 + _LOOKAHEAD])

    def norm_new(j1: int, j2: int) -> str:
        return _normalize(new_lex[j1:j2], syntax, new_lex[j2 : j2 + _LOOKAHEAD])

    def equivalent(i1: int, i2: int, j1: int, j2: int) -> bool:
        if old_tree is not None and new_tree is not None:
            if tuple(chain.from_iterable(old_tree.rows[i1:i2])) == tuple(chain.from_iterable(new_tree.rows[j1:j2])):
                return True
        return norm_old(i1, i2) == norm_new(j1, j2)

    def standalone(i1: int, i2: int, j1: int, j2: int) -> bool:
        if i1 == i2 or j1 == j2:
            return False
        first_old, first_new = old_lines[i1], new_lines[j1]
        if (
            first_old[: len(first_old) - len(first_old.lstrip())]
            != first_new[: len(first_new) - len(first_new.lstrip())]
        ):
            return False
        return [line.lstrip() for line in old_lines[i1:i2]] != [line.lstrip() for line in new_lines[j1:j2]]

    def mixed(i1: int, i2: int, j1: int, j2: int) -> Iterable[tuple[int, int, int, int]]:
        """Formatting-only pieces of a region that also changes content."""

        if equivalent(i1, i2, j1, j2):
            yield (i1, i2, j1, j2)
            return
        old_norms = [norm_old(i, i + 1) for i in range(i1, i2)]
        new_norms = [norm_new(j, j + 1) for j in range(j1, j2)]
        for tag, a1, a2, b1, b2 in SequenceMatcher(None, old_norms, new_norms, autojunk=False).get_opcodes():
            if tag == "equal":
                pairs = zip(range(i1 + a1, i1 + a2), range(j1 + b1, j1 + b2), strict=True)
                yield from ((i, i + 1, j, j + 1) for i, j in pairs)
            elif tag == "replace":
                yield from edges(i1 + a1, i1 + a2, j1 + b1, j1 + b2)

    def edges(i1: int, i2: int, j1: int, j2: int) -> Iterable[tuple[int, int, int, int]]:
        """The longest formatting-only head and tail of a region whose middle changes content."""

        if equivalent(i1, i2, j1, j2):
            pieces = [(i1, i2, j1, j2)]
        elif i2 - i1 > _EDGE_LIMIT or j2 - j1 > _EDGE_LIMIT:
            return
        else:
            heads = {norm_old(i1, a): a for a in range(i1, i2)}
            head = max(((heads[key], b) for b in range(j1, j2) if (key := norm_new(j1, b)) in heads), default=None)
            a0, b0 = head or (i1, j1)
            tails = {norm_old(a, i2): a for a in range(i2, a0, -1)}
            tail = min(((tails[key], b) for b in range(j2, b0, -1) if (key := norm_new(b, j2)) in tails), default=None)
            pieces = [(i1, a0, j1, b0)] if head else []
            if tail:
                pieces.append((tail[0], i2, tail[1], j2))
        yield from pieces

    def splice(regions: Sequence[tuple[int, int, int, int]]) -> str:
        out: list[str] = []
        cursor = 0
        for i1, i2, j1, j2 in sorted(regions):
            out.extend(old_lines[cursor:i1])
            out.extend(new_lines[j1:j2])
            cursor = i2
        out.extend(old_lines[cursor:])
        return "\n".join(out)

    changes = [
        op[1:] for op in SequenceMatcher(None, old_lines, new_lines, autojunk=False).get_opcodes() if op[0] != "equal"
    ]
    whole: list[tuple[int, int, int, int]] = []
    pieces: list[tuple[int, int, int, int]] = []
    for cluster in _clusters(changes):
        (i1, _, j1, _), (_, i2, _, j2) = cluster[0], cluster[-1]
        if equivalent(i1, i2, j1, j2):
            whole.append((i1, i2, j1, j2))
            continue
        for region in cluster:
            pieces.extend(piece for piece in mixed(*region) if standalone(*piece))

    # Pieces that match locally can still change their surrounding syntax (ASI, indentation, a
    # string or comment). Verify the entire candidate before taking anything.
    expected = _normalize(old_lex, syntax)

    def valid(text: str) -> bool:
        if text == old:
            return True
        if old_tree is not None:
            root = parse(text, grammar)
            tree = format_tree(root, text, grammar) if root is not None else None
            if tree is None:
                return False
            if old_tree.tokens == tree.tokens and old_tree.python == tree.python:
                return True
            # The Rust lexer already accounts for rewrapped comments and string continuations.
            if not syntax.rust:
                return False
        return _normalize(_lex(text, syntax), syntax) == expected

    for regions in (whole + pieces, whole):
        text = splice(regions)
        if valid(text):
            return text
    if old_tree is not None:
        # A lexical false positive (for example a JSX text edit) shouldn't poison an otherwise
        # independent formatting region. Re-check the final combination as well.
        safe = [region for region in whole + pieces if valid(splice([region]))]
        text = splice(safe)
        if valid(text):
            return text
    return old


def _clusters(changes: Sequence[tuple[int, int, int, int]]) -> Iterable[list[tuple[int, int, int, int]]]:
    """Group changed regions separated by at most `_MERGE_GAP` equal lines."""

    cluster: list[tuple[int, int, int, int]] = []
    for region in changes:
        if cluster and region[0] - cluster[-1][1] > _MERGE_GAP:
            yield cluster
            cluster = []
        cluster.append(region)
    if cluster:
        yield cluster


@dataclass(frozen=True, slots=True)
class FormattedFile:
    """A file whose pending change contains formatting-only regions."""

    path: str
    mode: str
    head_oid: str
    contents: bytes  # HEAD content with only the formatting-only regions applied
    complete: bool  # the whole change is formatting-only
    staged: bool  # the change came from the index rather than the worktree


@dataclass(frozen=True, slots=True)
class FormattingPlan:
    """Formatting-only changes ready to commit on top of `head`."""

    head: str
    files: tuple[FormattedFile, ...]
    partially_staged: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class _Change:
    path: str
    mode: str
    old_oid: str
    new_oid: str


def plan_formatting(dir: str | os.PathLike[str] = ".") -> FormattingPlan:
    """Collect the formatting-only regions of every pending modification.

    Staged-only files are read from the index, unstaged-only files from the worktree (clean filters
    applied); files with both are reported in `partially_staged` and left alone.

    Raises GitError when HEAD does not exist yet.
    """

    head = get_head_hash(dir)
    staged = _modified(["diff", "--cached"], dir)
    unstaged = _modified(["diff"], dir)
    partial = staged.keys() & unstaged.keys()
    worktree = [change for path, change in unstaged.items() if path not in partial]
    worktree_oids = _hash_worktree_files([change.path for change in worktree], dir)
    changes = [
        *((change, True) for path, change in staged.items() if path not in partial),
        *(
            (_Change(change.path, change.mode, change.old_oid, worktree_oids[change.path]), False)
            for change in worktree
        ),
    ]
    blobs = _read_blobs({oid for change, _ in changes for oid in (change.old_oid, change.new_oid)}, dir)
    files: list[FormattedFile] = []
    for change, from_index in sorted(changes, key=lambda item: item[0].path):
        syntax = _SYNTAX_BY_SUFFIX.get(PurePosixPath(change.path).suffix.lower())
        old, new = blobs[change.old_oid], blobs[change.new_oid]
        if syntax is None or b"\0" in old or b"\0" in new:
            continue
        try:
            old_text, new_text = old.decode(), new.decode()
        except UnicodeDecodeError:
            continue
        text = reformat_only(old_text, new_text, syntax)
        if text != old_text:
            contents = text.encode()
            files.append(FormattedFile(change.path, change.mode, change.old_oid, contents, contents == new, from_index))
    return FormattingPlan(head, tuple(files), tuple(sorted(partial)))


def commit_formatting(
    plan: FormattingPlan, message: str, dir: str | os.PathLike[str] = ".", *, sign: bool = False
) -> str:
    """Commit `plan` on top of its HEAD and return the new commit oid.

    Raises GitError when HEAD moved since planning; nothing is committed then.
    """

    oids = {file.path: hash_blob(file.contents, dir) for file in plan.files}
    with TempGitIndex(dir) as index:
        read_tree_into_index(index.path, plan.head, dir)
        _set_index_entries(((file.mode, oids[file.path], file.path) for file in plan.files), dir, index.path)
        tree = write_index_tree(index.path, dir)
    commit = commit_tree(tree, [plan.head], message, dir, sign=sign)
    update_ref_checked(current_head_ref(dir), commit, plan.head, dir)
    # Unstaged files: the index still holds the old HEAD blob; advance it to the committed one so the
    # remaining edits stay unstaged instead of showing up as a staged revert. Staged files already
    # hold their full content. Entries the user changed meanwhile are left alone.
    unstaged = [file for file in plan.files if not file.staged]
    current = _index_entries([file.path for file in unstaged], dir)
    _set_index_entries(
        (
            (file.mode, oids[file.path], file.path)
            for file in unstaged
            if current.get(file.path) == (file.mode, file.head_oid)
        ),
        dir,
    )
    return commit


def _modified(command: Sequence[str], dir: str | os.PathLike[str]) -> dict[str, _Change]:
    """Content-only modifications of regular files reported by `git diff --raw` (`command`)."""

    raw = run_git_bytes(
        [*command, "--raw", "-z", "--no-abbrev", "--no-renames", "--no-ext-diff", "--diff-filter=M"], cwd=dir
    ).stdout.decode()
    fields = raw.split("\0")
    changes: dict[str, _Change] = {}
    for meta, path in zip(fields[0::2], fields[1::2], strict=False):
        old_mode, new_mode, old_oid, new_oid, _status = meta.lstrip(":").split()
        if old_mode == new_mode and old_mode in ("100644", "100755"):
            changes[path] = _Change(path, old_mode, old_oid, new_oid)
    return changes


def _hash_worktree_files(paths: Sequence[str], dir: str | os.PathLike[str]) -> dict[str, str]:
    """Hash worktree files as `git add` would store them (clean filters applied)."""

    if not paths:
        return {}
    if any("\n" in path for path in paths):
        raise GitError("cannot hash worktree paths containing newlines")
    out = run_git(["hash-object", "-w", "--stdin-paths"], cwd=dir, input_text="".join(f"{path}\n" for path in paths))
    oids = out.stdout.split()
    if len(oids) != len(paths):
        raise GitError("git hash-object returned an unexpected number of oids")
    return dict(zip(paths, oids, strict=True))


def _read_blobs(oids: Iterable[str], dir: str | os.PathLike[str]) -> dict[str, bytes]:
    wanted = sorted(oids)
    data = run_git_bytes(
        ["cat-file", "--batch"], cwd=dir, input_bytes="".join(f"{oid}\n" for oid in wanted).encode()
    ).stdout
    blobs: dict[str, bytes] = {}
    pos = 0
    for oid in wanted:
        end = data.index(b"\n", pos)
        header = data[pos:end].split()
        if len(header) != 3 or header[1] != b"blob":
            raise GitError(f"git cat-file could not read blob {oid}")
        size = int(header[2])
        blobs[oid] = data[end + 1 : end + 1 + size]
        pos = end + 1 + size + 1
    return blobs


def _index_entries(paths: Sequence[str], dir: str | os.PathLike[str]) -> dict[str, tuple[str, str]]:
    if not paths:
        return {}
    raw = run_git_bytes(["ls-files", "--stage", "-z", "--", *(f":(literal){path}" for path in paths)], cwd=dir)
    entries: dict[str, tuple[str, str]] = {}
    for record in raw.stdout.decode().split("\0"):
        if record:
            meta, path = record.split("\t", 1)
            mode, oid, stage = meta.split()
            if stage == "0":
                entries[path] = (mode, oid)
    return entries


def _set_index_entries(
    entries: Iterable[tuple[str, str, str]],
    dir: str | os.PathLike[str],
    index_file: str | os.PathLike[str] | None = None,
) -> None:
    info = "".join(f"{mode} {oid}\t{path}\0" for mode, oid, path in entries)
    if info:
        run_git_bytes(["update-index", "-z", "--index-info"], cwd=dir, index_file=index_file, input_bytes=info.encode())


__all__ = ["FormattedFile", "FormattingPlan", "commit_formatting", "plan_formatting", "reformat_only"]

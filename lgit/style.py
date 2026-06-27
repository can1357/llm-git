"""Terminal styling utilities for consistent CLI output."""

from __future__ import annotations

import asyncio
import os
import shutil
import sys
from collections.abc import Awaitable
from typing import Any, TextIO

_COLOR_ENABLED: bool | None = None
_PIPE_MODE: bool | None = None


class icons:
    """Status icon constants used by CLI output."""

    SUCCESS = "✓"
    WARNING = "⚠"
    ERROR = "✗"
    INFO = "ℹ"
    ARROW = "→"
    BULLET = "•"
    CLIPBOARD = "📋"
    SEARCH = "🔍"
    ROBOT = "🤖"
    SAVE = "💾"


class box_chars:
    """Unicode box drawing characters."""

    TOP_LEFT = "╭"
    TOP_RIGHT = "╮"
    BOTTOM_LEFT = "╰"
    BOTTOM_RIGHT = "╯"
    HORIZONTAL = "─"
    VERTICAL = "│"


def colors_enabled() -> bool:
    """Return true when ANSI colors should be emitted."""

    global _COLOR_ENABLED
    if _COLOR_ENABLED is None:
        term = os.environ.get("TERM", "")
        _COLOR_ENABLED = "NO_COLOR" not in os.environ and sys.stdout.isatty() and term.lower() != "dumb"
    return _COLOR_ENABLED


def pipe_mode() -> bool:
    """Return true when stdout is not a terminal."""

    global _PIPE_MODE
    if _PIPE_MODE is None:
        _PIPE_MODE = not sys.stdout.isatty()
    return _PIPE_MODE


def success(s: str) -> str:
    """Style success text."""

    return _ansi(s, "32", bold_text=True)


def warning(s: str) -> str:
    """Style warning text."""

    return _ansi(s, "33")


def error(s: str) -> str:
    """Style error text."""

    return _ansi(s, "31", bold_text=True)


def info(s: str) -> str:
    """Style informational text."""

    return _ansi(s, "36")


def dim(s: str) -> str:
    """Style low-emphasis text."""

    return _ansi(s, "2")


def bold(s: str) -> str:
    """Style bold text."""

    return _ansi(s, "1")


def model(s: str) -> str:
    """Style model names."""

    return _ansi(s, "35")


def commit_type(s: str) -> str:
    """Style conventional commit types."""

    return _ansi(s, "34", bold_text=True)


def scope(s: str) -> str:
    """Style conventional commit scopes."""

    return _ansi(s, "36")


def warn(msg: str) -> None:
    """Print a warning, clearing an active spinner line first."""

    if not pipe_mode():
        print("\r\x1b[K", end="", file=sys.stdout, flush=True)
    print(f"{warning(icons.WARNING)} {warning(msg)}", file=sys.stderr)


def _status_stream() -> TextIO:
    return sys.stderr if pipe_mode() else sys.stdout


def _clear_status_line(stream: TextIO) -> None:
    if stream.isatty() and colors_enabled():
        print("\r\x1b[K", end="", file=stream, flush=True)


def status(msg: str = "") -> None:
    """Print a status line to stderr in pipe mode, stdout otherwise."""

    stream = _status_stream()
    _clear_status_line(stream)
    print(msg, file=stream)


def status_text(text: str) -> None:
    """Write raw status text to stderr in pipe mode, stdout otherwise."""

    stream = _status_stream()
    _clear_status_line(stream)
    stream.write(text)
    stream.flush()


def print_info(msg: str) -> None:
    """Print an informational message, clearing an active spinner line first."""

    prefix = info(icons.INFO) if colors_enabled() else icons.INFO
    if sys.stderr.isatty() and colors_enabled():
        print(f"\r\x1b[K{prefix} {msg}", file=sys.stderr)
    else:
        print(f"{prefix} {msg}", file=sys.stderr)


def term_width() -> int:
    """Return terminal width capped at 120 columns."""

    return min(shutil.get_terminal_size((80, 24)).columns, 120)


def boxed_message(title: str, content: str, width: int) -> str:
    """Render ``content`` inside a titled Unicode box."""

    width = max(width, 4)
    inner_width = max(0, width - 4)
    border_width = max(0, width - 2)
    title_text = bold(title) if colors_enabled() else title
    title_len = len(title)
    padding = max(0, border_width - title_len - 2)
    left_pad = padding // 2
    right_pad = padding - left_pad
    lines = [
        f"{box_chars.TOP_LEFT}{box_chars.HORIZONTAL * left_pad} {title_text} "
        f"{box_chars.HORIZONTAL * right_pad}{box_chars.TOP_RIGHT}"
    ]
    for raw_line in content.splitlines() or [""]:
        for wrapped in _wrap_line(raw_line, inner_width):
            pad = max(0, inner_width - len(wrapped))
            lines.append(f"{box_chars.VERTICAL} {wrapped}{' ' * pad} {box_chars.VERTICAL}")
    lines.append(f"{box_chars.BOTTOM_LEFT}{box_chars.HORIZONTAL * border_width}{box_chars.BOTTOM_RIGHT}")
    return "\n".join(lines)


def separator(width: int) -> str:
    """Return a horizontal separator line."""

    line = box_chars.HORIZONTAL * max(0, width)
    return dim(line) if colors_enabled() else line


def section_header(title: str, width: int) -> str:
    """Return a centered section header with decorative lines."""

    line_len = max(0, (width - len(title) - 2) // 2)
    line = box_chars.HORIZONTAL * line_len
    if colors_enabled():
        return f"{dim(line)} {bold(title)} {dim(line)}"
    return f"{line} {title} {line}"


async def with_spinner[T](message: str, awaitable: Awaitable[T]) -> T:
    """Await an operation while displaying a simple terminal spinner."""

    if not colors_enabled() or not sys.stdout.isatty():
        print(message, file=sys.stderr)
        return await awaitable
    task = asyncio.ensure_future(awaitable)
    spinner_task = asyncio.create_task(_spin(message, task))
    try:
        return await task
    finally:
        spinner_task.cancel()
        try:
            await spinner_task
        except asyncio.CancelledError:
            pass
        outcome = icons.SUCCESS if not task.cancelled() and task.exception() is None else icons.ERROR
        print(
            f"\r\x1b[K{success(outcome) if outcome == icons.SUCCESS else error(outcome)} {message}",
            file=sys.stdout,
            flush=True,
        )


async def with_spinner_result[T](message: str, awaitable: Awaitable[T]) -> T:
    """Alias for ``with_spinner`` for call sites expecting result-aware naming."""

    return await with_spinner(message, awaitable)


def _ansi(s: str, code: str, *, bold_text: bool = False) -> str:
    if not colors_enabled():
        return s
    prefix = f"1;{code}" if bold_text and code != "1" else code
    return f"\x1b[{prefix}m{s}\x1b[0m"


def _wrap_line(line: str, max_width: int) -> list[str]:
    if line == "":
        return [""]
    if max_width <= 0:
        return [line]
    wrapped: list[str] = []
    current = ""
    for word in line.split():
        if not current:
            current = word
        elif len(current) + 1 + len(word) <= max_width:
            current += " " + word
        else:
            wrapped.append(current)
            current = word
    if current:
        wrapped.append(current)
    return wrapped or [""]


async def _spin(message: str, task: asyncio.Future[Any]) -> None:
    frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
    index = 0
    while not task.done():
        print(f"\r{info(frames[index])} {message}", end="", file=sys.stdout, flush=True)
        index = (index + 1) % len(frames)
        await asyncio.sleep(0.08)


__all__ = [
    "bold",
    "box_chars",
    "boxed_message",
    "colors_enabled",
    "commit_type",
    "dim",
    "error",
    "icons",
    "info",
    "model",
    "pipe_mode",
    "print_info",
    "scope",
    "section_header",
    "separator",
    "status",
    "status_text",
    "success",
    "term_width",
    "warn",
    "warning",
    "with_spinner",
    "with_spinner_result",
]

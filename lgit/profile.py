"""JSONL profiling, trace, progress, and timing helpers."""

from __future__ import annotations

import json
import math
import os
import sys
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from types import TracebackType
from typing import Any, Self

TARGET = "lgit"

_TRACE_LOCK = threading.Lock()
_TRACE: TraceGuard | None = None


@dataclass(slots=True)
class TraceGuard:
    """Own an open JSONL trace file and flush events on close."""

    path: Path
    _file: Any
    _closed: bool = False

    def close(self) -> None:
        """Flush and close the trace file."""

        global _TRACE
        if self._closed:
            return
        _write_event("trace_stopped", path=str(self.path), pid=os.getpid())
        with _TRACE_LOCK:
            self._file.flush()
            self._file.close()
            self._closed = True
            if _TRACE is self:
                _TRACE = None

    def __enter__(self) -> Self:
        """Return this active guard."""

        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        """Close the trace file when leaving a context."""

        self.close()


@dataclass(frozen=True, slots=True)
class TimingPhase:
    """One named phase duration in a finalized timing report."""

    phase: str
    duration_ms: float
    share_pct: float = 0.0

    def to_dict(self) -> dict[str, float | str]:
        """Return the Rust-compatible JSON shape for this phase."""

        return {"phase": self.phase, "duration_ms": self.duration_ms, "share_pct": self.share_pct}


@dataclass(frozen=True, slots=True)
class TimingReport:
    """Final timing report matching the Rust ``TimingReport`` JSON shape."""

    total_ms: float
    phases: list[TimingPhase]

    def to_dict(self) -> dict[str, Any]:
        """Return the Rust-compatible JSON shape for this report."""

        return {"total_ms": self.total_ms, "phases": [phase.to_dict() for phase in self.phases]}


@dataclass(slots=True)
class TimingCollector:
    """Collect named phase timings and finalize share percentages at the end."""

    enabled: bool = True
    _start: float = field(default_factory=time.perf_counter)
    phases: list[TimingPhase] = field(default_factory=list)

    def record(self, phase: str, seconds: float) -> None:
        """Record one phase duration, preserving Rust rounding and trace events."""

        record_timing(self, phase, seconds)

    def finalize(self, total_seconds: float | None = None) -> TimingReport:
        """Build a timing report, using elapsed collector lifetime by default."""

        return finalize_timings(self, total_seconds)


@dataclass(slots=True)
class ProfileSection:
    """Synchronous and asynchronous context manager for a traced section."""

    name: str
    collector: TimingCollector | None = None
    _start: float = 0.0

    def __enter__(self) -> Self:
        """Enter a synchronous profiling section."""

        self._start = time.perf_counter()
        _write_event("section_started", section=self.name)
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        """Leave a synchronous profiling section and record elapsed time."""

        self._finish(exc)

    async def __aenter__(self) -> Self:
        """Enter an asynchronous profiling section."""

        self._start = time.perf_counter()
        _write_event("section_started", section=self.name)
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        """Leave an asynchronous profiling section and record elapsed time."""

        self._finish(exc)

    def _finish(self, exc: BaseException | None) -> None:
        seconds = time.perf_counter() - self._start if self._start else 0.0
        fields: dict[str, Any] = {
            "section": self.name,
            "elapsed_ms": seconds * 1000.0,
            "elapsed_us": _duration_us(seconds),
        }
        if exc is not None:
            fields["error"] = f"{type(exc).__name__}: {exc}"
        _write_event("section_finished", **fields)
        if self.collector is not None:
            record_timing(self.collector, self.name, seconds)


def env_flag_value_enabled(value: str | None) -> bool:
    """Return Rust-compatible truthiness for LLM_GIT_* boolean env values."""

    if value is None:
        return False
    return value.strip().lower() not in {"", "0", "false", "no", "off"}


def env_flag_enabled(name: str) -> bool:
    """Return true when environment variable ``name`` is set to an enabled value."""

    return env_flag_value_enabled(os.environ.get(name))


def trace_enabled() -> bool:
    """Return true when ``LLM_GIT_TRACE`` enables API trace logging."""

    return env_flag_enabled("LLM_GIT_TRACE")


def progress_enabled() -> bool:
    """Return true when LLM progress lines should be printed."""

    return env_flag_enabled("LLM_GIT_PROGRESS") or trace_enabled()


def trace_file_path(args: Any | None = None) -> Path | None:
    """Resolve CLI/env JSONL trace output path, preferring CLI args."""

    if args is not None:
        trace_output = getattr(args, "trace_output", None)
        if trace_output is not None:
            return Path(trace_output)
    env_path = os.environ.get("LLM_GIT_TRACE_FILE")
    return Path(env_path) if env_path is not None and env_path != "" else None


def timings_enabled(args: Any | None = None) -> bool:
    """Return true when phase timing collection should be enabled."""

    if args is not None and (
        getattr(args, "debug_output", None) is not None or getattr(args, "trace_output", None) is not None
    ):
        return True
    return trace_file_path() is not None or "LLM_GIT_TRACE" in os.environ


def init_file_tracing(path: str | os.PathLike[str]) -> TraceGuard:
    """Initialize process-wide JSONL profiling to ``path``."""

    global _TRACE
    trace_path = Path(path)
    if trace_path.parent and str(trace_path.parent) != ".":
        trace_path.parent.mkdir(parents=True, exist_ok=True)
    file = trace_path.open("a", encoding="utf-8", buffering=1)
    guard = TraceGuard(path=trace_path, _file=file)
    with _TRACE_LOCK:
        _TRACE = guard
    _write_event("trace_started", path=str(trace_path), pid=os.getpid())
    return guard


def enabled() -> bool:
    """Return true when file tracing is currently active."""

    return _TRACE is not None and not _TRACE._closed


def section(name: str, collector: TimingCollector | None = None) -> ProfileSection:
    """Create a profiling context manager for a logical section."""

    return ProfileSection(name, collector)


def create_timing_collector(enabled: bool = True) -> TimingCollector:
    """Create a phase-timing collector."""

    return TimingCollector(enabled=enabled)


def record_timing(collector: TimingCollector | None, phase: str, seconds: float) -> None:
    """Record a named phase duration and emit the Rust-compatible trace event."""

    _write_event(
        "timing_recorded",
        section=phase,
        elapsed_ms=seconds * 1000.0,
        elapsed_us=_duration_us(seconds),
    )
    if collector is not None and collector.enabled:
        collector.phases.append(TimingPhase(phase=phase, duration_ms=round_ms(seconds), share_pct=0.0))


def finalize_timings(
    collector: TimingCollector | list[TimingPhase], total_seconds: float | None = None
) -> TimingReport:
    """Finalize collected timings by computing total milliseconds and shares."""

    if isinstance(collector, TimingCollector):
        phases = collector.phases
        elapsed = time.perf_counter() - collector._start if total_seconds is None else total_seconds
    else:
        phases = collector
        elapsed = 0.0 if total_seconds is None else total_seconds

    total_ms = round_ms(elapsed)
    finalized: list[TimingPhase] = []
    for phase in phases:
        share_pct = _round_one_decimal((phase.duration_ms / total_ms) * 100.0) if total_ms > 0.0 else 0.0
        finalized.append(TimingPhase(phase=phase.phase, duration_ms=phase.duration_ms, share_pct=share_pct))
    return TimingReport(total_ms=total_ms, phases=finalized)


def format_timing_report(timings: TimingCollector | TimingReport) -> str:
    """Format the human ``[TIMING]`` report printed when ``LLM_GIT_TRACE`` is set."""

    report = _coerce_report(timings)
    lines = [f"[TIMING] total={report.total_ms:.1f}ms"]
    lines.extend(
        f"[TIMING] {phase.phase:>28} {phase.duration_ms:>8.1f}ms {phase.share_pct:>5.1f}%" for phase in report.phases
    )
    return "\n".join(lines)


def write_timings_json(path: str | os.PathLike[str], timings: TimingCollector | TimingReport) -> Path:
    """Write a pretty ``timings.json`` debug artifact and return its path."""

    output_path = _timings_json_path(path)
    if output_path.parent and str(output_path.parent) != ".":
        output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(_coerce_report(timings).to_dict(), indent=2), encoding="utf-8")
    return output_path


def emit_timing_report(args: Any, timings: TimingCollector | TimingReport) -> TimingReport:
    """Write/debug-log/print a finalized timing report using Rust CLI rules."""

    report = _coerce_report(timings)
    debug_output = getattr(args, "debug_output", None)
    if debug_output is not None:
        write_timings_json(debug_output, report)

    _write_event("timing_report_finished", total_ms=report.total_ms, phase_count=len(report.phases))

    if "LLM_GIT_TRACE" in os.environ:
        print(format_timing_report(report), file=sys.stderr)
    return report


def print_llm_progress(message: str | Callable[[], str]) -> None:
    """Print an LLM progress line when ``LLM_GIT_PROGRESS`` or trace is enabled."""

    if not progress_enabled():
        return
    text = message() if callable(message) else message
    try:
        from . import style

        style.print_info(text)
    except Exception:
        print(text, file=sys.stderr)


def print_trace(message: str) -> None:
    """Print a low-level ``[TRACE]`` line when ``LLM_GIT_TRACE`` is enabled."""

    if not trace_enabled():
        return
    if _stdout_is_status_stream():
        print("\r\x1b[K", end="", file=sys.stdout, flush=True)
    print(f"[TRACE] {message}", file=sys.stderr)


def trace_event(event: str, *, level: str = "INFO", **fields: Any) -> None:
    """Emit one JSONL trace event if file tracing is active."""

    _write_event(event, level=level, **fields)


def _coerce_report(timings: TimingCollector | TimingReport) -> TimingReport:
    if isinstance(timings, TimingReport):
        return timings
    return timings.finalize()


def _timings_json_path(path: str | os.PathLike[str]) -> Path:
    output_path = Path(path)
    if output_path.exists() and output_path.is_dir():
        return output_path / "timings.json"
    if output_path.name != "timings.json" and output_path.suffix == "":
        return output_path / "timings.json"
    return output_path


def _round_one_decimal(value: float) -> float:
    return math.floor(value * 10.0 + 0.5) / 10.0


def round_ms(seconds: float) -> float:
    """Round seconds to milliseconds with Rust's one-decimal rounding rule."""

    return _round_one_decimal(seconds * 1000.0)


def _duration_us(seconds: float) -> int:
    return max(0, min(int(seconds * 1_000_000), (1 << 64) - 1))


def _stdout_is_status_stream() -> bool:
    try:
        from . import style

        return not style.pipe_mode()
    except Exception:
        return sys.stdout.isatty()


def _write_event(event: str, *, level: str = "INFO", **fields: Any) -> None:
    guard = _TRACE
    if guard is None or guard._closed:
        return
    record = {
        "ts": time.time(),
        "target": TARGET,
        "level": level,
        "event": event,
        **fields,
    }
    try:
        line = json.dumps(record, sort_keys=True, separators=(",", ":"), default=str)
        with _TRACE_LOCK:
            if not guard._closed:
                guard._file.write(line + "\n")
    except Exception:
        return


__all__ = [
    "TARGET",
    "ProfileSection",
    "TimingCollector",
    "TimingPhase",
    "TimingReport",
    "TraceGuard",
    "create_timing_collector",
    "emit_timing_report",
    "enabled",
    "env_flag_enabled",
    "env_flag_value_enabled",
    "finalize_timings",
    "format_timing_report",
    "init_file_tracing",
    "print_llm_progress",
    "print_trace",
    "progress_enabled",
    "record_timing",
    "round_ms",
    "section",
    "timings_enabled",
    "trace_enabled",
    "trace_event",
    "trace_file_path",
    "write_timings_json",
]

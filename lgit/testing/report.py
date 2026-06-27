"""Dark HTML report generation for fixture test results."""

from __future__ import annotations

from collections.abc import Sequence
from datetime import UTC, datetime
from html import escape
from pathlib import Path

from .fixture import Fixture
from .runner import RunResult, TestSummary


def generate_html_report(results: Sequence[RunResult], fixtures: Sequence[Fixture], output_path: str | Path) -> Path:
    """Write a dark HTML fixture report and return the output path."""

    path = Path(output_path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(render_report(results, fixtures, TestSummary.from_results(list(results))), encoding="utf-8")
    return path


def render_report(results: Sequence[RunResult], fixtures: Sequence[Fixture], summary: TestSummary | None = None) -> str:
    summary = TestSummary.from_results(list(results)) if summary is None else summary
    fixture_by_name = {fixture.name: fixture for fixture in fixtures}
    rows = "\n".join(_render_fixture_result(result, fixture_by_name.get(result.name)) for result in results)
    generated = datetime.now(UTC).strftime("%Y-%m-%d %H:%M:%S UTC")
    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>lgit Fixture Test Report</title>
  <style>
    :root {{
      --bg: #0d1117;
      --fg: #c9d1d9;
      --muted: #8b949e;
      --border: #30363d;
      --card: #161b22;
      --green: #3fb950;
      --red: #f85149;
      --yellow: #d29922;
      --blue: #58a6ff;
      --purple: #a371f7;
    }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; padding: 2rem; background: var(--bg); color: var(--fg); font: 15px/1.55 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }}
    .container {{ max-width: 1400px; margin: 0 auto; }}
    h1 {{ margin: 0 0 .5rem; font-size: 1.8rem; }}
    .timestamp {{ color: var(--muted); margin: 0 0 1.5rem; }}
    .summary {{ display: flex; flex-wrap: wrap; gap: 1rem; margin-bottom: 2rem; }}
    .stat {{ min-width: 120px; padding: 1rem 1.25rem; background: var(--card); border: 1px solid var(--border); border-radius: 8px; }}
    .stat-value {{ font-size: 2rem; font-weight: 700; }}
    .stat-label {{ color: var(--muted); font-size: .85rem; }}
    .passed .stat-value, .match {{ color: var(--green); }}
    .failed .stat-value, .mismatch, .error-color {{ color: var(--red); }}
    .no-golden .stat-value, .warn {{ color: var(--yellow); }}
    .fixture {{ margin-bottom: 1rem; background: var(--card); border: 1px solid var(--border); border-radius: 8px; overflow: hidden; }}
    .fixture-header {{ display: flex; justify-content: space-between; align-items: center; gap: 1rem; padding: 1rem 1.25rem; cursor: pointer; border-bottom: 1px solid var(--border); }}
    .fixture-header:hover {{ background: rgba(255, 255, 255, .035); }}
    .fixture-name {{ font-weight: 650; word-break: break-word; }}
    .badge {{ white-space: nowrap; padding: .25rem .7rem; border-radius: 999px; font-size: .8rem; font-weight: 600; }}
    .badge.passed {{ color: var(--green); background: rgba(63, 185, 80, .13); }}
    .badge.failed, .badge.error {{ color: var(--red); background: rgba(248, 81, 73, .13); }}
    .badge.no-golden {{ color: var(--yellow); background: rgba(210, 153, 34, .13); }}
    .fixture-content {{ display: none; padding: 1.25rem; }}
    .fixture.expanded .fixture-content {{ display: block; }}
    .diff-row {{ display: flex; gap: 1rem; margin-bottom: .45rem; }}
    .diff-label {{ width: 6rem; min-width: 6rem; color: var(--muted); font-weight: 600; }}
    .comparison {{ display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; margin-top: 1.25rem; }}
    @media (max-width: 900px) {{ .comparison {{ grid-template-columns: 1fr; }} }}
    h3 {{ margin: 0 0 .5rem; text-transform: uppercase; letter-spacing: .05em; font-size: .78rem; }}
    h3.golden {{ color: var(--purple); }}
    h3.actual {{ color: var(--blue); }}
    .message-box, .error-message {{ white-space: pre-wrap; word-break: break-word; padding: 1rem; border-radius: 8px; font: 13px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }}
    .message-box {{ background: var(--bg); border: 1px solid var(--border); }}
    .error-message {{ color: var(--red); background: rgba(248, 81, 73, .08); border: 1px solid rgba(248, 81, 73, .5); }}
  </style>
</head>
<body>
  <div class="container">
    <h1>lgit Fixture Test Report</h1>
    <p class="timestamp">Generated: {escape(generated)}</p>
    <div class="summary">
      {_stat("Total", summary.total)}
      {_stat("Passed", summary.passed, "passed")}
      {_stat("Failed", summary.failed, "failed")}
      {_stat("No Golden", summary.no_golden, "no-golden")}
      {_stat("Errors", summary.errors, "failed")}
    </div>
    {rows}
  </div>
  <script>
    document.querySelectorAll('.fixture-header').forEach((header) => {{
      header.addEventListener('click', () => header.parentElement.classList.toggle('expanded'));
    }});
    document.querySelectorAll('.fixture.failed, .fixture.error').forEach((fixture) => fixture.classList.add('expanded'));
  </script>
</body>
</html>
"""


def _stat(label: str, value: int, css: str = "") -> str:
    return f'<div class="stat {css}"><div class="stat-value">{value}</div><div class="stat-label">{escape(label)}</div></div>'


def _render_fixture_result(result: RunResult, fixture: Fixture | None) -> str:
    status_class, status_text = _status(result)
    body = _render_error(result.error) if result.error else _render_success_body(result, fixture)
    return f"""
    <section class="fixture {status_class}">
      <div class="fixture-header">
        <span class="fixture-name">{escape(result.name)}</span>
        <span class="badge {status_class}">{status_text}</span>
      </div>
      <div class="fixture-content">{body}</div>
    </section>"""


def _render_success_body(result: RunResult, fixture: Fixture | None) -> str:
    if result.comparison is None:
        return _render_actual_only(result)
    cmp = result.comparison
    golden = fixture.golden if fixture is not None else None
    type_row = ""
    golden_message = ""
    if golden is not None:
        type_row = _diff_row(
            "Type",
            f"{escape(str(golden.analysis.commit_type))} &rarr; {escape(str(result.analysis.commit_type))}",
            "match" if cmp.type_match else "mismatch",
        )
        golden_message = f"""
        <div>
          <h3 class="golden">Golden (Expected)</h3>
          <div class="message-box">{escape(golden.final_message or _analysis_summary(golden.analysis))}</div>
        </div>"""
    scope_value = escape(cmp.scope_diff or _scope_text(result.analysis, none="(none)"))
    details_value = f"{cmp.golden_detail_count} golden &rarr; {cmp.actual_detail_count} actual"
    return f"""
      <div>
        {type_row}
        {_diff_row("Scope", scope_value, "match" if cmp.scope_match else "mismatch")}
        {_diff_row("Details", details_value)}
      </div>
      <div class="comparison">
        {golden_message}
        <div>
          <h3 class="actual">Actual (Current)</h3>
          <div class="message-box">{escape(result.final_message)}</div>
        </div>
      </div>"""


def _render_actual_only(result: RunResult) -> str:
    return f"""
      <div>
        {_diff_row("Type", escape(str(result.analysis.commit_type)))}
        {_diff_row("Scope", escape(_scope_text(result.analysis, none="(none)")))}
        {_diff_row("Details", f"{len(result.analysis.details)} points")}
        <h3 class="actual" style="margin-top: 1rem;">Generated Message</h3>
        <div class="message-box">{escape(result.final_message)}</div>
      </div>"""


def _render_error(error: str | None) -> str:
    return f'<div class="error-message">{escape(error or "")}</div>'


def _diff_row(label: str, value: str, css: str = "") -> str:
    return f'<div class="diff-row"><span class="diff-label">{escape(label)}:</span><span class="{css}">{value}</span></div>'


def _status(result: RunResult) -> tuple[str, str]:
    if result.error is not None:
        return "error", "Error"
    if result.comparison is None:
        return "no-golden", "No Golden"
    return ("passed", "Passed") if result.comparison.passed else ("failed", "Failed")


def _scope_text(analysis: object, *, none: str = "null") -> str:
    scope = getattr(analysis, "scope", None)
    return none if scope is None else str(scope)


def _analysis_summary(analysis: object) -> str:
    commit_type = str(getattr(analysis, "commit_type", ""))
    scope = getattr(analysis, "scope", None)
    summary = getattr(analysis, "summary", None)
    details = getattr(analysis, "details", ())
    header = commit_type + (f"({scope})" if scope else "")
    if summary:
        header += f": {summary}"
    detail_lines = "\n".join(f"- {getattr(detail, 'text', detail)}" for detail in details)
    return header + ("\n\n" + detail_lines if detail_lines else "")


__all__ = ["generate_html_report", "render_report"]

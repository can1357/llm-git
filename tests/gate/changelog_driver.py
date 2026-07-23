"""Subprocess driver for the changelog quality gate.

Runs a batch of changelog prompts against whichever ``lgit`` is first on
``PYTHONPATH`` (baseline worktree or the working tree) and prints raw model
responses as JSON. Invoked by ``tests/test_changelog_gate.py``; only uses
``lgit`` APIs that exist in both versions. The ``observations`` field of a job
is applied only when the running ``lgit`` supports it, so the baseline silently
falls back to its diff-based prompt.

Usage: ``python changelog_driver.py <jobs.json>`` -> JSON array on stdout.
"""

from __future__ import annotations

import asyncio
import inspect
import json
import sys
from dataclasses import replace
from pathlib import Path
from typing import Any

import lgit
from lgit.api import OneShotSpec, run_oneshot
from lgit.config import CommitConfig
from lgit.templates import render_changelog_prompt

CONCURRENCY = 6


async def _run_job(base: CommitConfig, job: dict[str, Any], supports_observations: bool) -> dict[str, Any]:
    kwargs = {}
    if supports_observations and job.get("observations"):
        kwargs["observations"] = job["observations"]
    prompt = render_changelog_prompt(
        job["changelog_path"],
        bool(job["is_package"]),
        job["stat"],
        job["diff"],
        existing_entries=job.get("existing_entries"),
        authored_entries=job.get("authored_entries"),
        can_revise=bool(job.get("can_revise")),
        **kwargs,
    )
    config = replace(base, analysis_model=job["model"])
    spec = OneShotSpec(
        operation="changelog",
        model=job["model"],
        prompt_family="changelog",
        system_prompt=prompt.system,
        user_prompt=prompt.user,
        tool_name="create_changelog_entries",
        progress_label=f"gate {job['id']}",
        cacheable=False,
        reasoning_effort=base.changelog_reasoning_effort,
    )
    try:
        response = await run_oneshot(config, spec)
        text = response.text_content if getattr(response, "text_content", None) else str(response.output)
        return {"id": job["id"], "text": text, "error": None, "user_prompt": prompt.user}
    except Exception as exc:  # Live-eval driver: report, never crash the batch.
        return {"id": job["id"], "text": "", "error": f"{type(exc).__name__}: {exc}", "user_prompt": prompt.user}


async def _main() -> None:
    payload = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    base = CommitConfig.load()
    supports_observations = "observations" in inspect.signature(render_changelog_prompt).parameters
    semaphore = asyncio.Semaphore(CONCURRENCY)

    async def bounded(job: dict[str, Any]) -> dict[str, Any]:
        async with semaphore:
            return await _run_job(base, job, supports_observations)

    results = await asyncio.gather(*(bounded(job) for job in payload["jobs"]))
    print(
        json.dumps(
            {
                "lgit_file": lgit.__file__,
                "supports_observations": supports_observations,
                "results": list(results),
            }
        )
    )


if __name__ == "__main__":
    asyncio.run(_main())

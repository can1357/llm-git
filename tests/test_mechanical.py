from __future__ import annotations

import asyncio
from collections.abc import Sequence
from typing import Any

import pytest
from lgit import mechanical
from lgit.config import CommitConfig
from lgit.diffing import split_hunks
from lgit.judgment import Answers, Choice, ChoiceAnswer, Judge, JudgmentRequest, NoulAnswer
from lgit.mechanical import (
    KIND_MARKER,
    MECHANICAL_MARKER,
    HunkJudgments,
    HunkRef,
    HunkVerdict,
    PrunableHunk,
    judge_hunks,
    kind_criteria,
    prune_mechanical_hunks,
    render_pruned_diff,
)

_DIFF = """diff --git a/src/app.py b/src/app.py
index 1111111..2222222 100644
--- a/src/app.py
+++ b/src/app.py
@@ -1,3 +1,4 @@
 import os
+from .util import helper
 import sys
@@ -20,3 +21,5 @@ def run():
     value = load()
+    if value is None:
+        raise ValueError("missing")
     return value
diff --git a/src/util.py b/src/util.py
index 3333333..4444444 100644
--- a/src/util.py
+++ b/src/util.py
@@ -1,2 +1,2 @@
-from x import a, b
+from x import a, b, c
diff --git a/logo.png b/logo.png
index 5555555..6666666 100644
Binary files a/logo.png and b/logo.png differ"""


class _FakeJudge(Judge):
    """Answers mechanical/kind questions from a key→verdict table, recording requests."""

    def __init__(self, table: dict[str, HunkVerdict]) -> None:
        super().__init__(base_url="https://judge.test", model="fake", api_key="k")
        self.table = table
        self.requests: list[JudgmentRequest] = []

    async def judge_all(self, operation: str, requests: Sequence[JudgmentRequest]) -> list[Answers | None]:
        self.requests.extend(requests)
        out: list[Answers | None] = []
        for request in requests:
            answers: Answers = {}
            for key, question in request.questions.items():
                verdict = self.table.get(key[2:], HunkVerdict(0.0))
                if isinstance(question, Choice):
                    kind = verdict.kind or next(iter(question.criteria))
                    answers[key] = ChoiceAnswer(kind, {kind: verdict.kind_probability}, verdict.kind_probability)
                else:
                    answers[key] = NoulAnswer(verdict.mechanical)
            out.append(answers)
        return out


def _config(**overrides: Any) -> CommitConfig:
    return CommitConfig(judgment_api_key="k", **{"mechanical_prune_min_tokens": 0, **overrides})


def test_split_hunks_counts_changes_and_round_trips_text() -> None:
    content = "@@ -1,2 +1,3 @@\n a\n+b\n-c\n@@ -9,1 +10,1 @@ ctx\n-d\n+e"
    hunks = split_hunks(content)
    assert [(h.additions, h.deletions) for h in hunks] == [(1, 1), (1, 1)]
    assert "\n".join(h.text for h in hunks) == content
    assert split_hunks("no hunks here") == []


def test_render_pruned_diff_marks_hunks_hints_kinds_and_collapses_fully_mechanical_files() -> None:
    files = [
        (
            "H1",
            [
                PrunableHunk("@@ -1 +1 @@\n+a", True, 1, 0, kind="refactor"),
                PrunableHunk("@@ -5 +5 @@ ctx\n+b", False, 1, 0, kind="feat"),
            ],
        ),
        ("H2", [PrunableHunk("@@ -1 +1 @@\n+c", True, 1, 0), PrunableHunk("@@ -3 +3 @@\n-d", True, 0, 1)]),
        ("H3 binary", []),
    ]
    pruned = render_pruned_diff(files)
    assert pruned.pruned and pruned.omitted_hunks == 3 and pruned.omitted_lines == 3 and pruned.total_hunks == 4
    # A pruned hunk never shows its kind; a kept hunk carries the hint on its header line only.
    assert pruned.diff == (
        f"H1\n@@ -1 +1 @@\n{MECHANICAL_MARKER}: +1/-0 lines]\n@@ -5 +5 @@ ctx {KIND_MARKER} feat]\n+b\n"
        f"H2\n{MECHANICAL_MARKER}: 2 hunks, +1/-1 lines]\n"
        "H3 binary"
    )


@pytest.mark.parametrize(
    "hunks",
    [
        [PrunableHunk("@@\n+a", True, 1, 0), PrunableHunk("@@\n+b", True, 1, 0)],  # everything mechanical
        [PrunableHunk("@@\n+a", True, 1, 0)],  # nothing to isolate
        [PrunableHunk("@@\n+a", False, 1, 0), PrunableHunk("@@\n+b", False, 1, 0)],  # nothing mechanical
    ],
)
def test_render_pruned_diff_guards_leave_diff_untouched(hunks: list[PrunableHunk]) -> None:
    pruned = render_pruned_diff([("H", hunks)])
    assert not pruned.pruned
    assert pruned.diff == "H\n" + "\n".join(h.text for h in hunks)


def test_hunk_judgments_thresholds_drop_all_mechanical_and_low_confidence_kinds() -> None:
    verdicts = {
        "a": HunkVerdict(0.9, "refactor", 0.9),
        "b": HunkVerdict(0.2, "feat", 0.95),
        "c": HunkVerdict(0.1, "test", 0.4),
    }
    judgments = HunkJudgments.from_verdicts(verdicts, total=3, threshold=0.7)
    assert judgments == HunkJudgments(frozenset({"a"}), {"b": "feat"})
    assert judgments.hunk("b", "@@\n+x", 1, 0) == PrunableHunk("@@\n+x", False, 1, 0, "feat")
    everything = HunkJudgments.from_verdicts({k: HunkVerdict(0.9, "feat", 0.9) for k in "abc"}, total=3, threshold=0.7)
    assert everything.mechanical == frozenset() and everything.kinds == {k: "feat" for k in "abc"}
    assert not HunkJudgments.EMPTY and HunkJudgments(frozenset(), {"a": "feat"})


def test_kind_criteria_follows_configured_types() -> None:
    criteria = kind_criteria(CommitConfig())
    assert set(criteria) == set(mechanical.HUNK_KINDS)
    assert "config" not in criteria and "ux" not in criteria
    trimmed = CommitConfig()
    trimmed.types = {"feat": trimmed.types["feat"], "docs": trimmed.types["docs"]}
    assert list(kind_criteria(trimmed)) == ["feat", "docs"]


def test_prune_skips_without_judge_or_below_size_threshold() -> None:
    judge = _FakeJudge({"0:0": HunkVerdict(0.99)})
    untouched = asyncio.run(prune_mechanical_hunks(_DIFF, None, _config()))
    assert untouched.diff == _DIFF and not untouched.pruned
    small = asyncio.run(prune_mechanical_hunks(_DIFF, judge, _config(mechanical_prune_min_tokens=10_000)))
    assert small.diff == _DIFF and judge.requests == []


def test_prune_applies_threshold_per_hunk_hints_kinds_and_keeps_binary_sections() -> None:
    judge = _FakeJudge(
        {"0:0": HunkVerdict(0.9, "refactor", 0.9), "0:1": HunkVerdict(0.2, "fix", 0.97), "1:0": HunkVerdict(0.75)}
    )
    pruned = asyncio.run(prune_mechanical_hunks(_DIFF, judge, _config(mechanical_hunk_threshold=0.7)))
    assert pruned.omitted_hunks == 2 and pruned.total_hunks == 3
    assert "+from .util import helper" not in pruned.diff
    assert "@@ -20,3 +21,5 @@ def run(): [judged: fix]\n     value = load()" in pruned.diff
    assert f"{MECHANICAL_MARKER}: 1 hunk, +1/-1 lines]" in pruned.diff
    assert pruned.diff.endswith("Binary files a/logo.png and b/logo.png differ")
    # One request per file, binary file never judged; each hunk gets a mechanical and a kind question.
    assert [request.state["path"] for request in judge.requests] == ["src/app.py", "src/util.py"]
    assert list(judge.requests[0].questions) == ["m:0:0", "k:0:0", "m:0:1", "k:0:1"]
    assert all("rubric" in request.state for request in judge.requests)

    stricter = asyncio.run(
        prune_mechanical_hunks(_DIFF, _FakeJudge({"0:0": HunkVerdict(0.9), "1:0": HunkVerdict(0.75)}), _config())
    )
    assert stricter.omitted_hunks == 2  # default 0.7 threshold prunes both


def test_judge_hunks_chunks_large_files_and_merges_verdicts(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(mechanical, "_MAX_HUNKS_PER_REQUEST", 3)
    refs = [HunkRef(f"k{i}", f"@@\n+{i}") for i in range(7)]
    judge = _FakeJudge({f"k{i}": HunkVerdict(i / 10, "feat", 0.8) for i in range(7)})
    verdicts = asyncio.run(judge_hunks(judge, [("a.py", refs)], CommitConfig()))
    assert [len(request.questions) for request in judge.requests] == [6, 6, 2]
    assert verdicts == {f"k{i}": HunkVerdict(pytest.approx(i / 10), "feat", 0.8) for i in range(7)}

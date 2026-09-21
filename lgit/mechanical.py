"""Hunk-level judgments: prune mechanical after-effects and tag each hunk's intent.

A commit's diff mixes hunks that carry intent (new behaviour, a fix, a design
change) with hunks that merely follow from them — import lists updated for a
moved symbol, call sites updated for a renamed one, re-exports, formatting,
generated output. The analysis prompts already tell the model to ignore the
latter, yet every such hunk is still paid for and still competes for attention.

:func:`judge_hunks` asks the judgment model, one file per request, two things
about every hunk: whether it is a mechanical after-effect (a probability) and
which conventional-commit kind its intent has (a choice over
:data:`HUNK_KINDS`). :func:`render_pruned_diff` then replaces mechanical hunks
with a one-line marker — so the analysis model still sees which files and
regions changed (for type, scope, and wide-change decisions) without the
bodies — and annotates the remaining hunk headers with ``[judged: <kind>]`` as
a hint for the smaller analysis/fast models. Both the standard/fast commit flows
and compose (observe diff, planner summary, per-group analysis) use this.

Guards: a diff that is entirely mechanical is left untouched (the mechanical
change *is* the change, e.g. a rename sweep), and so is a diff with fewer than
two hunks (nothing to isolate). Prompt-side only — pruned text is never staged.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, ClassVar, Self

from .diffing import parse_diff, split_hunks
from .judgment import (
    JUDGMENT_STATE_CHAR_BUDGET,
    Choice,
    ChoiceAnswer,
    Judge,
    JudgmentRequest,
    Noul,
    NoulAnswer,
    chunk_by_chars,
    clip_text,
)
from .models import resolve_model_chain
from .pricing import rates_for_model

if TYPE_CHECKING:
    from .config import CommitConfig

MECHANICAL_MARKER = "[mechanical after-effect omitted"
KIND_MARKER = "[judged:"

# Commit types that make sense for a single hunk; the configured type descriptions
# supply the rubric. `config`/`ux`/`hotfix`/`release`/... are commit-level
# judgments that mislead at hunk granularity (a constant tweak is not "config").
HUNK_KINDS = ("feat", "fix", "refactor", "perf", "docs", "test", "style", "build", "ci", "deps", "security", "chore")
# A kind is only shown as a hint when the judge is at least this sure of it.
KIND_CONFIDENCE_FLOOR = 0.6

# The full rubric travels in the state (read once per request); each per-hunk
# question restates only the gist. Phrasing was A/B-tested on real commits: a
# bare "per the rubric" question halves the separation between import hunks
# and logic hunks, while the gist plus yes/no criteria keeps imports at 0.75+
# and behaviour changes under 0.3.
MECHANICAL_RUBRIC = (
    "A hunk is a MECHANICAL AFTER-EFFECT when it only follows through on a change made elsewhere "
    "and carries no intent of its own worth a line in a commit message: import lists or re-exports "
    "updated for an added, moved, or removed symbol; call sites or signatures updated for a rename, "
    "a moved definition, or a new pass-through parameter; type annotation propagation; "
    "formatting, whitespace, line wrapping, or lint-driven rewrites; generated files, lockfiles, "
    "snapshots, or fixtures updated to match new names. A hunk is NOT mechanical when it adds, "
    "removes, or changes behaviour, control flow, data, configuration values, documentation content, "
    "tests of behaviour, or an API surface — even when small."
)

_MAX_HUNKS_PER_REQUEST = 48
_MECHANICAL_YES = (
    "mechanical after-effect: follow-through only (imports, exports, call sites, renames, formatting, generated output)"
)
_MECHANICAL_NO = (
    "carries its own intent: behaviour, logic, data, config values, docs content, tests, or API surface changed"
)


@dataclass(frozen=True, slots=True)
class HunkRef:
    """One hunk to judge: a caller-chosen key and the hunk text as it appears in the diff."""

    key: str
    text: str


@dataclass(frozen=True, slots=True)
class HunkVerdict:
    """Raw judgment for one hunk: mechanical probability and the most likely intent kind."""

    mechanical: float
    kind: str | None = None
    kind_probability: float = 0.0


@dataclass(frozen=True, slots=True)
class HunkJudgments:
    """Thresholded verdicts for a set of hunks: which to prune and which kind hint each keeps.

    ``mechanical`` is empty when every judged hunk was mechanical (the guard in
    :func:`render_pruned_diff` would refuse to prune anyway, and the planner must
    then see the hunks). ``kinds`` only holds hints above
    :data:`KIND_CONFIDENCE_FLOOR` for non-mechanical hunks.
    """

    mechanical: frozenset[str] = frozenset()
    kinds: Mapping[str, str] = field(default_factory=dict)

    EMPTY: ClassVar[HunkJudgments]

    @classmethod
    def from_verdicts(cls, verdicts: Mapping[str, HunkVerdict], total: int, threshold: float) -> Self:
        """Apply ``threshold`` to raw verdicts over ``total`` judged hunks."""
        mechanical = frozenset(key for key, verdict in verdicts.items() if verdict.mechanical >= threshold)
        if len(mechanical) >= total:
            mechanical = frozenset()
        kinds = {
            key: verdict.kind
            for key, verdict in verdicts.items()
            if verdict.kind and verdict.kind_probability >= KIND_CONFIDENCE_FLOOR and key not in mechanical
        }
        return cls(mechanical, kinds)

    def __bool__(self) -> bool:
        return bool(self.mechanical or self.kinds)

    def hunk(self, key: str, text: str, additions: int, deletions: int) -> PrunableHunk:
        """Build the renderable hunk for ``key``."""
        return PrunableHunk(text, key in self.mechanical, additions, deletions, self.kinds.get(key))


HunkJudgments.EMPTY = HunkJudgments()


@dataclass(frozen=True, slots=True)
class PrunableHunk:
    """A hunk of a prompt-bound diff with its verdicts."""

    text: str
    mechanical: bool
    additions: int
    deletions: int
    kind: str | None = None


@dataclass(frozen=True, slots=True)
class PrunedDiff:
    """A prompt-bound diff with mechanical hunks replaced by markers, plus what was removed."""

    diff: str
    total_hunks: int
    omitted_hunks: int
    omitted_lines: int

    @property
    def pruned(self) -> bool:
        """Whether any hunk was replaced."""
        return self.omitted_hunks > 0


def kind_criteria(config: CommitConfig) -> dict[str, str]:
    """Hunk-kind rubric: :data:`HUNK_KINDS` present in the configured types, with their descriptions."""
    return {kind: config.types[kind].description for kind in HUNK_KINDS if kind in config.types}


async def judge_hunks(
    judge: Judge, files: Sequence[tuple[str, Sequence[HunkRef]]], config: CommitConfig
) -> dict[str, HunkVerdict]:
    """Judge every hunk's mechanical probability and intent kind; hunks whose request failed are absent."""
    kinds = kind_criteria(config) if config.mechanical_kind_hints else {}
    # One request per file, deliberately: packing several files' hunks into one
    # request cut tokens ~15% but pushed a core logic hunk from 0.04 to 0.78
    # mechanical in testing — a pruned intent hunk costs far more than tokens.
    # Rubrics live in the state (sent once) and the kind choice lists bare labels;
    # the mechanical question keeps its full wording (cheaper phrasings blur the
    # import/logic separation).
    labels: dict[str, str | None] = dict.fromkeys(kinds)
    requests: list[JudgmentRequest] = []
    for path, hunks in files:
        for chunk in _chunk_hunks(hunks):
            questions: dict[str, Noul | Choice] = {}
            for hunk in chunk:
                questions[f"m:{hunk.key}"] = _mechanical_question(hunk.key)
                if kinds:
                    questions[f"k:{hunk.key}"] = Choice(f"Commit type of hunk {hunk.key} per the kinds rubric?", labels)
            requests.append(JudgmentRequest(_hunk_state(path, chunk, kinds), questions))
    verdicts: dict[str, HunkVerdict] = {}
    for answers in await judge.judge_all("hunks", requests):
        if answers is None:
            continue
        for key, answer in answers.items():
            if key.startswith("m:") and isinstance(answer, NoulAnswer):
                verdicts[key[2:]] = HunkVerdict(answer.probability)
        for key, answer in answers.items():
            if key.startswith("k:") and isinstance(answer, ChoiceAnswer) and key[2:] in verdicts:
                verdicts[key[2:]] = HunkVerdict(
                    verdicts[key[2:]].mechanical, answer.choice, answer.probabilities.get(answer.choice, 0.0)
                )
    return verdicts


def _mechanical_question(key: str) -> Noul:
    return Noul(
        f"Is hunk {key} a mechanical after-effect of a change made elsewhere (imports/exports, call-site or rename "
        "propagation, signature pass-through, formatting, generated/lockfile/fixture updates) rather than a change "
        "carrying its own intent worth a commit-message line?",
        yes=_MECHANICAL_YES,
        no=_MECHANICAL_NO,
    )


def _hunk_state(path: str, hunks: Sequence[HunkRef], kinds: Mapping[str, str]) -> dict[str, str]:
    state = {"rubric": MECHANICAL_RUBRIC}
    if kinds:
        state["kinds"] = "\n".join(f"{kind}: {description}" for kind, description in kinds.items())
    state["path"] = path
    state["diff"] = "\n".join(f"### {hunk.key}\n{clip_text(hunk.text)}" for hunk in hunks)
    return state


def _chunk_hunks(hunks: Sequence[HunkRef]) -> list[list[HunkRef]]:
    by_size = chunk_by_chars(hunks, lambda hunk: len(clip_text(hunk.text)), JUDGMENT_STATE_CHAR_BUDGET)
    return [
        chunk[start : start + _MAX_HUNKS_PER_REQUEST]
        for chunk in by_size
        for start in range(0, len(chunk), _MAX_HUNKS_PER_REQUEST)
    ]


def render_pruned_diff(files: Sequence[tuple[str, Sequence[PrunableHunk]]]) -> PrunedDiff:
    """Rebuild a diff from ``(file header, hunks)`` pairs: mechanical hunks become markers, kinds become hints.

    Hunk-less sections (binary files, mode changes, pure renames) pass their whole
    text as the header with no hunks. Applies the all-or-nothing guards: when
    every hunk is mechanical, or fewer than two hunks exist, nothing is pruned
    (kind hints are still rendered).
    """
    total = sum(len(hunks) for _, hunks in files)
    mechanical = sum(1 for _, hunks in files for hunk in hunks if hunk.mechanical)
    prune = 0 < mechanical < total and total >= 2
    sections: list[str] = []
    omitted_lines = 0
    for header, hunks in files:
        if not hunks:
            sections.append(header)
            continue
        if prune and all(hunk.mechanical for hunk in hunks):
            additions = sum(hunk.additions for hunk in hunks)
            deletions = sum(hunk.deletions for hunk in hunks)
            omitted_lines += additions + deletions
            noun = "hunk" if len(hunks) == 1 else "hunks"
            sections.append(f"{header}\n{MECHANICAL_MARKER}: {len(hunks)} {noun}, +{additions}/-{deletions} lines]")
            continue
        body: list[str] = []
        for hunk in hunks:
            header_line, _, rest = hunk.text.partition("\n")
            if prune and hunk.mechanical:
                omitted_lines += hunk.additions + hunk.deletions
                body.append(f"{header_line}\n{MECHANICAL_MARKER}: +{hunk.additions}/-{hunk.deletions} lines]")
            elif hunk.kind:
                body.append(f"{header_line} {KIND_MARKER} {hunk.kind}]" + (f"\n{rest}" if rest else ""))
            else:
                body.append(hunk.text)
        sections.append(header + "\n" + "\n".join(body))
    return PrunedDiff("\n".join(sections), total, mechanical if prune else 0, omitted_lines if prune else 0)


def should_prune(diff: str, config: CommitConfig) -> bool:
    """Whether pruning ``diff`` can pay for itself: big enough, and the analysis model dear enough.

    Judging costs about twice the diff's tokens at jev's rate (question text per
    hunk), so with a flash-tier analysis model the judge costs more than the
    mechanical share it removes. Models missing from the rate table are assumed
    expensive.
    """
    if len(diff) // 4 < int(config.mechanical_prune_min_tokens):
        return False
    rates = rates_for_model(resolve_model_chain(config.analysis_model)[0])
    return rates is None or rates.input >= float(config.mechanical_prune_min_model_rate)


async def prune_mechanical_hunks(diff: str, judge: Judge | None, config: CommitConfig) -> PrunedDiff:
    """Judge every hunk of a prompt-bound diff, replace mechanical ones with markers, and hint the rest's kind.

    Returns the diff unchanged (with zero omissions) when no judge is configured,
    the diff is below :attr:`CommitConfig.mechanical_prune_min_tokens`, or a
    guard in :func:`render_pruned_diff` applies.
    """
    if judge is None or not should_prune(diff, config):
        return PrunedDiff(diff, 0, 0, 0)
    split = [(file, [] if file.is_binary else split_hunks(file.content)) for file in parse_diff(diff)]
    refs = [
        (file.filename, [HunkRef(f"{file_idx}:{hunk_idx}", hunk.text) for hunk_idx, hunk in enumerate(hunks)])
        for file_idx, (file, hunks) in enumerate(split)
        if hunks
    ]
    total = sum(len(hunks) for _, hunks in refs)
    if total < 2:
        return PrunedDiff(diff, total, 0, 0)
    judgments = HunkJudgments.from_verdicts(
        await judge_hunks(judge, refs, config), total, float(config.mechanical_hunk_threshold)
    )
    return render_pruned_diff(
        [
            (
                file.header if hunks or not file.content else f"{file.header}\n{file.content}",
                [
                    judgments.hunk(f"{file_idx}:{hunk_idx}", hunk.text, hunk.additions, hunk.deletions)
                    for hunk_idx, hunk in enumerate(hunks)
                ],
            )
            for file_idx, (file, hunks) in enumerate(split)
        ]
    )


__all__ = [
    "HUNK_KINDS",
    "KIND_CONFIDENCE_FLOOR",
    "KIND_MARKER",
    "MECHANICAL_MARKER",
    "MECHANICAL_RUBRIC",
    "HunkJudgments",
    "HunkRef",
    "HunkVerdict",
    "PrunableHunk",
    "PrunedDiff",
    "judge_hunks",
    "kind_criteria",
    "prune_mechanical_hunks",
    "render_pruned_diff",
    "should_prune",
]

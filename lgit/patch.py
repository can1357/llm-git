"""Compose snapshot parsing and isolated staging helpers."""

from __future__ import annotations

import os
import stat
from collections import defaultdict
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from enum import StrEnum
from pathlib import Path

from .errors import GitError, ValidationFailure
from .git import run_git, run_git_bytes
from .models import ComposeFile, ComposeHunk, ComposeSnapshot, WorktreePin, WorktreePinKind


@dataclass(slots=True)
class _ParsedHunk:
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    header: str
    lines: list[str]


@dataclass(slots=True)
class _ParsedFile:
    path: str
    header_lines: list[str]
    hunks: list[_ParsedHunk] = field(default_factory=list)
    additions: int = 0
    deletions: int = 0
    is_binary: bool = False
    old_path: str | None = None


class StageResult(StrEnum):
    """Outcome of staging a planned compose group."""

    STAGED = "staged"
    ALREADY_APPLIED = "already_applied"
    EMPTY_PATCH = "empty_patch"

    def combine(self, other: StageResult) -> StageResult:
        """Combine two staging outcomes, preferring materialized changes."""
        if self == StageResult.STAGED or other == StageResult.STAGED:
            return StageResult.STAGED
        if self == StageResult.ALREADY_APPLIED or other == StageResult.ALREADY_APPLIED:
            return StageResult.ALREADY_APPLIED
        return StageResult.EMPTY_PATCH


@dataclass(frozen=True, slots=True)
class SkippedFile:
    """A file whose selected patch could not apply cleanly to the index."""

    path: str
    reason: str


@dataclass(frozen=True, slots=True)
class ComposeStageOutcome:
    """Result of staging a compose group into an index."""

    result: StageResult
    skipped: tuple[SkippedFile, ...] = ()


@dataclass(frozen=True, slots=True)
class _FilePatch:
    path: str
    patch: str


@dataclass(frozen=True, slots=True)
class _IndexBlob:
    path: str
    mode: str
    oid: str | None = None
    contents: bytes | None = None


@dataclass(frozen=True, slots=True)
class ComposeGroupPatch:
    """Patch, stat, and staging actions for one executable compose group."""

    diff: str
    stat: str
    apply_patches: tuple[_FilePatch, ...] = ()
    fallback_files: tuple[str, ...] = ()
    index_blobs: tuple[_IndexBlob, ...] = ()
    removed_paths: tuple[str, ...] = ()


def build_compose_snapshot(diff: str, stat: str) -> ComposeSnapshot:
    """Parse a compose diff into stable file and hunk identifiers."""
    parsed_files: list[_ParsedFile] = []
    current_file: _ParsedFile | None = None
    current_hunk: _ParsedHunk | None = None

    def finish_hunk() -> None:
        nonlocal current_hunk
        if current_file is not None and current_hunk is not None:
            current_file.hunks.append(current_hunk)
        current_hunk = None

    def finish_file() -> None:
        nonlocal current_file
        finish_hunk()
        if current_file is not None:
            parsed_files.append(current_file)
        current_file = None

    for line in _diff_lines_preserve_cr(diff):
        if line.startswith("diff --git "):
            finish_file()
            current_file = _ParsedFile(path=_parse_file_path(line), header_lines=[line])
            continue
        if current_file is None:
            continue
        if line.startswith("@@ "):
            finish_hunk()
            old_start, old_count, new_start, new_count = _parse_hunk_header(line)
            current_hunk = _ParsedHunk(
                old_start=old_start,
                old_count=old_count,
                new_start=new_start,
                new_count=new_count,
                header=line,
                lines=[line],
            )
            continue
        if current_hunk is not None:
            if line.startswith("+"):
                current_file.additions += 1
            elif line.startswith("-"):
                current_file.deletions += 1
            current_hunk.lines.append(line)
            continue
        if line.startswith("Binary files "):
            current_file.is_binary = True
        elif line.startswith("rename from "):
            current_file.old_path = line.removeprefix("rename from ")
        current_file.header_lines.append(line)
    finish_file()

    files: list[ComposeFile] = []
    hunks: list[ComposeHunk] = []
    for file_index, parsed in enumerate(parsed_files, start=1):
        file_id = f"F{file_index:03d}"
        patch_header = _join_lines(parsed.header_lines)
        full_patch = patch_header
        hunk_ids: list[str] = []
        if not parsed.hunks:
            hunk_id = f"{file_id}-H001"
            snippet = _build_synthetic_snippet(parsed)
            hunk_ids.append(hunk_id)
            hunks.append(
                ComposeHunk(
                    hunk_id=hunk_id,
                    file_id=file_id,
                    path=parsed.path,
                    old_start=0,
                    old_count=0,
                    new_start=0,
                    new_count=0,
                    header=snippet,
                    raw_patch="",
                    snippet=snippet,
                    semantic_key=_build_semantic_key(parsed.path, parsed.header_lines, snippet),
                    synthetic=True,
                )
            )
        else:
            for hunk_index, hunk in enumerate(parsed.hunks, start=1):
                hunk_id = f"{file_id}-H{hunk_index:03d}"
                raw_patch = _join_lines(hunk.lines)
                snippet = _build_hunk_snippet(hunk.lines, hunk.header)
                hunk_ids.append(hunk_id)
                full_patch += raw_patch
                hunks.append(
                    ComposeHunk(
                        hunk_id=hunk_id,
                        file_id=file_id,
                        path=parsed.path,
                        old_start=hunk.old_start,
                        old_count=hunk.old_count,
                        new_start=hunk.new_start,
                        new_count=hunk.new_count,
                        header=hunk.header,
                        raw_patch=raw_patch,
                        snippet=snippet,
                        semantic_key=_build_semantic_key(parsed.path, hunk.lines, snippet),
                    )
                )
        hunk_word = "hunk" if len(hunk_ids) == 1 else "hunks"
        files.append(
            ComposeFile(
                file_id=file_id,
                path=parsed.path,
                patch_header=patch_header,
                full_patch=full_patch,
                summary=f"{parsed.path} (+{parsed.additions}/-{parsed.deletions}, {len(hunk_ids)} {hunk_word})",
                hunk_ids=tuple(hunk_ids),
                additions=parsed.additions,
                deletions=parsed.deletions,
                is_binary=parsed.is_binary,
                synthetic_only=not parsed.hunks,
                old_path=parsed.old_path,
            )
        )
    return ComposeSnapshot(diff=diff, stat=stat, files=tuple(files), hunks=tuple(hunks), pins={})


def pin_snapshot_worktree_state(snapshot: ComposeSnapshot, dir: str | os.PathLike[str] = ".") -> ComposeSnapshot:
    """Pin snapshot paths to object ids captured from the current worktree."""
    root = Path(dir)
    regular_paths: list[str] = []
    pins = dict(snapshot.pins)
    for file in snapshot.files:
        full_path = root / file.path
        try:
            metadata = full_path.lstat()
        # Keep this before OSError so missing paths are recorded as deletions.
        except FileNotFoundError:
            pins[file.path] = WorktreePin.deleted()
            continue
        except OSError as exc:
            raise GitError(f"Failed to inspect worktree path {full_path}: {exc}") from exc
        if stat.S_ISLNK(metadata.st_mode):
            target = os.readlink(full_path)
            oid = _hash_blob_bytes(os.fsencode(target), file.path, dir)
            pins[file.path] = WorktreePin.object(mode="120000", oid=oid)
        elif stat.S_ISDIR(metadata.st_mode):
            submodule_oid = _submodule_head(full_path)
            if submodule_oid:
                pins[file.path] = WorktreePin.object(mode="160000", oid=submodule_oid)
        elif "\n" not in file.path:
            regular_paths.append(file.path)

    for path, oid in zip(regular_paths, _hash_worktree_paths(regular_paths, dir), strict=True):
        pins[path] = WorktreePin.object(mode=_worktree_file_mode(root / path), oid=oid)
    return ComposeSnapshot(
        diff=snapshot.diff, stat=snapshot.stat, files=snapshot.files, hunks=snapshot.hunks, pins=pins
    )


def create_executable_group_patch(snapshot: ComposeSnapshot, group: object) -> ComposeGroupPatch:
    """Create the exact patch/stat and staging actions for a planned group."""
    selected_by_file = _selected_hunks_by_file(snapshot, group)
    diff_parts: list[str] = []
    stat_parts: list[str] = []
    apply_patches: list[_FilePatch] = []
    fallback_files: list[str] = []
    index_blobs: list[_IndexBlob] = []
    removed_paths: list[str] = []

    for file in snapshot.files:
        selected_for_file = selected_by_file.get(file.file_id)
        if not selected_for_file:
            continue
        ordered_hunks = _ordered_selected_hunks(file, selected_for_file)
        if file.old_path is not None:
            # A rename is atomic: stage the entire destination blob and drop the source path
            # in the same group, so a whole-file move never splits into add + delete commits.
            removed_paths.append(file.old_path)
            fallback_files.append(file.path)
            diff_parts.append(file.full_patch)
            stat_parts.append(_stat_line(file.path, file.additions, file.deletions, file.is_binary))
            continue
        if file.synthetic_only or file.is_binary:
            if not _selected_hunks_cover_file(file, selected_for_file):
                raise ValidationFailure(
                    f"group {_group_id(group)} cannot partially stage unpatchable file {file.path}",
                    field="compose",
                )
            if file.synthetic_only and not file.is_binary and _new_file_mode(file):
                index_blobs.append(_new_file_index_blob(file, ordered_hunks))
            else:
                fallback_files.append(file.path)
            diff_parts.append(file.full_patch)
            stat_parts.append(_stat_line(file.path, file.additions, file.deletions, file.is_binary))
            continue

        file_patch = _create_patch_for_file(file, ordered_hunks)
        additions, deletions = _count_hunk_changes(ordered_hunks)
        diff_parts.append(file_patch)
        if _new_file_mode(file):
            if _selected_hunks_cover_file(file, selected_for_file):
                index_blobs.append(_new_file_index_blob(file, ordered_hunks))
            else:
                apply_patches.append(_FilePatch(file.path, file_patch))
        elif _selected_hunks_cover_file(file, selected_for_file):
            fallback_files.append(file.path)
        else:
            apply_patches.append(_FilePatch(file.path, file_patch))
        stat_parts.append(_stat_line(file.path, additions, deletions, False))

    return ComposeGroupPatch(
        diff="".join(diff_parts),
        stat="".join(stat_parts),
        apply_patches=tuple(apply_patches),
        fallback_files=tuple(sorted(set(fallback_files))),
        index_blobs=tuple(index_blobs),
        removed_paths=tuple(dict.fromkeys(removed_paths)),
    )


def stage_executable_group_in_index(
    snapshot: ComposeSnapshot,
    group: object,
    dir: str | os.PathLike[str],
    index_file: str | os.PathLike[str],
) -> ComposeStageOutcome:
    """Stage a planned group into a temporary index without reading live files unless unpinned."""
    group_patch = create_executable_group_patch(snapshot, group)
    result = StageResult.EMPTY_PATCH
    skipped: list[SkippedFile] = []

    for file_patch in group_patch.apply_patches:
        outcome, reason = _apply_file_patch_to_index(file_patch.patch, dir, index_file)
        if outcome == "staged":
            result = result.combine(StageResult.STAGED)
        elif outcome == "already":
            result = result.combine(StageResult.ALREADY_APPLIED)
        elif outcome == "empty":
            result = result.combine(StageResult.EMPTY_PATCH)
        else:
            _restore_index_path_to_head(file_patch.path, dir, index_file)
            skipped.append(SkippedFile(file_patch.path, reason or "git apply failed"))

    for path in group_patch.fallback_files:
        pin = snapshot.pins.get(path)
        if pin is None:
            run_git(["add", "--", path], cwd=dir, index_file=index_file)
            result = result.combine(StageResult.STAGED)
        elif pin.kind == WorktreePinKind.DELETED:
            result = result.combine(_remove_index_path(path, dir, index_file))
        else:
            result = result.combine(
                _stage_index_blob(_IndexBlob(path=path, mode=pin.mode or "100644", oid=pin.oid), dir, index_file)
            )

    for blob in group_patch.index_blobs:
        result = result.combine(_stage_index_blob(blob, dir, index_file))

    for path in group_patch.removed_paths:
        result = result.combine(_remove_index_path(path, dir, index_file))

    return ComposeStageOutcome(result=result, skipped=tuple(skipped))


def force_stage_file_from_base_in_index(
    snapshot: ComposeSnapshot,
    file_id: str,
    selected_hunk_ids: Sequence[str],
    dir: str | os.PathLike[str],
    index_file: str | os.PathLike[str],
) -> None:
    """Rewrite one index entry as base blob plus selected hunks from the snapshot."""
    file = snapshot.file_by_id(file_id)
    if file is None:
        raise ValidationFailure(f"unknown compose file id {file_id}", field="compose")
    ordered = [
        hunk
        for hunk_id in file.hunk_ids
        if hunk_id in set(selected_hunk_ids)
        for hunk in [snapshot.hunk_by_id(hunk_id)]
        if hunk is not None and hunk.raw_patch
    ]
    if not ordered:
        return
    _restore_index_path_to_head(file.path, dir, index_file)
    base_bytes, mode = _resolve_base_blob(file, dir)
    target = _splice_hunks_into_base(base_bytes, ordered)
    _stage_index_blob(_IndexBlob(path=file.path, mode=mode, contents=target), dir, index_file)


def _parse_hunk_header(header: str) -> tuple[int, int, int, int]:
    trimmed = header.strip()
    if not trimmed.startswith("@@"):
        raise ValidationFailure(f"failed to parse hunk header {header!r}", field="diff")
    middle = trimmed.removeprefix("@@").split("@@", 1)[0].strip().split()
    if len(middle) < 2:
        raise ValidationFailure(f"failed to parse hunk header {header!r}", field="diff")

    def parse_range(raw: str, prefix: str) -> tuple[int, int]:
        if not raw.startswith(prefix):
            raise ValueError(f"hunk range {raw!r} does not start with {prefix!r}")
        body = raw.removeprefix(prefix)
        if "," in body:
            start, count = body.split(",", 1)
            return int(start), int(count)
        return int(body), 1

    try:
        old_start, old_count = parse_range(middle[0], "-")
        new_start, new_count = parse_range(middle[1], "+")
    except ValueError as exc:
        raise ValidationFailure(f"failed to parse hunk header {header!r}", field="diff") from exc
    return old_start, old_count, new_start, new_count


def _parse_file_path(diff_header: str) -> str:
    parts = diff_header.split()
    if len(parts) >= 4 and parts[3].startswith("b/"):
        return parts[3][2:]
    raise ValidationFailure(f"failed to parse file path from {diff_header!r}", field="diff")


def _diff_lines_preserve_cr(input: str) -> Iterable[str]:
    for line in input.splitlines(keepends=True):
        yield line[:-1] if line.endswith("\n") else line


def _join_lines(lines: Sequence[str]) -> str:
    return "" if not lines else "\n".join(lines) + "\n"


def _truncate_snippet(snippet: str, max_chars: int) -> str:
    trimmed = snippet.strip()
    return trimmed if len(trimmed) <= max_chars else trimmed[:max_chars] + "..."


def _build_hunk_snippet(lines: Sequence[str], fallback: str) -> str:
    interesting = [
        _truncate_snippet(line.lstrip("+-"), 80) for line in lines[1:] if line.startswith("+") or line.startswith("-")
    ][:3]
    return " | ".join(interesting) if interesting else _truncate_snippet(fallback, 80)


def _build_synthetic_snippet(file: _ParsedFile) -> str:
    for line in file.header_lines[1:]:
        if not line.startswith(("index ", "--- ", "+++ ")) and line.strip():
            return _truncate_snippet(line, 80)
    return _truncate_snippet(f"whole-file change in {file.path}", 80)


def _fnv1a_64(input: str) -> str:
    value = 0xCBF29CE484222325
    for byte in input.encode():
        value ^= byte
        value = (value * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{value:016x}"


def _build_semantic_key(path: str, lines: Sequence[str], fallback: str) -> str:
    changed = [
        line
        for line in lines
        if (line.startswith("+") and not line.startswith("+++"))
        or (line.startswith("-") and not line.startswith("---"))
    ]
    return f"{path}:{_fnv1a_64(chr(10).join(changed) if changed else fallback)}"


def _group_id(group: object) -> str:
    return str(getattr(group, "group_id", getattr(group, "id", "compose-group")))


def _group_hunk_ids(snapshot: ComposeSnapshot, group: object) -> tuple[str, ...]:
    hunk_ids = getattr(group, "hunk_ids", None)
    if hunk_ids:
        return tuple(str(hunk_id) for hunk_id in hunk_ids)
    changes = getattr(group, "changes", None)
    if changes:
        selected: list[str] = []
        for change in changes:
            file = snapshot.file_by_path(str(change.path))
            if file is None:
                continue
            selectors = tuple(getattr(change, "hunks", ()))
            if not selectors or any(getattr(selector, "kind", "").upper() == "ALL" for selector in selectors):
                selected.extend(file.hunk_ids)
                continue
            for hunk in snapshot.hunks_for_file(file.file_id):
                if _hunk_selected(hunk, selectors):
                    selected.append(hunk.hunk_id)
        return tuple(dict.fromkeys(selected))
    file_ids = getattr(group, "file_ids", None)
    if file_ids:
        selected = []
        wanted = {str(file_id) for file_id in file_ids}
        for file in snapshot.files:
            if file.file_id in wanted:
                selected.extend(file.hunk_ids)
        return tuple(selected)
    return ()


def _hunk_selected(hunk: ComposeHunk, selectors: Sequence[object]) -> bool:
    for selector in selectors:
        kind = str(getattr(selector, "kind", "")).upper()
        if kind == "ALL":
            return True
        if kind == "LINES":
            start = getattr(selector, "start", None)
            end = getattr(selector, "end", None)
            if (
                start is not None
                and end is not None
                and hunk.new_start <= int(end)
                and hunk.new_start + hunk.new_count >= int(start)
            ):
                return True
        if kind == "SEARCH":
            pattern = getattr(selector, "pattern", None)
            if pattern and str(pattern) in hunk.raw_patch:
                return True
    return False


def _selected_hunks_by_file(snapshot: ComposeSnapshot, group: object) -> dict[str, list[ComposeHunk]]:
    hunk_ids = _group_hunk_ids(snapshot, group)
    if not hunk_ids:
        raise ValidationFailure(f"group {_group_id(group)} has no assigned hunks", field="compose")
    selected: dict[str, list[ComposeHunk]] = defaultdict(list)
    for hunk_id in hunk_ids:
        hunk = snapshot.hunk_by_id(hunk_id)
        if hunk is None:
            raise ValidationFailure(f"group {_group_id(group)} references unknown hunk id {hunk_id}", field="compose")
        selected[hunk.file_id].append(hunk)
    return dict(selected)


def _ordered_selected_hunks(file: ComposeFile, selected_for_file: Sequence[ComposeHunk]) -> list[ComposeHunk]:
    by_id = {hunk.hunk_id: hunk for hunk in selected_for_file}
    ordered = [by_id[hunk_id] for hunk_id in file.hunk_ids if hunk_id in by_id]
    if not ordered:
        raise ValidationFailure(f"selected no patchable hunks for {file.path}", field="compose")
    return ordered


def _selected_hunks_cover_file(file: ComposeFile, selected_for_file: Sequence[ComposeHunk]) -> bool:
    return {hunk.hunk_id for hunk in selected_for_file} == set(file.hunk_ids)


def _create_patch_for_file(file: ComposeFile, hunks: Sequence[ComposeHunk]) -> str:
    return file.patch_header + "".join(hunk.raw_patch for hunk in hunks)


def _count_hunk_changes(hunks: Sequence[ComposeHunk]) -> tuple[int, int]:
    additions = deletions = 0
    for hunk in hunks:
        for line in hunk.raw_patch.splitlines():
            if line.startswith("+"):
                additions += 1
            elif line.startswith("-"):
                deletions += 1
    return additions, deletions


def _stat_line(path: str, additions: int, deletions: int, is_binary: bool) -> str:
    if is_binary and additions == 0 and deletions == 0:
        return f" {path} | Bin\n"
    return f" {path} | {additions + deletions} {'+' * min(additions, 50)}{'-' * min(deletions, 50)}\n"


def _new_file_mode(file: ComposeFile) -> str | None:
    for line in file.patch_header.splitlines():
        if line.startswith("new file mode "):
            return line.removeprefix("new file mode ").strip()
    return None


def _validate_new_file_mode(file: ComposeFile) -> str:
    mode = _new_file_mode(file) or "100644"
    if mode not in {"100644", "100755", "120000", "160000"}:
        raise ValidationFailure(f"invalid new file mode {mode!r} for {file.path}", field="diff")
    return mode


def _materialize_new_file_contents(hunks: Sequence[ComposeHunk]) -> bytes:
    out = bytearray()
    last_had_newline = False
    for hunk in hunks:
        for line in _diff_lines_preserve_cr(hunk.raw_patch):
            if line.startswith("@@"):
                last_had_newline = False
            elif line == r"\ No newline at end of file":
                if last_had_newline and out.endswith(b"\n"):
                    out.pop()
                    last_had_newline = False
            elif line.startswith("+") or line.startswith(" "):
                out.extend(line[1:].encode())
                out.extend(b"\n")
                last_had_newline = True
            else:
                last_had_newline = False
    return bytes(out)


def _new_file_index_blob(file: ComposeFile, hunks: Sequence[ComposeHunk]) -> _IndexBlob:
    mode = _validate_new_file_mode(file)
    if mode == "160000":
        oid = _materialize_gitlink_oid(file, hunks)
        return _IndexBlob(path=file.path, mode=mode, oid=oid)
    return _IndexBlob(path=file.path, mode=mode, contents=_materialize_new_file_contents(hunks))


def _materialize_gitlink_oid(file: ComposeFile, hunks: Sequence[ComposeHunk]) -> str:
    for line in _materialize_new_file_contents(hunks).decode(errors="replace").splitlines():
        if line.startswith("Subproject commit "):
            return _validate_git_object_id(line.removeprefix("Subproject commit ").split()[0], file)
    for line in file.patch_header.splitlines():
        if line.startswith("index ") and ".." in line:
            return _validate_git_object_id(line.split()[1].split("..", 1)[1], file)
    raise ValidationFailure(f"missing gitlink object id for {file.path}", field="diff")


def _validate_git_object_id(oid: str, file: ComposeFile) -> str:
    value = oid.strip()
    if value and all(ch in "0123456789abcdefABCDEF" for ch in value) and any(ch != "0" for ch in value):
        return value
    raise ValidationFailure(f"invalid gitlink object id {oid!r} for {file.path}", field="diff")


def _apply_file_patch_to_index(
    patch: str,
    dir: str | os.PathLike[str],
    index_file: str | os.PathLike[str] | None,
) -> tuple[str, str | None]:
    if not patch.strip():
        return "empty", None
    reverse = run_git(
        ["apply", "--cached", "--reverse", "--check", "--recount"],
        cwd=dir,
        input_text=patch,
        check=False,
        index_file=index_file,
    )
    if reverse.returncode == 0:
        return "already", None
    applied = run_git(
        ["apply", "--cached", "--3way", "--recount"],
        cwd=dir,
        input_text=patch,
        check=False,
        index_file=index_file,
    )
    if applied.returncode == 0:
        return "staged", None
    return "failed", applied.stderr.strip()


def _restore_index_path_to_head(
    path: str, dir: str | os.PathLike[str], index_file: str | os.PathLike[str] | None
) -> None:
    run_git(["reset", "-q", "HEAD", "--", path], cwd=dir, index_file=index_file)


def _remove_index_path(
    path: str, dir: str | os.PathLike[str], index_file: str | os.PathLike[str] | None
) -> StageResult:
    listed = run_git(["ls-files", "--", path], cwd=dir, index_file=index_file)
    if not listed.stdout:
        return StageResult.ALREADY_APPLIED
    run_git(["update-index", "--force-remove", "--", path], cwd=dir, index_file=index_file)
    return StageResult.STAGED


def _stage_index_blob(
    blob: _IndexBlob, dir: str | os.PathLike[str], index_file: str | os.PathLike[str] | None
) -> StageResult:
    oid = blob.oid if blob.oid is not None else _hash_blob_bytes(blob.contents or b"", blob.path, dir)
    current = run_git(["ls-files", "-s", "--", blob.path], cwd=dir, index_file=index_file)
    parts = current.stdout.split()
    if len(parts) >= 2 and parts[0] == blob.mode and parts[1] == oid:
        return StageResult.ALREADY_APPLIED
    run_git(["update-index", "--add", "--cacheinfo", f"{blob.mode},{oid},{blob.path}"], cwd=dir, index_file=index_file)
    return StageResult.STAGED


def _hash_blob_bytes(contents: bytes, path: str, dir: str | os.PathLike[str]) -> str:
    result = run_git_bytes(["hash-object", "-w", "--stdin"], cwd=dir, input_bytes=contents)
    oid = result.stdout.decode("utf-8", errors="strict").strip()
    if not oid:
        raise GitError(f"git hash-object returned empty oid for {path}")
    return oid


def _hash_worktree_paths(paths: Sequence[str], dir: str | os.PathLike[str]) -> list[str]:
    if not paths:
        return []
    result = run_git(["hash-object", "-w", "--stdin-paths"], cwd=dir, input_text="\n".join(paths) + "\n")
    oids = result.stdout.splitlines()
    if len(oids) != len(paths):
        raise GitError(f"git hash-object returned {len(oids)} oids for {len(paths)} paths")
    return oids


def _worktree_file_mode(path: Path) -> str:
    try:
        mode = path.stat().st_mode
    except OSError:
        return "100644"
    return "100755" if mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH) else "100644"


def _submodule_head(path: Path) -> str | None:
    result = run_git(["rev-parse", "HEAD"], cwd=path, check=False)
    oid = result.stdout.strip()
    return oid if result.returncode == 0 and oid else None


def _resolve_base_blob(file: ComposeFile, dir: str | os.PathLike[str]) -> tuple[bytes, str]:
    index_line = next((line for line in file.patch_header.splitlines() if line.startswith("index ")), None)
    base_oid = None
    if index_line:
        range_part = index_line.removeprefix("index ").split()[0]
        if ".." in range_part:
            base_oid = range_part.split("..", 1)[0]
    if base_oid and any(ch != "0" for ch in base_oid):
        full = run_git(["rev-parse", "--verify", "--quiet", f"{base_oid}^{{blob}}"], cwd=dir).stdout.strip()
        data = run_git_bytes(["cat-file", "blob", full], cwd=dir).stdout
        mode = "100644"
        if index_line and len(index_line.split()) > 2:
            mode = index_line.split()[2]
        else:
            old_mode = next(
                (
                    line.removeprefix("old mode ").strip()
                    for line in file.patch_header.splitlines()
                    if line.startswith("old mode ")
                ),
                None,
            )
            if old_mode:
                mode = old_mode
        return data, mode
    return b"", _new_file_mode(file) or "100644"


def _split_lines_keep_eol(data: bytes) -> list[bytes]:
    if not data:
        return []
    return data.splitlines(keepends=True)


def _dominant_eol(lines: Sequence[bytes]) -> bytes:
    crlf = sum(1 for line in lines if line.endswith(b"\r\n"))
    lf = sum(1 for line in lines if line.endswith(b"\n") and not line.endswith(b"\r\n"))
    return b"\r\n" if crlf > 0 and crlf >= lf else b"\n"


def _strip_trailing_eol(buf: bytearray) -> None:
    if buf.endswith(b"\n"):
        del buf[-1:]
        if buf.endswith(b"\r"):
            del buf[-1:]


def _splice_hunks_into_base(base: bytes, hunks: Sequence[ComposeHunk]) -> bytes:
    base_lines = _split_lines_keep_eol(base)
    eol = _dominant_eol(base_lines)
    out = bytearray()
    cursor = 0
    for hunk in sorted(hunks, key=lambda item: item.old_start):
        start = max(hunk.old_start - 1, 0)
        while cursor < start and cursor < len(base_lines):
            out.extend(base_lines[cursor])
            cursor += 1
        prev = b""
        for index, line in enumerate(_diff_lines_preserve_cr(hunk.raw_patch)):
            if index == 0:
                continue
            raw = line.encode()
            if raw.startswith(b"\\"):
                if prev in {b"+", b" "}:
                    _strip_trailing_eol(out)
                continue
            if raw.startswith(b"-"):
                cursor += 1
                prev = b"-"
            elif raw.startswith(b"+"):
                content = raw[1:]
                if content.endswith(b"\r"):
                    content = content[:-1]
                out.extend(content)
                out.extend(eol)
                prev = b"+"
            else:
                if cursor < len(base_lines):
                    out.extend(base_lines[cursor])
                    cursor += 1
                prev = b" "
    while cursor < len(base_lines):
        out.extend(base_lines[cursor])
        cursor += 1
    return bytes(out)


__all__ = [
    "ComposeGroupPatch",
    "ComposeStageOutcome",
    "SkippedFile",
    "StageResult",
    "build_compose_snapshot",
    "create_executable_group_patch",
    "force_stage_file_from_base_in_index",
    "pin_snapshot_worktree_state",
    "stage_executable_group_in_index",
]

"""Git subprocess plumbing and snapshot/index helpers."""

from __future__ import annotations

import os
import stat as stat_module
import subprocess
import time
from collections import Counter
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

from .errors import GitError, GitIndexLocked, NoChanges, ValidationFailure
from .models import CommitMetadata

_GIT_BACKGROUND_CONFIG = (
    ("core.fsmonitor", "false"),
    ("core.untrackedCache", "false"),
)

_DISABLE_GIT_BACKGROUND_FEATURES = True

# A concurrent git process (editor integration, fsmonitor, a parallel `git` call) often
# holds `index.lock` for a fraction of a second. Retrying briefly clears that transient
# contention without ever deleting a lock a live process may still own.
_INDEX_LOCK_RETRY_ATTEMPTS = 5
_INDEX_LOCK_RETRY_DELAY_S = 0.25


def init_git_command_settings(config: object) -> None:
    """Initialize process-wide git subprocess settings from config."""

    global _DISABLE_GIT_BACKGROUND_FEATURES
    _DISABLE_GIT_BACKGROUND_FEATURES = bool(getattr(config, "disable_git_background_features", True))


@dataclass(slots=True)
class GitResult:
    """Captured git subprocess result."""

    args: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str


@dataclass(slots=True)
class GitBytesResult:
    """Captured git subprocess result with raw output bytes."""

    args: tuple[str, ...]
    returncode: int
    stdout_bytes: bytes
    stderr_bytes: bytes

    @property
    def stdout(self) -> bytes:
        return self.stdout_bytes

    @property
    def stderr(self) -> bytes:
        return self.stderr_bytes


@dataclass(slots=True)
class StylePatterns:
    """Quantified commit-message style patterns from history."""

    scope_usage_pct: float
    common_verbs: list[tuple[str, int]]
    avg_length: int
    length_range: tuple[int, int]
    lowercase_pct: float
    top_scopes: list[tuple[str, int]]

    def format_for_prompt(self) -> str:
        """Format style patterns for prompt injection."""

        lines = [f"Scope usage: {self.scope_usage_pct:.0f}% of commits use scopes"]
        if self.common_verbs:
            verbs = ", ".join(f"{verb} ({count})" for verb, count in self.common_verbs[:5])
            lines.append(f"Common verbs: {verbs}")
        lines.append(f"Average length: {self.avg_length} chars (range: {self.length_range[0]}-{self.length_range[1]})")
        lines.append(f"Capitalization: {self.lowercase_pct:.0f}% start lowercase")
        if self.top_scopes:
            scopes = ", ".join(f"{scope} ({count})" for scope, count in self.top_scopes[:5])
            lines.append(f"Top scopes: {scopes}")
        return "\n".join(lines)


def git_command_env(
    extra: Mapping[str, str | os.PathLike[str]] | None = None,
    *,
    index_file: str | os.PathLike[str] | None = None,
    disable_background_features: bool | None = None,
) -> dict[str, str]:
    """Return an environment for git with optional temp index and background disables."""

    env = os.environ.copy()
    if extra:
        env.update({key: os.fspath(value) for key, value in extra.items()})
    if index_file is not None:
        env["GIT_INDEX_FILE"] = os.fspath(index_file)
    if disable_background_features is None:
        disable_background_features = _DISABLE_GIT_BACKGROUND_FEATURES
    if disable_background_features:
        # Read-only ops (status/diff/ls-files/...) opportunistically take `index.lock` to
        # write back refreshed stat info. Suppress that so lgit never contends with the
        # user's concurrent git while it sits in long LLM calls; mandatory locks for
        # add/commit are unaffected.
        env["GIT_OPTIONAL_LOCKS"] = "0"
        try:
            offset = int(env.get("GIT_CONFIG_COUNT", "0"))
        except ValueError:
            offset = 0
        for idx, (key, value) in enumerate(_GIT_BACKGROUND_CONFIG, start=offset):
            env[f"GIT_CONFIG_KEY_{idx}"] = key
            env[f"GIT_CONFIG_VALUE_{idx}"] = value
        env["GIT_CONFIG_COUNT"] = str(offset + len(_GIT_BACKGROUND_CONFIG))
    return env


def _git_argv(args: Sequence[str | os.PathLike[str]]) -> tuple[str, ...]:
    return ("git", *(os.fspath(arg) for arg in args))


def _run_git_process(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: str | os.PathLike[str],
    input_data: str | bytes | None,
    text: bool,
    env: Mapping[str, str | os.PathLike[str]] | None,
    index_file: str | os.PathLike[str] | None,
    disable_background_features: bool | None,
) -> tuple[tuple[str, ...], subprocess.CompletedProcess]:
    argv = _git_argv(args)
    # Always feed stdin as raw bytes. A text-mode pipe rewrites "\n" -> os.linesep
    # on Windows, which corrupts patches piped to `git apply` (CRLF context no
    # longer matches the LF index blob) and appends stray CR to commit messages
    # and stdin-paths. Encoding here keeps the bytes byte-exact on every platform.
    stdin_bytes = input_data.encode("utf-8") if isinstance(input_data, str) else input_data
    completed = subprocess.run(
        argv,
        cwd=os.fspath(cwd),
        input=stdin_bytes,
        capture_output=True,
        env=git_command_env(
            env,
            index_file=index_file,
            disable_background_features=disable_background_features,
        ),
        shell=False,
        check=False,
    )
    if not text:
        return argv, completed
    stdout = completed.stdout.decode("utf-8", errors="replace").replace("\r\n", "\n")
    stderr = completed.stderr.decode("utf-8", errors="replace").replace("\r\n", "\n")
    return argv, subprocess.CompletedProcess(completed.args, completed.returncode, stdout, stderr)


def _raise_git_error(
    args: Sequence[str | os.PathLike[str]],
    cwd: str | os.PathLike[str],
    stdout: str,
    stderr: str,
) -> None:
    locked = _index_lock_error(stderr, cwd)
    if locked is not None:
        raise locked
    detail = f"{stderr.strip()}\n{stdout.strip()}".strip()
    raise GitError(f"git {' '.join(os.fspath(arg) for arg in args)} failed: {detail}")


def _retry_index_lock(stderr: str, attempt: int) -> bool:
    """Sleep and report whether a locked-index failure should be retried.

    Returns ``True`` after a short backoff when ``stderr`` reports an `index.lock`
    contention and retries remain; ``False`` otherwise (not a lock error, or the
    bounded attempts are exhausted). Never removes the lock — a live git process may
    still own it.
    """
    if "index.lock" not in stderr or attempt + 1 >= _INDEX_LOCK_RETRY_ATTEMPTS:
        return False
    time.sleep(_INDEX_LOCK_RETRY_DELAY_S)
    return True


def run_git(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: str | os.PathLike[str] = ".",
    input_text: str | None = None,
    check: bool = True,
    allow_exit_codes: Iterable[int] = (),
    env: Mapping[str, str | os.PathLike[str]] | None = None,
    index_file: str | os.PathLike[str] | None = None,
    disable_background_features: bool | None = None,
) -> GitResult:
    """Run git with explicit argv and return captured UTF-8 text output."""

    allowed = set(allow_exit_codes)
    for attempt in range(_INDEX_LOCK_RETRY_ATTEMPTS):
        argv, completed = _run_git_process(
            args,
            cwd=cwd,
            input_data=input_text,
            text=True,
            env=env,
            index_file=index_file,
            disable_background_features=disable_background_features,
        )
        if check and completed.returncode != 0 and completed.returncode not in allowed:
            if _retry_index_lock(completed.stderr, attempt):
                continue
            _raise_git_error(args, cwd, completed.stdout, completed.stderr)
        return GitResult(argv, completed.returncode, completed.stdout, completed.stderr)
    raise AssertionError("unreachable: run_git retry loop exited without returning")


def run_git_bytes(
    args: Sequence[str | os.PathLike[str]],
    *,
    cwd: str | os.PathLike[str] = ".",
    input_bytes: bytes | None = None,
    check: bool = True,
    allow_exit_codes: Iterable[int] = (),
    env: Mapping[str, str | os.PathLike[str]] | None = None,
    index_file: str | os.PathLike[str] | None = None,
    disable_background_features: bool | None = None,
) -> GitBytesResult:
    """Run git and preserve stdout as raw bytes."""

    allowed = set(allow_exit_codes)
    for attempt in range(_INDEX_LOCK_RETRY_ATTEMPTS):
        argv, completed = _run_git_process(
            args,
            cwd=cwd,
            input_data=input_bytes,
            text=False,
            env=env,
            index_file=index_file,
            disable_background_features=disable_background_features,
        )
        if check and completed.returncode != 0 and completed.returncode not in allowed:
            stderr = completed.stderr.decode("utf-8", errors="replace")
            if _retry_index_lock(stderr, attempt):
                continue
            stdout = completed.stdout.decode("utf-8", errors="replace")
            _raise_git_error(args, cwd, stdout, stderr)
        return GitBytesResult(argv, completed.returncode, completed.stdout, completed.stderr)
    raise AssertionError("unreachable: run_git_bytes retry loop exited without returning")


class TempGitIndex:
    """Temporary Git index under `.git/llm-git`, removed on context exit."""

    def __init__(self, dir: str | os.PathLike[str] = ".") -> None:
        temp_dir = get_git_dir(dir) / "llm-git"
        temp_dir.mkdir(parents=True, exist_ok=True)
        pid = os.getpid()
        nanos = time.time_ns()
        for attempt in range(100):
            path = temp_dir / f"index-{pid}-{nanos}-{attempt}"
            try:
                fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            except FileExistsError:
                continue
            except OSError as exc:
                raise GitError(f"Failed to create temporary git index: {exc}") from exc
            else:
                os.close(fd)
                path.unlink(missing_ok=True)
                self.path = path
                return
        raise GitError("Failed to allocate unique temporary git index path")

    def __fspath__(self) -> str:
        return os.fspath(self.path)

    def __enter__(self) -> TempGitIndex:
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        self.cleanup()

    def cleanup(self) -> None:
        """Remove the temp index and a sibling lock if either exists."""

        self.path.unlink(missing_ok=True)
        self.path.with_suffix(self.path.suffix + ".lock").unlink(missing_ok=True)


def ensure_git_repo(dir: str | os.PathLike[str] = ".") -> None:
    """Raise unless `dir` is inside a git work tree."""

    result = run_git(["rev-parse", "--show-toplevel"], cwd=dir, check=False)
    if result.returncode == 0:
        return
    if "not a git repository" in result.stderr:
        raise GitError("Not a git repository (or any of the parent directories): .git")
    raise GitError(f"Failed to detect git repository: {result.stderr.strip()}")


def get_git_dir(dir: str | os.PathLike[str] = ".") -> Path:
    """Return the absolute git directory for `dir`."""

    result = run_git(["rev-parse", "--absolute-git-dir"], cwd=dir)
    return Path(result.stdout.strip())


def get_git_diff(
    mode: object,
    target: str | None = None,
    dir: str | os.PathLike[str] = ".",
    config: object | None = None,
) -> str:
    """Return a diff for staged, unstaged, or commit mode."""

    mode_name = _mode_name(mode)
    max_len = int(getattr(config, "max_diff_length", 200_000))
    exclude_old_message = bool(getattr(config, "exclude_old_message", False))

    if mode_name == "staged":
        diff = _diff_with_retry(["diff", "--cached"], dir, max_len)
    elif mode_name == "commit":
        if target is None:
            raise ValidationFailure("--target required for commit mode")
        args = ["show"]
        if exclude_old_message:
            args.append("--format=")
        args.append(target)
        diff = _diff_with_retry(args, dir, max_len, insert_u1_before=target)
    elif mode_name == "unstaged":
        diff = _diff_with_retry(["diff"], dir, max_len)
        diff = _append_untracked_diff(diff, dir, _list_untracked_files(dir))
    elif mode_name == "compose":
        raise GitError("compose mode diff is handled by get_compose_diff")
    else:
        raise ValidationFailure(f"unknown mode: {mode!r}")

    if not diff.strip():
        raise NoChanges(mode_name)
    return diff


def get_git_stat(
    mode: object,
    target: str | None = None,
    dir: str | os.PathLike[str] = ".",
    config: object | None = None,
) -> str:
    """Return git diff --stat or git show --stat output for a mode."""

    mode_name = _mode_name(mode)
    exclude_old_message = bool(getattr(config, "exclude_old_message", False))
    if mode_name == "staged":
        return run_git(["diff", "--cached", "--stat"], cwd=dir).stdout
    if mode_name == "commit":
        if target is None:
            raise ValidationFailure("--target required for commit mode")
        args = ["show"]
        if exclude_old_message:
            args.append("--format=")
        args.extend(["--stat", target])
        return run_git(args, cwd=dir).stdout
    if mode_name == "unstaged":
        stat = run_git(["diff", "--stat"], cwd=dir).stdout
        return _append_untracked_stat(stat, dir, _list_untracked_files(dir))
    if mode_name == "compose":
        raise GitError("compose mode stat is handled by get_compose_stat")
    raise ValidationFailure(f"unknown mode: {mode!r}")


def get_git_numstat(
    mode: object,
    target: str | None = None,
    dir: str | os.PathLike[str] = ".",
    config: object | None = None,
) -> str:
    """Return git diff --numstat or git show --numstat output for a mode."""

    mode_name = _mode_name(mode)
    exclude_old_message = bool(getattr(config, "exclude_old_message", False))
    if mode_name == "staged":
        return run_git(["diff", "--cached", "--numstat"], cwd=dir).stdout
    if mode_name == "commit":
        if target is None:
            raise ValidationFailure("--target required for commit mode")
        args = ["show"]
        if exclude_old_message:
            args.append("--format=")
        args.extend(["--numstat", target])
        return run_git(args, cwd=dir).stdout
    if mode_name == "unstaged":
        numstat = run_git(["diff", "--numstat"], cwd=dir).stdout
        return _append_untracked_numstat(numstat, dir, _list_untracked_files(dir))
    if mode_name == "compose":
        raise GitError("compose mode does not produce numstat")
    raise ValidationFailure(f"unknown mode: {mode!r}")


def get_compose_diff(
    dir: str | os.PathLike[str] = ".",
    config: object | None = None,
    target_tree: str | None = None,
    exclude: Sequence[str] = (),
) -> str:
    """Return the compose-mode diff with rename detection.

    With ``target_tree`` (the staged tree captured at invocation), diff ``HEAD`` against that
    fixed tree, so each loop round sees only the changes still needed to reach it — never
    anything the user staged mid-run. Without it, diff the live index (``--cached``); this is
    what a normal commit would commit, since callers auto-stage first. ``exclude`` contains
    pathspecs omitted from compose planning and convergence.
    """

    max_len = int(getattr(config, "max_diff_length", 200_000))
    args = [
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--find-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]
    scope = ["--cached"] if target_tree is None else ["HEAD", target_tree]
    pathspecs = ["--", *exclude] if exclude else []
    retry_anchor = "HEAD" if target_tree is not None else "--" if exclude else None
    diff = _diff_with_retry(
        [*args, *scope, *pathspecs],
        dir,
        max_len,
        insert_u1_before=retry_anchor,
    )
    if not diff.strip():
        raise NoChanges("compose")
    return diff


def get_compose_stat(
    dir: str | os.PathLike[str] = ".",
    target_tree: str | None = None,
    exclude: Sequence[str] = (),
) -> str:
    """Return the compose-mode --stat output with rename detection (see :func:`get_compose_diff`)."""

    args = ["diff", "--no-ext-diff", "--no-textconv", "--no-color", "--find-renames"]
    scope = ["--cached"] if target_tree is None else ["HEAD", target_tree]
    pathspecs = ["--", *exclude] if exclude else []
    stat = run_git([*args, *scope, "--stat", *pathspecs], cwd=dir).stdout
    if not stat.strip():
        raise NoChanges("compose")
    return stat


def write_real_index_tree(dir: str | os.PathLike[str] = ".") -> str:
    """Write the live index to a tree and return its oid."""

    return run_git(["write-tree"], cwd=dir).stdout.strip()


def index_matches_tree(tree: str, dir: str | os.PathLike[str] = ".") -> bool:
    """Return true when the live index currently writes to `tree`."""

    return write_real_index_tree(dir) == tree


def read_tree_into_index(index_file: str | os.PathLike[str], treeish: str, dir: str | os.PathLike[str] = ".") -> None:
    """Populate a temporary index with `treeish`."""

    run_git(["read-tree", treeish], cwd=dir, index_file=index_file)


def write_index_tree(index_file: str | os.PathLike[str], dir: str | os.PathLike[str] = ".") -> str:
    """Write a temporary index to a tree and return its oid."""

    return run_git(["write-tree"], cwd=dir, index_file=index_file).stdout.strip()


def get_head_hash(dir: str | os.PathLike[str] = ".") -> str:
    """Return HEAD's commit oid."""

    return run_git(["rev-parse", "HEAD"], cwd=dir).stdout.strip()


def current_head_ref(dir: str | os.PathLike[str] = ".") -> str:
    """Return the symbolic HEAD ref, or HEAD for detached/unborn state."""

    result = run_git(["symbolic-ref", "-q", "HEAD"], cwd=dir, check=False)
    refname = result.stdout.strip()
    return refname if result.returncode == 0 and refname else "HEAD"


def commit_snapshot_tree(
    message: str,
    tree: str,
    dir: str | os.PathLike[str] = ".",
    *,
    sign: bool = False,
    signoff: bool = False,
    amend: bool = False,
) -> str | None:
    """Commit a captured tree without touching the live index or worktree."""

    final_message = append_signoff_trailer(message, dir) if signoff else message
    try:
        head = get_head_hash(dir)
    except GitError:
        head = None
    head_ref = current_head_ref(dir)

    parents: list[str] = []
    if head is not None:
        if amend:
            parents = _rev_parse_parents(head, dir)
        else:
            if _rev_parse_tree_of(head, dir) == tree:
                return None
            parents.append(head)

    new_hash = commit_tree(tree, parents, final_message, dir, sign=sign)
    update_ref_checked(head_ref, new_hash, head or "", dir)
    return new_hash


def commit_tree(
    tree: str,
    parents: Sequence[str] = (),
    message: str = "",
    dir: str | os.PathLike[str] = ".",
    *,
    sign: bool = False,
    env: Mapping[str, str | os.PathLike[str]] | None = None,
) -> str:
    """Create a commit object for `tree` and return its oid."""

    args = ["commit-tree"]
    if sign:
        args.append("-S")
    args.append(tree)
    for parent in parents:
        args.extend(["-p", parent])
    args.extend(["-F", "-"])
    result = run_git(args, cwd=dir, input_text=message, env=env)
    commit_hash = result.stdout.strip()
    if not commit_hash:
        raise GitError("git commit-tree returned an empty hash")
    return commit_hash


def update_ref_checked(refname: str, new: str, old: str, dir: str | os.PathLike[str] = ".") -> None:
    """Atomically update a ref, verifying the old value Git sees."""

    run_git(["update-ref", refname, new, old], cwd=dir)


def append_signoff_trailer(message: str, dir: str | os.PathLike[str] = ".") -> str:
    """Append a Signed-off-by trailer from Git's committer identity."""

    ident = run_git(["var", "GIT_COMMITTER_IDENT"], cwd=dir).stdout
    end = ident.find(">")
    if end == -1:
        raise GitError(f"Could not parse committer identity: {ident.strip()}")
    signer = ident[: end + 1].strip()
    return f"{message.rstrip()}\n\nSigned-off-by: {signer}"


def get_commit_list(start_ref: str | None = None, dir: str | os.PathLike[str] = ".") -> list[str]:
    """Return commit hashes to rewrite in chronological order."""

    target = f"{start_ref}..HEAD" if start_ref else "HEAD"
    stdout = run_git(["rev-list", "--reverse", target], cwd=dir).stdout
    return [line for line in stdout.splitlines() if line]


def get_commit_metadata(commit_hash: str, dir: str | os.PathLike[str] = ".") -> CommitMetadata:
    """Extract author, committer, message, parent, and tree metadata for a commit."""

    fmt = "%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B"
    info = run_git(["show", "-s", f"--format={fmt}", commit_hash], cwd=dir).stdout
    parts = info.split("\0", 6)
    if len(parts) < 7:
        raise GitError(f"Failed to parse commit metadata for {commit_hash}")
    tree_hash = _rev_parse_tree_of(commit_hash, dir)
    parents_line = run_git(["rev-list", "--parents", "-n", "1", commit_hash], cwd=dir).stdout
    parent_hashes = parents_line.split()[1:]
    return CommitMetadata(
        hash=commit_hash,
        author_name=parts[0],
        author_email=parts[1],
        author_date=parts[2],
        committer_name=parts[3],
        committer_email=parts[4],
        committer_date=parts[5],
        message=parts[6].strip(),
        parents=tuple(parent_hashes),
        tree_hash=tree_hash,
    )


def check_working_tree_clean(dir: str | os.PathLike[str] = ".") -> bool:
    """Return true if git status --porcelain is empty."""

    return run_git(["status", "--porcelain"], cwd=dir).stdout == ""


def create_backup_branch(dir: str | os.PathLike[str] = ".") -> str:
    """Create a timestamped backup branch at the current HEAD and return its name."""

    branch_name = f"backup-rewrite-{datetime.now().strftime('%Y%m%d-%H%M%S')}"
    run_git(["branch", branch_name, "HEAD"], cwd=dir)
    return branch_name


def get_recent_commits(dir: str | os.PathLike[str] = ".", count: int = 10) -> list[str]:
    """Return recent commit subjects."""

    return run_git(["log", f"-{count}", "--pretty=format:%s"], cwd=dir).stdout.splitlines()


def get_common_scopes(dir: str | os.PathLike[str] = ".", limit: int = 100) -> list[tuple[str, int]]:
    """Extract common conventional-commit scopes from history."""

    counts: Counter[str] = Counter()
    for line in run_git(["log", f"-{limit}", "--pretty=format:%s"], cwd=dir).stdout.splitlines():
        scope = _extract_scope_from_commit(line)
        if scope:
            counts[scope] += 1
    return counts.most_common()


def extract_style_patterns(commits: Sequence[str]) -> StylePatterns | None:
    """Extract style conventions from commit subjects."""

    if not commits:
        return None
    verb_counts: Counter[str] = Counter()
    scope_counts: Counter[str] = Counter()
    lowercase_count = 0
    lengths: list[int] = []

    for commit in commits:
        if ":" not in commit:
            continue
        prefix, summary = commit.split(":", 1)
        summary = summary.strip()
        scope = _extract_scope_from_prefix(prefix)
        if scope:
            scope_counts[scope] += 1
        if summary[:1].islower():
            lowercase_count += 1
        words = summary.split()
        if words:
            verb_counts[words[0].lower()] += 1
        lengths.append(len(summary))

    total = len(commits)
    avg_length = sum(lengths) // len(lengths) if lengths else 0
    length_range = (min(lengths), max(lengths)) if lengths else (0, 0)
    return StylePatterns(
        scope_usage_pct=scope_counts.total() / total * 100,
        common_verbs=verb_counts.most_common(),
        avg_length=avg_length,
        length_range=length_range,
        lowercase_pct=lowercase_count / total * 100,
        top_scopes=scope_counts.most_common(),
    )


def rewrite_history(
    commits: Sequence[CommitMetadata],
    new_messages: Sequence[str],
    dir: str | os.PathLike[str] = ".",
) -> None:
    """Rewrite commits with new messages while preserving metadata."""

    if len(commits) != len(new_messages):
        raise ValidationFailure("Commit count mismatch")
    current_branch = run_git(["rev-parse", "--abbrev-ref", "HEAD"], cwd=dir).stdout.strip()
    old_head = get_head_hash(dir)
    parent_map: dict[str, str] = {}
    new_head: str | None = None

    for commit, new_message in zip(commits, new_messages, strict=True):
        old_hash = commit.hash
        new_parents = [parent_map.get(parent, parent) for parent in commit.parents]
        env = {
            "GIT_AUTHOR_NAME": commit.author_name,
            "GIT_AUTHOR_EMAIL": commit.author_email,
            "GIT_AUTHOR_DATE": commit.author_date,
            "GIT_COMMITTER_NAME": commit.committer_name,
            "GIT_COMMITTER_EMAIL": commit.committer_email,
            "GIT_COMMITTER_DATE": commit.committer_date,
        }
        new_hash = commit_tree(commit.tree_hash, new_parents, new_message, dir, env=env)
        parent_map[old_hash] = new_hash
        new_head = new_hash

    if new_head is not None:
        refname = "HEAD" if current_branch == "HEAD" else f"refs/heads/{current_branch}"
        update_ref_checked(refname, new_head, old_head, dir)
        run_git(["reset", "--hard", new_head], cwd=dir)


def _diff_with_retry(
    args: list[str],
    dir: str | os.PathLike[str],
    max_len: int,
    *,
    insert_u1_before: str | None = None,
) -> str:
    result = run_git(args, cwd=dir)
    if len(result.stdout.encode()) <= max_len:
        return result.stdout
    retry_args = args.copy()
    if insert_u1_before is not None and insert_u1_before in retry_args:
        retry_args.insert(retry_args.index(insert_u1_before), "-U1")
    else:
        retry_args.append("-U1")
    return run_git(retry_args, cwd=dir).stdout


def _list_untracked_files(dir: str | os.PathLike[str]) -> list[str]:
    stdout = run_git(["ls-files", "--others", "--exclude-standard"], cwd=dir).stdout
    return [line for line in stdout.splitlines() if line]


def _append_untracked_diff(base_diff: str, dir: str | os.PathLike[str], files: Sequence[str]) -> str:
    diff = base_diff
    for file in files:
        result = run_git(
            [
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                os.devnull,
                file,
            ],
            cwd=dir,
            check=True,
            allow_exit_codes={1},
        )
        lines = list(_diff_lines_preserve_cr(result.stdout))
        if not lines:
            continue
        mode = next(
            (line.removeprefix("new file mode ") for line in lines if line.startswith("new file mode ")), "100644"
        )
        if diff:
            diff += "\n"
        diff += f"diff --git a/{file} b/{file}\n"
        diff += f"new file mode {mode}\n"
        diff += "index 0000000..0000000\n"
        diff += "--- /dev/null\n"
        diff += f"+++ b/{file}\n"
        for line in _content_diff_lines(lines):
            diff += f"{line}\n"
    return diff


def _append_untracked_stat(stat: str, dir: str | os.PathLike[str], files: Sequence[str]) -> str:
    output = stat
    root = Path(dir)
    for file in files:
        path = root / file
        try:
            metadata = path.stat()
        except OSError:
            continue
        lines = 0
        if stat_module.S_ISREG(metadata.st_mode):
            try:
                lines = len(path.read_text(encoding="utf-8", errors="replace").splitlines())
            except OSError:
                lines = 0
        if output and not output.endswith("\n"):
            output += "\n"
        output += f" {file} | {lines} {'+' * min(lines, 50)}\n"
    return output


def _append_untracked_numstat(numstat: str, dir: str | os.PathLike[str], files: Sequence[str]) -> str:
    output = numstat
    root = Path(dir)
    for file in files:
        path = root / file
        try:
            metadata = path.stat()
        except OSError:
            continue
        if stat_module.S_ISREG(metadata.st_mode):
            try:
                content = path.read_bytes()
            except OSError:
                continue
            if b"\0" in content:
                line = f"-\t-\t{file}"
            else:
                line_count = content.decode("utf-8", errors="replace").count("\n")
                line = f"{line_count}\t0\t{file}"
        else:
            line = f"0\t0\t{file}"
        if output and not output.endswith("\n"):
            output += "\n"
        output += line + "\n"
    return output


def _diff_lines_preserve_cr(input: str) -> Iterable[str]:
    """Split lines while only stripping the final LF, preserving bare CR bytes."""
    for line in input.splitlines(keepends=True):
        yield line[:-1] if line.endswith("\n") else line


def _content_diff_lines(lines: Sequence[str]) -> list[str]:
    for index, line in enumerate(lines):
        if line.startswith("@@") or line.startswith("Binary files "):
            return list(lines[index:])
    return []


def _rev_parse_tree_of(commitish: str, dir: str | os.PathLike[str]) -> str:
    return run_git(["rev-parse", f"{commitish}^{{tree}}"], cwd=dir).stdout.strip()


def _rev_parse_parents(commitish: str, dir: str | os.PathLike[str]) -> list[str]:
    return run_git(["rev-parse", f"{commitish}^@"], cwd=dir).stdout.splitlines()


def _index_lock_error(stderr: str, dir: str | os.PathLike[str]) -> GitIndexLocked | None:
    if "index.lock" not in stderr:
        return None
    for line in stderr.splitlines():
        start = line.find("'")
        if start == -1:
            continue
        end = line.find("'", start + 1)
        if end == -1:
            continue
        candidate = line[start + 1 : end]
        if candidate.endswith("index.lock"):
            return GitIndexLocked(Path(candidate))
    return GitIndexLocked(Path(dir) / ".git" / "index.lock")


def _mode_name(mode: object) -> str:
    if isinstance(mode, str):
        return mode.lower()
    value = getattr(mode, "value", None)
    if isinstance(value, str):
        return value.lower()
    name = getattr(mode, "name", None)
    if isinstance(name, str):
        return name.lower()
    return str(mode).lower()


def _extract_scope_from_commit(commit_msg: str) -> str | None:
    prefix, sep, _ = commit_msg.partition(":")
    return _extract_scope_from_prefix(prefix) if sep else None


def _extract_scope_from_prefix(prefix: str) -> str | None:
    start = prefix.find("(")
    end = prefix.find(")", start + 1)
    if start != -1 and end != -1 and start < end:
        return prefix[start + 1 : end]
    return None

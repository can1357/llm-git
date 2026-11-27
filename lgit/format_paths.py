"""Built-in pathname and shebang conventions for format mode."""

from __future__ import annotations

from pathlib import PurePosixPath

_BASENAMES = {
    ".bash_profile": ".sh",
    ".bashrc": ".sh",
    ".clang-format": ".yaml",
    ".clang-tidy": ".yaml",
    ".devcontainer.json": ".jsonc",
    ".editorconfig": ".editorconfig",
    ".env": ".env",
    ".envrc": ".sh",
    ".gitattributes": ".gitattributes",
    ".gitconfig": ".gitconfig",
    ".gitignore": ".gitignore",
    ".gitmodules": ".gitconfig",
    ".profile": ".sh",
    ".yamllint": ".yaml",
    ".zlogin": ".sh",
    ".zprofile": ".sh",
    ".zshenv": ".sh",
    ".zshrc": ".sh",
    "BUILD": ".bzl",
    "BUILD.bazel": ".bzl",
    "CMakeLists.txt": ".cmake",
    "Cargo.lock": ".toml",
    "Containerfile": ".dockerfile",
    "Dockerfile": ".dockerfile",
    "GNUmakefile": ".mk",
    "MODULE.bazel": ".bzl",
    "Makefile": ".mk",
    "Pipfile": ".toml",
    "Pipfile.lock": ".json",
    "WORKSPACE": ".bzl",
    "WORKSPACE.bazel": ".bzl",
    "WORKSPACE.bzlmod": ".bzl",
    "bun.lock": ".jsonc",
    "composer.lock": ".json",
    "deno.lock": ".json",
    "flake.lock": ".json",
    "makefile": ".mk",
    "pdm.lock": ".toml",
    "poetry.lock": ".toml",
    "uv.lock": ".toml",
}
_EDITOR_JSONC = frozenset({"extensions.json", "keybindings.json", "launch.json", "settings.json", "tasks.json"})
_SHEBANGS = {
    b"ash": ".sh",
    b"bash": ".sh",
    b"dash": ".sh",
    b"ksh": ".sh",
    b"sh": ".sh",
    b"zsh": ".sh",
    b"fish": ".fish",
    b"pwsh": ".ps1",
    b"powershell": ".ps1",
}


def path_suffix(path: str) -> str:
    """Return the format suffix implied by a repository-relative path, if any."""
    name = PurePosixPath(path)
    base, suffix = name.name, name.suffix.lower()
    if known := _BASENAMES.get(base):
        return known
    if name.parent.name == ".vscode" and base in _EDITOR_JSONC:
        return ".jsonc"
    if name.parent.name == ".devcontainer" and base == "devcontainer.json":
        return ".jsonc"
    if name.parent.name == ".kube" and base == "config":
        return ".yaml"
    if suffix == ".json" and base.startswith(("tsconfig.", "jsconfig.")):
        return ".jsonc"
    if base.startswith(("Dockerfile.", "Containerfile.")) and suffix not in {".md", ".markdown", ".txt"}:
        return ".dockerfile"
    if base.startswith(".env.") or suffix == ".env":
        return ".env"
    return suffix


def _interpreter(blob: bytes) -> bytes:
    if not blob.startswith(b"#!"):
        return b""
    end = blob.find(b"\n", 2)
    command = blob[2 : end if end != -1 else len(blob)].split()
    if not command:
        return b""
    name = command[0].rsplit(b"/", 1)[-1]
    if name != b"env":
        return name
    args = command[1:]
    if args and args[0] in (b"-S", b"--split-string", b"--"):
        args = args[1:]
    return args[0].rsplit(b"/", 1)[-1] if args else b""


def shebang_suffix(old: bytes, new: bytes) -> str:
    """Identify extensionless shell scripts only when both snapshots select the same interpreter."""
    name = _interpreter(old)
    return _SHEBANGS.get(name, "") if name and _interpreter(new) == name else ""

"""Repository metadata detection for prompt context."""

from __future__ import annotations

import json
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Self


@dataclass(slots=True)
class RepoMetadata:
    """Detected repository language, framework, and workspace metadata."""

    language: str | None = None
    framework: str | None = None
    package_manager: str | None = None
    is_monorepo: bool = False
    package_count: int | None = None

    @classmethod
    def detect(cls, dir: str | Path) -> Self:
        """Detect repository metadata from ``dir``."""

        root = Path(dir)
        for detector in (_detect_rust, _detect_node, _detect_python, _detect_go):
            meta = detector(root)
            if meta is not None:
                return meta
        return cls()

    def format_for_prompt(self) -> str | None:
        """Format detected metadata for LLM prompt injection."""

        if not self.language:
            return None
        lines: list[str] = []
        language = self.language
        if self.is_monorepo:
            if self.package_count is not None:
                language = f"{language} (workspace, {self.package_count} packages)"
            else:
                language = f"{language} (workspace)"
        lines.append(f"Language: {language}")
        if self.framework:
            lines.append(f"Framework: {self.framework}")
        if self.package_manager:
            lines.append(f"Package manager: {self.package_manager}")
        return "\n".join(lines)


def detect(dir: str | Path = ".") -> RepoMetadata:
    """Detect repository metadata from ``dir``."""

    return RepoMetadata.detect(dir)


def _detect_rust(root: Path) -> RepoMetadata | None:
    manifest = root / "Cargo.toml"
    if not manifest.exists():
        return None
    content = _read_text(manifest)
    meta = RepoMetadata(language="Rust", package_manager="cargo")
    data = _read_toml(manifest)
    workspace = data.get("workspace") if isinstance(data, dict) else None
    if isinstance(workspace, dict):
        meta.is_monorepo = True
        members = workspace.get("members")
        if isinstance(members, list):
            meta.package_count = len(members)
    elif "[workspace]" in content:
        meta.is_monorepo = True
        meta.package_count = _count_workspace_members(content)
    meta.framework = _detect_framework(content, _RUST_FRAMEWORKS)
    return meta


def _detect_node(root: Path) -> RepoMetadata | None:
    package_json = root / "package.json"
    if not package_json.exists():
        return None
    content = _read_text(package_json)
    data = _read_json(package_json)
    deps = _node_dependency_names(data, content)
    is_typescript = "typescript" in deps or (root / "tsconfig.json").exists()
    meta = RepoMetadata(language="TypeScript" if is_typescript else "JavaScript")
    if (root / "pnpm-lock.yaml").exists():
        meta.package_manager = "pnpm"
    elif (root / "yarn.lock").exists():
        meta.package_manager = "yarn"
    elif (root / "bun.lockb").exists() or (root / "bun.lock").exists():
        meta.package_manager = "bun"
    else:
        meta.package_manager = "npm"
    workspaces = data.get("workspaces") if isinstance(data, dict) else None
    if workspaces or (root / "pnpm-workspace.yaml").exists():
        meta.is_monorepo = True
        meta.package_count = _count_node_workspaces(workspaces)
    meta.framework = _detect_dependency_framework(deps, _NODE_FRAMEWORKS)
    return meta


def _detect_python(root: Path) -> RepoMetadata | None:
    pyproject = root / "pyproject.toml"
    setup_py = root / "setup.py"
    requirements = root / "requirements.txt"
    if not pyproject.exists() and not setup_py.exists() and not requirements.exists():
        return None
    meta = RepoMetadata(language="Python")
    chunks: list[str] = []
    data: dict[str, Any] = {}
    if pyproject.exists():
        chunks.append(_read_text(pyproject))
        data = _read_toml(pyproject)
    if requirements.exists():
        chunks.append(_read_text(requirements))
    if setup_py.exists():
        chunks.append(_read_text(setup_py))
    text = "\n".join(chunks).lower()
    tool = data.get("tool") if isinstance(data, dict) else None
    tool = tool if isinstance(tool, dict) else {}
    if "poetry" in tool:
        meta.package_manager = "poetry"
    elif "uv" in tool or (root / "uv.lock").exists():
        meta.package_manager = "uv"
    elif "pdm" in tool or (root / "pdm.lock").exists():
        meta.package_manager = "pdm"
    elif (root / "Pipfile").exists():
        meta.package_manager = "pipenv"
    else:
        meta.package_manager = "pip"
    meta.framework = _detect_framework(text, _PYTHON_FRAMEWORKS)
    if (root / "pyproject.toml").exists() and _has_python_workspace(tool):
        meta.is_monorepo = True
    return meta


def _detect_go(root: Path) -> RepoMetadata | None:
    go_mod = root / "go.mod"
    if not go_mod.exists():
        return None
    content = _read_text(go_mod).lower()
    return RepoMetadata(
        language="Go",
        framework=_detect_framework(content, _GO_FRAMEWORKS),
        package_manager="go mod",
    )


_RUST_FRAMEWORKS = (
    ("axum", "Axum"),
    ("actix-web", "Actix Web"),
    ("rocket", "Rocket"),
    ("warp", "Warp"),
    ("tide", "Tide"),
    ("poem", "Poem"),
    ("tower-http", "Tower HTTP"),
    ("hyper", "Hyper"),
    ("tokio", "Tokio async runtime"),
    ("bevy", "Bevy game engine"),
    ("iced", "Iced GUI"),
    ("egui", "egui GUI"),
    ("tauri", "Tauri"),
    ("leptos", "Leptos"),
    ("yew", "Yew"),
    ("dioxus", "Dioxus"),
)

_NODE_FRAMEWORKS = (
    ("next", "Next.js"),
    ("nuxt", "Nuxt"),
    ("@angular/core", "Angular"),
    ("vue", "Vue"),
    ("react", "React"),
    ("svelte", "Svelte"),
    ("solid-js", "SolidJS"),
    ("express", "Express"),
    ("fastify", "Fastify"),
    ("hono", "Hono"),
    ("nestjs", "NestJS"),
    ("@nestjs/core", "NestJS"),
    ("electron", "Electron"),
    ("expo", "Expo"),
    ("react-native", "React Native"),
)

_PYTHON_FRAMEWORKS = (
    ("fastapi", "FastAPI"),
    ("django", "Django"),
    ("flask", "Flask"),
    ("starlette", "Starlette"),
    ("litestar", "Litestar"),
    ("sanic", "Sanic"),
    ("tornado", "Tornado"),
    ("aiohttp", "aiohttp"),
    ("pytorch", "PyTorch"),
    ("torch", "PyTorch"),
    ("tensorflow", "TensorFlow"),
    ("jax", "JAX"),
    ("transformers", "Hugging Face"),
)

_GO_FRAMEWORKS = (
    ("github.com/gin-gonic/gin", "Gin"),
    ("github.com/labstack/echo", "Echo"),
    ("github.com/gofiber/fiber", "Fiber"),
    ("github.com/go-chi/chi", "chi"),
    ("github.com/gorilla/mux", "Gorilla Mux"),
    ("connectrpc.com/connect", "Connect"),
    ("google.golang.org/grpc", "gRPC"),
)


def _read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError:
        return ""


def _read_toml(path: Path) -> dict[str, Any]:
    try:
        return tomllib.loads(_read_text(path))
    except Exception:
        return {}


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(_read_text(path))
    except Exception:
        return {}
    return value if isinstance(value, dict) else {}


def _detect_framework(content: str, frameworks: tuple[tuple[str, str], ...]) -> str | None:
    lowered = content.lower()
    for needle, name in frameworks:
        if needle.lower() in lowered:
            return name
    return None


def _detect_dependency_framework(deps: set[str], frameworks: tuple[tuple[str, str], ...]) -> str | None:
    for package, name in frameworks:
        if package in deps:
            return name
    return None


def _node_dependency_names(data: dict[str, Any], fallback: str) -> set[str]:
    deps: set[str] = set()
    for field in ("dependencies", "devDependencies", "peerDependencies", "optionalDependencies"):
        values = data.get(field)
        if isinstance(values, dict):
            deps.update(str(key) for key in values)
    if not deps:
        deps.update(re.findall(r'"([@\w./-]+)"\s*:', fallback))
    return deps


def _count_node_workspaces(workspaces: Any) -> int | None:
    if isinstance(workspaces, list):
        return len(workspaces)
    if isinstance(workspaces, dict):
        packages = workspaces.get("packages")
        if isinstance(packages, list):
            return len(packages)
    return None


def _count_workspace_members(content: str) -> int | None:
    match = re.search(r"members\s*=\s*\[([^\]]*)\]", content, re.S)
    if not match:
        return None
    return len(re.findall(r'"[^"]+"', match.group(1)))


def _has_python_workspace(tool: dict[str, Any]) -> bool:
    uv = tool.get("uv")
    pdm = tool.get("pdm")
    return (isinstance(uv, dict) and "workspace" in uv) or (isinstance(pdm, dict) and "workspace" in pdm)


__all__ = ["RepoMetadata", "detect"]

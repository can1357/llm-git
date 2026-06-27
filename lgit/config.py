"""Configuration loading for the llm-git Python runtime."""

from __future__ import annotations

import os
import subprocess
import tomllib
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Self

from .errors import ConfigError
from .models import (
    ApiMode,
    CategoryConfig,
    CategoryMatch,
    ResolvedApiMode,
    TypeConfig,
    default_categories,
    default_classifier_hint,
    default_types,
)

DEFAULT_API_BASE_URL = "http://localhost:4000"
DEFAULT_ANALYSIS_MODEL = "claude-opus-4.8"
DEFAULT_SUMMARY_MODEL = "claude-haiku-4-5"
DEFAULT_CONFIG_SUBPATH = Path(".config/llm-git/config.toml")

DEFAULT_EXCLUDED_FILES = [
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "shrinkwrap.yaml",
    "bun.lock",
    "bun.lockb",
    "deno.lock",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
    "Pipfile.lock",
    "pdm.lock",
    "uv.lock",
    "go.sum",
    "flake.lock",
    "pubspec.lock",
    "Podfile.lock",
    "Packages.resolved",
    "mix.lock",
    "packages.lock.json",
    "gradle.lockfile",
]

DEFAULT_LOW_PRIORITY_EXTENSIONS = [
    ".lock",
    ".snap",
    ".sum",
    ".toml",
    ".yaml",
    ".yml",
    ".json",
    ".md",
    ".txt",
    ".log",
    ".tmp",
    ".bak",
]

_TRUE_VALUES = frozenset({"1", "true", "yes", "on"})
_FALSE_VALUES = frozenset({"0", "false", "no", "off"})


@dataclass(slots=True)
class CommitConfig:
    """Runtime configuration loaded from defaults, TOML, and environment."""

    api_base_url: str = DEFAULT_API_BASE_URL
    api_mode: ApiMode = ApiMode.AUTO
    api_key: str | None = None
    request_timeout_secs: int = 120
    connect_timeout_secs: int = 30
    disable_git_background_features: bool = True
    compose_max_rounds: int = 5
    summary_guideline: int = 72
    summary_soft_limit: int = 96
    summary_hard_limit: int = 128
    max_retries: int = 3
    initial_backoff_ms: int = 1000
    auto_fast_threshold_lines: int = 200
    max_diff_length: int = 100000
    max_diff_tokens: int = 25000
    wide_change_threshold: float = 0.50
    analysis_model: str = DEFAULT_ANALYSIS_MODEL
    summary_model: str = DEFAULT_SUMMARY_MODEL
    legacy_model: str | None = None
    excluded_files: list[str] = field(default_factory=lambda: list(DEFAULT_EXCLUDED_FILES))
    low_priority_extensions: list[str] = field(default_factory=lambda: list(DEFAULT_LOW_PRIORITY_EXTENSIONS))
    max_detail_tokens: int = 200
    analysis_prompt_variant: str = "default"
    summary_prompt_variant: str = "default"
    wide_change_abstract: bool = True
    markdown_output: bool = True
    exclude_old_message: bool = True
    gpg_sign: bool = False
    signoff: bool = False
    types: dict[str, TypeConfig] = field(default_factory=default_types)
    classifier_hint: str = field(default_factory=default_classifier_hint)
    categories: list[CategoryConfig] = field(default_factory=default_categories)
    changelog_enabled: bool = True
    map_reduce_enabled: bool = True
    map_reduce_threshold: int = 5000
    map_batch_token_budget: int = 16000
    cache_enabled: bool = True
    cache_ttl_days: int = 14
    cache_dir: str | None = None
    analysis_prompt: str = ""
    summary_prompt: str = ""

    @classmethod
    def load(cls, path: str | os.PathLike[str] | None = None) -> Self:
        """Load configuration from the default path or ``LLM_GIT_CONFIG``."""
        config_path = _selected_config_path(path)
        if config_path is not None and config_path.exists():
            return cls.from_file(config_path)
        config = cls()
        config._finalize()
        return config

    @classmethod
    def from_file(cls, path: str | os.PathLike[str]) -> Self:
        """Load configuration from a TOML file, then apply environment overrides."""
        config_path = Path(path).expanduser()
        try:
            contents = config_path.read_text(encoding="utf-8")
        except OSError as exc:
            raise ConfigError(f"Failed to read config {config_path}: {exc}") from exc
        try:
            data = tomllib.loads(contents)
        except tomllib.TOMLDecodeError as exc:
            raise ConfigError(f"Failed to parse config {config_path}: {exc}") from exc
        config = cls.from_mapping(data)
        config._finalize()
        return config

    @classmethod
    def from_mapping(cls, data: Mapping[str, Any]) -> Self:
        """Build configuration from a TOML-compatible mapping."""
        kwargs: dict[str, Any] = {}
        for raw_key, value in data.items():
            key = _normalize_config_key(str(raw_key))
            if key == "model":
                kwargs["legacy_model"] = None if value is None else str(value)
            elif key == "api_mode":
                kwargs[key] = ApiMode.from_raw(str(value))
            elif key == "types":
                kwargs[key] = _parse_types(value)
            elif key == "categories":
                kwargs[key] = _parse_categories(value)
            elif key in _FIELD_COERCERS:
                kwargs[key] = _FIELD_COERCERS[key](value)
        return cls(**kwargs)

    @property
    def resolved_api_mode(self) -> ResolvedApiMode:
        """Return the concrete API protocol selected for this configuration."""
        return ResolvedApiMode.from_api_mode(self.api_mode, self.api_base_url)

    def resolve_api_mode(self, model_name: str | None = None) -> ResolvedApiMode:
        """Return the concrete API protocol; ``model_name`` is accepted for compatibility."""
        return self.resolved_api_mode

    def _finalize(self) -> None:
        _apply_env_overrides(self)
        self._normalize_models()
        if self.api_key is not None:
            self.api_key = _resolve_config_value(self.api_key)
        self._load_prompts()

    def _normalize_models(self) -> None:
        if self.legacy_model:
            model = self.legacy_model
            self.analysis_model = model
            if self.summary_model == DEFAULT_SUMMARY_MODEL:
                self.summary_model = model

    def _load_prompts(self) -> None:
        from .templates import ensure_prompts_dir

        ensure_prompts_dir()
        self.analysis_prompt = ""
        self.summary_prompt = ""


def default_config_path() -> Path:
    """Return the default llm-git TOML config path for the current user."""
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE")
    if not home:
        raise ConfigError("No home directory found (tried HOME and USERPROFILE)")
    return Path(home).joinpath(DEFAULT_CONFIG_SUBPATH)


def _selected_config_path(path: str | os.PathLike[str] | None) -> Path | None:
    if path is not None:
        return Path(path).expanduser()
    if custom_path := os.environ.get("LLM_GIT_CONFIG"):
        return Path(custom_path).expanduser()
    try:
        return default_config_path()
    except ConfigError:
        return None


def _apply_env_overrides(config: CommitConfig) -> None:
    if "LLM_GIT_API_URL" in os.environ:
        config.api_base_url = os.environ["LLM_GIT_API_URL"]
    if "LLM_GIT_API_KEY" in os.environ:
        config.api_key = os.environ["LLM_GIT_API_KEY"]
    if "LLM_GIT_API_MODE" in os.environ:
        config.api_mode = _parse_api_mode(os.environ["LLM_GIT_API_MODE"])
    if value := os.environ.get("LLM_GIT_DISABLE_GIT_BACKGROUND_FEATURES"):
        parsed = _parse_env_bool(value)
        if parsed is not None:
            config.disable_git_background_features = parsed
    if value := os.environ.get("LLM_GIT_CACHE_DISABLED"):
        parsed = _parse_env_bool(value)
        if parsed is not None:
            config.cache_enabled = not parsed
    if value := os.environ.get("LLM_GIT_CACHE_TTL_DAYS"):
        try:
            config.cache_ttl_days = int(value.strip())
        except ValueError:
            pass
    if "LLM_GIT_CACHE_DIR" in os.environ:
        value = os.environ["LLM_GIT_CACHE_DIR"].strip()
        config.cache_dir = value or None


def _parse_env_bool(value: str) -> bool | None:
    normalized = value.strip().lower()
    if normalized in _TRUE_VALUES:
        return True
    if normalized in _FALSE_VALUES:
        return False
    return None


def _parse_api_mode(value: str) -> ApiMode:
    match value.strip().lower().replace("_", "-"):
        case "chat" | "chat-completions":
            return ApiMode.CHAT_COMPLETIONS
        case "anthropic" | "messages" | "anthropic-messages":
            return ApiMode.ANTHROPIC_MESSAGES
        case _:
            return ApiMode.AUTO


def _resolve_config_value(raw: str) -> str:
    value = raw.strip()
    if not value.startswith("!"):
        return raw
    command = value[1:].strip()
    if not command:
        raise ConfigError("api_key command is empty")
    if command == "cat" or command.startswith("cat "):
        path_text = command[3:].strip()
        if not path_text:
            raise ConfigError("api_key `!cat` command requires a path")
        path = Path(path_text).expanduser()
        try:
            return path.read_text(encoding="utf-8").strip()
        except OSError as exc:
            raise ConfigError(f"api_key `!cat` failed to read {path}: {exc}") from exc
    try:
        output = subprocess.run(
            ["/bin/sh", "-c", command],
            check=False,
            capture_output=True,
            stdin=subprocess.DEVNULL,
            text=True,
        )
    except OSError as exc:
        raise ConfigError(f"api_key `!{command}` failed to spawn: {exc}") from exc
    if output.returncode != 0:
        stderr = output.stderr.strip()
        raise ConfigError(f"api_key `!{command}` exited with status {output.returncode}: {stderr}")
    return output.stdout.strip()


def _normalize_config_key(key: str) -> str:
    return key.strip().replace("-", "_")


def _to_bool(value: Any) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        parsed = _parse_env_bool(value)
        if parsed is not None:
            return parsed
    return bool(value)


def _to_int(value: Any) -> int:
    return int(value)


def _to_float(value: Any) -> float:
    return float(value)


def _to_str(value: Any) -> str:
    return str(value)


def _to_optional_str(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value).strip()
    return text or None


def _to_str_list(value: Any) -> list[str]:
    if value is None:
        return []
    if isinstance(value, str):
        return [value]
    return [str(item) for item in value]


def _to_str_tuple(value: Any) -> tuple[str, ...]:
    return tuple(_to_str_list(value))


def _parse_types(value: Any) -> dict[str, TypeConfig]:
    if value is None:
        return {}
    if isinstance(value, Mapping):
        return {str(name).strip().lower(): _parse_type_config(config) for name, config in value.items()}
    types: dict[str, TypeConfig] = {}
    for item in value:
        if not isinstance(item, Mapping) or "name" not in item:
            raise ConfigError("types entries must be tables with a name")
        name = str(item["name"]).strip().lower()
        types[name] = _parse_type_config(item)
    return types


def _parse_type_config(value: Any) -> TypeConfig:
    if isinstance(value, str):
        return TypeConfig(description=value)
    if not isinstance(value, Mapping):
        raise ConfigError("type config entries must be tables or strings")
    return TypeConfig(
        description=str(value.get("description", "")),
        diff_indicators=_to_str_tuple(value.get("diff_indicators", ())),
        file_patterns=_to_str_tuple(value.get("file_patterns", ())),
        examples=_to_str_tuple(value.get("examples", ())),
        hint=str(value.get("hint", "")),
        aliases=_to_str_tuple(value.get("aliases", ())),
    )


def _parse_categories(value: Any) -> list[CategoryConfig]:
    if value is None:
        return []
    return [_parse_category_config(item) for item in value]


def _parse_category_config(value: Any) -> CategoryConfig:
    if isinstance(value, str):
        return CategoryConfig(name=value)
    if not isinstance(value, Mapping):
        raise ConfigError("category entries must be tables or strings")
    match_data = value.get("match", {})
    if not isinstance(match_data, Mapping):
        raise ConfigError("category match entries must be tables")
    return CategoryConfig(
        name=str(value.get("name", "")),
        header=_to_optional_str(value.get("header")),
        match=CategoryMatch(
            types=_to_str_tuple(match_data.get("types", ())),
            body_contains=_to_str_tuple(match_data.get("body_contains", ())),
        ),
        default=_to_bool(value.get("default", False)),
    )


_FIELD_COERCERS = {
    "api_base_url": _to_str,
    "api_key": _to_optional_str,
    "request_timeout_secs": _to_int,
    "connect_timeout_secs": _to_int,
    "disable_git_background_features": _to_bool,
    "compose_max_rounds": _to_int,
    "summary_guideline": _to_int,
    "summary_soft_limit": _to_int,
    "summary_hard_limit": _to_int,
    "max_retries": _to_int,
    "initial_backoff_ms": _to_int,
    "auto_fast_threshold_lines": _to_int,
    "max_diff_length": _to_int,
    "max_diff_tokens": _to_int,
    "wide_change_threshold": _to_float,
    "analysis_model": _to_str,
    "summary_model": _to_str,
    "legacy_model": _to_optional_str,
    "excluded_files": _to_str_list,
    "low_priority_extensions": _to_str_list,
    "max_detail_tokens": _to_int,
    "analysis_prompt_variant": _to_str,
    "summary_prompt_variant": _to_str,
    "wide_change_abstract": _to_bool,
    "markdown_output": _to_bool,
    "exclude_old_message": _to_bool,
    "gpg_sign": _to_bool,
    "signoff": _to_bool,
    "classifier_hint": _to_str,
    "changelog_enabled": _to_bool,
    "map_reduce_enabled": _to_bool,
    "map_reduce_threshold": _to_int,
    "map_batch_token_budget": _to_int,
    "cache_enabled": _to_bool,
    "cache_ttl_days": _to_int,
    "cache_dir": _to_optional_str,
}

__all__ = [
    "CommitConfig",
    "DEFAULT_API_BASE_URL",
    "DEFAULT_ANALYSIS_MODEL",
    "DEFAULT_SUMMARY_MODEL",
    "DEFAULT_EXCLUDED_FILES",
    "DEFAULT_LOW_PRIORITY_EXTENSIONS",
    "default_config_path",
]

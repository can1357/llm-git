"""SQLite-backed best-effort cache for LLM responses."""

from __future__ import annotations

import os
import sqlite3
import threading
import time
from dataclasses import dataclass
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING, Any, ClassVar, Self

from blake3 import blake3

if TYPE_CHECKING:
    from .config import CommitConfig

SCHEMA_VERSION = 3
PRUNE_DIVISOR = 64
MAX_FAILURES = 64


@dataclass(frozen=True, slots=True)
class CachedLlmResponse:
    """Stored request/response payload returned for a cache hit."""

    request: str
    response: str
    created_at: int


@dataclass(frozen=True, slots=True)
class FailureRecord:
    """Recorded LLM failure retained for offline diagnosis only."""

    model: str
    operation: str
    request: str
    response: str
    error: str


@dataclass(frozen=True, slots=True)
class CacheMaterial:
    """Material that uniquely identifies a one-shot LLM request."""

    operation: str
    model: str
    tool_name: str
    system_prompt: str
    user_prompt: str
    api_mode: str


class LlmCache:
    """SQLite-backed cache of parsed LLM responses."""

    _instance: ClassVar[LlmCache | None] = None
    _instance_lock: ClassVar[threading.Lock] = threading.Lock()
    _initialized: ClassVar[bool] = False

    def __init__(self, path: str | os.PathLike[str], ttl: timedelta | int | float = 0) -> None:
        self.path = Path(path)
        self.ttl_secs = _ttl_seconds(ttl)
        self._lock = threading.Lock()
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(self.path, check_same_thread=False)
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        self._create_schema()

    @classmethod
    def open(cls, path: str | os.PathLike[str], ttl: timedelta | int | float = 0) -> Self:
        """Open or create a cache database at ``path``."""

        return cls(path, ttl)

    @classmethod
    def init(cls, config: CommitConfig) -> None:
        """Initialize the process-global cache from configuration; first call wins."""

        with cls._instance_lock:
            if cls._initialized:
                return
            cls._instance = _build_from_config(config)
            cls._initialized = True

    @classmethod
    def instance(cls) -> LlmCache | None:
        """Return the initialized process-global cache handle, if any."""

        return cls._instance

    def get_entry(self, key: str) -> CachedLlmResponse | None:
        """Return the stored request/response for ``key`` or ``None`` on miss."""

        try:
            with self._lock:
                row = self._conn.execute(
                    """
                    SELECT request, response, created_at
                    FROM responses
                    WHERE key = ? AND schema_version = ?
                    """,
                    (key, SCHEMA_VERSION),
                ).fetchone()
                if row is None:
                    return None
                request, response, created_at = str(row[0]), str(row[1]), int(row[2])
                if self.ttl_secs > 0 and created_at < _now_unix() - self.ttl_secs:
                    self._conn.execute("DELETE FROM responses WHERE key = ?", (key,))
                    self._conn.commit()
                    return None
                self._conn.execute("UPDATE responses SET accessed_at = ? WHERE key = ?", (_now_unix(), key))
                self._conn.commit()
                return CachedLlmResponse(request=request, response=response, created_at=created_at)
        except Exception:
            return None

    def get(self, key: str) -> str | None:
        """Return the cached response payload for ``key`` if available."""

        entry = self.get_entry(key)
        return entry.response if entry is not None else None

    def put(self, key: str, model: str, operation: str, request: str, response: str) -> None:
        """Insert or replace a successful response, swallowing cache failures."""

        try:
            now = _now_unix()
            with self._lock:
                self._conn.execute(
                    """
                    INSERT OR REPLACE INTO responses
                    (key, schema_version, model, operation, request, response, created_at, accessed_at)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (key, SCHEMA_VERSION, model, operation, request, response, now, now),
                )
                if self.ttl_secs > 0 and now % PRUNE_DIVISOR == 0:
                    self._conn.execute("DELETE FROM responses WHERE created_at < ?", (now - self.ttl_secs,))
                self._conn.commit()
        except Exception:
            return

    def put_failure(
        self,
        key: str,
        model: str,
        operation: str,
        request: str,
        response: str,
        error: str,
    ) -> None:
        """Record a failed response for diagnostics without serving it as a hit."""

        try:
            now = _now_unix()
            with self._lock:
                self._conn.execute(
                    """
                    INSERT INTO failures
                    (schema_version, key, model, operation, request, response, error, created_at)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    """,
                    (SCHEMA_VERSION, key, model, operation, request, response, error, now),
                )
                if self.ttl_secs > 0:
                    self._conn.execute("DELETE FROM failures WHERE created_at < ?", (now - self.ttl_secs,))
                self._conn.execute(
                    """
                    DELETE FROM failures
                    WHERE id NOT IN (SELECT id FROM failures ORDER BY id DESC LIMIT ?)
                    """,
                    (MAX_FAILURES,),
                )
                self._conn.commit()
        except Exception:
            return

    def recent_failures(self, limit: int) -> list[FailureRecord]:
        """Return recent diagnostic failures, newest first."""

        try:
            with self._lock:
                rows = self._conn.execute(
                    """
                    SELECT model, operation, request, response, error
                    FROM failures
                    ORDER BY id DESC
                    LIMIT ?
                    """,
                    (max(0, int(limit)),),
                ).fetchall()
            return [FailureRecord(*(str(value) for value in row)) for row in rows]
        except Exception:
            return []

    def close(self) -> None:
        """Close the underlying SQLite connection."""

        with self._lock:
            self._conn.close()

    def _create_schema(self) -> None:
        with self._lock:
            self._conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS responses (
                    key TEXT PRIMARY KEY,
                    schema_version INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    request TEXT NOT NULL,
                    response TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    accessed_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_responses_created_at ON responses(created_at);
                CREATE TABLE IF NOT EXISTS failures (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    schema_version INTEGER NOT NULL,
                    key TEXT NOT NULL,
                    model TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    request TEXT NOT NULL,
                    response TEXT NOT NULL,
                    error TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_failures_created_at ON failures(created_at);
                """
            )
            try:
                self._conn.execute("ALTER TABLE responses ADD COLUMN request TEXT NOT NULL DEFAULT ''")
            except sqlite3.OperationalError as error:
                if "duplicate column name" not in str(error).lower():
                    raise
            self._conn.commit()


def compute_key(material: CacheMaterial) -> str:
    """Compute a stable BLAKE3 cache key over request material."""

    hasher = blake3()
    hasher.update(b"llm-cache/v1\n")
    _write_field(hasher, "operation", material.operation)
    _write_field(hasher, "model", material.model)
    _write_field(hasher, "api_mode", material.api_mode)
    _write_field(hasher, "tool_name", material.tool_name)
    _write_field(hasher, "system", material.system_prompt)
    _write_field(hasher, "user", material.user_prompt)
    hasher.update(b"\n")
    return hasher.hexdigest()


def _build_from_config(config: CommitConfig) -> LlmCache | None:
    if not config.cache_enabled:
        return None
    cache_dir = _resolve_cache_dir(config)
    if cache_dir is None:
        return None
    ttl_days = config.cache_ttl_days
    try:
        return LlmCache.open(cache_dir / "responses.sqlite", timedelta(days=ttl_days))
    except Exception:
        return None


def _resolve_cache_dir(config: CommitConfig) -> Path | None:
    explicit = config.cache_dir
    if explicit:
        return Path(explicit)
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return Path(xdg) / "llm-git"
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE")
    if home:
        return Path(home) / ".cache" / "llm-git"
    return None


def _ttl_seconds(ttl: timedelta | int | float) -> int:
    if isinstance(ttl, timedelta):
        return max(0, int(ttl.total_seconds()))
    return max(0, int(ttl))


def _now_unix() -> int:
    return int(time.time())


def _write_field(hasher: Any, name: str, value: str) -> None:
    hasher.update(name.encode())
    hasher.update(b"\x00")
    hasher.update(value.encode())
    hasher.update(b"\n")


__all__ = [
    "SCHEMA_VERSION",
    "CacheMaterial",
    "CachedLlmResponse",
    "FailureRecord",
    "LlmCache",
    "compute_key",
]

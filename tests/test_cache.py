from __future__ import annotations

import sqlite3
from dataclasses import replace
from pathlib import Path

from lgit.cache import MAX_FAILURES, CacheMaterial, LlmCache, compute_key


def _material() -> CacheMaterial:
    return CacheMaterial(
        operation="test",
        model="test-model",
        tool_name="tool",
        system_prompt="system",
        user_prompt="user",
        api_mode="ChatCompletions",
    )


def test_key_is_stable_and_collision_resistant() -> None:
    material = _material()
    first = compute_key(material)
    second = compute_key(material)

    other = replace(_material(), user_prompt="different")

    assert first == second
    assert first != compute_key(other)


def test_roundtrip_get_put(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite", 60)

    assert cache.get("k") is None

    cache.put("k", "model", "op", '{"request":1}', '{"x":1}')
    assert cache.get("k") == '{"x":1}'
    entry = cache.get_entry("k")
    assert entry is not None
    assert entry.request == '{"request":1}'
    assert entry.response == '{"x":1}'

    cache.put("k", "model", "op", '{"request":2}', '{"x":2}')
    assert cache.get("k") == '{"x":2}'
    entry = cache.get_entry("k")
    assert entry is not None
    assert entry.request == '{"request":2}'


def test_open_migrates_old_schema_before_storing_requests(tmp_path: Path) -> None:
    path = tmp_path / "c.sqlite"
    with sqlite3.connect(path) as conn:
        conn.executescript(
            """
            CREATE TABLE responses (
                key TEXT PRIMARY KEY,
                schema_version INTEGER NOT NULL,
                model TEXT NOT NULL,
                operation TEXT NOT NULL,
                response TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                accessed_at INTEGER NOT NULL
            );
            """
        )

    cache = LlmCache.open(path, 60)
    cache.put("k", "model", "op", '{"request":true}', '{"response":true}')

    entry = cache.get_entry("k")
    assert entry is not None
    assert entry.request == '{"request":true}'
    assert entry.response == '{"response":true}'


def test_ttl_zero_disables_expiry(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite", 0)

    cache.put("k", "model", "op", "request", "v")

    assert cache.get("k") == "v"


def test_put_failure_records_for_diagnosis_without_serving_cache_hits(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite", 0)

    cache.put_failure(
        "k1",
        "gemini-flash-lite",
        "changelog",
        '{"req":1}',
        "**Added**\n- a thing",
        "markdown changelog: no entries found",
    )

    recent = cache.recent_failures(10)
    assert len(recent) == 1
    assert recent[0].operation == "changelog"
    assert recent[0].response == "**Added**\n- a thing"
    assert recent[0].error == "markdown changelog: no entries found"
    assert cache.get("k1") is None


def test_put_failure_caps_retained_rows(tmp_path: Path) -> None:
    cache = LlmCache.open(tmp_path / "c.sqlite", 0)
    total = MAX_FAILURES + 50

    for index in range(total):
        cache.put_failure("k", "m", "op", "req", f"resp{index}", "err")

    recent = cache.recent_failures(total)
    assert len(recent) == MAX_FAILURES
    assert recent[0].response == f"resp{total - 1}"

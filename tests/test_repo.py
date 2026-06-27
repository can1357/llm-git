from __future__ import annotations

from lgit.repo import RepoMetadata


def test_format_for_prompt_empty() -> None:
    meta = RepoMetadata()

    assert meta.format_for_prompt() is None


def test_format_for_prompt_rust() -> None:
    meta = RepoMetadata(
        language="Rust",
        framework="Axum",
        package_manager="cargo",
        is_monorepo=True,
        package_count=5,
    )

    formatted = meta.format_for_prompt()

    assert formatted is not None
    assert "Rust (workspace, 5 packages)" in formatted
    assert "Framework: Axum" in formatted

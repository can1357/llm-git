from __future__ import annotations

from lgit.analysis import ScopeAnalyzer, extract_components_from_path, extract_path_from_rename
from lgit.config import CommitConfig


def _default_config() -> CommitConfig:
    return CommitConfig(
        excluded_files=["Cargo.lock", "package-lock.json", "yarn.lock"],
        wide_change_threshold=0.5,
    )


def test_extract_path_from_rename_brace() -> None:
    # git's compact-rename `lib/{old => new}/file.rs` resolves to `lib/new/file.rs`:
    # the path segment trailing the brace is part of the destination. (Rust drops
    # it to `lib/new`; Python keeps it, which is the correct git semantics.)
    assert extract_path_from_rename("lib/{old => new}/file.rs") == "lib/new/file.rs"


def test_extract_path_from_rename_brace_complex() -> None:
    assert extract_path_from_rename("src/api/{client.rs => http_client.rs}") == "src/api/http_client.rs"


def test_extract_path_from_rename_arrow() -> None:
    assert extract_path_from_rename("old/file.rs => new/file.rs") == "new/file.rs"


def test_extract_path_from_rename_arrow_with_spaces() -> None:
    assert extract_path_from_rename("  old/path.rs => new/path.rs  ") == "new/path.rs"


def test_extract_path_from_rename_no_rename() -> None:
    assert extract_path_from_rename("lib/file.rs") == "lib/file.rs"


def test_extract_path_from_rename_malformed_brace() -> None:
    assert extract_path_from_rename("lib/{old => new/file.rs") == "lib/{old => new/file.rs"


def test_extract_components_simple() -> None:
    assert extract_components_from_path("src/api/client.rs") == ["api"]


def test_extract_components_with_placeholder() -> None:
    assert extract_components_from_path("lib/foo/bar/baz.tsx") == ["foo", "foo/bar"]


def test_extract_components_skip_tests() -> None:
    assert extract_components_from_path("tests/api/client_test.rs") == ["api"]


def test_extract_components_skip_node_modules() -> None:
    assert extract_components_from_path("node_modules/foo/bar.js") == ["foo"]


def test_extract_components_single_segment() -> None:
    assert extract_components_from_path("src/main.rs") == []


def test_extract_components_dotfile_skipped() -> None:
    assert extract_components_from_path("lib/.git/config") == ["config"]


def test_extract_components_strips_extension() -> None:
    assert "api" in extract_components_from_path("src/api/client.rs")


def test_extract_components_go_internal() -> None:
    assert extract_components_from_path("internal/agent/worker.go") == ["agent"]


def test_extract_components_go_internal_nested() -> None:
    assert extract_components_from_path("internal/config/parser/json.go") == ["config", "config/parser"]


def test_extract_components_go_pkg() -> None:
    assert extract_components_from_path("pkg/util/strings.go") == ["util"]


def test_extract_components_monorepo_packages() -> None:
    assert extract_components_from_path("packages/core/index.ts") == ["core"]


def test_process_numstat_line_normal() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("10\t5\tlib/foo/bar.rs", _default_config())

    assert analyzer.total_lines == 15
    assert analyzer.component_lines.get("foo") == 15


def test_process_numstat_line_excluded_file() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("10\t5\tCargo.lock", _default_config())

    assert analyzer.total_lines == 0
    assert analyzer.component_lines == {}


def test_process_numstat_line_binary_file() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("-\t-\timage.png", _default_config())

    assert analyzer.total_lines == 0


def test_process_numstat_line_invalid() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("invalid line", _default_config())

    assert analyzer.total_lines == 0


def test_process_numstat_line_rename_brace() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("20\t10\tlib/{old => new}/file.rs", _default_config())

    assert analyzer.total_lines == 30
    assert analyzer.component_lines.get("new") == 30


def test_process_numstat_line_multiple_files() -> None:
    analyzer = ScopeAnalyzer()
    analyzer.process_numstat_line("10\t5\tsrc/api/client.rs", _default_config())
    analyzer.process_numstat_line("20\t10\tsrc/api/server.rs", _default_config())

    assert analyzer.total_lines == 45
    assert analyzer.component_lines.get("api") == 45

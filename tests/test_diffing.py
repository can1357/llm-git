from __future__ import annotations

import pytest
from lgit.diffing import FileDiff, collapse_blob_lines, parse_diff, reconstruct_diff, scrub_diff_for_prompt


def _file_diff(
    filename: str,
    *,
    header: str = "",
    content: str = "",
    additions: int = 0,
    deletions: int = 0,
    is_binary: bool = False,
) -> FileDiff:
    return FileDiff(
        filename=filename,
        header=header,
        content=content,
        additions=additions,
        deletions=deletions,
        is_binary=is_binary,
    )


def test_parse_diff_simple() -> None:
    diff = """diff --git a/src/main.rs b/src/main.rs
index 123..456 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
+use std::collections::HashMap;
 fn main() {
     println!("hello");
 }"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "src/main.rs"
    assert files[0].additions == 1
    assert files[0].deletions == 0
    assert not files[0].is_binary
    assert "diff --git" in files[0].header
    assert "use std::collections::HashMap" in files[0].content


def test_parse_diff_multi_file() -> None:
    diff = """diff --git a/src/lib.rs b/src/lib.rs
index 111..222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
+pub mod utils;
 pub fn test() {}
diff --git a/src/main.rs b/src/main.rs
index 333..444 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,2 @@
 fn main() {}
+fn helper() {}"""

    files = parse_diff(diff)

    assert len(files) == 2
    assert files[0].filename == "src/lib.rs"
    assert files[1].filename == "src/main.rs"
    assert files[0].additions == 1
    assert files[1].additions == 1


def test_parse_diff_rename() -> None:
    diff = """diff --git a/old.rs b/new.rs
similarity index 95%
rename from old.rs
rename to new.rs
index 123..456 100644
--- a/old.rs
+++ b/new.rs
@@ -1,2 +1,3 @@
 fn test() {}
+fn helper() {}"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "new.rs"
    assert "rename from" in files[0].header
    assert "rename to" in files[0].header
    assert files[0].additions == 1


def test_parse_diff_binary() -> None:
    diff = """diff --git a/image.png b/image.png
index 123..456 100644
Binary files a/image.png and b/image.png differ"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "image.png"
    assert files[0].is_binary
    assert "Binary files" in files[0].header


def test_parse_diff_empty() -> None:
    assert parse_diff("") == []


def test_parse_diff_malformed_missing_hunks() -> None:
    diff = """diff --git a/src/main.rs b/src/main.rs
index 123..456 100644
--- a/src/main.rs
+++ b/src/main.rs"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "src/main.rs"
    assert files[0].content == ""


def test_parse_diff_new_file() -> None:
    diff = """diff --git a/new.rs b/new.rs
new file mode 100644
index 000..123 100644
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+fn test() {}
+fn main() {}"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "new.rs"
    assert "new file mode" in files[0].header
    assert files[0].additions == 2


def test_parse_diff_deleted_file() -> None:
    diff = """diff --git a/old.rs b/old.rs
deleted file mode 100644
index 123..000 100644
--- a/old.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn test() {}
-fn main() {}"""

    files = parse_diff(diff)

    assert len(files) == 1
    assert files[0].filename == "old.rs"
    assert "deleted file mode" in files[0].header
    assert files[0].deletions == 2


def test_file_diff_size() -> None:
    file = _file_diff("test.rs", header="header", content="content")

    assert file.size == len("headercontent")


@pytest.mark.parametrize("filename", ["src/main.rs", "script.py", "app.js"])
def test_file_diff_priority_source_files(filename: str) -> None:
    assert _file_diff(filename).priority() == 100


def test_file_diff_priority_binary() -> None:
    assert _file_diff("image.png", is_binary=True).priority() == -100


@pytest.mark.parametrize("filename", ["src/test_utils.rs", "tests/integration_test.rs"])
def test_file_diff_priority_test_files(filename: str) -> None:
    assert _file_diff(filename).priority() == 10


def test_file_diff_priority_low_priority_extensions() -> None:
    assert _file_diff("README.md").priority() == 20
    assert _file_diff("prompts/analysis/default.md").priority() == 100
    assert _file_diff("system/analysis/default.md").priority() == 100
    assert _file_diff("config.toml").priority() == 20


@pytest.mark.parametrize("filename", ["Cargo.toml", "package.json", "go.mod"])
def test_file_diff_priority_dependency_manifests(filename: str) -> None:
    assert _file_diff(filename).priority() == 70


def test_file_diff_priority_default() -> None:
    assert _file_diff("data.csv").priority() == 50


def test_file_diff_truncate_small() -> None:
    file = _file_diff("test.rs", header="header", content="short content")
    original_size = file.size

    file.truncate(1000)

    assert file.size == original_size
    assert file.content == "short content"


def test_file_diff_truncate_large() -> None:
    content = "\n".join(f"line {i}" for i in range(100))
    file = _file_diff("test.rs", header="header", content=content)

    file.truncate(500)

    assert "... (truncated" in file.content
    assert "line 0" in file.content
    assert "line 99" in file.content


def test_file_diff_truncate_utf8_boundary() -> None:
    file = _file_diff("test.rs", header="header", content="😀" * 80)

    file.truncate(121)

    assert file.content.endswith("\n... (truncated)")
    truncated_payload = file.content.removesuffix("\n... (truncated)")
    assert truncated_payload
    assert len(truncated_payload.encode("utf-8")) % len("😀".encode()) == 0


def test_reconstruct_diff_single_file() -> None:
    files = [_file_diff("test.rs", header="diff --git a/test.rs b/test.rs", content="+new line", additions=1)]

    result = reconstruct_diff(files)

    assert result == "diff --git a/test.rs b/test.rs\n+new line"


def test_reconstruct_diff_multiple_files() -> None:
    files = [
        _file_diff("a.rs", header="diff --git a/a.rs b/a.rs", content="+line a", additions=1),
        _file_diff("b.rs", header="diff --git a/b.rs b/b.rs", content="+line b", additions=1),
    ]

    result = reconstruct_diff(files)

    assert "a.rs" in result
    assert "b.rs" in result
    assert "+line a" in result
    assert "+line b" in result


def test_reconstruct_diff_empty_content() -> None:
    files = [_file_diff("test.rs", header="diff --git a/test.rs b/test.rs")]

    result = reconstruct_diff(files)

    assert result == "diff --git a/test.rs b/test.rs"


def test_reconstruct_diff_empty_sequence() -> None:
    assert reconstruct_diff([]) == ""


def test_collapse_blob_lines_leaves_normal_diff_untouched() -> None:
    diff = "diff --git a/test.rs b/test.rs\n+let x = 1;\n-let x = 0;"

    assert collapse_blob_lines(diff) is diff


def test_collapse_blob_lines_collapses_hex_blob() -> None:
    blob = '+pub static SLIR: &[u8] = b"' + "\\x53" * 8000 + '";'
    diff = f"diff --git a/blob.rs b/blob.rs\n{blob}\n+let x = 1;"

    result = collapse_blob_lines(diff)

    assert len(result) < 500
    assert "[..omitted 31KB..]" in result
    assert result.startswith("diff --git a/blob.rs b/blob.rs\n+pub static SLIR")
    assert result.endswith("+let x = 1;")
    # head and tail of the blob line survive around the marker
    assert '\\x53";' in result


def test_collapse_blob_lines_formats_megabytes() -> None:
    diff = "+" + "A" * (3 * 1024 * 1024)

    result = collapse_blob_lines(diff)

    assert "[..omitted 3.0MB..]" in result


def test_collapse_blob_lines_respects_threshold() -> None:
    line = "+" + "B" * 400

    assert collapse_blob_lines(line, threshold=512) is line
    assert "[..omitted" in collapse_blob_lines(line, threshold=100)


def test_file_diff_truncate_many_lines_respects_byte_budget() -> None:
    # >30 lines where the kept head lines are themselves huge: the 15/10 line
    # sampling alone would blow the budget, so the byte guard must kick in.
    content = "\n".join("x" * 10_000 for _ in range(100))
    file = _file_diff("test.rs", header="header", content=content)

    file.truncate(4_000)

    assert file.size <= 4_000


def test_scrub_diff_for_prompt_leaves_small_diff_untouched() -> None:
    diff = "diff --git a/test.rs b/test.rs\n@@ -1 +1 @@\n+let x = 1;"

    assert scrub_diff_for_prompt(diff) is diff


def test_scrub_diff_for_prompt_caps_oversized_file_section() -> None:
    # Many short lines: blob-line collapse can't shrink this, only the per-file cap can.
    big_content = "\n".join(f"+generated row {i}" for i in range(20_000))
    diff = (
        f"diff --git a/generated.json b/generated.json\n@@ -0,0 +1,20000 @@\n{big_content}\n"
        "diff --git a/small.rs b/small.rs\n@@ -1 +1 @@\n+let x = 1;"
    )

    result = scrub_diff_for_prompt(diff, max_file_bytes=10_000)

    assert len(result) < 12_000
    assert "... (truncated" in result
    # both file headers and the small file's content survive
    assert "diff --git a/generated.json b/generated.json" in result
    assert "diff --git a/small.rs b/small.rs\n@@ -1 +1 @@\n+let x = 1;" in result


def test_scrub_diff_for_prompt_collapses_blob_lines_first() -> None:
    diff = "diff --git a/asset.svg b/asset.svg\n@@ -0,0 +1 @@\n+" + "Q" * 200_000

    result = scrub_diff_for_prompt(diff)

    # the blob-line pass alone brings the section under the per-file cap
    assert "[..omitted 195KB..]" in result
    assert len(result) < 500


def test_scrub_diff_for_prompt_keeps_binary_sections() -> None:
    diff = (
        "diff --git a/image.png b/image.png\nindex 123..456 100644\nBinary files a/image.png and b/image.png differ\n"
        "diff --git a/big.txt b/big.txt\n@@ -0,0 +1,20000 @@\n" + "\n".join(f"+row {i}" for i in range(20_000))
    )

    result = scrub_diff_for_prompt(diff, max_file_bytes=10_000)

    assert "Binary files a/image.png and b/image.png differ" in result
    assert "... (truncated" in result

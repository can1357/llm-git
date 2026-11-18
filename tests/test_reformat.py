from __future__ import annotations

import pytest
from lgit.reformat import _SYNTAX_BY_SUFFIX, reformat_only

RS = ".rs"
PY = ".py"
TS = ".ts"
GO = ".go"


@pytest.mark.parametrize(
    ("old", "new", "suffix", "formatting"),
    [
        pytest.param(
            "fn f(&mut self, a: u8, b: u8) -> X {\n}\n",
            "fn f(\n\t&mut self,\n\ta: u8,\n\tb: u8,\n) -> X {\n}\n",
            RS,
            True,
            id="exploded-signature-with-trailing-comma",
        ),
        pytest.param(
            'let p = f(\n\tb"x y",\n)\n.unwrap();\n', 'let p = f(b"x y").unwrap();\n', RS, True, id="joined-call"
        ),
        pytest.param("/// a b c\n/// d e\nfn f() {}\n", "/// a b\n/// c d e\nfn f() {}\n", RS, True, id="rewrap"),
        pytest.param(
            "def f():\n    x = foo(a, b)\n",
            "def f():\n    x = foo(\n        a,\n        b,\n    )\n",
            PY,
            True,
            id="python-reflow",
        ),
        pytest.param("const s = `${f(x, y)}`;\n", "const s = `${f(x,y)}`;\n", TS, True, id="template-code"),
        pytest.param('let s = "a b";\n', 'let s = "ab";\n', RS, False, id="string-whitespace"),
        pytest.param('let s = r#"a\n  b"#;\n', 'let s = r#"a\nb"#;\n', RS, False, id="raw-string-whitespace"),
        pytest.param("const s = `a ${x} b`;\n", "const s = `a ${x}  b`;\n", TS, False, id="template-text"),
        pytest.param("let t = (x,);\n", "let t = (x);\n", RS, False, id="one-tuple"),
        pytest.param("if x in (a,):\n    pass\n", "if x in (a):\n    pass\n", PY, False, id="keyword-one-tuple"),
        pytest.param("return x;\n", "returnx;\n", RS, False, id="joined-identifiers"),
        pytest.param("/// a b c\nfn f() {}\n", "/// a b x\nfn f() {}\n", RS, False, id="comment-word"),
        pytest.param("// a\n", "/// a\n", RS, False, id="comment-kind"),
        pytest.param("\tfoo();\n", "\t// foo();\n", RS, False, id="commented-out-code"),
        pytest.param("if a:\n    x()\n    y()\n", "if a:\n    x()\ny()\n", PY, False, id="python-dedent"),
        pytest.param("a();\nb();\n", "b();\na();\n", TS, False, id="reordered-statements"),
        pytest.param('s.replace(/a b/, "");\n', 's.replace(/a  b/, "");\n', TS, False, id="regex-literal-whitespace"),
        pytest.param(
            "}\n\nfunction f(a: string): string {\n",
            "}\n\n/**\n * Doc.\n */\nexport function g(a: string): string {\n",
            TS,
            False,
            id="doc-comment-added-to-renamed-function",
        ),
        pytest.param(
            "use b::B;\nuse parking_lot::Mutex;\nuse a::A;\n",
            "use a::A;\nuse b::B;\nuse parking_lot::Mutex;\n",
            RS,
            True,
            id="rust-use-reorder",
        ),
        pytest.param(
            "pub use b::B;\n// Windows spellings.\npub use paths::P;\n// Re-exported.\npub use a::A;\n",
            "// Re-exported.\npub use a::A;\npub use b::B;\n\n// Windows spellings.\npub use paths::P;\n",
            RS,
            True,
            id="rust-use-reorder-moves-comments",
        ),
        pytest.param(
            "#[cfg(unix)]\nuse b::B;\nuse a::A;\n",
            "use a::A;\n#[cfg(unix)]\nuse b::B;\n",
            RS,
            True,
            id="rust-use-reorder-keeps-attribute",
        ),
        pytest.param(
            "#[cfg(unix)]\nuse b::B;\nuse a::A;\n",
            "use b::B;\n#[cfg(unix)]\nuse a::A;\n",
            RS,
            False,
            id="rust-use-reorder-detaches-attribute",
        ),
        pytest.param(
            "use b::B;\nuse a::A;\n", "use a::A;\nuse b::B;\nuse c::C;\n", RS, False, id="rust-use-reorder-adds-import"
        ),
        pytest.param("mod b;\nmod a;\n", "mod a;\nmod b;\n", RS, True, id="rust-mod-declaration-reorder"),
        pytest.param(
            "#[macro_use]\nmod b;\nmod a;\n", "mod a;\n#[macro_use]\nmod b;\n", RS, False, id="rust-macro-use-order"
        ),
        pytest.param(
            "use b::B;\nfn f() {}\nuse a::A;\n", "use a::A;\nfn f() {}\nuse b::B;\n", RS, False, id="use-across-item"
        ),
        pytest.param(
            "import b from 'b';\nimport a from 'a'; // keep\n",
            "import a from 'a'; // keep\nimport b from 'b';\n",
            TS,
            True,
            id="ts-import-reorder",
        ),
        pytest.param(
            "import 'polyfill';\nimport b from 'b';\n",
            "import b from 'b';\nimport 'polyfill';\n",
            TS,
            False,
            id="ts-side-effect-import-order",
        ),
        pytest.param(
            "import b\nfrom a import (\n    x,\n    y,\n)\n",
            "from a import (\n    x,\n    y,\n)\nimport b\n",
            PY,
            True,
            id="python-import-reorder",
        ),
        pytest.param(
            'import (\n\t"b"\n\t// a\n\t"a"\n)\n',
            'import (\n\t// a\n\t"a"\n\t"b"\n)\n',
            GO,
            True,
            id="go-import-reorder",
        ),
        pytest.param(
            'let s = "a b \\\n\t\t c d";\n',
            'let s = "a b c \\\n\t\t d";\n',
            RS,
            True,
            id="rust-string-continuation-reflow",
        ),
        pytest.param(
            'let s = "a b \\\n\t\t c d";\n',
            'let s = "a b C \\\n\t\t d";\n',
            RS,
            False,
            id="rust-string-continuation-word",
        ),
        pytest.param(
            'let s = r"a \\\n  b";\n', 'let s = r"a \\\nb";\n', RS, False, id="rust-raw-string-backslash-newline"
        ),
        pytest.param(
            "import b from 'b';\nimport a from 'a';\n",
            "import a from 'a';\nimport b from 'b';\n",
            ".jsx",
            True,
            id="jsx-import-reorder",
        ),
        pytest.param(
            "import b from 'b';\nimport a from 'a';\n",
            "import a from 'a';\nimport b from 'b';\n",
            ".mjs",
            True,
            id="mjs-import-reorder",
        ),
        pytest.param(
            "import b from 'b';\nimport a from 'a';\n",
            "import a from 'a';\nimport b from 'b';\n",
            ".tsx",
            True,
            id="tsx-import-reorder",
        ),
        pytest.param(
            "import b.B;\nimport static a.A;\nimport a.C;\n",
            "import a.C;\nimport b.B;\nimport static a.A;\n",
            ".java",
            True,
            id="java-import-reorder",
        ),
        pytest.param("import b.B\nimport a.A\n", "import a.A\nimport b.B\n", ".kt", True, id="kotlin-import-reorder"),
        pytest.param(
            "import UIKit\nimport Foundation\n",
            "import Foundation\nimport UIKit\n",
            ".swift",
            True,
            id="swift-import-reorder",
        ),
        pytest.param("using B;\nusing A;\n", "using A;\nusing B;\n", ".cs", True, id="csharp-using-reorder"),
        pytest.param(
            "#include <b.h>\n#include <a.h>\n",
            "#include <a.h>\n#include <b.h>\n",
            ".cpp",
            True,
            id="cpp-include-reorder",
        ),
        pytest.param(
            "#include <b.h>\n\n#include <a.h>\n",
            "#include <a.h>\n\n#include <b.h>\n",
            ".c",
            False,
            id="c-include-across-blank-line",
        ),
        pytest.param("const f = x => g('a')\n", 'const f = (x) => g("a");\n', TS, True, id="ts-prettier-style-switch"),
        pytest.param(
            "const f = async x => g(x)\n", "const f = async (x) => g(x);\n", TS, True, id="ts-async-arrow-parens"
        ),
        pytest.param("const f = (x: A) => g(x)\n", "const f = x => g(x);\n", TS, False, id="ts-typed-arrow-parens"),
        pytest.param("a()\n;(b)\n", "a();\n(b);\n", TS, True, id="ts-leading-semicolon-guard"),
        pytest.param("a();\n(b)\n", "a()\n(b)\n", TS, False, id="ts-semicolon-before-paren"),
        pytest.param("if (a);\nfoo()\n", "if (a)\nfoo()\n", TS, False, id="ts-empty-if-body"),
        pytest.param("for (;;) {}\n", "for (;) {}\n", TS, False, id="ts-for-header-semicolons"),
        pytest.param('const s = "a\'b";\n', "const s = 'a\\'b';\n", TS, True, id="ts-escaped-quote"),
        pytest.param("const s = 'a';\n", "const s = `a`;\n", TS, False, id="ts-template-literal"),
        pytest.param(
            "const o = {\n\ta: 1 // c\n};\n",
            "const o = {\n\ta: 1, // c\n};\n",
            TS,
            True,
            id="ts-trailing-comma-before-comment",
        ),
        pytest.param("let x: A<B<C> >;\n", "let x: A<B<C>>;\n", TS, True, id="ts-generic-close"),
        pytest.param("const a = x && (\n\t<p/>\n);\n", "const a = x && <p/>;\n", ".tsx", True, id="tsx-wrapped-jsx"),
        pytest.param("render(<A/>);\n", "render<A/>;\n", ".tsx", False, id="tsx-call-parens"),
        pytest.param("const a = (<any>x).y;\n", "const a = <any>x.y;\n", TS, False, id="ts-type-assertion-parens"),
        pytest.param("x = 'a'\n", 'x = "a"\n', PY, True, id="python-quotes"),
        pytest.param("x = 'a'\n", 'x = "b"\n', PY, False, id="python-quotes-content"),
        pytest.param("x = r'\\d'\n", 'x = r"\\d"\n', PY, True, id="python-raw-quotes"),
        pytest.param("x = f'{a[\"k\"]}'\n", "x = f\"{a['k']}\"\n", PY, True, id="python-fstring-nested-quotes"),
        pytest.param("x = (\n    a + b\n)\ny = 1\n", "x = a + b\ny = 1\n", PY, True, id="python-redundant-parens"),
        pytest.param("x = (a or b) and c\n", "x = a or b and c\n", PY, False, id="python-precedence-parens"),
        pytest.param("if x in (a or b):\n    pass\n", "if x in a or b:\n    pass\n", PY, False, id="python-in-parens"),
        pytest.param(
            "from m import (\n    a,\n    b,\n)\n", "from m import a, b\n", PY, True, id="python-import-parens"
        ),
        pytest.param(
            "with (\n    a(),\n    b(),\n):\n    pass\n",
            "with a(), b():\n    pass\n",
            PY,
            True,
            id="python-with-parens",
        ),
        pytest.param("let x: Vec<Vec<u8> > = v;\n", "let x: Vec<Vec<u8>> = v;\n", RS, True, id="rust-generic-close"),
        pytest.param(
            'import (\n\t"b"\n\tx "a"\n)\n', 'import (\n\tx "a"\n\t"b"\n)\n', GO, True, id="go-import-alias-reorder"
        ),
    ],
)
def test_reformat_only_applies_formatting_and_nothing_else(old: str, new: str, suffix: str, formatting: bool) -> None:
    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[suffix]) == (new if formatting else old)


@pytest.mark.parametrize(
    ("suffix", "old", "new", "accepted"),
    [
        pytest.param(
            ".tsx",
            "const url = <Link>\n  https://example.test\n  docs\n</Link>;\n",
            "const url = <Link>https://example.test docs</Link>;\n",
            True,
            id="jsx-text-with-comment-like-characters",
        ),
        pytest.param(
            ".jsx",
            "const a = <p>Hello{' '}world</p>;\n",
            "const a = <p>Hello world</p>;\n",
            True,
            id="jsx-explicit-space",
        ),
        pytest.param(
            ".jsx",
            "const a = <Widget>Hello{' '}world</Widget>;\n",
            "const a = <Widget>Hello world</Widget>;\n",
            False,
            id="jsx-component-child-shape",
        ),
        pytest.param(
            ".tsx",
            "const a = <p>hello <b>world</b></p>;\n",
            "const a = <p>hello<b>world</b></p>;\n",
            False,
            id="jsx-significant-space",
        ),
        pytest.param(PY, 'x = ("foo" r"\\d")\n', 'x = "foo\\\\d"\n', True, id="python-concatenated-string"),
        pytest.param(PY, 'x = (b"foo" b"bar")\n', 'x = "foobar"\n', False, id="python-concatenation-bytes"),
        pytest.param(
            PY, 'x = ("foo"  # keep\n     "bar")\n', 'x = "foobar"\n', False, id="python-concatenation-comment"
        ),
        pytest.param(
            PY,
            "value: (\n    list[int]  # items\n)\n",
            "value: list[int]  # items\n",
            True,
            id="python-annotation-parens",
        ),
        pytest.param(
            RS,
            "fn f() { match x {\n    0 => {\n        omp_xfs::walk::Action::Continue\n    },\n    _ => 0,\n} }\n",
            "fn f() { match x {\n    0 => omp_xfs::walk::Action::Continue,\n    _ => 0,\n} }\n",
            True,
            id="rust-match-arm-braces",
        ),
        pytest.param(
            RS,
            "fn f() { match x { _ => { let value = 1; value } } }\n",
            "fn f() { match x { _ => value } }\n",
            False,
            id="rust-match-arm-drops-statement",
        ),
        pytest.param(
            GO,
            "package p\nvar x=0Xff+2E3+0XfP2\nfunc f(){ if (x<1) {_ = x}; x++; }\n",
            "package p\n\nvar x = 0xff + 2e3 + 0xfp2\n\nfunc f() {\n\tif x < 1 {\n\t\t_ = x\n\t}\n\tx++\n}\n",
            True,
            id="gofmt-literals-semicolons-and-condition",
        ),
        pytest.param(
            GO,
            "package p\nfunc f(){ x = (1+2)*3 }\n",
            "package p\nfunc f(){ x = 1+2*3 }\n",
            False,
            id="go-expression-precedence",
        ),
        pytest.param(
            RS,
            "use a::{z as alias, b::{y, x}, c};\n",
            "use a::{b::{x, y}, c, z as alias};\n",
            True,
            id="rust-nested-import-lists",
        ),
        pytest.param(
            TS,
            'import {z as alias, type B} from "m";\n',
            'import {type B, z as alias} from "m";\n',
            True,
            id="ts-named-import-list",
        ),
        pytest.param(
            TS,
            'import {z as alias, type B} from "m";\n',
            'import {type B, z as different} from "m";\n',
            False,
            id="ts-import-alias-changed",
        ),
        pytest.param(
            PY,
            "from m import z as alias, b\n",
            "from m import b, z as alias\n",
            True,
            id="python-named-import-list",
        ),
        pytest.param(
            ".js",
            "#!/usr/bin/env node\n;(function () {})();\n",
            "#!/usr/bin/env node\n(function () {})();\n",
            True,
            id="js-leading-empty-statement-after-shebang",
        ),
        pytest.param(
            ".js",
            "if (ok);\n(function () {})();\n",
            "if (ok)\n(function () {})();\n",
            False,
            id="js-non-standalone-empty-statement",
        ),
        pytest.param(
            ".js",
            ';"use strict";\nf();\n',
            '"use strict";\nf();\n',
            False,
            id="js-directive-prologue",
        ),
        pytest.param(
            PY,
            "from m import a as x, b as x\n",
            "from m import b as x, a as x\n",
            False,
            id="python-import-binding-order",
        ),
    ],
)
def test_syntax_sensitive_formatting(suffix: str, old: str, new: str, accepted: bool) -> None:
    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[suffix]) == (new if accepted else old)


def test_ast_formatting_keeps_unrelated_edits_pending() -> None:
    old = "value: (\n    int\n)\n\ndef f():\n    return 1\n"
    new = "value: int\n\ndef f():\n    return 2\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[PY]) == "value: int\n\ndef f():\n    return 1\n"


def test_reformat_only_takes_formatting_lines_out_of_a_mixed_region() -> None:
    old = "x(1,2);\nlet a = 1;\ny( 3 );\n"
    new = "x(1, 2);\nlet a = 2;\ny(3);\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[TS]) == "x(1, 2);\nlet a = 1;\ny(3);\n"


def test_reformat_only_leaves_indentation_that_follows_a_content_change() -> None:
    old = "let t = self\n\t.times\n\t.iter()\n\t.any(f);\n"
    new = "let t = self.times.iter().any(|x| {\n\tg(self\n\t\t.times\n\t\t.iter())\n});\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[RS]) == old


def test_fmt_commits_formatting_and_keeps_other_changes_pending(repo, run_git, run_lgit) -> None:
    (repo / "lib.rs").write_text("fn f(a: u8, b: u8) -> u8 {\n\ta + b\n}\n", encoding="utf-8")
    run_git(repo, "add", "lib.rs")
    run_git(repo, "commit", "-m", "feat: lib")
    worktree = "fn f(\n\ta: u8,\n\tb: u8,\n) -> u8 {\n\ta * b\n}\n"
    (repo / "lib.rs").write_text(worktree, encoding="utf-8")

    result = run_lgit(repo, "--fmt")

    assert result.returncode == 0, result.stderr
    assert run_git(repo, "log", "-1", "--format=%s").stdout.strip() == "style: reformatted lib.rs"
    assert run_git(repo, "show", "HEAD:lib.rs").stdout == "fn f(\n\ta: u8,\n\tb: u8,\n) -> u8 {\n\ta + b\n}\n"
    assert (repo / "lib.rs").read_text(encoding="utf-8") == worktree
    assert run_git(repo, "diff", "--cached", "--name-only").stdout == ""
    assert run_git(repo, "diff", "-U0", "--", "lib.rs").stdout.splitlines()[-2:] == ["-\ta + b", "+\ta * b"]


def test_fmt_reads_staged_files_from_the_index_and_skips_partially_staged_ones(repo, run_git, run_lgit) -> None:
    for name in ("a.rs", "b.rs", "c.rs"):
        (repo / name).write_text("fn f(a: u8, b: u8) {}\n", encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "feat: files")
    exploded = "fn f(\n\ta: u8,\n\tb: u8,\n) {}\n"
    (repo / "a.rs").write_text(exploded, encoding="utf-8")
    (repo / "b.rs").write_text("fn g(a: u8, b: u8) {}\n", encoding="utf-8")
    (repo / "c.rs").write_text(exploded, encoding="utf-8")
    run_git(repo, "add", "a.rs", "b.rs", "c.rs")
    (repo / "c.rs").write_text(exploded + "fn h() {}\n", encoding="utf-8")

    result = run_lgit(repo, "--fmt")

    assert result.returncode == 0, result.stderr
    assert run_git(repo, "show", "--name-only", "--format=", "HEAD").stdout.split() == ["a.rs"]
    assert run_git(repo, "diff", "--cached", "--name-only").stdout.split() == ["b.rs", "c.rs"]
    assert "Skipped 1 files with both staged and unstaged changes" in result.stderr + result.stdout


def test_reformat_only_reorders_imports_without_taking_other_changes() -> None:
    old = "use b::B;\nuse a::A;\n\nfn f() -> u8 {\n\t1\n}\n"
    new = "use a::A;\nuse b::B;\n\nfn f() -> u8 {\n\t2\n}\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[RS]) == "use a::A;\nuse b::B;\n\nfn f() -> u8 {\n\t1\n}\n"

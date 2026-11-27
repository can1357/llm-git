from __future__ import annotations

import pytest
from lgit.reformat import _SYNTAX_BY_SUFFIX, plan_formatting, reformat_only

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
            TS,
            'interface Result {\n  content: (\n    | { type: "text"; text: string }\n'
            '    | { type: "image"; data: string; mimeType: string }\n  )[];\n}\n',
            'interface Result {\n  content: ({ type: "text"; text: string } | '
            '{ type: "image"; data: string; mimeType: string })[];\n}\n',
            True,
            id="ts-leading-union-in-array",
        ),
        pytest.param(".tsx", "type Props = & A & B;\n", "type Props = A & B;\n", True, id="tsx-leading-intersection"),
        pytest.param(TS, "type T = | A | B;\n", "type T = A | C;\n", False, id="ts-union-member-change"),
        pytest.param(TS, "type T = | A | B;\n", "type T = A & B;\n", False, id="ts-union-to-intersection"),
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


def test_ts_extracts_leading_union_layout_without_taking_a_type_change() -> None:
    old = "interface Result {\n  content: (\n    | Text\n    | Image\n  )[];\n  retries: number;\n}\n"
    new = "interface Result {\n  content: (Text | Image)[];\n  retries: string;\n}\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[TS]) == new.replace("retries: string", "retries: number")


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


@pytest.mark.parametrize(
    ("suffix", "old", "new", "expected"),
    [
        pytest.param(
            ".yaml",
            "jobs:\n    build:\n        runs-on: linux\n        steps: [one,two] # setup\n",
            "jobs:\n  build:\n    runs-on: linux\n    steps: [one, two] # setup\n",
            None,
            id="yaml-reindent-flow",
        ),
        pytest.param(
            ".yml",
            "message: 'say \"hello\"'\nscript: |\n    echo hello\n",
            'message: "say \\"hello\\""\nscript: |\n  echo hello\n',
            None,
            id="yml-quotes-and-block-indent",
        ),
        pytest.param(".yaml", "a:\n  b: 1\nc: 2\n", "a:\n  b: 1\n  c: 2\n", False, id="yaml-nesting"),
        pytest.param(".yaml", "a:b\n", "a: b\n", False, id="yaml-scalar-to-mapping"),
        pytest.param(".yaml", "a: hello#world\n", "a: hello #world\n", False, id="yaml-comment-boundary"),
        pytest.param(".yaml", "a: x # old\n", "a: x # new\n", False, id="yaml-comment-edit"),
        pytest.param(".yaml", 'a: "a # b" # old\n', 'a: "a # b" # new\n', False, id="yaml-comment-after-quoted-hash"),
        pytest.param(".yaml", "a: |\n  x  y\n", "a: |\n  x y\n", False, id="yaml-block-value"),
        pytest.param(".yaml", "value: true\n", 'value: "true"\n', False, id="yaml-scalar-type"),
        pytest.param(
            ".json",
            r'{"a":"\/","b":[1,2]}' + "\n",
            '{\n  "a": "/",\n  "b": [1, 2]\n}\n',
            None,
            id="json-layout-and-escape",
        ),
        pytest.param(".json", '{"a": 1}\n', '{"a": 1,}\n', False, id="json-invalid-trailing-comma"),
        pytest.param(".json", '{"a": 1, "a": 2}\n', '{"a": 2}\n', False, id="json-duplicate-member"),
        pytest.param(
            ".md",
            "Title\n=====\n\nThis is a long paragraph\nwith _emphasis_.\n\n*  First\n*  Second\n",
            "# Title\n\nThis is a long paragraph with *emphasis*.\n\n- First\n- Second\n",
            None,
            id="markdown-reflow-and-markers",
        ),
        pytest.param(
            ".md", "|A|B|\n|-|:-:|\n|x|y|\n", "| A | B |\n| --- | :---: |\n| x | y |\n", None, id="markdown-table"
        ),
        pytest.param(".md", "line  \nnext\n", "line\nnext\n", False, id="markdown-hard-break"),
        pytest.param(".md", "- a\n- b\n", "- a\n  - b\n", False, id="markdown-nesting"),
        pytest.param(".md", "- [ ] task\n", "- [  ] task\n", False, id="markdown-task-marker"),
        pytest.param(".md", "> [!NOTE]\n> text\n", "> [!NOTE] text\n", False, id="markdown-alert"),
        pytest.param(".md", "`a  b`\n", "`a b`\n", False, id="markdown-inline-code"),
        pytest.param(".md", "[ref]: /old\n", "[ref]: /new\n", False, id="markdown-unused-reference"),
        pytest.param(
            ".markdown",
            "> ```python\n> def f():\n>   print('hello')\n> ```\n",
            '> ~~~python\n> def f():\n>     print("hello")\n> ~~~\n',
            None,
            id="markdown-nested-fence",
        ),
        pytest.param(
            ".md",
            "#  Sample\n\n```ts\nconst a=x=>x+1\nconst b=2;\n```\n",
            "# Sample\n\n```ts\nconst a = (x) => x + 1;\nconst b = 3;\n```\n",
            "# Sample\n\n```ts\nconst a = (x) => x + 1;\nconst b=2;\n```\n",
            id="markdown-mixed-fence-and-prose",
        ),
        pytest.param(".md", "```text\nx  y\n```\n", "```text\nx y\n```\n", False, id="markdown-opaque-fence"),
        pytest.param(
            ".md",
            "-   ```python\n    x=1\n    ```\n",
            "- ```python\n  x = 1\n  ```\n",
            None,
            id="markdown-list-fence-reindent",
        ),
        pytest.param(
            ".md",
            "#  Example\n\n```python\nif ok:\n    go()\nstop()\n```\n",
            "# Example\n\n```python\nif ok:\n    go()\n    stop()\n```\n",
            "# Example\n\n```python\nif ok:\n    go()\nstop()\n```\n",
            id="markdown-keeps-code-nesting-edit-pending",
        ),
        pytest.param(
            ".css",
            "@media (width:500px){.a>.b,.c{color:rgb(1,2,3)}}\n",
            "@media (width: 500px) {\n  .a > .b, .c {\n    color: rgb(1, 2, 3);\n  }\n}\n",
            None,
            id="css-rule-and-declaration-layout",
        ),
        pytest.param(".css", ".a.b{color:red}\n", ".a .b{color:red}\n", False, id="css-descendant-selector"),
        pytest.param(".css", ".a{width:calc(1px + 2px)}\n", ".a{width:calc(1px+2px)}\n", False, id="css-calc-gap"),
        pytest.param(".css", ".a {--x: a b;}\n", ".a {--x: ab;}\n", False, id="css-custom-property"),
        pytest.param(".css", ".a {color:red\n", ".a {color: red;}\n", False, id="css-unclosed-block"),
        pytest.param(".css", '@charset "iso-8859-1";\n', ' @charset "iso-8859-1";\n', False, id="css-encoding-prefix"),
        pytest.param(
            ".kdl",
            'layout { pane size="60%"; pane size="40%"; }\n',
            'layout {\n    pane size="60%"\n    pane size="40%"\n}\n',
            None,
            id="kdl-block-layout-and-semicolons",
        ),
        pytest.param(".kdl", "node  value=#true\n", "node value=#true\n", None, id="kdl-v2-property-spacing"),
        pytest.param(".kdl", "node 1\nnode 2\n", "node 1 2\n", False, id="kdl-node-boundary"),
        pytest.param(".kdl", "/- node 1\n", "node 1\n", False, id="kdl-slashdash"),
        pytest.param(".kdl", 'node #"a  b"#\n', 'node #"a b"#\n', False, id="kdl-raw-string"),
        pytest.param(
            ".html",
            "<div id = 'a' class=\"layout\"><p>One</p><p>Two</p></div>\n",
            '<div id="a" class="layout">\n  <p>One</p>\n  <p>Two</p>\n</div>\n',
            None,
            id="html-attributes-and-block-layout",
        ),
        pytest.param(
            ".html",
            '<html><head><meta charset="utf-8"><title>Title</title></head><body><p>Hello</p></body></html>\n',
            '<html>\n  <head>\n    <meta charset="utf-8" />\n    <title>Title</title>\n  </head>\n'
            "  <body>\n    <p>Hello</p>\n  </body>\n</html>\n",
            None,
            id="html-document-layout-and-void-tag",
        ),
        pytest.param(
            ".html",
            "<input  disabled/>\n",
            "<input disabled />\n",
            None,
            id="html-boolean-attribute-before-slash",
        ),
        pytest.param(
            ".html",
            "<input disabled />\n",
            "<input />\n",
            False,
            id="html-boolean-attribute-removal",
        ),
        pytest.param(
            ".html",
            "<p><em>one</em> <em>two</em></p>\n",
            "<p><em>one</em><em>two</em></p>\n",
            False,
            id="html-inline-space",
        ),
        pytest.param(".html", "<pre>  padded\n</pre>\n", "<pre> padded\n</pre>\n", False, id="html-preformatted-text"),
        pytest.param(
            ".html",
            '<div style="display:inline">one</div> <div>two</div>\n',
            '<div style="display:inline">one</div><div>two</div>\n',
            False,
            id="html-styled-block-can-act-inline",
        ),
        pytest.param(
            ".html",
            "<style>p {color: red}</style>\n",
            "<style>p {color: blue}</style>\n",
            False,
            id="html-style-body",
        ),
        pytest.param(
            ".html",
            '<div x="1"y="2"></div>\n',
            '<div x="1" y="2"></div>\n',
            False,
            id="html-adjacent-attributes",
        ),
    ],
)
def test_document_formatting(suffix: str, old: str, new: str, expected: str | bool | None) -> None:
    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[suffix]) == (
        new if expected is None else old if expected is False else expected
    )


def test_fmt_splits_document_formatting_from_content_in_each_file(repo, run_git, run_lgit) -> None:
    files = {
        "settings.yml": ("a:  1\nb: 2\n", "a: 1\nb: 2\n", "a: 1\nb: 3\n"),
        "data.json": (
            '{\n  "a":[1,2],\n  "b": 2\n}\n',
            '{\n  "a": [1, 2],\n  "b": 2\n}\n',
            '{\n  "a": [1, 2],\n  "b": 3\n}\n',
        ),
        "report.sarif": (
            '{\n  "version":"2.1.0",\n  "runs": [{"tool": {"driver": {"name": "Example"}}, "results": []}]\n}\n',
            '{\n  "version": "2.1.0",\n  "runs": [{"tool": {"driver": {"name": "Example"}}, "results": []}]\n}\n',
            '{\n  "version": "2.1.0",\n  "runs": [{"tool": {"driver": {"name": "Other"}}, "results": []}]\n}\n',
        ),
        "guide.md": ("#  Guide\n\nBefore.\n", "# Guide\n\nBefore.\n", "# Guide\n\nAfter.\n"),
        "example.sublime-syntax": (
            "%YAML 1.2\n---\nname:  Example\ncontexts:\n    main:\n        - match: '\\d+'\n          scope: constant.numeric\n",
            "%YAML 1.2\n---\nname: Example\ncontexts:\n    main:\n        - match: '\\d+'\n          scope: constant.numeric\n",
            "%YAML 1.2\n---\nname: Example\ncontexts:\n    main:\n        - match: '\\w+'\n          scope: constant.numeric\n",
        ),
        "layout.kdl": (
            'layout {\n    pane   size="60%"\n    pane size="40%"\n}\n',
            'layout {\n    pane size="60%"\n    pane size="40%"\n}\n',
            'layout {\n    pane size="60%"\n    pane size="50%"\n}\n',
        ),
        "index.html": (
            "<main  id='app' hidden>\n  <p>Before.</p>\n</main>\n",
            '<main id="app" hidden>\n  <p>Before.</p>\n</main>\n',
            '<main id="app" hidden>\n  <p>After.</p>\n</main>\n',
        ),
        "main.css": (
            ".box {\n  color:red;\n  margin: 1px;\n}\n",
            ".box {\n  color: red;\n  margin: 1px;\n}\n",
            ".box {\n  color: red;\n  margin: 2px;\n}\n",
        ),
    }
    for name, (old, _, _) in files.items():
        (repo / name).write_text(old, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "feat: added examples")
    for name, (_, _, new) in files.items():
        (repo / name).write_text(new, encoding="utf-8")
    run_git(repo, "add", "data.json", "example.sublime-syntax", "guide.md")

    result = run_lgit(repo, "--fmt")

    assert result.returncode == 0, result.stderr
    assert run_git(repo, "log", "-1", "--format=%s").stdout.strip() == "style: reformatted 8 files"
    for name, (_, formatted, new) in files.items():
        assert run_git(repo, "show", f"HEAD:{name}").stdout == formatted
        assert (repo / name).read_text(encoding="utf-8") == new
    assert run_git(repo, "diff", "--cached", "--name-only").stdout.splitlines() == [
        "data.json",
        "example.sublime-syntax",
        "guide.md",
    ]
    assert run_git(repo, "diff", "--name-only").stdout.splitlines() == [
        "index.html",
        "layout.kdl",
        "main.css",
        "report.sarif",
        "settings.yml",
    ]


def test_fmt_recognizes_repo_filenames(repo, run_git) -> None:
    files = [
        (".clang-format", "BasedOnStyle:  LLVM\n", "BasedOnStyle: LLVM\n"),
        (".clang-tidy", "Checks:  '-*,modernize-*'\n", "Checks: '-*,modernize-*'\n"),
        (".yamllint", "rules: [one,two]\n", "rules: [one, two]\n"),
        (".editorconfig", "root=true\n", "root = true\n"),
        (".gitconfig", "[core]\nfilemode=true\n", "[core]\n  filemode = true\n"),
        (".gitignore", "dist/  \n", "dist/\n"),
        (".gitattributes", "*.txt  text eol=lf\n", "*.txt text eol=lf\n"),
        (".env.local", "export  DEBUG=true\n", "export DEBUG=true\n"),
        ("Pipfile", '[packages]\nrequests="*"\n', '[packages]\nrequests = "*"\n'),
        ("Cargo.lock", '[[package]]\nname="demo"\n', '[[package]]\nname = "demo"\n'),
        ("uv.lock", 'requires-python=">=3.14"\n', 'requires-python = ">=3.14"\n'),
        ("Pipfile.lock", '{"default":{}}\n', '{\n  "default": {}\n}\n'),
        ("flake.lock", '{"version":7}\n', '{\n  "version": 7\n}\n'),
        ("composer.lock", '{"packages":[]}\n', '{\n  "packages": []\n}\n'),
        ("site.webmanifest", '{"name":"App"}\n', '{\n  "name": "App"\n}\n'),
        ("area.geojson", '{"type":"Point","coordinates":[1,2]}\n', '{"type": "Point", "coordinates": [1, 2]}\n'),
        ("scene.gltf", '{"asset":{"version":"2.0"}}\n', '{"asset": {"version": "2.0"}}\n'),
        ("notes.ipynb", '{"cells":[],"nbformat":4}\n', '{"cells": [], "nbformat": 4}\n'),
        ("event.avsc", '{"type":"string"}\n', '{"type": "string"}\n'),
        ("project.code-workspace", '{\n// Worktrees\n"folders":[],\n}\n', '{\n  // Worktrees\n  "folders": []\n}\n'),
        (
            ".vscode/settings.json",
            '{\n"editor.tabSize":2, // Default\n}\n',
            '{\n  "editor.tabSize": 2, // Default\n}\n',
        ),
        ("User.sublime-settings", '{\n"font_size":12,\n}\n', '{\n  "font_size": 12,\n}\n'),
        ("User.sublime-keymap", '[{"keys":["ctrl+x"]},]\n', '[{"keys": ["ctrl+x"]}]\n'),
        ("reports.ndjson", '{"one":1}\n{"two":2}\n', '{"one": 1}\n{"two": 2}\n'),
        ("Dockerfile", "from   alpine\n", "FROM alpine\n"),
        ("ops/Containerfile.dev", "FROM   alpine\n", "FROM alpine\n"),
        ("Makefile", "all : a   b\n\t@echo build\n", "all: a b\n\t@echo build\n"),
        ("CMakeLists.txt", 'set ( NAME  "a b" )\n', 'set(NAME\n  "a b"\n)\n'),
        ("BUILD.bazel", 'filegroup( name="demo" )\n', 'filegroup(name = "demo")\n'),
        (
            "scripts/report",
            "#!/usr/bin/env -S bash -e\nif ok; then\n echo yes\nfi\n",
            "#!/usr/bin/env -S bash -e\nif ok; then\n  echo yes\nfi\n",
        ),
    ]
    for path, before, _ in files:
        file = repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(before, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "add fixtures")
    for path, _, after in files:
        (repo / path).write_text(after, encoding="utf-8")

    plan = plan_formatting(repo)
    assert {item.path: item.contents for item in plan.files} == {path: after.encode() for path, _, after in files}


def test_fmt_path_overrides_and_explanations(repo, run_git, run_lgit) -> None:
    (repo / ".gitattributes").write_text(
        "settings/*.app lgit-format=jsonc\nopaque.json -lgit-format\nbad.conf lgit-format=not-a-parser\n",
        encoding="utf-8",
    )
    (repo / "settings").mkdir()
    fixtures = {
        "opaque.json": ('{"x":1}\n', '{"x": 1}\n'),
        "unknown.lock": ('{"x":1}\n', '{"x": 1}\n'),
        "bad.conf": ("a  b\n", "a b\n"),
        "values.json": ('{"x":1}\n', '{"x": 2}\n'),
        "settings/example.app": ('{\n// x\n"x":1,\n}\n', '{\n  // x\n  "x": 1,\n}\n'),
    }
    for path, (before, _) in fixtures.items():
        (repo / path).write_text(before, encoding="utf-8")
    run_git(repo, "add", ".")
    run_git(repo, "commit", "-m", "add fixtures")
    for path, (_, after) in fixtures.items():
        (repo / path).write_text(after, encoding="utf-8")
    (repo / "new.json").write_text("{}\n", encoding="utf-8")
    run_git(repo, "add", "new.json")
    (repo / "pending.json").write_text('{"x":1}\n', encoding="utf-8")
    run_git(repo, "add", "pending.json")
    (repo / "pending.json").write_text('{"x": 1}\n', encoding="utf-8")
    (repo / "untracked").write_text("new\n", encoding="utf-8")

    result = run_lgit(repo, "--fmt", "--dry-run", "--fmt-explain")

    assert result.returncode == 0, result.stderr
    for line in (
        "opaque.json: disabled by lgit-format",
        "bad.conf: unknown lgit-format 'not-a-parser'",
        "unknown.lock: unknown file format",
        "values.json: no formatting-only edits proved",
        "new.json: added file",
        "pending.json: both staged and unstaged changes",
        "untracked: untracked file",
    ):
        assert line in result.stdout
    assert "  settings/example.app" in result.stdout
    assert run_git(repo, "diff", "--name-only").stdout.splitlines() == sorted([*fixtures, "pending.json"])


@pytest.mark.parametrize(
    ("suffix", "before", "after"),
    [
        (".jsonl", '{"x":1}\n{"y":2}\n', '{"x":1,"y":2}\n'),
        (".ndjson", '{"x":1}\n', '{\n"x":1\n}\n'),
        (".jsonc", '{\n"x":1, // before\n}\n', '{\n"x": 1, // after\n}\n'),
        (".jsonc", '{"x":[]}\n', '{"x":[,]}\n'),
        (".toml", "x = '''a  b'''\n", "x = '''a b'''\n"),
    ],
)
def test_format_preserves_record_boundaries_comments_and_literals(suffix: str, before: str, after: str) -> None:
    assert reformat_only(before, after, _SYNTAX_BY_SUFFIX[suffix]) == before


def test_reformat_only_reorders_imports_without_taking_other_changes() -> None:
    old = "use b::B;\nuse a::A;\n\nfn f() -> u8 {\n\t1\n}\n"
    new = "use a::A;\nuse b::B;\n\nfn f() -> u8 {\n\t2\n}\n"

    assert reformat_only(old, new, _SYNTAX_BY_SUFFIX[RS]) == "use a::A;\nuse b::B;\n\nfn f() -> u8 {\n\t1\n}\n"

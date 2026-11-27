"""Infrastructure syntax compares formatting without losing literal data or ordering."""

import pytest
from lgit.format_infra import infra_document
from lgit.reformat import _SYNTAX_BY_SUFFIX, reformat_only


def _tokens(text: str, language: str) -> tuple[tuple[object, ...], ...]:
    document = infra_document(text, language)
    assert document is not None
    return document.tokens


@pytest.mark.parametrize(
    ("language", "old", "formatted", "edited"),
    [
        (
            "hcl",
            'resource "test" "one"{labels={a=" one ",b="two"}}\n',
            'resource "test" "one" {\n  labels = {\n    a = " one "\n    b = "two"\n  }\n}\n',
            'resource "test" "one"{labels={a="one",b="two"}}\n',
        ),
        (
            "nix",
            '{ name, extra ? " x " }: { inherit name; files=[ ./local extra ]; }',
            '{\n  name,\n  extra ? " x ",\n}: {\n  inherit name;\n  files = [ ./local extra ];\n}\n',
            '{ name, extra ? " x " }: { inherit name; files=[ extra ./local ]; }',
        ),
        (
            "proto",
            'syntax="proto3";message Item{string name=1;bytes data=2;}\n',
            'syntax = "proto3";\nmessage Item {\n  string name = 1;\n  bytes data = 2;\n}\n',
            'syntax="proto3";message Item{string name=3;bytes data=2;}\n',
        ),
        (
            "textproto",
            'details{path:"a b" flags:[GO,STOP]}\n[project.more]:<id:0x12> # comment\n',
            'details {\n  path: "a b"\n  flags: [GO, STOP]\n}\n[project.more]: <\n  id: 0x12\n> # comment\n',
            'details{path:"a  b" flags:[GO,STOP]}\n[project.more]:<id:0x12> # comment\n',
        ),
        (
            "graphql",
            'query Get($id:ID!){item(id:$id,tag:"a b"){id,name}}\n',
            'query Get($id: ID!) {\n  item(id: $id tag: "a b") {\n    id\n    name\n  }\n}\n',
            'query Get($id:ID!){item(id:$id,tag:"a  b"){id,name}}\n',
        ),
        (
            "sql",
            "select id,name from events where id=42 and name='a b';\n",
            "SELECT id, name\nFROM events\nWHERE id = 42 AND name = 'a b';\n",
            "select id,name from events where id=42 and name='a  b';\n",
        ),
        (
            "ini",
            "[database]\nhost=db # local\nport=123\n",
            "\n[database]\n  host = db # local\n  port = 123\n",
            "[database]\nhost=db# local\nport=123\n",
        ),
        (
            "editorconfig",
            "root=true\n[*.py]\nindent_style=space\nindent_size=4\n",
            "root = true\n\n[*.py]\n  indent_style = space\n  indent_size = 4\n",
            "root=true\n[*.py ]\nindent_style=space\nindent_size=4\n",
        ),
        (
            "gitconfig",
            '[includeIf "gitdir:~/Work/"]\npath="other config"\n[core]\nfilemode\n',
            '[includeIf   "gitdir:~/Work/"]\n\tpath = "other config"\n\n[core]\n\tfilemode\n',
            '[includeIf "gitdir:~/work/"]\npath="other config"\n[core]\nfilemode\n',
        ),
        (
            "gitignore",
            "# Build output\nbuild/  \n!build/README\\  \n",
            "\n# Build output\nbuild/\n\n!build/README\\ \n",
            "# Build output\nbuild/  \n!build/README\n",
        ),
        (
            "gitattributes",
            '*.txt text eol=lf\n"a b.txt" -text\n',
            '  *.txt\ttext   eol=lf\n\n"a b.txt"\t-text  \n',
            '"a b.txt" -text\n*.txt text eol=lf\n',
        ),
        (
            "dotenv",
            'export TOKEN=" a b "\nMODE= a  b \n',
            '\n  export  TOKEN =" a b "\nMODE = a  b \n',
            'export TOKEN=" a b "\nMODE=a  b \n',
        ),
        (
            "properties",
            "key\\ with\\ spaces=literal  \npath=first\\\n  second\n",
            "\n  key\\ with\\ spaces : literal  \npath=first\\\n\tsecond\n",
            "key\\ with\\ spaces=literal \npath=first\\\n  second\n",
        ),
    ],
)
def test_infra_formats_keep_meaning(language: str, old: str, formatted: str, edited: str) -> None:
    assert _tokens(old, language) == _tokens(formatted, language)
    assert _tokens(old, language) != _tokens(edited, language)


@pytest.mark.parametrize(
    ("language", "old", "edited"),
    [
        ("hcl", "x = <<EOF\n  payload\nEOF\n", "x = <<EOF\npayload\nEOF\n"),
        ("nix", "a/b", "a / b"),
        ("nix", "[ 1 foo ]", "[ 1foo ]"),
        ("proto", 'option name = "left " "right";\n', 'option name = "left" "right";\n'),
        ("graphql", '{ item(tag: """first\n second""") }', '{ item(tag: """first\nsecond""") }'),
        ("sql", "SELECT 'left'\n'right';\n", "SELECT 'left' 'right';\n"),
        ("sql", "SELECT COUNT(id) FROM t;\n", "SELECT COUNT (id) FROM t;\n"),
        ("sql", "SELECT /*+ INDEX(t fast) */ id FROM t;\n", "SELECT /*+ INDEX(t slow) */ id FROM t;\n"),
        ("sql", "SELECT a - -2 FROM t;\n", "SELECT a --2 FROM t;\n"),
        ("ini", "[section]\nflag=# value\n", "[section]\nflag= # value\n"),
        ("gitconfig", '[core]\nkey="a\\tb"\n', '[core]\nkey="a b"\n'),
        ("gitconfig", "[core]\nkey=left\\\n  right\n", "[core]\nkey=left\\\n right\n"),
        ("gitignore", "\\#pattern\n", "#pattern\n"),
        ("gitignore", " pattern\n", "pattern\n"),
        ("gitignore", "pattern\t\n", "pattern\n"),
        ("gitattributes", "*.txt text -text\n", "*.txt -text text\n"),
        ("dotenv", 'TOKEN="first\n  second"\n', 'TOKEN="first\n second"\n'),
        ("properties", "k\N{NO-BREAK SPACE}ey=text\n", "k=\N{NO-BREAK SPACE}ey=text\n"),
    ],
)
def test_infra_preserves_sensitive_source(language: str, old: str, edited: str) -> None:
    assert _tokens(old, language) != _tokens(edited, language)


@pytest.mark.parametrize(
    ("language", "text"),
    [
        ("hcl", "first=1 second=2"),
        ("hcl", "value = {first=1 second=2}"),
        ("textproto", "foo: [1 2]"),
        ("textproto", "foo: [1, {bar: 2}]"),
        ("textproto", "foo: {bar: 2 >"),
        ("textproto", r'foo: "\q"'),
        ("ini", "[section]\nfirst=1\n  second=2\n"),
        ("gitconfig", "[core]\nkey=left\\q\n"),
        ("dotenv", 'TOKEN="unterminated\n'),
    ],
)
def test_infra_declines_ambiguous_or_malformed_input(language: str, text: str) -> None:
    assert infra_document(text, language) is None


def test_gitconfig_extracts_layout_beside_a_value_change() -> None:
    before = "[core]\n\tfilemode=true\n\tname=old\n"
    after = "[core]\n\tfilemode = true\n\tname=new\n"

    assert reformat_only(before, after, _SYNTAX_BY_SUFFIX[".gitconfig"]) == "[core]\n\tfilemode = true\n\tname=old\n"

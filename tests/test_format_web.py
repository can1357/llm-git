from pathlib import Path

import pytest
from lgit.format_web import web_document
from lgit.reformat import _SYNTAX_BY_SUFFIX, reformat_only


def _same(format_name: str, old: str, new: str) -> bool:
    before, after = web_document(old, format_name), web_document(new, format_name)
    assert before is not None and after is not None
    return before.tokens == after.tokens


@pytest.mark.parametrize(
    ("format_name", "old", "formatted", "semantic"),
    [
        (
            "xml",
            '<root xmlns:p="urn:p" p:a = "x" >a <p:b n="1" /> b</root >',
            "<root xmlns:p='urn:p'\n  p:a=\"x\">a <p:b n = '1'/> b</root>",
            '<root xmlns:p="urn:p" p:a="x">a<p:b n="1"/> b</root>',
        ),
        (
            "scss",
            ".a>.b{color:$x;\n  margin: 2px;}",
            ".a > .b {\n  color: $x;\n  margin:2px;\n}",
            ".a .b {\n  color: $x;\n  margin:2px;\n}",
        ),
        (
            "sass",
            "$primary:red\n.card\n  color:    $primary\n",
            "$primary: red\n.card\n  color: $primary\n",
            "$primary: red\n.card\n    color: $primary\n",
        ),
        (
            "less",
            "@color: red;\n.a{color:@color;}",
            "@color :  red;\n.a {\n  color: @color;\n}",
            "@color: blue;\n.a{color:@color;}",
        ),
        (
            "mdx",
            'import X from "./x"\n\n# Heading\n\n<X value = {1 + 2} />\n',
            'import X from "./x"\n\n# Heading\n\n<X\n value={1 + 2}\n/>\n',
            'import X from "./x"\n\n# Heading\n\n<X value={1 + 3}/>\n',
        ),
        (
            "vue",
            '<template>Text <X :v = "one + two" foo="2" />\n</template>',
            '<template>Text <X\n :v="one + two" foo = "2"/>\n</template>',
            '<template>Text<X :v="one + two" foo="2"/>\n</template>',
        ),
        (
            "svelte",
            "{#if name}\n<p a = {name}> {name}</p>\n{/if}",
            "{#if name}\n<p\n a={name}> {name}</p>\n{/if}",
            "{#if name}\n<p a={name}>{name}</p>\n{/if}",
        ),
        (
            "astro",
            "---\nlet x = 1;\n---\n<X y = {x + 1} />",
            "---\nlet x = 1;\n---\n<X y={x + 1}/>",
            "---\nlet x = 2;\n---\n<X y={x + 1}/>",
        ),
        (
            "asciidoc",
            "=   Doc\n\n==   Heading\n\n[source]\n----\n==   code\n----\n\n==    Last",
            "= Doc\n\n== Heading\n\n[source]\n----\n==   code\n----\n\n== Last",
            "= Doc\n\n== Heading\n\n[source]\n----\n== code\n----\n\n== Last",
        ),
        (
            "rst",
            "Title\n=====\n\nParagraph\n",
            "Title\n========\n\nParagraph\n",
            "Title\n--------\n\nParagraph\n",
        ),
        (
            "latex",
            "\\documentclass {article}\n\\begin{document}\n\\section  {Hello}\nText \\textbf  {bold}\n\\end{document}\n",
            "\\documentclass{article}\n\\begin{document}\n\\section{Hello}\nText \\textbf{bold}\n\\end{document}\n",
            "\\documentclass{article}\n\\begin{document}\n\\section{Hello}\nText \\textbf{plain}\n\\end{document}\n",
        ),
    ],
)
def test_web_layout_does_not_hide_content(format_name: str, old: str, formatted: str, semantic: str) -> None:
    assert _same(format_name, old, formatted)
    assert not _same(format_name, old, semantic)


def test_xml_indentation_requires_element_only_model_and_allows_xml_space(tmp_path: Path) -> None:
    dtd = "<!DOCTYPE root [<!ELEMENT root (x*)><!ELEMENT x EMPTY>]>"
    compact = "<root><x/><x/></root>"
    pretty = "<root>\n  <x/>\n  <x/>\n</root>"
    assert _same("xml", dtd + compact, dtd + pretty)
    assert not _same("xml", compact, pretty)
    assert not _same(
        "xml",
        dtd + compact.replace("<root>", '<root xml:space="preserve">'),
        dtd + pretty.replace("<root>", '<root xml:space="preserve">'),
    )
    assert not _same("xml", dtd + compact.replace("<x/>", "x<x/>"), dtd + pretty.replace("<x/>", "x<x/>"))

    external = tmp_path / "external.dtd"
    external.write_text("<!ELEMENT root (x*)><!ELEMENT x EMPTY>")
    external_dtd = f'<!DOCTYPE root SYSTEM "{external.as_uri()}">'
    assert not _same("xml", external_dtd + compact, external_dtd + pretty)


def test_xml_preserves_raw_doctype_comments_cdata_entities_and_namespaces() -> None:
    dtd = '<!DOCTYPE p:r [<!ENTITY ext SYSTEM "file:///nonexistent-external-entity">]>'
    body = '<p:r xmlns:p="urn:one" x = "literal">\n &ext; <!-- two  spaces --> <![CDATA[a  b]]> </p:r>'
    assert _same("xml", dtd + body, dtd + body.replace('x = "literal"', "x='literal'"))
    for old, new in (
        ("nonexistent-external-entity", "other-entity"),
        ("urn:one", "urn:two"),
        ("<!-- two  spaces -->", "<!-- two spaces -->"),
        ("<![CDATA[a  b]]>", "<![CDATA[a b]]>"),
        ("&ext;", "ext"),
    ):
        original = dtd + body
        assert not _same("xml", original, original.replace(old, new))
    assert web_document("<unclosed>", "xml") is None


@pytest.mark.parametrize(
    ("format_name", "old", "new"),
    [
        ("scss", ".x { --custom: value; }", ".x { --custom:value; }"),
        ("scss", ".x { width: 1px + 2px; }", ".x { width: 1px+2px; }"),
        ("scss", '.x { content: "a  b"; }', '.x { content: "a b"; }'),
        ("sass", ".x\n  // comment\n    color:  red\n", ".x\n  // comment\n    color: red\n"),
        ("less", ".x { --custom: value; }", ".x { --custom:value; }"),
        ("less", '.x { content: "a  b"; }', '.x { content: "a b"; }'),
        ("mdx", '```mdx\n<X a = "x" />\n```\n', '```mdx\n<X a="x"/>\n```\n'),
        ("mdx", '{`\n\n<X a = "x" />\n\n`}\n', '{`\n\n<X a="x"/>\n\n`}\n'),
        ("vue", '<template><x title="a  b"/></template>', '<template><x title="a b"/></template>'),
        (
            "vue",
            '<template><textarea><X a = "x"/></textarea></template>',
            '<template><textarea><X a="x"/></textarea></template>',
        ),
        ("vue", '<docs><X a = "x"/></docs>', '<docs><X a="x"/></docs>'),
        ("vue", '<template lang="pug"><X a = "x"/></template>', '<template lang="pug"><X a="x"/></template>'),
        ("svelte", '<x a={"a  b"}/>', '<x a={"a b"}/>'),
        ("astro", '<x a={"a  b"}/>', '<x a={"a b"}/>'),
        ("astro", '<div is:raw><X a = "x"/></div>', '<div is:raw><X a="x"/></div>'),
        ("rst", "Code::\n\n    x  y\n", "Code::\n\n    x y\n"),
        (
            "latex",
            "\\begin{verbatim}\n\\textbf  {literal}\n\\end{verbatim}",
            "\\begin{verbatim}\n\\textbf{literal}\n\\end{verbatim}",
        ),
        ("latex", "Inline \\verb|\\textbf  {literal}|.", "Inline \\verb|\\textbf{literal}|."),
    ],
)
def test_web_retains_opaque_syntax(format_name: str, old: str, new: str) -> None:
    assert not _same(format_name, old, new)


def test_component_keeps_a_value_edit_pending_next_to_attribute_formatting() -> None:
    before = '<template>\n  <x\n    name = "a"\n    color="red"\n  />\n</template>'
    after = '<template>\n  <x\n    name="a"\n    color="blue"\n  />\n</template>'

    assert reformat_only(before, after, _SYNTAX_BY_SUFFIX[".vue"]) == after.replace("blue", "red")

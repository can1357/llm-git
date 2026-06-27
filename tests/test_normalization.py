from __future__ import annotations

from lgit.normalization import normalize_summary_verb, normalize_unicode

# normalize_unicode tests


def test_normalize_unicode_smart_quotes() -> None:
    assert normalize_unicode("\u2018smart quotes\u2019") == "'smart quotes'"
    assert normalize_unicode("\u201cdouble quotes\u201d") == '"double quotes"'
    assert normalize_unicode("\u201alow quote\u2019") == "'low quote'"
    assert normalize_unicode("\u201elow double\u201d") == '"low double"'


def test_normalize_unicode_dashes() -> None:
    assert normalize_unicode("en\u2013dash") == "en--dash"
    assert normalize_unicode("em\u2014dash") == "em--dash"
    assert normalize_unicode("fig\u2012dash") == "fig-dash"
    assert normalize_unicode("minus\u2212sign") == "minus-sign"


def test_normalize_unicode_arrows() -> None:
    assert normalize_unicode("arrow\u2192right") == "arrow->right"
    assert normalize_unicode("arrow\u2190left") == "arrow<-left"
    assert normalize_unicode("arrow\u2194both") == "arrow<->both"
    assert normalize_unicode("double\u21d2arrow") == "double=>arrow"
    assert normalize_unicode("up\u2191arrow") == "up^arrow"


def test_normalize_unicode_math() -> None:
    assert normalize_unicode("a\u00d7b") == "axb"
    assert normalize_unicode("a\u00f7b") == "a/b"
    assert normalize_unicode("x\u2264y") == "x<=y"
    assert normalize_unicode("x\u2265y") == "x>=y"
    assert normalize_unicode("x\u2260y") == "x!=y"
    assert normalize_unicode("x\u2248y") == "x~=y"


def test_normalize_unicode_greek() -> None:
    assert normalize_unicode("\u03bb function") == "lambda function"
    assert normalize_unicode("\u03b1 beta \u03b3") == "alpha beta gamma"
    assert normalize_unicode("\u03bc service") == "mu service"
    assert normalize_unicode("\u03a3 total") == "Sigma total"


def test_normalize_unicode_fractions() -> None:
    assert normalize_unicode("\u00bd cup") == "1/2 cup"
    assert normalize_unicode("\u00be done") == "3/4 done"
    assert normalize_unicode("\u2153 left") == "1/3 left"


def test_normalize_unicode_superscripts() -> None:
    assert normalize_unicode("x\u00b2") == "x^2"
    assert normalize_unicode("10\u00b3") == "10^3"


def test_normalize_unicode_multiple_replacements() -> None:
    input_text = "\u2018smart\u2019\u2192straight \u201cquotes\u201d\u00d7math\u2264ops"
    expected = "'smart'->straight \"quotes\"xmath<=ops"
    assert normalize_unicode(input_text) == expected


def test_normalize_unicode_ellipsis() -> None:
    assert normalize_unicode("wait\u2026") == "wait..."
    assert normalize_unicode("more\u22efdots") == "more...dots"


def test_normalize_unicode_bullets() -> None:
    assert normalize_unicode("\u2022item") == "-item"
    assert normalize_unicode("\u25e6item") == "-item"


def test_normalize_unicode_check_marks() -> None:
    assert normalize_unicode("\u2713done") == "vdone"
    assert normalize_unicode("\u2717failed") == "xfailed"


# normalize_summary_verb tests


def test_normalize_summary_verb_present_to_past() -> None:
    assert normalize_summary_verb("add new feature", "feat") == "added new feature"
    assert normalize_summary_verb("fix bug", "fix") == "fixed bug"
    assert normalize_summary_verb("update docs", "docs") == "updated docs"


def test_normalize_summary_verb_already_past() -> None:
    assert normalize_summary_verb("added feature", "feat") == "added feature"
    assert normalize_summary_verb("fixed bug", "fix") == "fixed bug"


def test_normalize_summary_verb_third_person() -> None:
    assert normalize_summary_verb("adds feature", "feat") == "added feature"
    assert normalize_summary_verb("fixes bug", "fix") == "fixed bug"


def test_normalize_summary_verb_non_verb_start() -> None:
    assert normalize_summary_verb("123 files changed", "chore") == "123 files changed"


def test_normalize_summary_verb_refactor_special_case() -> None:
    assert normalize_summary_verb("refactored code", "refactor") == "restructured code"


def test_normalize_summary_verb_refactor_present() -> None:
    assert normalize_summary_verb("refactor code", "refactor") == "restructured code"
    assert normalize_summary_verb("refactor logic", "feat") == "refactored logic"


def test_normalize_summary_verb_empty() -> None:
    assert normalize_summary_verb("", "feat") == ""

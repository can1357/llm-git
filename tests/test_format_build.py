from __future__ import annotations

import pytest
from lgit.format_build import build_document


@pytest.mark.parametrize(
    ("language", "before", "after"),
    [
        (
            "shell",
            'f(){\n    printf "%s\\n"  "$name"|sort  # order\n}\n',
            'f() {\n  printf "%s\\n" "$name" | sort # order\n}\n',
        ),
        (
            "fish",
            'if test "$mode" = build\n    echo   bin/tool|sort\nend\n',
            'if test "$mode" = build\n  echo bin/tool | sort\n\nend\n',
        ),
        (
            "powershell",
            'if($true){\n    $n=1+2\n    Write-Output   "ok"\n}\n',
            'if ( $true ) {\n  $n = 1 + 2\n  Write-Output "ok"\n}\n',
        ),
        ("powershell", "Get-Thing|Select-Object\n", "Get-Thing | Select-Object\n"),
        (
            "dockerfile",
            '  from   alpine AS base\nCOPY ["a","/b"]\nRUN   [ "echo",  "a b" ]\n',
            'FROM alpine AS base\nCOPY [ "a", "/b" ]\nRUN ["echo", "a b"]\n',
        ),
        ("dockerfile", ' RUN   echo "a b"\n', 'RUN echo "a b"\n'),
        (
            "make",
            "CFLAGS:=   -Wall\nall : a   b\n\t@echo build\n",
            "CFLAGS := -Wall\nall: a b\n\t@echo build\n",
        ),
        (
            "cmake",
            'set ( NAME  "a b"  [=[c d]=] )\n',
            'set(NAME\n  "a b"\n  [=[c d]=]\n)\n',
        ),
        (
            "starlark",
            'load("//tools:defs.bzl","tool")\ndef rule():\n  tool(srcs=["a","b"])\n',
            'load("//tools:defs.bzl", "tool")\ndef rule():\n    tool(srcs=[\n        "a",\n        "b",\n    ])\n',
        ),
    ],
)
def test_layout_only(language: str, before: str, after: str) -> None:
    old = build_document(before, language)
    new = build_document(after, language)
    assert old is not None and new is not None
    assert old.tokens == new.tokens


@pytest.mark.parametrize(
    ("language", "before", "after"),
    [
        ("shell", 'echo "$name"/file\n', 'echo "$name" /file\n'),
        ("shell", "cat 2>err\n", "cat 2 >err\n"),
        ("shell", "echo a\\\nb\n", "echo a \\\nb\n"),
        ("shell", "echo a\r\n", "echo a\n"),
        ("shell", "echo ${value:-  fallback}\n", "echo ${value:- fallback}\n"),
        ("shell", "cat <<EOF\n  a\nEOF\n", "cat <<EOF\n a\nEOF\n"),
        ("shell", "#!/bin/bash\necho ok\n", "\n#!/bin/bash\necho ok\n"),
        ("shell", "echo ok # before\n", "echo ok # after\n"),
        ("fish", 'echo "$name"/file\n', 'echo "$name" /file\n'),
        ("fish", "echo a\\\nb\n", "echo a \\\nb\n"),
        ("powershell", "Write-Output a b\n", "Write-Output a\nb\n"),
        ("powershell", "Write-Output $x.Length\n", "Write-Output $x .Length\n"),
        ("powershell", 'Write-Output @"\n a\n"@\n', 'Write-Output @"\na\n"@\n'),
        ("powershell", "Write-Output --% a  b\n", "Write-Output --% a b\n"),
        ("powershell", "Write-Output a `\n b\n", "Write-Output a\n b\n"),
        ("dockerfile", 'RUN ["echo", "a b"]\n', 'RUN echo "a b"\n'),
        ("dockerfile", 'COPY ["a  b", "/dst"]\n', 'COPY ["a b", "/dst"]\n'),
        ("dockerfile", "RUN echo  a\n", "RUN echo a\n"),
        ("dockerfile", "COPY ${files:-a  b} /dst\n", "COPY ${files:-a b} /dst\n"),
        ("dockerfile", "RUN <<EOF\n  echo a\nEOF\n", "RUN <<EOF\n echo a\nEOF\n"),
        ("dockerfile", "# syntax=docker/dockerfile:1\nFROM alpine\n", "\n# syntax=docker/dockerfile:1\nFROM alpine\n"),
        ("make", "all: a\n\t@echo hi\n", "all: a\n \t@echo hi\n"),
        ("make", "all: a\n\t@echo hi\n", "all: a\n\t-echo hi\n"),
        ("make", "FLAGS := value  \n", "FLAGS := value\n"),
        ("make", "all: a b\n", "all: a/b\n"),
        ("make", "ifeq (a,b )\nendif\n", "ifeq (a,b)\nendif\n"),
        ("make", "$(file >out, hello  world)\n", "$(file >out, hello world)\n"),
        ("cmake", 'set(VAR "a  b")\n', 'set(VAR "a b")\n'),
        ("cmake", 'set(VAR "a"b)\n', 'set(VAR "a" b)\n'),
        ("cmake", "set(VAR a b)\n", "set(VAR a/b)\n"),
        ("starlark", "if enabled:\n    run()\n    done()\n", "if enabled:\n    run()\ndone()\n"),
        ("starlark", "if enabled:\n    run()\n", "if enabled:\n\trun()\n"),
        ("starlark", "x = (1,)\n", "x = (1)\n"),
        ("starlark", 'load("//a", "a")\nload("//b", "b")\n', 'load("//b", "b")\nload("//a", "a")\n'),
    ],
)
def test_meaningful_edits_are_not_equivalent(language: str, before: str, after: str) -> None:
    old = build_document(before, language)
    new = build_document(after, language)
    assert old is not None
    assert new is None or old.tokens != new.tokens


def test_unsupported_zsh_syntax_is_not_reinterpreted() -> None:
    assert build_document('echo =(printf "%s\\n" value)\n', "shell") is None

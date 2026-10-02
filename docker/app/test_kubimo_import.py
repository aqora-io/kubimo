import json
import subprocess
import sys
from pathlib import Path

import pytest
from marimo._ast.parse import parse_notebook

import kubimo_import
from kubimo_import import convert

IMPORT_SCRIPT = Path(__file__).parent / "kubimo_import.py"

MARIMO_NOTEBOOK = '''import marimo

app = marimo.App()


@app.cell
def _():
    x = 1
    return


if __name__ == "__main__":
    app.run()
'''


def _ipynb(*cells):
    return json.dumps(
        {
            "cells": [
                {"cell_type": kind, "metadata": {}, "source": source}
                | ({"outputs": [], "execution_count": None} if kind == "code" else {})
                for kind, source in cells
            ],
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
        }
    )


def _cells(text):
    notebook = parse_notebook(text)
    assert notebook is not None and notebook.valid, text
    return [cell.code for cell in notebook.cells]


def test_ipynb_becomes_a_marimo_notebook(tmp_path):
    src = tmp_path / "source.ipynb"
    src.write_text(_ipynb(("markdown", "# Title"), ("code", "y = 21 * 2")))
    cells = _cells(convert(src))
    assert any("mo.md" in cell and "# Title" in cell for cell in cells)
    assert any("y = 21 * 2" in cell for cell in cells)


def test_pip_install_lines_resolve_offline(tmp_path, monkeypatch):
    # The Job runs with uv offline, so resolution fails fast and marimo falls
    # back to the unpinned package names.
    monkeypatch.setenv("UV_OFFLINE", "1")
    monkeypatch.setenv("UV_NO_BUILD", "1")
    src = tmp_path / "source.ipynb"
    src.write_text(_ipynb(("code", "!pip install polars")))
    header = convert(src).split("import marimo")[0]
    assert '"polars"' in header


@pytest.mark.parametrize(
    "suffix, fence", [(".md", "```python {.marimo}"), (".qmd", "```{python}")]
)
def test_markdown_becomes_a_marimo_notebook(tmp_path, suffix, fence):
    src = tmp_path / f"source{suffix}"
    src.write_text(f"# Title\n\n{fence}\nz = 1\n```\n")
    cells = _cells(convert(src))
    assert any("z = 1" in cell for cell in cells)


def test_a_marimo_notebook_passes_through_unchanged(tmp_path):
    src = tmp_path / "source.py"
    src.write_text(MARIMO_NOTEBOOK)
    assert convert(src) == MARIMO_NOTEBOOK


def test_a_plain_script_becomes_one_cell(tmp_path):
    src = tmp_path / "source.py"
    src.write_text("a = 1\nb = a + 1\n")
    assert len(_cells(convert(src))) == 1


def test_a_percent_script_keeps_its_cells(tmp_path):
    pytest.importorskip("jupytext")
    src = tmp_path / "source.py"
    src.write_text("# %%\na = 1\n\n# %%\nb = a + 1\n")
    assert len(_cells(convert(src))) == 2


def test_an_unsupported_format_fails(tmp_path):
    src = tmp_path / "source.txt"
    src.write_text("hello")
    with pytest.raises(ValueError, match="unsupported"):
        convert(src)


def test_output_path_stays_in_the_root(tmp_path):
    assert kubimo_import.output_path(tmp_path, "a/b.py") == tmp_path.resolve() / "a/b.py"
    assert kubimo_import.output_path(tmp_path, "data.csv") == tmp_path.resolve() / "data.csv"
    for bad in ["../x.py", "/abs.py", "a/../b.py", "a/./b.py", "a//b.py", "a/"]:
        with pytest.raises(ValueError):
            kubimo_import.output_path(tmp_path, bad)


def test_output_path_refuses_a_symlink_out_of_the_root(tmp_path):
    root = tmp_path / "root"
    root.mkdir()
    (root / "escape").symlink_to(tmp_path)
    with pytest.raises(ValueError, match="leaves the workspace"):
        kubimo_import.output_path(root, "escape/x.py")


def test_a_file_is_created_then_replaced(tmp_path):
    root = tmp_path / "workspace"
    root.mkdir()
    for text in ["first", "second"]:
        src = tmp_path / "0"
        src.write_text(text)
        kubimo_import.import_files(root, [("copy", src, "a/b/c.py")])
    path = root / "a" / "b" / "c.py"
    assert path.read_text() == "second"
    assert path.stat().st_mode & 0o777 == 0o644
    assert sorted(p.name for p in path.parent.iterdir()) == ["c.py"]


def test_a_copy_is_byte_for_byte(tmp_path):
    src = tmp_path / "staged"
    src.write_bytes(b"\x00\xff,not utf-8\r\n")
    root = tmp_path / "workspace"
    root.mkdir()
    kubimo_import.import_files(root, [("copy", src, "out/data.bin")])
    path = root / "out" / "data.bin"
    assert path.read_bytes() == src.read_bytes()
    assert path.stat().st_mode & 0o777 == 0o644


def test_files_are_converted_or_copied(tmp_path):
    notebook = tmp_path / "0.ipynb"
    notebook.write_text(_ipynb(("code", "y = 2")))
    data = tmp_path / "1"
    data.write_text("a,b\n1,2\n")
    root = tmp_path / "workspace"
    root.mkdir()
    kubimo_import.import_files(
        root,
        [("convert", notebook, "nb/y.py"), ("copy", data, "data/table.csv")],
    )
    assert any("y = 2" in cell for cell in _cells((root / "nb" / "y.py").read_text()))
    assert (root / "data" / "table.csv").read_text() == "a,b\n1,2\n"


def test_a_copied_notebook_is_not_converted(tmp_path):
    notebook = tmp_path / "0"
    notebook.write_text(_ipynb(("code", "y = 2")))
    kubimo_import.import_files(tmp_path, [("copy", notebook, "y.ipynb")])
    assert (tmp_path / "y.ipynb").read_text() == notebook.read_text()


def test_a_converted_file_must_become_a_python_file(tmp_path):
    src = tmp_path / "0.ipynb"
    src.write_text(_ipynb(("code", "y = 2")))
    with pytest.raises(ValueError, match="must end with .py"):
        kubimo_import.plan(tmp_path, [("convert", src, "y.ipynb")])


def test_an_unknown_mode_fails(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    with pytest.raises(ValueError, match="unknown mode"):
        kubimo_import.plan(tmp_path, [("move", src, "x")])


def test_one_bad_file_writes_nothing(tmp_path):
    good = tmp_path / "0"
    good.write_text("a,b\n")
    bad = tmp_path / "1.ipynb"
    bad.write_text("not json")
    root = tmp_path / "workspace"
    root.mkdir()
    with pytest.raises(Exception):
        kubimo_import.import_files(
            root, [("copy", good, "data.csv"), ("convert", bad, "nb.py")]
        )
    assert list(root.iterdir()) == []


def _tree(root):
    return sorted(str(p.relative_to(root)) for p in root.rglob("*"))


def test_an_output_inside_another_writes_nothing(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    root = tmp_path / "workspace"
    root.mkdir()
    with pytest.raises(ValueError, match="'pfx/a/b.csv' is inside output path 'pfx/a'"):
        kubimo_import.import_files(root, [("copy", src, "pfx/a"), ("copy", src, "pfx/a/b.csv")])
    assert _tree(root) == []


def test_an_existing_directory_is_not_replaced(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    root = tmp_path / "workspace"
    (root / "notebooks").mkdir(parents=True)
    with pytest.raises(ValueError, match="existing directory: 'notebooks'"):
        kubimo_import.import_files(root, [("copy", src, "data.csv"), ("copy", src, "notebooks")])
    assert _tree(root) == ["notebooks"]


def test_a_file_cannot_hold_an_output(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    root = tmp_path / "workspace"
    root.mkdir()
    (root / "x").write_text("a file")
    with pytest.raises(ValueError, match="inside a file: 'x/y.csv'"):
        kubimo_import.import_files(root, [("copy", src, "data.csv"), ("copy", src, "x/y.csv")])
    assert _tree(root) == ["x"]


def test_a_failed_write_leaves_nothing_behind(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    root = tmp_path / "workspace"
    (root / "old").mkdir(parents=True)
    with pytest.raises(kubimo_import.ImportFailed, match="new/b/c.csv: cannot write"):
        kubimo_import.import_files(
            root,
            [("copy", src, "old/a.csv"), ("copy", tmp_path / "missing", "new/b/c.csv")],
        )
    # Neither the staged file nor the directories made for the second one.
    assert _tree(root) == ["old"]


def test_markdown_gains_the_marimo_import_it_uses(tmp_path):
    src = tmp_path / "source.md"
    src.write_text("# Title\n\nSome prose.\n\n```python {.marimo}\nz = 1\n```\n")
    cells = _cells(convert(src))
    assert cells[0] == "import marimo as mo"
    assert any("mo.md" in cell and "Some prose." in cell for cell in cells)


def test_markdown_that_imports_marimo_keeps_its_one_import(tmp_path):
    src = tmp_path / "source.md"
    src.write_text(
        "# Title\n\n```python {.marimo}\nimport marimo as mo\n```\n\n"
        "```python {.marimo}\nz = 1\n```\n"
    )
    cells = _cells(convert(src))
    assert sum("import marimo as mo" in cell for cell in cells) == 1


def test_markdown_without_prose_needs_no_import(tmp_path):
    src = tmp_path / "source.qmd"
    src.write_text("```{python}\nq = 2\n```\n")
    assert not any("import marimo" in cell for cell in _cells(convert(src)))


def test_the_script_imports_into_the_root(tmp_path):
    src = tmp_path / "0.ipynb"
    src.write_text(_ipynb(("code", "y = 2")))
    data = tmp_path / "1"
    data.write_text("a,b\n")
    root = tmp_path / "workspace"
    root.mkdir()
    subprocess.run(
        [
            sys.executable,
            str(IMPORT_SCRIPT),
            "--root",
            str(root),
            "--",
            "convert",
            str(src),
            "out/y.py",
            "copy",
            str(data),
            "-odd.csv",
        ],
        check=True,
    )
    assert any("y = 2" in cell for cell in _cells((root / "out" / "y.py").read_text()))
    assert (root / "-odd.csv").read_text() == "a,b\n"


def test_the_script_fails_on_a_bad_input(tmp_path):
    src = tmp_path / "source.txt"
    src.write_text("hello")
    result = subprocess.run(
        [sys.executable, str(IMPORT_SCRIPT), "--root", str(tmp_path), "--", "convert", str(src), "y.py"],
        capture_output=True,
    )
    assert result.returncode != 0
    assert not (tmp_path / "y.py").exists()


def test_the_script_says_why_it_failed(tmp_path):
    src = tmp_path / "0.ipynb"
    src.write_text("not json")
    root = tmp_path / "workspace"
    root.mkdir()
    log = tmp_path / "termination-log"
    result = subprocess.run(
        [
            sys.executable,
            str(IMPORT_SCRIPT),
            "--root",
            str(root),
            f"--termination-log={log}",
            "--",
            "convert",
            str(src),
            "nb/bad.py",
        ],
        capture_output=True,
    )
    assert result.returncode != 0
    assert log.read_text().startswith("nb/bad.py: cannot convert: JSONDecodeError: Expecting value")
    # The traceback still goes to the log.
    assert b"Traceback" in result.stderr


def test_the_script_wants_whole_triples(tmp_path):
    src = tmp_path / "0"
    src.write_text("x")
    result = subprocess.run(
        [sys.executable, str(IMPORT_SCRIPT), "--root", str(tmp_path), "--", "copy", str(src)],
        capture_output=True,
    )
    assert result.returncode != 0
    assert b"triples" in result.stderr

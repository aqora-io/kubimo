import subprocess
import sys
from pathlib import Path

CACHE_SCRIPT = Path(__file__).parent / "cache.py"

NOTEBOOK = '''import marimo

app = marimo.App()


@app.cell
def _():
    import marimo as mo
    return (mo,)


@app.cell
def _(mo):
    mo.md("# Hello\\n\\nSome **markdown** here.")
    return


@app.cell
def _():
    x = 21 * 2
    x
    return (x,)


if __name__ == "__main__":
    app.run()
'''


def test_ignore_file_excludes_like_gitignore(tmp_path):
    # The workspace template ships `.ignore` (not `.gitignore`, which would tie
    # the exclusions to git); outside a git repo the fallback walker must honour
    # it, and it must win over `.gitignore` like in the indexer's walker.
    import cache

    (tmp_path / "keep.py").write_text("x = 1\n")
    (tmp_path / ".ignore").write_text("excluded/\n!vendored.py\n")
    (tmp_path / ".gitignore").write_text("vendored.py\n")
    (tmp_path / "vendored.py").write_text("x = 2\n")
    excluded = tmp_path / "excluded"
    excluded.mkdir()
    (excluded / "dropped.py").write_text("x = 3\n")

    files = cache._get_python_files(str(tmp_path))
    names = sorted(path.relative_to(tmp_path).as_posix() for path in files)
    assert names == ["keep.py", "vendored.py"]


def test_cache_exports_html_and_markdown(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(NOTEBOOK)

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), "--include-code", str(tmp_path)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr

    export_dir = tmp_path / "__marimo__"
    html = export_dir / "nb.html"
    md = export_dir / "nb.md"

    assert html.is_file() and html.stat().st_size > 0, result.stderr
    assert md.is_file() and md.stat().st_size > 0, result.stderr

    md_text = md.read_text()
    assert "# Hello" in md_text
    assert "x = 21 * 2" in md_text


def test_cache_writes_the_snapshots_the_renderer_serves(tmp_path):
    # marimo-ssr refuses to render without both: it answers "Notebook has no
    # session cache" without the session file and "Notebook was not properly
    # exported" without the notebook file.
    notebook = tmp_path / "nb.py"
    notebook.write_text(NOTEBOOK)

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), "--include-code", str(tmp_path)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stderr

    session = tmp_path / "__marimo__" / "session" / "nb.py.json"
    snapshot = tmp_path / "__marimo__" / "notebook" / "nb.py.json"
    assert session.is_file() and session.stat().st_size > 0, result.stderr
    assert snapshot.is_file() and snapshot.stat().st_size > 0, result.stderr

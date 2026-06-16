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

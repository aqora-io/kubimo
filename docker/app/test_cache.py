import asyncio
import logging
import subprocess
import sys
import time
from pathlib import Path
from types import SimpleNamespace

import cache
import kubimo_walk
import pytest
from marimo._environments import backends, process
from marimo._environments.environment import Environment
from marimo._environments.errors import EnvironmentManagerError
from marimo._environments.overlay import runtime_overlay

CACHE_SCRIPT = Path(__file__).parent / "cache.py"

NOTEBOOK = """import marimo

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
"""

# The canonical header: a notebook with an environment of its own.
HEADER = """# /// script
# requires-python = "==3.12.*"
# dependencies = [
#     "marimo",
# ]
# ///
"""

# What the stubbed pixi syncs a notebook with a header to.
ENVIRONMENT = Environment(python=sys.executable, root=sys.prefix, action="unchanged")


@pytest.fixture
def sandbox(monkeypatch):
    """pixi, stubbed: a notebook with a header syncs to ENVIRONMENT and its
    worker runs on this interpreter, as launch_fallback plans it (or `worker`
    runs instead, when set). A name in `failures` fails that many syncs."""
    stub = SimpleNamespace(synced=[], launched=[], failures={}, worker=None)

    async def sync_notebook_async(path, *, backend):
        stub.synced.append((path, backend))
        name = Path(path).name
        if stub.failures.get(name):
            stub.failures[name] -= 1
            raise EnvironmentManagerError(f"pixi could not sync {name}")
        return ENVIRONMENT

    def launch(environment, args, *, backend, overlay):
        stub.launched.append((environment, list(args), backend, overlay))
        return backends.launch_fallback(stub.worker or args)

    monkeypatch.setattr(backends, "sync_notebook_async", sync_notebook_async)
    monkeypatch.setattr(backends, "launch", launch)
    return stub


def test_ignore_file_excludes_like_gitignore(tmp_path):
    # The workspace template ships `.ignore` (not `.gitignore`, which would tie
    # the exclusions to git); outside a git repo the fallback walker must honour
    # it, and it must win over `.gitignore` like in the indexer's walker.
    (tmp_path / "keep.py").write_text("x = 1\n")
    (tmp_path / ".ignore").write_text("excluded/\n!vendored.py\n")
    (tmp_path / ".gitignore").write_text("vendored.py\n")
    (tmp_path / "vendored.py").write_text("x = 2\n")
    excluded = tmp_path / "excluded"
    excluded.mkdir()
    (excluded / "dropped.py").write_text("x = 3\n")

    files = kubimo_walk.find_files(str(tmp_path))
    names = sorted(path.relative_to(tmp_path).as_posix() for path in files)
    assert names == ["keep.py", "vendored.py"]


@pytest.mark.parametrize("git", [False, True], ids=["ignore-rules", "git"])
def test_the_walk_never_follows_a_directory_symlink(tmp_path, git):
    # A workspace's symlinks come back from S3 as they were archived: a loop
    # would otherwise end the walk with ELOOP, and a link to an ancestor would
    # walk the whole tree again, caches included, once per level.
    if git:
        subprocess.run(["git", "init", "-q"], cwd=tmp_path, check=True)
    notebooks = tmp_path / "notebooks"
    notebooks.mkdir()
    (notebooks / "nb.py").write_text("x = 1\n")
    (tmp_path / "shortcut").symlink_to("notebooks")
    (notebooks / "up").symlink_to("..")
    (notebooks / "loop").symlink_to("loop")

    files = kubimo_walk.find_files(str(tmp_path))
    assert [path.relative_to(tmp_path).as_posix() for path in files] == [
        "notebooks/nb.py"
    ]


def test_cache_exports_html_and_markdown(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(NOTEBOOK)

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), "--include-code", str(tmp_path)],
        capture_output=True,
        text=True,
        check=False,
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
        check=False,
    )
    assert result.returncode == 0, result.stderr

    session = tmp_path / "__marimo__" / "session" / "nb.py.json"
    snapshot = tmp_path / "__marimo__" / "notebook" / "nb.py.json"
    assert session.is_file() and session.stat().st_size > 0, result.stderr
    assert snapshot.is_file() and snapshot.stat().st_size > 0, result.stderr


@pytest.mark.parametrize(
    ("flags", "backend"),
    [([], "pixi"), (["--backend", "uv"], "uv"), (["--backend", "pixi"], "pixi")],
    ids=["default", "uv", "pixi"],
)
def test_notebook_with_a_header_is_cached_in_its_own_environment(
    tmp_path, sandbox, flags, backend
):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)
    (tmp_path / "plain.py").write_text(NOTEBOOK)

    cache.main(["--include-code", *flags, str(tmp_path)])

    # Only the notebook with a header has an environment, built by the
    # workspace's backend; the other one's worker runs on this interpreter,
    # like marimo run.
    assert sandbox.synced == [(str(notebook), backend)]
    assert sandbox.launched == [
        (
            ENVIRONMENT,
            [
                str(CACHE_SCRIPT),
                "--one",
                str(notebook),
                "--include-code",
                "--log-level",
                "info",
            ],
            backend,
            runtime_overlay(),
        )
    ]
    for name in ("nb", "plain"):
        for output in (
            f"{name}.html",
            f"{name}.md",
            f"session/{name}.py.json",
            f"notebook/{name}.py.json",
        ):
            assert (tmp_path / "__marimo__" / output).stat().st_size > 0, output


def test_notebooks_are_never_edited(tmp_path, sandbox):
    notebooks = {
        tmp_path / "nb.py": HEADER + NOTEBOOK,
        tmp_path / "plain.py": NOTEBOOK,
    }
    for path, text in notebooks.items():
        path.write_text(text)

    cache.main([str(tmp_path)])

    for path, text in notebooks.items():
        assert path.read_bytes() == text.encode()
    assert (tmp_path / "__marimo__" / "nb.html").is_file()
    assert (tmp_path / "__marimo__" / "plain.html").is_file()


def test_modules_that_are_not_notebooks_are_never_imported(tmp_path):
    marker = tmp_path / "imported"
    (tmp_path / "helpers.py").write_text(f"open({str(marker)!r}, 'w').close()\n")

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), str(tmp_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert "helpers.py" in result.stderr
    assert not marker.exists()


def test_log_level_warn_is_accepted(tmp_path):
    (tmp_path / "nb.py").write_text(NOTEBOOK)
    (tmp_path / "helpers.py").write_text("x = 1\n")

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), "--log-level=warn", str(tmp_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert (tmp_path / "__marimo__" / "nb.html").is_file(), result.stderr
    # warn means WARNING: the skip is logged, the info lines are left out.
    assert "Skipping" in result.stderr
    assert "Caching" not in result.stderr


def test_jobs_bounds_the_workers_running_at_once(tmp_path, monkeypatch):
    directories = [tmp_path / f"project{index}" for index in range(5)]
    for directory in directories:
        directory.mkdir()
        (directory / "nb.py").write_text(NOTEBOOK)
    running = 0
    peak = 0
    cwds = []

    async def run_command(argv, *, env=None, cwd=None, timeout=None, on_stderr=None):
        nonlocal running, peak
        cwds.append(cwd)
        running += 1
        peak = max(peak, running)
        await asyncio.sleep(0.05)
        running -= 1
        return subprocess.CompletedProcess(argv, 0, "", "")

    monkeypatch.setattr(process, "run_command", run_command)

    cache.main(["--jobs", "2", str(tmp_path)])
    assert peak == 2
    # Every worker runs from the workspace root, like the runner's kernels, so
    # a nested notebook's relative paths resolve as in the live notebook.
    assert cwds == [str(tmp_path)] * len(directories)


def test_failed_worker_does_not_stop_the_others(tmp_path, sandbox, caplog):
    caplog.set_level(logging.INFO)
    sandbox.worker = ["-c", "import sys; sys.exit('the worker broke')"]
    (tmp_path / "broken.py").write_text(HEADER + NOTEBOOK)
    (tmp_path / "nb.py").write_text(NOTEBOOK)

    cache.main([str(tmp_path)])

    assert (tmp_path / "__marimo__" / "nb.html").is_file()
    assert not (tmp_path / "__marimo__" / "broken.html").exists()
    assert "the worker broke" in caplog.text
    assert "1 apps cached successfully, 1 failed or skipped" in caplog.text


def test_environment_sync_is_retried_once(tmp_path, sandbox, monkeypatch, caplog):
    caplog.set_level(logging.INFO)
    monkeypatch.setattr(cache, "_RETRY_DELAY", (0, 0))
    sandbox.worker = ["-c", "pass"]
    sandbox.failures = {"flaky.py": 1, "broken.py": 2}
    for name in ("flaky.py", "broken.py"):
        (tmp_path / name).write_text(HEADER + NOTEBOOK)

    cache.main([str(tmp_path)])

    synced = sorted(Path(path).name for path, _ in sandbox.synced)
    assert synced == ["broken.py", "broken.py", "flaky.py", "flaky.py"]
    assert [Path(args[2]).name for _, args, _, _ in sandbox.launched] == ["flaky.py"]
    assert "pixi could not sync broken.py" in caplog.text
    assert "1 apps cached successfully, 1 failed or skipped" in caplog.text


def test_worker_is_stopped_at_the_timeout(tmp_path, sandbox, caplog):
    caplog.set_level(logging.INFO)
    sandbox.worker = [
        "-c",
        "import sys, time; print('still exporting', file=sys.stderr); time.sleep(60)",
    ]
    (tmp_path / "slow.py").write_text(HEADER + NOTEBOOK)

    started = time.monotonic()
    cache.main(["--timeout", "5", str(tmp_path)])

    assert time.monotonic() - started < 30
    assert "its worker timed out" in caplog.text
    assert "still exporting" in caplog.text
    assert "0 apps cached successfully, 1 failed or skipped" in caplog.text


def test_environment_sync_counts_against_the_timeout(
    tmp_path, sandbox, monkeypatch, caplog
):
    caplog.set_level(logging.INFO)

    async def sync_notebook_async(path, *, backend):
        await asyncio.sleep(60)

    monkeypatch.setattr(backends, "sync_notebook_async", sync_notebook_async)
    (tmp_path / "stuck.py").write_text(HEADER + NOTEBOOK)

    started = time.monotonic()
    cache.main(["--timeout", "1", str(tmp_path)])

    assert time.monotonic() - started < 30
    assert sandbox.launched == []
    assert "syncing its environment timed out" in caplog.text
    assert "0 apps cached successfully, 1 failed or skipped" in caplog.text


def test_pixi_timing_out_is_a_failed_environment_sync(
    tmp_path, sandbox, monkeypatch, caplog
):
    # pixi's own `install --help` probe gives up with a TimeoutError of its own.
    monkeypatch.setattr(cache, "_RETRY_DELAY", (0, 0))
    synced = []

    async def sync_notebook_async(path, *, backend):
        synced.append(path)
        raise TimeoutError

    monkeypatch.setattr(backends, "sync_notebook_async", sync_notebook_async)
    (tmp_path / "nb.py").write_text(HEADER + NOTEBOOK)

    cache.main([str(tmp_path)])

    assert synced == [str(tmp_path / "nb.py")] * 2
    assert sandbox.launched == []
    assert "syncing its environment timed out" in caplog.text
    assert "its worker timed out" not in caplog.text

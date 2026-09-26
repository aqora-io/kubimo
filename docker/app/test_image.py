"""Tests that only make sense inside the built marimo image.

Each test builds a throwaway notebook in tmp_path (or where the image's seed
put one) and drives it through marimo's own sandbox API:
`sync_notebook_async` installs its pixi environment with the real pixi
binary, then `launch` plus `subprocess` runs a short check inside it with the
fork wheel overlaid, exactly as a kernel launches. The child prints its
result as one line of JSON.

Run inside the built image, with the rest of the suite, by the `marimo-test`
bake target (`pytest -q /app`); `pytest docker/app -m "not image"` skips this
file everywhere else, since none of it works without the image's real
pixi, uv, network and fork wheel.
"""

import asyncio
import json
import platform
import subprocess
import sys
import time
from pathlib import Path

import pytest
from marimo._environments import backends, pixi, script_metadata
from marimo._environments.overlay import runtime_overlay

pytestmark = pytest.mark.image

CACHE_SCRIPT = Path(__file__).parent / "cache.py"

# The canonical header: the environment the image's seed step pre-built.
HEADER = """# /// script
# requires-python = "==3.12.*"
# dependencies = [
#     "marimo",
# ]
# ///
"""

NOTEBOOK = """import marimo

app = marimo.App()


@app.cell
def _():
    return


if __name__ == "__main__":
    app.run()
"""

# Printed by a child launched in a notebook's environment: whether the fork
# wheel, not the notebook's own manifest, is what got imported.
CHECK_FORK = """
import json
from importlib.metadata import Distribution
from marimo._session.state.serialize import get_notebook_cache_file as _fork_only
direct_url = json.loads(Distribution.from_name("marimo").read_text("direct_url.json"))
print(json.dumps({"direct_url": direct_url}))
"""

# Every proxy pointed at a closed port on loopback: any attempt to reach the
# network fails immediately instead of hanging, so a trip online shows up as
# a failure rather than a slow, silent success.
CUT_NETWORK = {
    name: "http://127.0.0.1:9"
    for name in (
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    )
}


def _sync(notebook, *, timeout=300):
    """Install `notebook`'s pixi environment with the real pixi binary."""
    return asyncio.run(
        asyncio.wait_for(
            backends.sync_notebook_async(str(notebook), backend="pixi"),
            timeout=timeout,
        )
    )


def _run_json(environment, code, *, extra_env=None, timeout=300):
    """Launch `code` in `environment` with the runtime overlay, as a kernel
    would, and parse the JSON its stdout prints."""
    plan = backends.launch(
        environment, ["-c", code], backend="pixi", overlay=runtime_overlay()
    )
    env = dict(plan.env)
    if extra_env:
        env.update(extra_env)
    result = subprocess.run(
        list(plan.argv),
        env=env,
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=timeout,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def test_fork_kernel_overlays_the_built_wheel(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)

    environment = _sync(notebook)
    data = _run_json(environment, CHECK_FORK)

    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_user_pinned_marimo_is_shadowed(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(
        "# /// script\n"
        '# requires-python = "==3.12.*"\n'
        "# dependencies = [\n"
        '#     "marimo==0.23.0",\n'
        "# ]\n"
        "# ///\n" + NOTEBOOK
    )
    environment = _sync(notebook)

    # The notebook's own environment really has the pinned release...
    packages = {
        record["name"]: record.get("version")
        for record in pixi.list_script_packages(str(notebook), cwd=str(tmp_path))
    }
    assert packages.get("marimo") == "0.23.0"

    # ...but a launch into it still imports the fork, same as test 1.
    data = _run_json(environment, CHECK_FORK)
    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_conda_and_pypi_dependencies_in_one_notebook(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(
        "# /// script\n"
        '# requires-python = "==3.12.*"\n'
        "# dependencies = [\n"
        '#     "marimo",\n'
        '#     "six",\n'
        "# ]\n"
        "#\n"
        "# [tool.pixi.workspace]\n"
        '# channels = ["conda-forge"]\n'
        "#\n"
        "# [tool.pixi.dependencies]\n"
        '# jq = "*"\n'
        "# ///\n" + NOTEBOOK
    )
    environment = _sync(notebook)

    code = (
        "import json, shutil, six\n"
        "print(json.dumps({'jq': shutil.which('jq'), "
        "'six_version': six.__version__}))\n"
    )
    data = _run_json(environment, code)

    assert data["jq"] is not None
    assert Path(data["jq"]).is_relative_to(environment.root)
    assert data["six_version"]


def test_per_notebook_environments_are_isolated(tmp_path):
    check_six_version = (
        "import json, six\nprint(json.dumps({'six_version': six.__version__}))\n"
    )
    versions = {}
    for pin in ("1.16.0", "1.17.0"):
        notebook = tmp_path / f"nb-{pin}.py"
        notebook.write_text(
            "# /// script\n"
            '# requires-python = "==3.12.*"\n'
            "# dependencies = [\n"
            '#     "marimo",\n'
            f'#     "six=={pin}",\n'
            "# ]\n"
            "# ///\n" + NOTEBOOK
        )
        environment = _sync(notebook)
        versions[pin] = _run_json(environment, check_six_version)["six_version"]

    assert versions == {"1.16.0": "1.16.0", "1.17.0": "1.17.0"}


def test_requires_python_default_is_3_12(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(NOTEBOOK)

    script_metadata.ensure_metadata_block(str(notebook))
    assert 'requires-python = "==3.12.*"' in notebook.read_text()

    environment = _sync(notebook)
    data = _run_json(
        environment,
        "import json, sys\n"
        "print(json.dumps({'version': list(sys.version_info[:2])}))\n",
    )
    assert data["version"] == [3, 12]


def test_cache_job_worker_renders_without_editing_the_notebook(tmp_path):
    notebook = tmp_path / "nb.py"
    original = HEADER + NOTEBOOK
    notebook.write_text(original)

    result = subprocess.run(
        [sys.executable, str(CACHE_SCRIPT), "--include-code", str(tmp_path)],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    assert result.returncode == 0, result.stderr

    export_dir = tmp_path / "__marimo__"
    outputs = [
        export_dir / "session" / "nb.py.json",
        export_dir / "notebook" / "nb.py.json",
        export_dir / "nb.html",
        export_dir / "nb.md",
    ]
    for path in outputs:
        assert path.is_file() and path.stat().st_size > 0, result.stderr

    assert notebook.read_bytes() == original.encode()


def test_offline_first_overlay(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)
    environment = _sync(notebook)

    data = _run_json(environment, CHECK_FORK, extra_env=CUT_NETWORK)

    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_seeded_environment_is_reused_offline(monkeypatch):
    # pixi keys a script environment on the notebook's path as well as its
    # header, so the canonical notebook goes where the seed step synced it.
    notebook = Path("/home/me/workspace/readme.py")
    notebook.write_text(HEADER + NOTEBOOK)
    for name, value in CUT_NETWORK.items():
        monkeypatch.setenv(name, value)
    try:
        start = time.monotonic()
        _sync(notebook)
        elapsed = time.monotonic() - start
    finally:
        notebook.unlink()
    assert elapsed < 5

    # The virtual packages the seed was keyed on, the same on every node.
    info = subprocess.run(
        ["pixi", "info", "--json"], capture_output=True, text=True, check=True
    )
    virtual = dict(
        package.split("=", 1) for package in json.loads(info.stdout)["virtual_packages"]
    )
    assert virtual["__linux"] == "4.19=0"
    assert virtual["__archspec"] == f"1={platform.machine()}"

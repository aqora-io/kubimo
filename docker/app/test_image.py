"""Tests that only make sense inside the built marimo image.

Each test builds a throwaway notebook in tmp_path (or where the image's seed
put one) and drives it through marimo's own sandbox API, under each backend a
workspace's runtime can select: `sync_notebook_async` installs its
environment with the real uv or pixi binary, then `launch` plus `subprocess`
runs a short check inside it with the fork wheel overlaid, exactly as a
kernel launches. The child prints its result as one line of JSON.

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
from marimo._environments import backends, script_metadata
from marimo._environments.overlay import runtime_overlay
from marimo._environments.uv import UvCommandError

pytestmark = pytest.mark.image


@pytest.fixture(params=["uv", "pixi"])
def backend(request):
    """The sandbox backend a workspace's runtime selects: Uv or Conda."""
    return request.param

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

# Printed by a child launched in a notebook's environment: what the image's
# `dot` renders, found on the PATH a kernel gets, as the graphviz package
# finds it.
RENDER_WITH_DOT = """
import json, subprocess
svg = subprocess.run(
    ["dot", "-Tsvg"], input="digraph { a -> b }", capture_output=True,
    text=True, check=True,
).stdout
print(json.dumps({"svg": svg}))
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


def _sync(notebook, backend, *, timeout=300):
    """Install `notebook`'s environment with the real `backend` binary."""
    return asyncio.run(
        asyncio.wait_for(
            backends.sync_notebook_async(str(notebook), backend=backend),
            timeout=timeout,
        )
    )


def _run_json(environment, code, backend, *, extra_env=None, timeout=300):
    """Launch `code` in `environment` with the runtime overlay, as a kernel
    would, and parse the JSON its stdout prints."""
    plan = backends.launch(
        environment, ["-c", code], backend=backend, overlay=runtime_overlay()
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


def test_fork_kernel_overlays_the_built_wheel(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)

    environment = _sync(notebook, backend)
    data = _run_json(environment, CHECK_FORK, backend)

    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_graphviz_renders_from_a_kernel(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)

    environment = _sync(notebook, backend)
    data = _run_json(environment, RENDER_WITH_DOT, backend)

    assert "<svg" in data["svg"]


def test_user_pinned_marimo_is_shadowed(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    notebook.write_text(
        "# /// script\n"
        '# requires-python = "==3.12.*"\n'
        "# dependencies = [\n"
        '#     "marimo==0.23.0",\n'
        "# ]\n"
        "# ///\n" + NOTEBOOK
    )
    environment = _sync(notebook, backend)

    # The notebook's own environment really has the pinned release...
    installed = subprocess.run(
        [
            environment.python,
            "-c",
            "from importlib.metadata import version; print(version('marimo'))",
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    assert installed.stdout.strip() == "0.23.0"

    # ...but a launch into it still imports the fork, same as test 1.
    data = _run_json(environment, CHECK_FORK, backend)
    assert "/opt/marimo/dist" in data["direct_url"]["url"]


CONDA_AND_PYPI = (
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
    "# ///\n"
)

WHICH_JQ_AND_SIX = (
    "import json, shutil, six\n"
    "print(json.dumps({'jq': shutil.which('jq'), "
    "'six_version': six.__version__}))\n"
)


def test_conda_and_pypi_dependencies_in_one_notebook(tmp_path):
    notebook = tmp_path / "nb.py"
    notebook.write_text(CONDA_AND_PYPI + NOTEBOOK)
    environment = _sync(notebook, "pixi")
    data = _run_json(environment, WHICH_JQ_AND_SIX, "pixi")

    assert data["jq"] is not None
    assert Path(data["jq"]).is_relative_to(environment.root)
    assert data["six_version"]


def test_uv_ignores_conda_dependencies(tmp_path):
    # What the Uv runtime gives a notebook written for the Conda one: its PyPI
    # dependencies, and none of its [tool.pixi] tables.
    notebook = tmp_path / "nb.py"
    notebook.write_text(CONDA_AND_PYPI + NOTEBOOK)
    environment = _sync(notebook, "uv")
    data = _run_json(environment, WHICH_JQ_AND_SIX, "uv")

    assert data["jq"] is None or not Path(data["jq"]).is_relative_to(environment.root)
    assert data["six_version"]


def test_per_notebook_environments_are_isolated(tmp_path, backend):
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
        environment = _sync(notebook, backend)
        versions[pin] = _run_json(environment, check_six_version, backend)[
            "six_version"
        ]

    assert versions == {"1.16.0": "1.16.0", "1.17.0": "1.17.0"}


def test_requires_python_default_is_3_12(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    notebook.write_text(NOTEBOOK)

    script_metadata.ensure_metadata_block(str(notebook))
    assert 'requires-python = "==3.12.*"' in notebook.read_text()

    environment = _sync(notebook, backend)
    data = _run_json(
        environment,
        "import json, sys\n"
        "print(json.dumps({'version': list(sys.version_info[:2])}))\n",
        backend,
    )
    assert data["version"] == [3, 12]


def test_cache_job_worker_renders_without_editing_the_notebook(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    original = HEADER + NOTEBOOK
    notebook.write_text(original)

    result = subprocess.run(
        [
            sys.executable,
            str(CACHE_SCRIPT),
            "--include-code",
            "--backend",
            backend,
            str(tmp_path),
        ],
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


def test_offline_first_overlay(tmp_path, backend):
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)
    environment = _sync(notebook, backend)

    data = _run_json(environment, CHECK_FORK, backend, extra_env=CUT_NETWORK)

    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_seeded_environment_is_reused_offline(monkeypatch, backend):
    # Both backends key a script environment on the notebook's path, so the
    # canonical notebook goes where the seed step synced it.
    notebook = Path("/home/me/workspace/readme.py")
    notebook.write_text(HEADER + NOTEBOOK)
    for name, value in CUT_NETWORK.items():
        monkeypatch.setenv(name, value)
    try:
        start = time.monotonic()
        _sync(notebook, backend)
        elapsed = time.monotonic() - start
    finally:
        notebook.unlink()
    assert elapsed < 5
    if backend != "pixi":
        return

    # The virtual packages the seed was keyed on, the same on every node.
    info = subprocess.run(
        ["pixi", "info", "--json"], capture_output=True, text=True, check=True
    )
    virtual = dict(
        package.split("=", 1) for package in json.loads(info.stdout)["virtual_packages"]
    )
    assert virtual["__linux"] == "4.19=0"
    assert virtual["__archspec"] == f"1={platform.machine()}"


def test_prebuilt_overlay_is_reused_as_is(backend):
    prebuild = subprocess.run(
        [sys.executable, str(Path(__file__).parent / "kubimo_prebuild_overlay.py")],
        capture_output=True,
        text=True,
        timeout=300,
        check=False,
    )
    assert prebuild.returncode == 0, prebuild.stderr
    notebook = Path("/home/me/workspace/readme.py")
    assert not notebook.exists()

    notebook.write_text(HEADER + NOTEBOOK)
    try:
        environment = _sync(notebook, backend)
    finally:
        notebook.unlink()
    plan = backends.launch(
        environment, ["-c", "pass"], backend=backend, overlay=runtime_overlay()
    )
    # What kubimo-uv execs, run directly: its trial launch would otherwise
    # build a missing overlay out of sight. A reused overlay adds no
    # ephemeral environment to uv's cache.
    run = list(plan.argv).index("run")
    environments = Path(plan.env["UV_CACHE_DIR"]) / "environments-v2"
    before = sorted(environments.iterdir())
    result = subprocess.run(
        ["uv", "run", "--offline", *plan.argv[run + 1 :]],
        env=dict(plan.env),
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert sorted(environments.iterdir()) == before


def test_a_new_uv_notebook_syncs_offline(tmp_path, monkeypatch):
    # The seed keeps the PyPI marimo the canonical header names, so a new
    # notebook's environment resolves from the cache alone: kubimo-uv tries
    # the sync offline first.
    notebook = tmp_path / "nb.py"
    notebook.write_text(HEADER + NOTEBOOK)
    for name, value in CUT_NETWORK.items():
        monkeypatch.setenv(name, value)

    environment = _sync(notebook, "uv")
    data = _run_json(environment, CHECK_FORK, "uv")

    assert "/opt/marimo/dist" in data["direct_url"]["url"]


def test_an_uncached_package_fails_the_uv_sync_cleanly(tmp_path, monkeypatch):
    # Offline, then online, both refused: marimo sees uv's own failure. Had
    # kubimo-uv printed its first attempt too, the report would not parse, a
    # UvSyncReportError instead.
    notebook = tmp_path / "nb.py"
    notebook.write_text(
        "# /// script\n"
        '# requires-python = "==3.12.*"\n'
        "# dependencies = [\n"
        '#     "marimo",\n'
        '#     "pyjokes==0.8.3",\n'
        "# ]\n"
        "# ///\n" + NOTEBOOK
    )
    for name, value in CUT_NETWORK.items():
        monkeypatch.setenv(name, value)

    with pytest.raises(UvCommandError):
        _sync(notebook, "uv")


def _flat_index_wheel(directory, name, version, source):
    """Write a pure-Python wheel of `name` into `directory`, a flat index."""
    import base64
    import hashlib
    import zipfile

    info = f"{name}-{version}.dist-info"
    files = {
        f"{name}/__init__.py": source,
        f"{info}/METADATA": f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n",
        f"{info}/WHEEL": "Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
    }
    record = []
    for path, text in files.items():
        digest = hashlib.sha256(text.encode()).digest()
        encoded = base64.urlsafe_b64encode(digest).rstrip(b"=").decode()
        record.append(f"{path},sha256={encoded},{len(text.encode())}")
    files[f"{info}/RECORD"] = "\n".join([*record, f"{info}/RECORD,,"]) + "\n"
    directory.mkdir(parents=True)
    with zipfile.ZipFile(
        directory / f"{name}-{version}-py3-none-any.whl", "w"
    ) as wheel:
        for path, text in files.items():
            wheel.writestr(path, text)


def test_a_migrated_uv_header_resolves_from_the_workspace_index(tmp_path, monkeypatch):
    # A legacy uv workspace whose package only an index of its own serves (a
    # local flat one, so nothing goes online): the migrated header of a
    # notebook below the root names that index, rebased onto the notebook,
    # and the real uv resolves the package from it.
    import kubimo_migrate

    workspace = tmp_path / "workspace"
    _flat_index_wheel(workspace / "wheels", "tinypkg", "0.1.0", "VALUE = 42\n")
    (workspace / "pyproject.toml").write_text(
        '[project]\ndependencies = ["tinypkg"]\n\n'
        '[[tool.uv.index]]\nname = "local"\nurl = "./wheels"\nformat = "flat"\n'
        "explicit = true\n\n"
        '[tool.uv.sources]\ntinypkg = { index = "local" }\n\n'
        '[tool.marimo.venv]\npath = "/home/me/venv"\n'
    )
    notebook = workspace / "analysis" / "nb.py"
    notebook.parent.mkdir()
    notebook.write_text(
        NOTEBOOK.replace("    return", "    import tinypkg\n    return")
    )
    for name, value in CUT_NETWORK.items():
        monkeypatch.setenv(name, value)

    assert kubimo_migrate.migrate(workspace, backend="uv") == 0
    environment = _sync(notebook, "uv")
    data = _run_json(
        environment,
        'import json, tinypkg; print(json.dumps({"value": tinypkg.VALUE}))',
        "uv",
    )

    assert data == {"value": 42}

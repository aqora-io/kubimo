"""The image's preinstalled Python must satisfy notebook requirements offline.

These checks use the shipped uv directly. They catch a missing system Python
seed, a stale managed interpreter taking priority, or a global UV_PYTHON
override that forces newer notebooks onto the seeded interpreter.
"""

import json
import os
import shutil
import subprocess
from pathlib import Path

import pytest

pytestmark = pytest.mark.image


def _run_script(tmp_path, requirement, *, managed=None):
    notebook = tmp_path / "python-selection.py"
    notebook.write_text(
        "# /// script\n"
        f'# requires-python = "{requirement}"\n'
        "# dependencies = []\n"
        "# ///\n"
        "import json, sys\n"
        "print(json.dumps({'version': list(sys.version_info[:2]), "
        "'base_prefix': sys.base_prefix}))\n"
    )
    env = os.environ.copy()
    if managed is not None:
        env["UV_PYTHON_INSTALL_DIR"] = str(managed)
    return subprocess.run(
        [
            "uv",
            "run",
            "--no-config",
            "--offline",
            "--no-python-downloads",
            "--script",
            str(notebook),
        ],
        cwd=tmp_path,
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )


def test_broad_requirement_uses_preinstalled_python_3_13(tmp_path):
    result = _run_script(tmp_path, ">=3.13")

    assert result.returncode == 0, result.stderr
    data = json.loads(result.stdout)
    assert data["version"] == [3, 13]
    system_python = shutil.which("python3.13")
    assert system_python is not None
    assert (
        Path(data["base_prefix"]).resolve()
        == Path(system_python).resolve().parent.parent
    )


@pytest.mark.parametrize("requirement", [">=3.14", "==3.15.*"])
def test_newer_requirements_are_not_coerced_to_the_seed(tmp_path, requirement):
    # Only 3.12 and 3.13 ship in this image. A newer explicit requirement must
    # request another interpreter; disabling downloads makes that observable.
    result = _run_script(tmp_path, requirement)

    assert result.returncode != 0
    assert "No interpreter found" in result.stderr
    assert requirement in result.stderr


@pytest.fixture(scope="module")
def newer_managed_python(tmp_path_factory):
    managed = tmp_path_factory.mktemp("managed-python")
    result = subprocess.run(
        [
            "uv", "python", "install", "--install-dir", str(managed),
            "--no-bin", "3.15.2",
        ],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    return managed


@pytest.mark.parametrize(
    ("requirement", "version"),
    [
        (">=3.13", [3, 13]),
        (">=3.14", [3, 15]),
        ("==3.15.*", [3, 15]),
        ("==3.12.*", [3, 12]),
    ],
)
def test_system_preference_honors_requirements_with_newer_managed_python(
    tmp_path, newer_managed_python, requirement, version
):
    result = _run_script(tmp_path, requirement, managed=newer_managed_python)

    assert result.returncode == 0, result.stderr
    assert json.loads(result.stdout)["version"] == version


@pytest.mark.parametrize(
    ("requirement", "version"),
    [(">=3.13", [3, 13]), (">=3.14", [3, 15]), ("==3.15.*", [3, 15])],
)
def test_sync_reselects_a_compatible_python_for_an_existing_cached_environment(
    tmp_path, newer_managed_python, monkeypatch, requirement, version
):
    from marimo._environments.environment import sync

    monkeypatch.setenv("UV_CACHE_DIR", str(tmp_path / "uv-cache"))
    monkeypatch.setenv("UV_PYTHON_INSTALL_DIR", str(newer_managed_python))
    monkeypatch.setenv("UV_PYTHON_PREFERENCE", "managed")
    initial = _run_script(tmp_path, requirement)
    assert initial.returncode == 0, initial.stderr
    assert json.loads(initial.stdout)["version"] == [3, 15]
    notebook = tmp_path / "python-selection.py"
    original = notebook.read_bytes()
    user_data = tmp_path / "user-data.txt"
    user_data.write_text("preserve this workspace file")

    monkeypatch.setenv("UV_PYTHON_PREFERENCE", "system")
    environment = sync(str(notebook), cwd=str(tmp_path))
    result = subprocess.run(
        [environment.python, str(notebook)],
        capture_output=True, text=True, timeout=30, check=True,
    )

    assert json.loads(result.stdout)["version"] == version
    assert notebook.read_bytes() == original
    assert user_data.read_text() == "preserve this workspace file"

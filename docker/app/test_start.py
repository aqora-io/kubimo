"""start.sh's guarantees, checked against the real marimo in the image.

The token reaches marimo on stdin alone, never argv, the environment kernels
inherit or the log; run mode serves notebooks from app hosts unless opted out;
and a warm pod reports its claim-time migration in the order the agent relies
on. A stray `set -x`, an exported helper or a subshell would regress any of
these without failing anything else.

Run inside the built image by the `marimo-test` bake target, like
test_image.py.
"""

import contextlib
import os
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

import pytest
import tomllib

pytestmark = pytest.mark.image

START = "/setup/start.sh"
TOKEN = "kubimo-test-token-5d1e"

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

# A workspace made for the old uv image, which the migration gives headers.
LEGACY_PYPROJECT = """[project]
name = "workspace"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = []

[tool.marimo.venv]
path = "/home/me/venv"
"""


def _workspace(tmp_path: Path, files: dict[str, str]) -> Path:
    workspace = tmp_path / "workspace"
    workspace.mkdir()
    for name, text in files.items():
        (workspace / name).write_text(text)
    return workspace


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextlib.contextmanager
def _serving(tmp_path, workspace, command, *, token_via="flag", env=None):
    """start.sh serving `workspace` as a runner pod runs it, until the block
    ends. Yields the port and the file collecting everything it printed."""
    port = _free_port()
    home = tmp_path / "home"
    home.mkdir(exist_ok=True)
    child_env = {
        name: value
        for name, value in os.environ.items()
        if name not in ("MARIMO_TOKEN", "XDG_CONFIG_HOME")
    }
    # marimo's config and state stay in tmp_path; the caches the image's
    # environment variables point at do not move.
    child_env["HOME"] = str(home)
    child_env.update(env or {})
    argv = ["bash", START, command, "--host", "127.0.0.1", "--port", str(port)]
    if token_via == "flag":
        argv += ["--token", TOKEN]
    else:
        child_env["MARIMO_TOKEN"] = TOKEN
    log = tmp_path / f"{command}.log"
    with log.open("wb") as output:
        process = subprocess.Popen(
            argv,
            cwd=workspace,
            env=child_env,
            stdout=output,
            stderr=subprocess.STDOUT,
        )
    try:
        _wait_for(lambda: _healthy(port, process, log), log, timeout=120)
        yield port, log
    finally:
        process.terminate()
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def _healthy(port: int, process: subprocess.Popen, log: Path) -> bool:
    if process.poll() is not None:
        pytest.fail(f"start.sh exited with {process.returncode}:\n{log.read_text()}")
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=2):
            return True
    except OSError:
        return False


def _wait_for(predicate, log: Path, *, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            pytest.fail(f"timed out; start.sh printed:\n{log.read_text()}")
        time.sleep(0.1)


def _connections_status(port: int, token: str | None = None) -> int:
    """What marimo answers kubimo's own status poll: it authenticates the way
    runner_status does, with the token as a bearer."""
    request = urllib.request.Request(f"http://127.0.0.1:{port}/api/status/connections")
    if token is not None:
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def _processes_holding(secret: bytes) -> list[str]:
    """Every process whose argv or initial environment holds `secret`."""
    found = []
    for process in Path("/proc").iterdir():
        if not process.name.isdigit():
            continue
        for name in ("cmdline", "environ"):
            with contextlib.suppress(OSError):
                if secret in (process / name).read_bytes():
                    argv = (process / "cmdline").read_bytes().replace(b"\0", b" ")
                    found.append(f"{name} of {argv[:200]!r}")
    return found


@pytest.mark.parametrize("token_via", ["flag", "env"])
@pytest.mark.parametrize("command", ["edit", "run"])
def test_the_token_reaches_marimo_on_stdin_alone(tmp_path, command, token_via):
    workspace = _workspace(tmp_path, {"readme.py": HEADER + NOTEBOOK})
    with _serving(tmp_path, workspace, command, token_via=token_via) as (port, log):
        # marimo got it: its API turns away anyone without it.
        assert _connections_status(port) == 401
        assert _connections_status(port, TOKEN) == 200
        # ...and nothing else did: kernels inherit the server's environment.
        assert _processes_holding(TOKEN.encode()) == []
    assert TOKEN not in log.read_text()


@pytest.mark.parametrize(("opt_out", "isolated"), [(None, True), ("false", False)])
def test_run_mode_serves_from_app_hosts_unless_opted_out(tmp_path, opt_out, isolated):
    workspace = _workspace(tmp_path, {"readme.py": HEADER + NOTEBOOK})
    env = {} if opt_out is None else {"KUBIMO_ISOLATE_APPS": opt_out}
    with _serving(tmp_path, workspace, "run", env=env):
        config = tmp_path / "home" / ".config" / "marimo" / "marimo.toml"
        experimental = tomllib.loads(config.read_text())["experimental"]
    assert experimental["isolate_apps"] is isolated


def test_a_warm_pod_reports_its_migration_once_claimed(tmp_path):
    workspace = _workspace(
        tmp_path, {"pyproject.toml": LEGACY_PYPROJECT, "readme.py": NOTEBOOK}
    )
    claim = tmp_path / "claim" / "claimed"
    report = tmp_path / "migration"
    env = {"KUBIMO_CLAIM_MARKER": str(claim), "KUBIMO_MIGRATION_MARKER": str(report)}
    with _serving(tmp_path, workspace, "edit", env=env) as (_, log):
        # Written before marimo serves: a claim landing from here on is held
        # until the migration it starts reports back.
        assert report.read_text() == "pending"
        assert (workspace / "readme.py").read_text() == NOTEBOOK
        # The waiter started before the exec keeps no token in its argv.
        assert _processes_holding(TOKEN.encode()) == []

        # What the agent does once the tenant's files are hydrated.
        claim.parent.mkdir()
        claim.with_name("claimed.tmp").write_text("uid-1")
        claim.with_name("claimed.tmp").rename(claim)
        _wait_for(lambda: report.read_text() == "uid-1", log, timeout=60)
        # Named back only once the header it was waited for is written.
        assert (workspace / "readme.py").read_text().startswith(HEADER)
    assert TOKEN not in log.read_text()

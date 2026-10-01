"""The node template's overlay pre-build: each backend within its share of
the agent's 300 s, whatever the sync or launch it drives does."""

import asyncio
import os
import signal
import subprocess
import time
from pathlib import Path

import pytest
from marimo._environments import backends, overlay
from marimo._environments.environment import ProcessPlan

import kubimo_prebuild_overlay


@pytest.fixture
def one_second_share(monkeypatch):
    monkeypatch.setattr(kubimo_prebuild_overlay, "TIMEOUT_SECONDS", 1)
    monkeypatch.setattr(overlay, "runtime_overlay", lambda: None)


def _synced(monkeypatch, seconds=0.0):
    """Stub both sync entry points to return after `seconds`."""

    async def sync_notebook_async(path, *, backend):
        await asyncio.sleep(seconds)

    def sync_notebook(path, *, backend):
        time.sleep(seconds)

    monkeypatch.setattr(backends, "sync_notebook_async", sync_notebook_async)
    monkeypatch.setattr(backends, "sync_notebook", sync_notebook)


def _gone(pid: int) -> bool:
    """Whether `pid` has exited (a zombie waiting on its reaper counts)."""
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        except (FileNotFoundError, ProcessLookupError):
            return True
        if state == "Z":
            return True
        time.sleep(0.1)
    return False


def test_a_launch_past_its_share_is_killed_with_what_it_started(
    tmp_path, monkeypatch, one_second_share
):
    # kubimo-uv's offline trial and the kernel uv starts are the launch's
    # grandchildren; left running, they would write into the cache while the
    # agent copies the template.
    pid_file = tmp_path / "grandchild"
    _synced(monkeypatch)
    monkeypatch.setattr(
        backends,
        "launch",
        lambda environment, args, *, backend, overlay: ProcessPlan(
            argv=("bash", "-c", f"sleep 60 & echo $! > {pid_file}; wait"),
            env=dict(os.environ),
        ),
    )

    with pytest.raises(subprocess.TimeoutExpired):
        kubimo_prebuild_overlay.prebuild("uv")

    grandchild = int(pid_file.read_text())
    try:
        assert _gone(grandchild)
    finally:
        if not _gone(grandchild):
            os.kill(grandchild, signal.SIGKILL)


def test_an_interrupted_launch_is_killed_with_what_it_started(tmp_path, monkeypatch):
    # The agent's `timeout -s INT` interrupts the pre-build with a SIGINT that
    # only its own process group gets: the launch, in a session of its own,
    # would otherwise go on writing into the cache during the template copy.
    pid_file = tmp_path / "grandchild"
    monkeypatch.setattr(kubimo_prebuild_overlay, "TIMEOUT_SECONDS", 30)
    monkeypatch.setattr(overlay, "runtime_overlay", lambda: None)
    _synced(monkeypatch)
    monkeypatch.setattr(
        backends,
        "launch",
        lambda environment, args, *, backend, overlay: ProcessPlan(
            argv=("bash", "-c", f"sleep 60 & echo $! > {pid_file}; wait"),
            env=dict(os.environ),
        ),
    )

    def interrupt(signum, frame):
        raise KeyboardInterrupt

    previous = signal.signal(signal.SIGALRM, interrupt)
    signal.alarm(1)
    try:
        with pytest.raises(KeyboardInterrupt):
            kubimo_prebuild_overlay.prebuild("uv")
    finally:
        signal.alarm(0)
        signal.signal(signal.SIGALRM, previous)

    grandchild = int(pid_file.read_text())
    try:
        assert _gone(grandchild)
    finally:
        if not _gone(grandchild):
            os.kill(grandchild, signal.SIGKILL)


def test_a_sync_past_its_share_gives_up(monkeypatch, one_second_share):
    # Otherwise one hung sync spends the whole budget and the other backend
    # never gets its turn.
    _synced(monkeypatch, seconds=5)
    start = time.monotonic()

    with pytest.raises(TimeoutError):
        kubimo_prebuild_overlay.prebuild("uv")

    assert time.monotonic() - start < 4

"""Pre-build the kernel overlays into the node template the agent stages.

A kernel runs in its notebook's environment, built by the workspace's
sandbox backend (uv or pixi), with this image's marimo wheel layered on top
by uv (MARIMO_RUNTIME_WHEEL, launched through kubimo-uv). The first kernel in
a slot otherwise builds that overlay and compiles its imports under gVisor.
The image cannot ship the result: uv keys a
file:// wheel on its ctime, which unpacking the image on a node sets. Every
container on a node shares that unpacked layer, runc and gVisor alike, so
building once per backend and Python version while the agent stages
/data/template, before it copies /home/me, gives every slot overlays uv
reuses as they are. If the node later unpacks the image again, uv misses and
each slot builds its own, as before.

Run by the agent's marimo-template initContainer as root, before the copy.
Best effort: the caller stages the template even if one preparation fails.
"""

import asyncio
import contextlib
import os
import pwd
import signal
import subprocess
import sys
import time
from pathlib import Path

USER = "me"
SEED_NOTEBOOK = Path("/setup/seed-notebook.py")
# Both backends key a script environment on the notebook's path, so the
# canonical notebook goes where the image's seed step synced it: the
# environment every new workspace's readme.py resolves to.
NOTEBOOK = Path(f"/home/{USER}/workspace/readme.py")
# What a kernel imports on startup, so their bytecode lands in the template.
KERNEL_IMPORTS = (
    "import marimo._ipc.launch_kernel, marimo._runtime.runtime, "
    "marimo._session.app_host.main"
)
# uv first: prepare its 3.12 and 3.13 overlays before pixi's 3.12 overlay.
# Three bounded preparations leave cleanup time in the caller's 300 s budget.
BACKENDS = ("uv", "pixi")
TIMEOUT_SECONDS = 90


def drop_privileges() -> None:
    """The template must stay writable by the uid runners run as."""
    if os.getuid() != 0:
        return
    user = pwd.getpwnam(USER)
    os.setgroups([])
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)
    os.environ["HOME"] = user.pw_dir


def prebuild(backend: str, notebook: Path | None = None) -> None:
    """Build `backend`'s overlay on the canonical notebook's environment,
    within TIMEOUT_SECONDS."""
    from marimo._environments import backends
    from marimo._environments.overlay import runtime_overlay

    notebook = NOTEBOOK if notebook is None else notebook
    start = time.monotonic()
    environment = asyncio.run(
        asyncio.wait_for(
            backends.sync_notebook_async(str(notebook), backend=backend),
            timeout=TIMEOUT_SECONDS,
        )
    )
    plan = backends.launch(
        environment,
        ["-c", KERNEL_IMPORTS],
        backend=backend,
        overlay=runtime_overlay(),
    )
    # A session of its own, so a timeout kills what the launch started too:
    # kubimo-uv's offline trial and the kernel uv starts would otherwise go on
    # writing into the cache while the agent copies the template. The agent's
    # own SIGINT reaches only our process group, so its KeyboardInterrupt
    # kills the session as well.
    with subprocess.Popen(
        list(plan.argv), env=dict(plan.env), start_new_session=True
    ) as process:
        try:
            process.wait(timeout=start + TIMEOUT_SECONDS - time.monotonic())
        except BaseException:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGKILL)
            raise
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, plan.argv)
    print(f"pre-built the {backend} kernel overlay in {time.monotonic() - start:.1f}s")


def main() -> int:
    drop_privileges()
    compatibility_notebook = NOTEBOOK.with_name("readme-3.13.py")
    if NOTEBOOK.exists() or compatibility_notebook.exists():
        # Only an image's own, empty workspace qualifies: never a tenant's.
        print(
            "A seed notebook exists; not pre-building the overlays",
            file=sys.stderr,
        )
        return 1
    source = SEED_NOTEBOOK.read_bytes()
    NOTEBOOK.write_bytes(source)
    compatibility_notebook.write_bytes(
        source.replace(b"==3.12.*", b"==3.13.*", 1)
    )
    failed = False
    try:
        for backend in BACKENDS:
            notebooks = (
                (NOTEBOOK, compatibility_notebook)
                if backend == "uv"
                else (NOTEBOOK,)
            )
            for notebook in notebooks:
                try:
                    prebuild(backend, notebook)
                except Exception as error:
                    print(
                        f"pre-building the {backend} kernel overlay failed: {error}",
                        file=sys.stderr,
                    )
                    failed = True
    finally:
        NOTEBOOK.unlink()
        compatibility_notebook.unlink()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

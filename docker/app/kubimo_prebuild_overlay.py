"""Pre-build the kernel overlays into the node template the agent stages.

A kernel runs in its notebook's environment, built by the workspace's
sandbox backend (uv or pixi), with this image's marimo wheel layered on top
by uv (MARIMO_RUNTIME_WHEEL, launched through kubimo-uv). The first kernel in
a slot builds that overlay and compiles the kernel's imports, about 6.5 s of
a first open under gVisor. The image cannot ship the result: uv keys a
file:// wheel on its ctime, which unpacking the image on a node sets. Every
container on a node shares that unpacked layer, runc and gVisor alike, so
building once per backend while the agent stages /data/template, before it
copies /home/me, gives every slot reflinked from the template overlays uv
reuses as they are. If the node later unpacks the image again, uv misses and
each slot builds its own, as before.

Run by the agent's marimo-template initContainer as root, before the copy.
Best effort, backend by backend: the caller stages the template whatever
this exits with.
"""

import os
import pwd
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
KERNEL_IMPORTS = "import marimo._ipc.launch_kernel, marimo._runtime.runtime"
# uv first: its overlay is the quicker, and both must fit in the caller's
# 300 s budget, each launch in its own share of it.
BACKENDS = ("uv", "pixi")
TIMEOUT_SECONDS = 120


def drop_privileges() -> None:
    """The template must stay writable by the uid runners run as."""
    if os.getuid() != 0:
        return
    user = pwd.getpwnam(USER)
    os.setgroups([])
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)
    os.environ["HOME"] = user.pw_dir


def prebuild(backend: str) -> None:
    """Build `backend`'s overlay on the canonical notebook's environment."""
    from marimo._environments import backends
    from marimo._environments.overlay import runtime_overlay

    start = time.monotonic()
    environment = backends.sync_notebook(str(NOTEBOOK), backend=backend)
    plan = backends.launch(
        environment,
        ["-c", KERNEL_IMPORTS],
        backend=backend,
        overlay=runtime_overlay(),
    )
    subprocess.run(
        list(plan.argv),
        env=dict(plan.env),
        check=True,
        timeout=TIMEOUT_SECONDS,
    )
    print(f"pre-built the {backend} kernel overlay in {time.monotonic() - start:.1f}s")


def main() -> int:
    drop_privileges()
    if NOTEBOOK.exists():
        # Only an image's own, empty workspace qualifies: never a tenant's.
        print(f"{NOTEBOOK} exists; not pre-building the overlays", file=sys.stderr)
        return 1
    NOTEBOOK.write_bytes(SEED_NOTEBOOK.read_bytes())
    failed = False
    try:
        for backend in BACKENDS:
            try:
                prebuild(backend)
            except Exception as error:
                print(
                    f"pre-building the {backend} kernel overlay failed: {error}",
                    file=sys.stderr,
                )
                failed = True
    finally:
        NOTEBOOK.unlink()
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

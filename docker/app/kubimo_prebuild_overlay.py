"""Pre-build the kernel overlay into the node template the agent stages.

A kernel runs in its notebook's pixi environment with this image's marimo
wheel layered on top by uv (MARIMO_RUNTIME_WHEEL, launched through
kubimo-uv). The first kernel in a slot builds that overlay and compiles the
kernel's imports, about 6.5 s of a first open under gVisor. The image cannot
ship the result: uv keys a file:// wheel on its ctime, which unpacking the
image on a node sets. Every container on a node shares that unpacked layer,
runc and gVisor alike, so building once while the agent stages
/data/template, before it copies /home/me, gives every slot reflinked from
the template an overlay uv reuses as it is. If the node later unpacks the
image again, uv misses and each slot builds its own, as before.

Run by the agent's marimo-template initContainer as root, before the copy.
Best effort: the caller stages the template whatever this exits with.
"""

import os
import pwd
import subprocess
import sys
import time
from pathlib import Path

USER = "me"
SEED_NOTEBOOK = Path("/setup/seed-notebook.py")
# pixi keys a script environment on the notebook's path as well as its
# header, so the canonical notebook goes where the image's seed step synced
# it: the environment every new workspace's readme.py resolves to.
NOTEBOOK = Path(f"/home/{USER}/workspace/readme.py")
# What a kernel imports on startup, so their bytecode lands in the template.
KERNEL_IMPORTS = "import marimo._ipc.launch_kernel, marimo._runtime.runtime"
TIMEOUT_SECONDS = 240


def drop_privileges() -> None:
    """The template must stay writable by the uid runners run as."""
    if os.getuid() != 0:
        return
    user = pwd.getpwnam(USER)
    os.setgroups([])
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)
    os.environ["HOME"] = user.pw_dir


def main() -> int:
    drop_privileges()
    from marimo._environments import backends
    from marimo._environments.overlay import runtime_overlay

    if NOTEBOOK.exists():
        # Only an image's own, empty workspace qualifies: never a tenant's.
        print(f"{NOTEBOOK} exists; not pre-building the overlay", file=sys.stderr)
        return 1
    NOTEBOOK.write_bytes(SEED_NOTEBOOK.read_bytes())
    try:
        start = time.monotonic()
        environment = backends.sync_notebook(str(NOTEBOOK), backend="pixi")
        plan = backends.launch(
            environment,
            ["-c", KERNEL_IMPORTS],
            backend="pixi",
            overlay=runtime_overlay(),
        )
        subprocess.run(
            list(plan.argv),
            env=dict(plan.env),
            check=True,
            timeout=TIMEOUT_SECONDS,
        )
    finally:
        NOTEBOOK.unlink()
    print(f"pre-built the kernel overlay in {time.monotonic() - start:.1f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())

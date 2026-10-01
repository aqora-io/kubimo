"""Cache a workspace's marimo notebooks for marimo-ssr to render.

Notebooks are found statically, never imported. Each one is exported by a
worker, this script with --one, launched inside the notebook's own pixi
environment, or on this interpreter for a notebook without a PEP 723 header,
like marimo run. A few run at once, and notebooks are never edited.
"""

import argparse
import asyncio
import collections
import json
import logging
import os
import random
from pathlib import Path

from kubimo_walk import find_files
from marimo._convert import MarimoConvert
from marimo._environments import backends, process, script_metadata
from marimo._environments.errors import EnvironmentManagerError
from marimo._environments.overlay import runtime_overlay
from marimo._export.file import export_html, export_markdown
from marimo._export.requests import (
    HTMLFileExportRequest,
    MarkdownFileExportRequest,
    NotebookExecutionOptions,
)
from marimo._schemas.export_options import HTMLExportOptions, MarkdownExportOptions
from marimo._server.files.directory_scanner import is_marimo_app
from marimo._utils.marimo_path import MarimoPath
from marimo._utils.paths import notebook_output_dir

logger = logging.getLogger(__name__)
_LOG_LEVEL_CHOICES = ["debug", "info", "warn", "warning", "error", "critical"]
# Seconds before the one retry of a failed environment sync, picked at random:
# a runner pod may be syncing the same environment.
_RETRY_DELAY = (1, 5)


def _write_export(export_dir: Path, result):
    contents = result.contents
    if isinstance(contents, str):
        contents = contents.encode("utf-8")
    (export_dir / result.download_filename).write_bytes(contents)


async def _cache_app(path: Path, *, include_code: bool):
    marimo_path = MarimoPath(path)
    html_result = await export_html(
        HTMLFileExportRequest(
            path=marimo_path,
            options=HTMLExportOptions(files=(), include_code=include_code),
            execution=NotebookExecutionOptions(cli_args={}, argv=[]),
        )
    )
    md_result = export_markdown(
        MarkdownFileExportRequest(path=marimo_path, options=MarkdownExportOptions())
    )
    # marimo-ssr renders from two snapshots: the session one export_html
    # persists as a side effect, and the notebook one that only the fork's
    # `marimo export json-notebook` writes. Same converter, same file.
    notebook_cache = notebook_output_dir(path) / "notebook" / f"{path.name}.json"
    notebook_cache.parent.mkdir(parents=True, exist_ok=True)
    notebook_cache.write_text(
        json.dumps(MarimoConvert.from_py(path.read_text()).to_notebook_v1())
    )
    export_dir = path.parent / "__marimo__"
    export_dir.mkdir(parents=True, exist_ok=True)
    _write_export(export_dir, html_result)
    _write_export(export_dir, md_result)


async def _launch_plan(path: Path, args: list[str], backend: str):
    """Plan `python <args>` in the notebook's environment, built by `backend`
    as the workspace's runners build it, or on this interpreter for a
    notebook without a header, like marimo run."""
    if script_metadata.loads(path.read_text(encoding="utf-8")) is None:
        return backends.launch_fallback(args)
    try:
        environment = await backends.sync_notebook_async(str(path), backend=backend)
    # pixi's `install --help` probe gives up with a TimeoutError of its own.
    except (EnvironmentManagerError, TimeoutError) as error:
        delay = random.uniform(*_RETRY_DELAY)
        logger.warning(
            f"Syncing the environment of {path} failed, retrying in {delay:.1f}s: "
            f"{_tail(str(error)) or 'timed out'}"
        )
        await asyncio.sleep(delay)
        environment = await backends.sync_notebook_async(str(path), backend=backend)
    return backends.launch(environment, args, backend=backend, overlay=runtime_overlay())


async def _cache_in_worker(
    path: Path, args: list[str], *, root: Path, timeout: float, backend: str
) -> bool:
    """Cache the notebook at `path` by running the worker `args` in its
    environment from the workspace `root`, all within `timeout` seconds;
    whether that worked."""
    logger.info(f"Caching {path}")
    plan = None
    # The worker's last lines, streamed: a timed-out command's output is lost.
    stderr = collections.deque(maxlen=20)
    try:
        async with asyncio.timeout(timeout):
            plan = await _launch_plan(path, args, backend)
            # From the workspace root, like the runner's kernels: relative
            # paths resolve as they do in the live notebook.
            completed = await process.run_command(
                plan.argv, env=plan.env, cwd=str(root), on_stderr=stderr.append
            )
    except EnvironmentManagerError as error:
        logger.error(f"Failed to cache {path}: {_tail(str(error))}")
        return False
    except TimeoutError:
        if plan is None:
            logger.error(f"Failed to cache {path}: syncing its environment timed out")
        else:
            logger.error(
                f"Failed to cache {path}: its worker timed out\n"
                f"{_tail(''.join(stderr))}"
            )
        return False
    except Exception:
        logger.exception(f"Failed to cache {path}")
        return False
    if completed.returncode != 0:
        logger.error(
            f"Failed to cache {path}: exit code {completed.returncode}\n"
            f"{_tail(completed.stderr)}"
        )
        return False
    logger.info(f"Cached {path}")
    return True


def _tail(text: str) -> str:
    """The last lines of a failure's output, where it says what went wrong."""
    return "\n".join(text.strip().splitlines()[-20:])


async def _cache_all_apps(
    directory: str,
    *,
    include_gitignored: bool,
    include_code: bool,
    log_level: str,
    jobs: int,
    timeout: float,
    backend: str,
):
    root = Path(directory).resolve()
    files = find_files(root, include_gitignored=include_gitignored)
    notebooks = []
    for path in files:
        if is_marimo_app(str(path)):
            notebooks.append(path)
        else:
            logger.warning(f"Skipping {path}")

    flags = ["--include-code"] if include_code else []
    flags += ["--log-level", log_level]
    semaphore = asyncio.Semaphore(jobs)

    async def cache_in_turn(path: Path) -> bool:
        args = [os.path.abspath(__file__), "--one", str(path), *flags]
        async with semaphore:
            return await _cache_in_worker(
                path, args, root=root, timeout=timeout, backend=backend
            )

    results = await asyncio.gather(*(cache_in_turn(path) for path in notebooks))
    successful = sum(results)
    failed = len(notebooks) - successful
    skipped = len(files) - len(notebooks)
    logger.info(
        f"Caching complete: {successful} apps cached successfully, {failed} failed, "
        f"{skipped} skipped as not marimo apps"
    )


def main(argv: list[str] | None = None):
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--include-gitignored", help="Include gitignored files", action="store_true"
    )
    parser.add_argument(
        "--include-code",
        action="store_true",
        help="Include code cells in cached HTML output.",
    )
    parser.add_argument(
        "--log-level",
        default="info",
        choices=_LOG_LEVEL_CHOICES,
        help="Log level.",
    )
    parser.add_argument(
        "--jobs",
        type=int,
        default=min(4, os.cpu_count() or 1),
        help="Notebooks cached at once.",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=1800,
        help="Seconds a notebook may take to cache, its environment sync included.",
    )
    parser.add_argument(
        "--backend",
        default="pixi",
        choices=("uv", "pixi"),
        help="The sandbox backend the workspace's runners build environments with.",
    )
    parser.add_argument(
        "--one",
        type=Path,
        metavar="NOTEBOOK",
        help="Cache only this notebook, in this process (the worker).",
    )
    parser.add_argument("directory", nargs="?", default=".", help="Directory to cache")
    args = parser.parse_args(argv)
    logging.basicConfig(level=args.log_level.upper())
    if args.one is not None:
        asyncio.run(_cache_app(args.one, include_code=args.include_code))
    else:
        asyncio.run(
            _cache_all_apps(
                args.directory,
                include_gitignored=args.include_gitignored,
                include_code=args.include_code,
                log_level=args.log_level,
                jobs=args.jobs,
                timeout=args.timeout,
                backend=args.backend,
            )
        )


if __name__ == "__main__":
    main()

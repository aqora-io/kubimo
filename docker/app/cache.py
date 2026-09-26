import logging
from pathlib import Path
import importlib.util
import asyncio
from concurrent.futures import ProcessPoolExecutor
import itertools
import json

import marimo
from marimo._convert import MarimoConvert
from marimo._export.file import export_html, export_markdown
from marimo._export.requests import (
    HTMLFileExportRequest,
    MarkdownFileExportRequest,
    NotebookExecutionOptions,
)
from marimo._schemas.export_options import HTMLExportOptions, MarkdownExportOptions
from marimo._utils.paths import notebook_output_dir
from marimo._utils.marimo_path import MarimoPath

from kubimo_walk import find_files as _get_python_files

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)
_LOG_LEVEL_CHOICES = ["debug", "info", "warning", "error", "critical"]


def _write_export(export_dir: Path, result):
    contents = result.contents
    if isinstance(contents, str):
        contents = contents.encode("utf-8")
    (export_dir / result.download_filename).write_bytes(contents)


async def _cache_app(path: Path, *, include_code: bool):
    logger.info(f"Caching {path}")
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


def _is_app(path: Path):
    logger.info(f"Checking {path}")
    spec = importlib.util.spec_from_file_location(str(path), path)
    if spec is None or spec.loader is None:
        return False
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return hasattr(module, "app") and isinstance(getattr(module, "app"), marimo.App)


def _cache_app_sync(path: Path, include_code: bool, log_level: str):
    logging.getLogger().setLevel(log_level.upper())
    try:
        if _is_app(path):
            asyncio.run(_cache_app(path, include_code=include_code))
            logger.info(f"Cached {path}")
            return True
        else:
            logger.warning(f"Skipping {path}")
            return False
    except Exception as e:
        logger.error(f"Failed to cache {path}: {e}", exc_info=True)
        return False


def _cache_all_apps(
    directory: str,
    *,
    include_gitignored: bool = False,
    include_code: bool = False,
    log_level: str = "info",
):
    files = _get_python_files(directory, include_gitignored=include_gitignored)

    # Run _cache_app in parallel with process workers
    with ProcessPoolExecutor() as executor:
        results = list(
            executor.map(
                _cache_app_sync,
                files,
                itertools.repeat(include_code),
                itertools.repeat(log_level),
            )
        )

    successful = sum(results)
    failed = len(results) - successful
    logger.info(
        f"Caching complete: {successful} apps cached successfully, {failed} failed or skipped"
    )


if __name__ == "__main__":
    import argparse

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
    parser.add_argument("directory", nargs="?", default=".", help="Directory to cache")
    args = parser.parse_args()
    logging.getLogger().setLevel(args.log_level.upper())
    _cache_all_apps(
        args.directory,
        include_gitignored=args.include_gitignored,
        include_code=args.include_code,
        log_level=args.log_level,
    )

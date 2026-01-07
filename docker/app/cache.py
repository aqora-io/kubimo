import logging
from pathlib import Path
import importlib.util
import json
import asyncio
import threading
from contextlib import asynccontextmanager
from concurrent.futures import ProcessPoolExecutor
import subprocess

import marimo
from marimo._server.notebook import (
    AppFileManager,
)  # will be moved to marimo._session.notebook
from marimo._server.session.serialize import (
    serialize_session_view,
)  # will be moved to marimo._session.state.serialize
from marimo._server.export import run_app_until_completion

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def _cache_app(path: Path):
    logger.info(f"Caching {path}")
    file_manager = AppFileManager(path)
    session_view, _ = await run_app_until_completion(
        file_manager, cli_args={}, argv=None
    )
    session_snapshot = serialize_session_view(
        session_view,
        cell_ids=list(file_manager.app.cell_manager.cell_ids()),
    )
    session_dir = path.parent / "__marimo__" / "session"
    session_dir.mkdir(parents=True, exist_ok=True)
    with open(session_dir / f"{path.name}.json", "w") as f:
        json.dump(session_snapshot, f, indent=2)


def _is_gitignored(path: Path, git_root: Path) -> bool:
    """Check if a file is gitignored using git check-ignore."""
    try:
        result = subprocess.run(
            ["git", "check-ignore", "--quiet", str(path)],
            cwd=git_root,
            capture_output=True,
        )
        # git check-ignore returns 0 if the file is ignored, 1 if not
        return result.returncode == 0
    except Exception:
        # If git command fails, assume file is not ignored
        return False


def _is_app(path: Path):
    logger.info(f"Checking {path}")
    spec = importlib.util.spec_from_file_location(str(path), path)
    if spec is None or spec.loader is None:
        return False
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return hasattr(module, "app") and isinstance(getattr(module, "app"), marimo.App)


def _cache_app_sync(path: Path):
    try:
        if _is_app(path):
            asyncio.run(_cache_app(path))
            logger.info(f"Cached {path}")
            return True
        else:
            logger.warning(f"Skipping {path}")
            return False
    except Exception as e:
        logger.error(f"Failed to cache {path}: {e}", exc_info=True)
        return False


def _get_python_files(directory: str) -> list[Path]:
    """Get all Python files in directory, excluding gitignored files and directories."""
    directory_path = Path(directory).resolve()

    # Find git root directory
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=directory_path,
            capture_output=True,
            text=True,
            check=True,
        )
        git_root = Path(result.stdout.strip())
    except Exception:
        # If not in a git repo, proceed without filtering
        logger.warning("Not in a git repository, skipping gitignore filtering")
        git_root = None

    files = []

    if git_root:
        # Walk directories manually to skip gitignored directories
        def walk_dir(path: Path):
            try:
                for item in path.iterdir():
                    if item.is_dir():
                        # Check if directory is gitignored, skip if it is
                        if not _is_gitignored(item, git_root):
                            walk_dir(item)
                    elif item.is_file() and item.suffix == ".py":
                        # Only check file if we got here (parent dirs not ignored)
                        if not _is_gitignored(item, git_root):
                            files.append(item)
            except PermissionError:
                logger.warning(f"Permission denied: {path}")

        walk_dir(directory_path)
        logger.info(f"Found {len(files)} non-gitignored python files")
    else:
        # If not in git repo, use simple rglob
        files = list(directory_path.rglob("*.py"))
        logger.info(f"Found {len(files)} python files")

    return files


def _cache_all_apps(directory: str):
    files = _get_python_files(directory)

    # Run _cache_app in parallel with process workers
    with ProcessPoolExecutor() as executor:
        results = list(executor.map(_cache_app_sync, files))

    successful = sum(results)
    failed = len(results) - successful
    logger.info(
        f"Caching complete: {successful} apps cached successfully, {failed} failed or skipped"
    )


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument("directory", nargs="?", default=".")
    args = parser.parse_args()
    _cache_all_apps(args.directory)

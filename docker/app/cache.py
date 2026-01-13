import logging
from pathlib import Path
import importlib.util
import asyncio
from concurrent.futures import ProcessPoolExecutor
import subprocess
import itertools
from dataclasses import dataclass
import fnmatch
from functools import lru_cache

import marimo
from marimo._server.export import run_app_then_export_as_html
from marimo._utils.marimo_path import MarimoPath

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def _cache_app(path: Path, *, include_code: bool):
    logger.info(f"Caching {path}")
    export_result = await run_app_then_export_as_html(
        MarimoPath(path),
        include_code=include_code,
        cli_args={},
        argv=[],
    )
    export_dir = path.parent / "__marimo__"
    export_dir.mkdir(parents=True, exist_ok=True)
    export_path = export_dir / export_result.download_filename
    export_path.write_text(export_result.contents, encoding="utf-8")


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


@dataclass(frozen=True)
class _GitignoreRule:
    base_path: Path
    pattern: str
    negation: bool
    directory_only: bool
    anchored: bool
    has_slash: bool


def _unescape_gitignore_pattern(pattern: str) -> str:
    unescaped = []
    escape = False
    for char in pattern:
        if escape:
            unescaped.append(char)
            escape = False
        elif char == "\\":
            escape = True
        else:
            unescaped.append(char)
    if escape:
        unescaped.append("\\")
    return "".join(unescaped)


def _load_gitignore_rules(directory: Path) -> list[_GitignoreRule]:
    gitignore_path = directory / ".gitignore"
    if not gitignore_path.is_file():
        return []
    try:
        lines = gitignore_path.read_text(
            encoding="utf-8", errors="replace"
        ).splitlines()
    except OSError as exc:
        logger.warning(f"Failed to read {gitignore_path}: {exc}")
        return []

    rules: list[_GitignoreRule] = []
    for line in lines:
        if line == "":
            continue
        if line.startswith("#"):
            continue
        if line.startswith("\\#") or line.startswith("\\!"):
            line = line[1:]
        negation = line.startswith("!")
        if negation:
            line = line[1:]
        line = _unescape_gitignore_pattern(line)
        if not line:
            continue
        directory_only = line.endswith("/")
        if directory_only:
            line = line[:-1]
        if not line:
            continue
        anchored = line.startswith("/")
        if anchored:
            line = line.lstrip("/")
        has_slash = "/" in line
        rules.append(
            _GitignoreRule(
                base_path=directory,
                pattern=line,
                negation=negation,
                directory_only=directory_only,
                anchored=anchored,
                has_slash=has_slash,
            )
        )
    return rules


def _match_path_parts(pattern: str, path_parts: list[str]) -> bool:
    parts = [part for part in pattern.split("/") if part != ""]
    if not parts:
        return False
    collapsed: list[str] = []
    for part in parts:
        if part == "**" and collapsed and collapsed[-1] == "**":
            continue
        collapsed.append(part)
    parts = collapsed

    @lru_cache(maxsize=None)
    def match(pattern_index: int, path_index: int) -> bool:
        if pattern_index == len(parts):
            return path_index == len(path_parts)
        part = parts[pattern_index]
        if part == "**":
            if match(pattern_index + 1, path_index):
                return True
            return path_index < len(path_parts) and match(pattern_index, path_index + 1)
        if path_index >= len(path_parts):
            return False
        if not fnmatch.fnmatchcase(path_parts[path_index], part):
            return False
        return match(pattern_index + 1, path_index + 1)

    return match(0, 0)


def _matches_gitignore_rule(rule: _GitignoreRule, path: Path, *, is_dir: bool) -> bool:
    if rule.directory_only and not is_dir:
        return False
    try:
        relative = path.relative_to(rule.base_path)
    except ValueError:
        return False
    relative_posix = relative.as_posix()
    if relative_posix == ".":
        return False
    path_parts = relative_posix.split("/")
    if rule.anchored or rule.has_slash:
        return _match_path_parts(rule.pattern, path_parts)
    return fnmatch.fnmatchcase(path_parts[-1], rule.pattern)


def _is_ignored_by_rules(
    path: Path, rules: list[_GitignoreRule], *, is_dir: bool
) -> bool:
    ignored = False
    for rule in rules:
        if _matches_gitignore_rule(rule, path, is_dir=is_dir):
            ignored = not rule.negation
    return ignored


def _is_app(path: Path):
    logger.info(f"Checking {path}")
    spec = importlib.util.spec_from_file_location(str(path), path)
    if spec is None or spec.loader is None:
        return False
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return hasattr(module, "app") and isinstance(getattr(module, "app"), marimo.App)


def _cache_app_sync(path: Path, include_code: bool):
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


def _get_python_files(directory: str, include_gitignored: bool = False) -> list[Path]:
    """Get all Python files in directory, excluding gitignored files and directories."""
    directory_path = Path(directory).resolve()

    git_root = None
    # Find git root directory
    if not include_gitignored:
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
            # If not in a git repo, fall back to .gitignore files in the tree
            logger.warning(
                "Not in a git repository, falling back to .gitignore filtering"
            )
            git_root = None

    files = []

    if git_root:
        # Walk directories manually to skip gitignored directories
        def walk_dir_git(path: Path):
            try:
                for item in path.iterdir():
                    if item.is_dir():
                        # Check if directory is gitignored, skip if it is
                        if not _is_gitignored(item, git_root):
                            walk_dir_git(item)
                    elif item.is_file() and item.suffix == ".py":
                        # Only check file if we got here (parent dirs not ignored)
                        if not _is_gitignored(item, git_root):
                            files.append(item)
            except PermissionError:
                logger.warning(f"Permission denied: {path}")

        walk_dir_git(directory_path)
        logger.info(f"Found {len(files)} non-gitignored python files")
    elif not include_gitignored:

        def walk_dir_rules(path: Path, rules: list[_GitignoreRule]):
            local_rules = rules + _load_gitignore_rules(path)
            try:
                for item in path.iterdir():
                    if item.is_dir():
                        if not _is_ignored_by_rules(item, local_rules, is_dir=True):
                            walk_dir_rules(item, local_rules)
                    elif item.is_file() and item.suffix == ".py":
                        if not _is_ignored_by_rules(item, local_rules, is_dir=False):
                            files.append(item)
            except PermissionError:
                logger.warning(f"Permission denied: {path}")

        walk_dir_rules(directory_path, [])
        logger.info(f"Found {len(files)} non-gitignored python files")
    else:
        # If not in git repo, use simple rglob
        files = list(directory_path.rglob("*.py"))
        logger.info(f"Found {len(files)} python files")

    return files


def _cache_all_apps(
    directory: str,
    *,
    include_gitignored: bool = False,
    include_code: bool = False,
):
    files = _get_python_files(directory, include_gitignored=include_gitignored)

    # Run _cache_app in parallel with process workers
    with ProcessPoolExecutor() as executor:
        results = list(
            executor.map(_cache_app_sync, files, itertools.repeat(include_code))
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
    parser.add_argument("directory", nargs="?", default=".", help="Directory to cache")
    args = parser.parse_args()
    _cache_all_apps(
        args.directory,
        include_gitignored=args.include_gitignored,
        include_code=args.include_code,
    )

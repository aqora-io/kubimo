"""The ignore-aware walk over a workspace, shared by cache.py and kubimo_migrate.py."""

import fnmatch
import logging
import os
import subprocess
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

logger = logging.getLogger(__name__)


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


def _gitignored(paths: list[Path], git_root: Path) -> set[Path]:
    """Which of `paths` git ignores, asked in one run. git refuses the whole
    batch over a single path it will not check (one inside a submodule, say),
    and then each path is asked about on its own."""
    if not paths:
        return set()
    try:
        result = subprocess.run(
            ["git", "check-ignore", "--stdin", "-z"],
            cwd=git_root,
            input=b"".join(os.fsencode(path) + b"\0" for path in paths),
            capture_output=True,
        )
    except Exception:
        # If git command fails, assume nothing is ignored
        return set()
    # git check-ignore returns 0 if a path is ignored, 1 if none is
    if result.returncode in (0, 1):
        return {Path(os.fsdecode(path)) for path in result.stdout.split(b"\0") if path}
    return {path for path in paths if _is_gitignored(path, git_root)}


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
    rules: list[_GitignoreRule] = []
    # `.ignore` after `.gitignore`: both use gitignore syntax, later rules win,
    # and the indexer's walker (the `ignore` crate) gives `.ignore` the higher
    # precedence. The workspace template ships `.ignore`, so exclusions work
    # without tying the workspace to git.
    for name in (".gitignore", ".ignore"):
        rules.extend(_load_ignore_file_rules(directory, directory / name))
    return rules


def _load_ignore_file_rules(directory: Path, path: Path) -> list[_GitignoreRule]:
    if not path.is_file():
        return []
    try:
        lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as exc:
        logger.warning(f"Failed to read {path}: {exc}")
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


def find_files(
    directory: str | Path,
    suffixes: tuple[str, ...] = (".py",),
    *,
    include_gitignored: bool = False,
) -> list[Path]:
    """Get all files with one of `suffixes` in directory, excluding gitignored
    files and directories."""
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
                "Not in a git repository, falling back to .gitignore/.ignore filtering"
            )
            git_root = None

    files = []

    # Directory symlinks are never followed, like the indexer's walk: a
    # workspace's links come back from S3 as they were archived, and a loop
    # would end the walk with ELOOP while a link to an ancestor would walk the
    # tree (caches included) again at every level.
    def is_real_dir(item: Path) -> bool:
        return item.is_dir() and not item.is_symlink()

    if git_root:
        # Walk level by level to skip gitignored directories, asking git about
        # a whole level at once: a process per path costs seconds on a large
        # tree under gVisor.
        level = [directory_path]
        while level:
            candidates = []
            for path in level:
                try:
                    for item in path.iterdir():
                        if is_real_dir(item):
                            candidates.append((item, True))
                        elif item.is_file() and item.suffix in suffixes:
                            candidates.append((item, False))
                except OSError as error:
                    logger.warning(f"Skipping {path}: {error}")
            ignored = _gitignored([item for item, _ in candidates], git_root)
            level = []
            for item, is_dir in candidates:
                if item not in ignored:
                    (level if is_dir else files).append(item)
        logger.info(f"Found {len(files)} non-gitignored {'/'.join(suffixes)} files")
    elif not include_gitignored:

        def walk_dir_rules(path: Path, rules: list[_GitignoreRule]):
            local_rules = rules + _load_gitignore_rules(path)
            try:
                for item in path.iterdir():
                    if is_real_dir(item):
                        if not _is_ignored_by_rules(item, local_rules, is_dir=True):
                            walk_dir_rules(item, local_rules)
                    elif item.is_file() and item.suffix in suffixes:
                        if not _is_ignored_by_rules(item, local_rules, is_dir=False):
                            files.append(item)
            except OSError as error:
                logger.warning(f"Skipping {path}: {error}")

        walk_dir_rules(directory_path, [])
        logger.info(f"Found {len(files)} non-gitignored {'/'.join(suffixes)} files")
    else:
        # If not in git repo, use simple rglob (which does not follow
        # directory symlinks either)
        files = [
            path for suffix in suffixes for path in directory_path.rglob(f"*{suffix}")
        ]
        logger.info(f"Found {len(files)} {'/'.join(suffixes)} files")

    return files

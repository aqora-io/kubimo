"""Import files into a workspace, for an ImportJob.

Each file arrives staged by the Job's fetch step, and is either copied as-is or
converted into a marimo notebook like `marimo convert`. The files are
untrusted. This runs without credentials, and with uv offline and forbidden to
build, since converting an ipynb resolves its `!pip install` lines with
`uv pip compile`.

Every file is converted, every output path checked, and every file staged
next to its output before the first one replaces anything: a file that fails
leaves the workspace as it was. Why it failed, naming its output path, goes to
`--termination-log` for the ImportJob's status.
"""

import logging
import os
import shutil
import tempfile
from pathlib import Path, PurePosixPath

from marimo._ast.parse import parse_notebook
from marimo._convert import MarimoConvert
from marimo._schemas.serialization import CellDef

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


class ImportFailed(Exception):
    """A file that cannot be imported; the message names its output path."""


def with_marimo_import(ir):
    """`ir`, with an `import marimo as mo` cell first if its cells use `mo`
    and none imports it. Converting markdown writes `mo.md(...)` cells but,
    unlike converting an ipynb, no such import, so every one of them would fail
    with a NameError."""
    codes = [cell.code for cell in ir.cells]
    uses_mo = any("mo.md(" in code or "mo.sql(" in code for code in codes)
    if uses_mo and not any("import marimo as mo" in code for code in codes):
        ir.cells.insert(0, CellDef(code="import marimo as mo"))
    return ir


def convert(src: Path) -> str:
    """The marimo notebook for `src`, picked by its extension like `marimo convert`."""
    text = src.read_text(encoding="utf-8")
    if src.suffix == ".ipynb":
        return MarimoConvert.from_ipynb(text).to_py()
    if src.suffix in (".md", ".qmd"):
        ir = MarimoConvert.from_md(text).to_ir()
        return MarimoConvert.from_ir(with_marimo_import(ir)).to_py()
    if src.suffix == ".py":
        # `marimo convert` writes nothing for a file that already is a marimo
        # notebook; a conversion must produce one, so it passes through as-is.
        notebook = parse_notebook(text)
        if notebook is not None and notebook.valid:
            return text
        return MarimoConvert.from_non_marimo_python_script(text).to_py()
    raise ValueError(f"unsupported input format: {src.suffix or 'no extension'}")


def output_path(root: Path, dst: str) -> Path:
    """`dst` under `root`, refusing anything that would land outside it, or
    that could not be written there."""
    relative = PurePosixPath(dst)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in dst.split("/")):
        raise ValueError(f"output path must be a relative file path: {dst!r}")
    root = root.resolve()
    path = root / relative
    # A directory on the way may already exist as a symlink out of the root.
    if not path.parent.resolve().is_relative_to(root):
        raise ValueError(f"output path leaves the workspace: {dst!r}")
    # A file (or a symlink, which is replaced rather than followed) can be
    # replaced in one step; a directory cannot.
    if path.is_dir() and not path.is_symlink():
        raise ValueError(f"output path is an existing directory: {dst!r}")
    for parent in path.parents:
        if parent == root:
            break
        if os.path.lexists(parent) and not parent.is_dir():
            raise ValueError(f"output path is inside a file: {dst!r}")
    return path


def plan(root: Path, files: list[tuple[str, Path, str]]) -> list[tuple[Path, str, str | Path]]:
    """Each `(mode, src, dst)` file's output path and `dst`, with what goes
    there: the converted notebook's text, or the staged file to copy."""
    planned = []
    for mode, src, dst in files:
        path = output_path(root, dst)
        if mode == "convert":
            if path.suffix != ".py":
                raise ValueError(f"a converted file's output path must end with .py: {dst!r}")
            try:
                planned.append((path, dst, convert(src)))
            except Exception as err:
                raise ImportFailed(f"{dst}: cannot convert: {type(err).__name__}: {err}") from err
        elif mode == "copy":
            planned.append((path, dst, src))
        else:
            raise ValueError(f"unknown mode {mode!r}: expected convert or copy")
    # A file cannot also be the directory another one is written into.
    paths = {path: dst for path, dst, _ in planned}
    for path, dst, _ in planned:
        for parent in path.parents:
            if parent in paths:
                raise ValueError(f"output path {dst!r} is inside output path {paths[parent]!r}")
    return planned


def stage(path: Path, content: str | Path, created: list[Path]) -> Path:
    """Write `content`, text or a file to copy, to a temporary file next to
    `path`, creating the directories on the way (appended to `created`)."""
    for directory in reversed([parent for parent in path.parents if not os.path.lexists(parent)]):
        directory.mkdir()
        created.append(directory)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        if isinstance(content, Path):
            with os.fdopen(fd, "wb") as file, content.open("rb") as source:
                shutil.copyfileobj(source, file)
        else:
            with os.fdopen(fd, "w", encoding="utf-8") as file:
                file.write(content)
        os.chmod(tmp, 0o644)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise
    return Path(tmp)


def import_files(root: Path, files: list[tuple[str, Path, str]]) -> None:
    created: list[Path] = []
    staged: list[tuple[Path, Path]] = []
    try:
        for path, dst, content in plan(root, files):
            try:
                staged.append((stage(path, content, created), path))
            except OSError as err:
                raise ImportFailed(f"{dst}: cannot write: {err.strerror or err}") from err
    except BaseException:
        for tmp, _ in staged:
            tmp.unlink(missing_ok=True)
        for directory in reversed(created):
            try:
                directory.rmdir()
            except OSError:
                pass
        raise
    # Renames only from here, each replacing its file in one step, so an
    # editor watching one never reads half of it.
    for tmp, path in staged:
        os.replace(tmp, path)
        logger.info(f"Imported {path}")


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root",
        required=True,
        type=Path,
        help="Workspace root the output paths are relative to.",
    )
    parser.add_argument(
        "--termination-log",
        type=Path,
        help="Where to write why the import failed, in one line.",
    )
    parser.add_argument(
        "files",
        nargs="+",
        metavar="ARG",
        help="Each file as three arguments: convert or copy, its staged input, "
        "and its output path relative to --root.",
    )
    args = parser.parse_args()
    if len(args.files) % 3:
        parser.error("files must come as convert|copy SRC DST triples")
    triples = zip(args.files[::3], args.files[1::3], args.files[2::3])
    try:
        import_files(args.root, [(mode, Path(src), dst) for mode, src, dst in triples])
    except Exception as err:
        if args.termination_log:
            try:
                args.termination_log.write_text(str(err), encoding="utf-8")
            except OSError:
                pass
        raise

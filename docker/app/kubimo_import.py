"""Import files into a workspace, for an ImportJob.

Each file arrives staged by the Job's fetch step, and is either copied as-is or
converted into a marimo notebook like `marimo convert`. The files are
untrusted. This runs without credentials, and with uv offline and forbidden to
build, since converting an ipynb resolves its `!pip install` lines with
`uv pip compile`.

Every file is converted, and every output path checked, before the first one
is written: a file that fails leaves the workspace as it was.
"""

import logging
import os
import shutil
import tempfile
from pathlib import Path, PurePosixPath

from marimo._ast.parse import parse_notebook
from marimo._convert import MarimoConvert

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


def convert(src: Path) -> str:
    """The marimo notebook for `src`, picked by its extension like `marimo convert`."""
    text = src.read_text(encoding="utf-8")
    if src.suffix == ".ipynb":
        return MarimoConvert.from_ipynb(text).to_py()
    if src.suffix in (".md", ".qmd"):
        return MarimoConvert.from_md(text).to_py()
    if src.suffix == ".py":
        # `marimo convert` writes nothing for a file that already is a marimo
        # notebook; a conversion must produce one, so it passes through as-is.
        notebook = parse_notebook(text)
        if notebook is not None and notebook.valid:
            return text
        return MarimoConvert.from_non_marimo_python_script(text).to_py()
    raise ValueError(f"unsupported input format: {src.name}")


def output_path(root: Path, dst: str) -> Path:
    """`dst` under `root`, refusing anything that would land outside it."""
    relative = PurePosixPath(dst)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in dst.split("/")):
        raise ValueError(f"output path must be a relative file path: {dst!r}")
    root = root.resolve()
    path = root / relative
    # A directory on the way may already exist as a symlink out of the root.
    if not path.parent.resolve().is_relative_to(root):
        raise ValueError(f"output path leaves the workspace: {dst!r}")
    return path


def plan(root: Path, files: list[tuple[str, Path, str]]) -> list[tuple[Path, str | Path]]:
    """Each `(mode, src, dst)` file's output path, with what goes there: the
    converted notebook's text, or the staged file to copy."""
    planned = []
    for mode, src, dst in files:
        path = output_path(root, dst)
        if mode == "convert":
            if path.suffix != ".py":
                raise ValueError(f"a converted file's output path must end with .py: {dst!r}")
            planned.append((path, convert(src)))
        elif mode == "copy":
            planned.append((path, src))
        else:
            raise ValueError(f"unknown mode {mode!r}: expected convert or copy")
    return planned


def write_atomic(path: Path, content: str | Path) -> None:
    """Replace `path` in one step with `content`, text or a file to copy, so an
    editor watching it never reads half a file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.", suffix=".tmp")
    try:
        if isinstance(content, Path):
            with os.fdopen(fd, "wb") as file, content.open("rb") as source:
                shutil.copyfileobj(source, file)
        else:
            with os.fdopen(fd, "w", encoding="utf-8") as file:
                file.write(content)
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def import_files(root: Path, files: list[tuple[str, Path, str]]) -> None:
    for path, content in plan(root, files):
        write_atomic(path, content)
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
    import_files(args.root, [(mode, Path(src), dst) for mode, src, dst in triples])

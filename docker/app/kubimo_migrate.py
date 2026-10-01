"""Give a legacy workspace's marimo notebooks PEP 723 headers, once.

Workspaces created for kubimo's uv or conda images pin `[tool.marimo.venv]` in
pyproject.toml to an environment the single pixi image no longer has, and
marimo bypasses its per-notebook sandbox while that table exists. Every notebook
without a header gets one carrying the workspace's declared dependencies (and,
for uv workspaces, the image packages its kernels imported without declaring
them), then the legacy tables are removed. A workspace without them exits
untouched before anything heavy is imported.

Headers reproduce the environment a notebook last ran in: a uv requirement
without a version of its own is pinned to uv.lock's, and git, path and url
sources carry over. Under the uv backend so do package index sources and the
workspace's package indexes, and only a workspace member is left out. Under
pixi, a dependency whose source pixi cannot express (a package index, a
workspace member) is left out and logged, never resolved from PyPI instead,
and so is every declared one without a source when the workspace replaced PyPI
with a default index of its own. Other package indexes set for the whole
workspace do not carry over to pixi: the requirements without a source of
their own resolve from PyPI, as logged.

Two runner pods can migrate one slot at once, and flock does not reach across
gVisor sandboxes, so every write is compare-then-replace: a file that changed
since it was read is left to whoever changed it.
"""

import argparse
import ast
import difflib
import logging
import os
import re
import shutil
import sys
import tempfile
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path

import tomllib

logger = logging.getLogger(__name__)
_LOG_LEVEL_CHOICES = ["debug", "info", "warn", "warning", "error", "critical"]

SYSTEM_REQUIREMENTS = Path("/setup/system-requirements.txt")
_DEFAULT_REQUIRES_PYTHON = "==3.12.*"
# The environments kubimo's uv and conda images pinned in [tool.marimo.venv].
_LEGACY_VENVS = {
    "/home/me/venv": "uv",
    "/home/me/.cache/rattler/cache/envs/workspace-5509610833061971390/envs/default": "conda",
}
# Packages notebooks use without importing them: `mo.sql` runs on these.
_USED_WITHOUT_IMPORT = {"mo.sql(": ("duckdb", "polars", "pyarrow", "sqlglot")}
# Dev-group distributions not imported as their name with `-` as `_`.
_DEV_IMPORT_NAMES = {"google-genai": "google.genai"}
# The conda image's python pin, which requires-python ==3.12.* covers.
_IMAGE_PYTHON = re.compile(r"==3\.12\.\d+")
# A PEP 508 requirement's name and extras.
_REQUIREMENT = re.compile(r"\s*([A-Za-z0-9][A-Za-z0-9._-]*)(\s*\[[^\]]*\])?")
# The [tool.uv.sources] keys pixi's pypi-dependencies tables take as well.
_PIXI_SOURCE_KEYS = {
    "git",
    "rev",
    "tag",
    "branch",
    "subdirectory",
    "lfs",
    "url",
    "path",
    "editable",
}
# The [tool.uv] package index settings uv headers carry, scalars before the
# [[tool.uv.index]] tables, as TOML has them.
_UV_INDEX_KEYS = ("index-url", "extra-index-url", "find-links", "index")
# Keys whose relative values are locations a script's own directory anchors.
_LOCATION_KEYS = {"path", "url", "index-url", "extra-index-url", "find-links"}
# Import statements, for a notebook that does not parse.
_IMPORT = re.compile(
    r"^[ \t]*(?:from[ \t]+(\w[\w.]*)[ \t]+import|import[ \t]+(\w[\w.]*))",
    re.MULTILINE,
)
# A PEP 263 source encoding declaration.
_ENCODING = re.compile(r"[ \t\f]*#.*?coding[:=][ \t]*[-\w.]+")


@dataclass(frozen=True)
class SystemPackages:
    """The image's system site-packages: what legacy uv kernels could import
    without declaring it (the image keeps the same set for its server)."""

    # import name -> the distributions providing it
    top_level: Mapping[str, Sequence[str]]
    # normalized distribution name -> version
    versions: Mapping[str, str]
    # distribution name -> its files, relative to site-packages
    files: Callable[[str], Iterable[str]]

    @classmethod
    def installed(cls) -> "SystemPackages":
        from importlib import metadata

        def files(name: str) -> list[str]:
            try:
                return [str(path) for path in metadata.files(name) or ()]
            except metadata.PackageNotFoundError:
                return []

        return cls(
            top_level=metadata.packages_distributions(),
            versions={
                _normalize(distribution.metadata["Name"]): distribution.version
                for distribution in metadata.distributions()
            },
            files=files,
        )


class _ImplicitPackages:
    """What a legacy uv kernel imported without declaring it: the image's
    system packages and the venv's dev group, pinned to the version the kernel
    ran, uv.lock's (the venv shadowed the image) or else the image's."""

    def __init__(
        self,
        root: Path,
        pyproject: dict,
        system_requirements: Path,
        system: SystemPackages,
    ):
        self.root = root
        self.system = system
        self.dev = {}
        for requirement in pyproject.get("dependency-groups", {}).get("dev", []):
            if isinstance(requirement, str):  # not an {include-group = ...}
                name = _name(requirement)
                self.dev[name] = _DEV_IMPORT_NAMES.get(name, name.replace("-", "_"))
        self.locked = {
            _normalize(package["name"]): package["version"]
            for package in _load_toml(root / "uv.lock").get("package", [])
            if "version" in package
        }
        self.extras = {}
        for line in system_requirements.read_text().splitlines():
            match = _REQUIREMENT.match(line)
            if match and match[2]:
                self.extras[_normalize(match[1])] = match[2]

    def requirements(self, notebook: Path, text: str) -> list[str]:
        """Pinned requirements for what `notebook` used undeclared."""
        names = set()
        for module in _imports(text):
            top = module.partition(".")[0]
            # stdlib_module_names includes __future__.
            if top in sys.stdlib_module_names or top == "marimo":
                continue
            if _is_local(top, notebook.parent, self.root):
                continue
            names.update(self._distributions(module))
        for usage, used in _USED_WITHOUT_IMPORT.items():
            if usage in text:
                names.update(used)
        pinned = []
        for name in sorted(names):
            version = self.locked.get(name) or self.system.versions.get(name)
            # Anything else was not importable by the old kernel either.
            if version:
                pinned.append(f"{name}{self.extras.get(name, '')}=={version}")
        return pinned

    def _distributions(self, module: str) -> list[str]:
        """The distributions the kernel imported `module` from."""
        top = module.partition(".")[0]
        candidates = [_normalize(name) for name in self.system.top_level.get(top, ())]
        candidates += [
            name
            for name, imported in self.dev.items()
            if imported.partition(".")[0] == top
        ]
        candidates = list(dict.fromkeys(candidates))
        if len(candidates) < 2:
            return candidates
        # A namespace package (google) spans several distributions: keep those
        # shipping the imported submodule, trying the longest name first.
        parts = module.split(".")
        for depth in range(len(parts), 1, -1):
            shipping = [name for name in candidates if self._ships(name, parts[:depth])]
            if shipping:
                return shipping
        return []

    def _ships(self, name: str, parts: list[str]) -> bool:
        """Whether distribution `name` ships the module named by `parts`."""
        if name in self.dev:
            dev_parts = self.dev[name].split(".")
            if parts[: len(dev_parts)] == dev_parts:
                return True
        return any(_in_module(file, parts) for file in self.system.files(name))


@dataclass(frozen=True)
class _Headers:
    """The header each of a workspace's notebooks gets."""

    root: Path
    requires_python: str
    # The workspace's declared requirements, and [tool.pixi] tables whose
    # paths are relative to the root.
    declared: list[str]
    pixi: dict
    # uv workspaces only: [tool.uv.sources] as _from_uv_sources gives them,
    # the [tool.uv] package indexes the uv backend carries along, and what
    # their kernels imported undeclared.
    sources: dict[str, dict | list | None]
    indexes: dict
    implicit: _ImplicitPackages | None
    # The sandbox backend the workspace's runners build environments with.
    backend: str = "pixi"

    def block(self, notebook: Path, text: str) -> str:
        """The `# /// script` block for `notebook`, whose source is `text`."""
        inferred = self.implicit.requirements(notebook, text) if self.implicit else []
        dependencies = []
        sourced = {}
        for requirement in _dependencies(self.declared, inferred):
            name = _name(requirement)
            if name not in self.sources:
                dependencies.append(requirement)
            elif self.sources[name] is None:
                continue
            elif self.backend == "uv":
                # uv keeps the requirement and reads its source apart.
                dependencies.append(requirement)
                sourced[name] = self.sources[name]
            else:
                sourced[name] = self.sources[name] | _extras(requirement)
        pixi, uv = self.pixi, dict(self.indexes)
        # Only uv workspaces have sources, and no [tool.pixi] tables of their own.
        if sourced and self.backend == "uv":
            uv["sources"] = sourced
        elif sourced:
            pixi = {**self.pixi, "pypi-dependencies": sourced}
        if notebook.parent != self.root:
            to_root = os.path.relpath(self.root, notebook.parent)
            pixi, uv = _rebased(pixi, to_root), _rebased(uv, to_root)
        return _render(self.requires_python, dependencies, pixi, uv, self.backend)


def migrate(
    workspace: Path,
    *,
    dry_run: bool = False,
    system_requirements: Path = SYSTEM_REQUIREMENTS,
    system: SystemPackages | None = None,
    backend: str = "pixi",
) -> int:
    """Migrate `workspace` if it is a legacy one; returns the exit code."""
    pyproject = _load_toml(workspace / "pyproject.toml")
    venv = _lookup(pyproject, "tool", "marimo", "venv", "path")
    flavour = _LEGACY_VENVS.get(venv) if isinstance(venv, str) else None
    if flavour is None:
        return 0

    root = workspace.resolve()
    results = {"migrated": [], "had a header": [], "skipped": [], "failed": []}
    unplanned = None
    try:
        # Imported only now: every boot after the migration stops above.
        from kubimo_walk import find_files
        from marimo._server.files.directory_scanner import is_marimo_app

        headers = _plan(root, flavour, pyproject, system_requirements, system, backend)
        notebooks = [
            path
            for path in find_files(root, (".py", ".md", ".qmd"))
            if is_marimo_app(str(path))
        ]
    except Exception as error:
        # As if every notebook failed; the tables still go below. Run mode then
        # serves the notebooks on the server's Python, edit mode gives them a
        # marimo-only header, and the declared dependencies stay in
        # pyproject.toml.
        unplanned = f"{type(error).__name__}: {error}"
        logger.error(
            f"Failed to plan the notebooks' headers: {unplanned}",
            exc_info=logger.isEnabledFor(logging.DEBUG),
        )
        notebooks = []
    for path in notebooks:
        name = path.relative_to(root).as_posix()
        if path.suffix != ".py":
            logger.warning(
                f"Skipping {name}: markdown notebooks are not migrated, so the "
                "workspace's dependencies are not carried into it"
            )
            results["skipped"].append(f"{name} (markdown, dependencies not carried)")
        elif path.resolve() != path:
            logger.warning(f"Skipping {name}: it is reached through a symlink")
            results["skipped"].append(name)
        else:
            try:
                results[_migrate_notebook(path, name, headers, dry_run)].append(name)
            except Exception:
                logger.exception(f"Failed to migrate {name}")
                results["failed"].append(name)

    # Last, even if notebooks failed: they would never get another chance.
    try:
        outcome = _remove_legacy_tables(root / "pyproject.toml", dry_run)
    except Exception:
        logger.exception("Failed to remove the legacy tables from pyproject.toml")
        outcome = None
    if unplanned:
        counts = [f"none migrated, failed to plan their headers ({unplanned})"]
    else:
        counts = [f"{len(names)} {status}" for status, names in results.items()]
        counts += [
            f"{status}: {' '.join(results[status])}"
            for status in ("skipped", "failed")
            if results[status]
        ]
    # Printed, not logged: it reaches the pod log at every --log-level.
    print(
        f"{'Dry run, nothing written: ' if dry_run else ''}"
        f"legacy {flavour} workspace notebooks: {', '.join(counts)}; "
        f"pyproject.toml: {outcome or 'legacy tables NOT removed'}",
        file=sys.stderr,
    )
    return 0 if outcome else 1


def _plan(
    root: Path,
    flavour: str,
    pyproject: dict,
    system_requirements: Path,
    system: SystemPackages | None,
    backend: str = "pixi",
) -> _Headers:
    """The headers the notebooks of a legacy `flavour` workspace get."""
    requires_python = (
        os.environ.get("MARIMO_DEFAULT_REQUIRES_PYTHON") or _DEFAULT_REQUIRES_PYTHON
    )
    if flavour == "uv":
        implicit = _ImplicitPackages(
            root, pyproject, system_requirements, system or SystemPackages.installed()
        )
        declared = [
            _pinned(requirement, implicit.locked)
            for requirement in _lookup(pyproject, "project", "dependencies") or []
        ]
        sources = _from_uv_sources(pyproject, backend)
        indexes = _uv_indexes(pyproject) if backend == "uv" else {}
        return _Headers(
            root, requires_python, declared, {}, sources, indexes, implicit, backend
        )
    declared, pixi = _from_pixi(_load_toml(root / "pixi.toml"))
    if backend == "uv" and pixi:
        logger.warning(
            "This workspace runs uv, which ignores the [tool.pixi] tables its "
            "pixi.toml is carried into: its conda dependencies need the Conda runtime"
        )
    return _Headers(root, requires_python, declared, pixi, {}, {}, None, backend)


def _migrate_notebook(path: Path, name: str, headers: _Headers, dry_run: bool) -> str:
    """Give the notebook at `path` a header; returns how that went."""
    from marimo._environments import script_metadata

    original = path.read_bytes()
    text = original.decode()
    if script_metadata.loads(text) is not None:
        return "had a header"
    block = headers.block(path, text)
    if dry_run:
        print(f"{name}:\n{block}\n")
    elif not _replace(path, original, _with_header(text, block).encode()):
        logger.warning(f"Skipping {name}: it changed while being migrated")
        return "skipped"
    return "migrated"


def _from_pixi(manifest: dict) -> tuple[list[str], dict]:
    """A pixi.toml's dependencies as PEP 508 requirements plus the [tool.pixi]
    tables pixi reads from a script."""
    declared = []
    sources = {}
    replaces_pypi = "index-url" in manifest.get("pypi-options", {})
    for name, spec in manifest.get("pypi-dependencies", {}).items():
        if isinstance(spec, str):
            requirement = name if spec == "*" else name + spec
        elif set(spec) <= {"version", "extras"}:
            extras = f"[{','.join(spec['extras'])}]" if spec.get("extras") else ""
            version = spec.get("version", "*")
            requirement = name + extras + ("" if version == "*" else version)
        else:
            sources[name] = spec  # a path, git or url source
            continue
        # Every header keeps marimo, which the fork overlays.
        if replaces_pypi and _normalize(name) != "marimo":
            logger.warning(
                f"Leaving {name} out of notebook headers: it came from the "
                "[pypi-options] index-url, not PyPI"
            )
        else:
            declared.append(requirement)
    pixi = {}
    channels = manifest.get("workspace", manifest.get("project", {})).get("channels")
    if channels:
        pixi["workspace"] = {"channels": channels}
    conda = {
        name: spec
        for name, spec in manifest.get("dependencies", {}).items()
        if not (
            name == "python" and isinstance(spec, str) and _IMAGE_PYTHON.fullmatch(spec)
        )
    }
    if conda:
        pixi["dependencies"] = conda
    if sources:
        pixi["pypi-dependencies"] = sources
    targets = {}
    for platform, tables in manifest.get("target", {}).items():
        if set(tables) <= {"dependencies", "pypi-dependencies"}:
            targets[platform] = tables
        else:
            logger.warning(
                f"pixi.toml [target.{platform}] is not carried into notebook headers"
            )
    if targets:
        pixi["target"] = targets
    if "pypi-options" in manifest:
        unsourced = "are left out" if replaces_pypi else "will resolve from PyPI"
        logger.warning(
            "pixi.toml [pypi-options] is not carried into notebook headers: "
            f"[pypi-dependencies] without a source of their own {unsourced}"
        )
    return declared, pixi


def _from_uv_sources(
    pyproject: dict, backend: str = "pixi"
) -> dict[str, dict | list | None]:
    """[tool.uv.sources] by normalized name, or None for a requirement headers
    leave out. uv takes every source, the package indexes they name coming
    along (_uv_indexes), but a workspace member's: a notebook belongs to no
    workspace. pixi takes a pypi-dependencies table instead, and has none for
    package indexes, workspace members or conditional sources, nor for a
    requirement that came from the default index the workspace set in PyPI's
    place."""
    uv = _lookup(pyproject, "tool", "uv") or {}
    if backend == "uv":
        sources = {}
        for name, source in uv.get("sources", {}).items():
            entries = source if isinstance(source, list) else [source]
            if any(
                isinstance(entry, dict) and "workspace" in entry for entry in entries
            ):
                logger.warning(
                    f"Leaving {name} out of notebook headers: it is a workspace "
                    "member, and a notebook belongs to no workspace"
                )
                sources[_normalize(name)] = None
            else:
                sources[_normalize(name)] = source
        return sources
    replaces_pypi = "index-url" in uv or any(
        index.get("default") for index in uv.get("index", [])
    )
    if uv.keys() & {"index", "index-url", "extra-index-url", "find-links"}:
        unsourced = (
            "declared requirements without a [tool.uv.sources] entry are left out"
            if replaces_pypi
            else "requirements without a [tool.uv.sources] entry will resolve from PyPI"
        )
        logger.warning(
            "pyproject.toml [tool.uv] package indexes are not carried into "
            f"notebook headers: {unsourced}"
        )
    sources = {}
    for name, source in uv.get("sources", {}).items():
        if (
            isinstance(source, dict)
            and source.keys() & {"git", "path", "url"}
            and source.keys() <= _PIXI_SOURCE_KEYS
        ):
            sources[_normalize(name)] = source
        else:
            logger.warning(
                f"Leaving {name} out of notebook headers: pixi cannot express "
                "its [tool.uv.sources] entry"
            )
            sources[_normalize(name)] = None
    if replaces_pypi:
        for requirement in _lookup(pyproject, "project", "dependencies") or []:
            match = _REQUIREMENT.match(requirement)
            name = _normalize(match[1])
            # Every header keeps marimo, which the fork overlays, and a
            # `name @ url` requirement names its own source.
            if (
                name in sources
                or name == "marimo"
                or requirement[match.end() :].lstrip().startswith("@")
            ):
                continue
            logger.warning(
                f"Leaving {match[1]} out of notebook headers: it came from the "
                "[tool.uv] default index, not PyPI"
            )
            sources[name] = None
    return sources


def _uv_indexes(pyproject: dict) -> dict:
    """The [tool.uv] settings saying where the workspace's packages come from,
    which uv reads from a script's header as well."""
    uv = _lookup(pyproject, "tool", "uv") or {}
    return {key: uv[key] for key in _UV_INDEX_KEYS if key in uv}


def _pinned(requirement: str, locked: Mapping[str, str]) -> str:
    """`requirement` pinned to its uv.lock version, the one the notebook last
    ran with, unless it has a version or URL of its own."""
    match = _REQUIREMENT.match(requirement)
    rest = requirement[match.end() :]
    version = locked.get(_normalize(match[1]))
    # Nothing but perhaps a marker after the name and extras.
    if version and rest.lstrip()[:1] in ("", ";"):
        return f"{requirement[: match.end()]}=={version}{rest}"
    return requirement


def _extras(requirement: str) -> dict:
    """`requirement`'s extras, as pypi-dependencies table entries."""
    extras = (_REQUIREMENT.match(requirement)[2] or "").strip()[1:-1].split(",")
    extras = [extra.strip() for extra in extras if extra.strip()]
    return {"extras": extras} if extras else {}


def _rebased(value, to_root: str, key: str | None = None):
    """`value` with each relative location (a path source, a package index, a
    find-links entry) rebased from the workspace root onto a notebook `to_root`
    below it: pixi and uv resolve a script's locations from its directory."""
    if isinstance(value, dict):
        return {entry: _rebased(item, to_root, entry) for entry, item in value.items()}
    if isinstance(value, list):
        return [_rebased(item, to_root, key) for item in value]
    if (
        key in _LOCATION_KEYS
        and isinstance(value, str)
        and "://" not in value
        and not os.path.isabs(value)
    ):
        return os.path.normpath(os.path.join(to_root, value))
    return value


def _dependencies(declared: list[str], inferred: list[str]) -> list[str]:
    """marimo first, then each distribution once, declared requirements winning."""
    names = {"marimo"}
    dependencies = ["marimo"]
    for requirement in [*declared, *inferred]:
        name = _name(requirement)
        if name not in names:
            names.add(name)
            dependencies.append(requirement)
    return dependencies


def _render(
    requires_python: str,
    dependencies: list[str],
    pixi: dict,
    uv: dict | None = None,
    backend: str = "pixi",
) -> str:
    """The `# /// script` block."""
    import tomlkit
    from marimo._environments.script_metadata import wrap_block

    document = tomlkit.document()
    # A Python pin carried over from pixi.toml wins over requires-python in
    # pixi's solve, and marimo checks requires-python against the running
    # kernel on every package change: writing both would contradict it. uv
    # ignores [tool.pixi], so under uv requires-python is the only pin.
    if backend == "uv" or not _pins_python(pixi):
        document["requires-python"] = requires_python
    array = tomlkit.array()
    array.extend(dependencies)
    document["dependencies"] = array.multiline(True)
    # One level of tables under each tool, or an array of them (uv's
    # [[tool.uv.index]]), everything deeper inline.
    tool = {
        name: {key: _tables(value) for key, value in tables.items()}
        for name, tables in (("pixi", pixi), ("uv", uv or {}))
        if tables
    }
    if tool:
        document["tool"] = tool
    return wrap_block(tomlkit.dumps(document))


def _tables(value):
    """A [tool.*] entry: a table, an array of tables, or a plain value."""
    if isinstance(value, dict):
        return {entry: _inline(item) for entry, item in value.items()}
    if isinstance(value, list) and value and all(isinstance(i, dict) for i in value):
        return [_tables(item) for item in value]
    return value


def _pins_python(pixi: dict) -> bool:
    """Whether the [tool.pixi] tables pin Python, for every platform or one."""
    targets = pixi.get("target", {}).values()
    tables = [pixi.get("dependencies", {})]
    tables += [target.get("dependencies", {}) for target in targets]
    return any("python" in table for table in tables)


def _inline(value):
    """`value` with its tables inline, as in `gdal = {version = ">=3.8"}`."""
    import tomlkit

    if isinstance(value, dict):
        table = tomlkit.inline_table()
        table.update({key: _inline(item) for key, item in value.items()})
        return table
    if isinstance(value, list):
        return [_inline(item) for item in value]
    return value


def _with_header(text: str, block: str) -> str:
    """`text` with `block` at the top, below a shebang and/or a source encoding
    line, in the file's own line endings. A blank line follows the block, as
    when marimo saves the notebook, so its first save leaves the header be."""
    newline = "\r\n" if "\r\n" in text else "\n"
    offset = text.find("\n") + 1 if text.startswith("#!") else 0
    if _ENCODING.match(text, offset):
        offset = text.find("\n", offset) + 1
    rest = text[offset:]
    gap = newline if rest.startswith(newline) else newline * 2
    return text[:offset] + block.replace("\n", newline) + gap + rest


def _strip_legacy_tables(text: str) -> str | None:
    """pyproject.toml `text` without the legacy tables; None once they are gone."""
    import tomlkit

    if not isinstance(_lookup(tomlkit.parse(text), "tool", "marimo", "venv"), dict):
        return None
    for key in ("venv", "package_management"):
        text = _without_table(text, ("tool", "marimo"), key)
    # Then what held them, if they left it empty.
    text = _without_table(text, ("tool",), "marimo", only_if_empty=True)
    return _without_table(text, (), "tool", only_if_empty=True)


def _without_table(
    text: str, parents: tuple[str, ...], key: str, *, only_if_empty: bool = False
) -> str:
    """TOML `text` without the table `key` in `parents`, keeping the comments
    below its keys: tomlkit files the comments heading a table under the one
    before it."""
    import tomlkit

    # Parsed afresh for every deletion: tomlkit's view of tables split across
    # the file goes stale after one, and the lines it took are found by diff.
    document = tomlkit.parse(text)
    table = _lookup(document, *parents)
    if not isinstance(table, dict) or key not in table:
        return text
    if only_if_empty and table[key]:
        return text
    del table[key]
    old = text.split("\n")
    new = tomlkit.dumps(document).split("\n")
    start = 0
    while start < len(new) and old[start] == new[start]:
        start += 1
    end = 0
    while end < len(new) - start and old[-1 - end] == new[-1 - end]:
        end += 1
    if start + end != len(new):  # not one run of lines
        return "\n".join(new)
    deleted = old[start : len(old) - end]
    # The table's own lines end with its last key and the blank lines after it.
    last_key = max(
        (
            index
            for index, line in enumerate(deleted)
            if line.strip() and not line.lstrip().startswith("#")
        ),
        default=-1,
    )
    kept = deleted[last_key + 1 :]
    while kept and not kept[0].strip():
        kept.pop(0)
    return "\n".join(new[:start] + kept + new[start:])


def _remove_legacy_tables(pyproject: Path, dry_run: bool) -> str | None:
    """Remove the legacy tables, retrying while pyproject.toml changes
    underneath; what was done, or None if it never held still."""
    for _ in range(3):
        original = _read(pyproject)
        updated = None if original is None else _strip_legacy_tables(original.decode())
        if updated is None:
            return "legacy tables already gone"
        if dry_run:
            if updated.strip():
                sys.stdout.writelines(
                    difflib.unified_diff(
                        original.decode().splitlines(keepends=True),
                        updated.splitlines(keepends=True),
                        "pyproject.toml",
                        "pyproject.toml (migrated)",
                    )
                )
                return "legacy tables removed"
            print("pyproject.toml would be deleted: it only holds the legacy tables")
            return "deleted, it only held the legacy tables"
        if updated.strip():
            if _replace(pyproject, original, updated.encode()):
                return "legacy tables removed"
        # Nothing else was in it: the conda image's pyproject.toml.
        elif _read(pyproject) == original:
            pyproject.unlink(missing_ok=True)
            return "deleted, it only held the legacy tables"
    return None


def _replace(path: Path, original: bytes, updated: bytes) -> bool:
    """Atomically write `updated` over `path`, unless it no longer holds
    `original`: whoever changed it (another pod, the user) wins."""
    descriptor, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".kubimo-tmp", dir=path.parent
    )
    try:
        with os.fdopen(descriptor, "wb") as file:
            file.write(updated)
            file.flush()
            os.fsync(file.fileno())
        shutil.copymode(path, temporary)
        # Compared as late as possible: a save while this was written wins too.
        if _read(path) != original:
            Path(temporary).unlink()
            return False
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise
    return True


def _read(path: Path) -> bytes | None:
    try:
        return path.read_bytes()
    except FileNotFoundError:
        return None


def _load_toml(path: Path) -> dict:
    """The TOML file at `path`, or empty if it is missing or unreadable."""
    try:
        with path.open("rb") as file:
            return tomllib.load(file)
    except FileNotFoundError:
        return {}
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        logger.warning(f"Ignoring {path.name}: {error}")
        return {}


def _lookup(table, *keys):
    """`table[keys[0]][keys[1]]...`, or None where a level is not a table."""
    for key in keys:
        table = table.get(key) if isinstance(table, dict) else None
    return table


def _imports(text: str) -> set[str]:
    """The modules `text` imports absolutely, anywhere in the file."""
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return {match[1] or match[2] for match in _IMPORT.finditer(text)}
    modules = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            modules.update(alias.name for alias in node.names)
        elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
            # `from google import genai` may import the submodule google.genai.
            modules.update(f"{node.module}.{alias.name}" for alias in node.names)
    return modules


def _is_local(module: str, *directories: Path) -> bool:
    """Whether `module` is the workspace's own code in one of `directories`: a
    module, or a directory holding some (not data sharing a package's name)."""
    return any(
        (directory / f"{module}.py").is_file() or any((directory / module).glob("*.py"))
        for directory in directories
    )


def _in_module(file: str, parts: list[str]) -> bool:
    """Whether the installed `file` is (in) the module named by `parts`."""
    file_parts = file.split("/")
    return (
        len(file_parts) >= len(parts)
        and file_parts[: len(parts) - 1] == parts[:-1]
        and file_parts[len(parts) - 1].partition(".")[0] == parts[-1]
    )


def _name(requirement: str) -> str:
    return _normalize(_REQUIREMENT.match(requirement)[1])


def _normalize(name: str) -> str:
    return re.sub(r"[-_.]+", "-", name).lower()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--log-level", default="info", choices=_LOG_LEVEL_CHOICES, help="Log level."
    )
    parser.add_argument(
        "--wait-for",
        type=Path,
        metavar="MARKER",
        help="Wait for this file to exist before migrating.",
    )
    parser.add_argument(
        "--report",
        type=Path,
        metavar="FILE",
        help="Once the migration is over, write the --wait-for marker's content here.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the planned headers and pyproject.toml change; write nothing.",
    )
    parser.add_argument(
        "--backend",
        default="pixi",
        choices=("uv", "pixi"),
        help="The sandbox backend the workspace's runners build environments with.",
    )
    parser.add_argument(
        "--system-requirements",
        type=Path,
        default=SYSTEM_REQUIREMENTS,
        help="The image's extra system packages, for their extras.",
    )
    parser.add_argument("workspace", type=Path, help="Workspace to migrate.")
    args = parser.parse_args(argv)
    if args.report is not None and args.wait_for is None:
        parser.error("--report needs --wait-for")
    logging.basicConfig(level=args.log_level.upper())
    system = None
    if args.wait_for is not None:
        # A warm-pool pod boots before its claim hydrates the tenant's files.
        if not args.wait_for.exists():
            system = _prepare()
        # Polls: the agent writes the marker from the host, and gVisor only
        # delivers inotify events for writes made inside the sandbox. Finely,
        # as a reported migration holds the claim's ack.
        while not args.wait_for.exists():
            time.sleep(0.1)
    try:
        return migrate(
            args.workspace,
            dry_run=args.dry_run,
            system_requirements=args.system_requirements,
            system=system,
            backend=args.backend,
        )
    finally:
        if args.report is not None:
            _report(args.report, args.wait_for)


def _prepare() -> SystemPackages | None:
    """Do what a migration needs regardless of the workspace while the pod
    waits for its claim: the claim's ack waits for the migration, and under
    gVisor these imports and the scan of the image's packages take 3.5 s."""
    try:
        import kubimo_walk  # noqa: F401
        from marimo._server.files import directory_scanner  # noqa: F401

        return SystemPackages.installed()
    except Exception:
        # Left to the migration itself, as without a claim to wait for.
        logger.exception("Failed to prepare the migration ahead of the claim")
        return None


def _report(report: Path, marker: Path) -> None:
    """Tell the agent the migration is over: it acks the claim once `report`
    names it, as the claim marker does."""
    try:
        tmp = report.with_name(f"{report.name}.tmp")
        tmp.write_text(marker.read_text().strip())
        os.replace(tmp, report)
    except OSError as error:
        logger.warning(
            f"Could not report the migration done, so the claim's ack waits "
            f"out its timeout: {error}"
        )


if __name__ == "__main__":
    sys.exit(main())

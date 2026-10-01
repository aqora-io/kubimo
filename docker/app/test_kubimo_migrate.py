import errno
import importlib
import logging
import multiprocessing
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

import kubimo_migrate
import kubimo_walk
import pytest
from marimo._environments import script_metadata

MIGRATE_SCRIPT = Path(__file__).parent / "kubimo_migrate.py"
SETUP = Path(__file__).parent.parent / "setup"
SYSTEM_REQUIREMENTS = SETUP / "system-requirements.txt"
SEED_HEADER = re.search(
    script_metadata.REGEX, (SETUP / "seed-notebook.py").read_text()
).group(0)

# The system site-packages legacy uv kernels saw, as the test machine's own
# site-packages must not decide the outcome.
SYSTEM = kubimo_migrate.SystemPackages(
    top_level={
        "aqora": ["aqora"],
        # a backport shadowing the standard library
        "dataclasses": ["dataclasses"],
        "duckdb": ["duckdb"],
        "google": ["protobuf", "googleapis-common-protos"],
        # PyPI names a notebook's own modules can shadow
        "helpers": ["helpers"],
        "numpy": ["numpy"],
        "pandas": ["pandas"],
        "polars": ["polars"],
        "pyarrow": ["pyarrow"],
        "sqlglot": ["sqlglot"],
        "utils": ["utils"],
    },
    versions={
        "aqora": "0.20.0",
        "dataclasses": "0.6",
        "duckdb": "1.4.1",
        "googleapis-common-protos": "1.70.0",
        "helpers": "0.2.0",
        "numpy": "2.3.4",
        "pandas": "2.3.3",
        "polars": "1.34.0",
        "protobuf": "6.32.1",
        "pyarrow": "21.0.0",
        "sqlglot": "27.28.1",
        "utils": "1.0.2",
    },
    files=lambda name: {
        "googleapis-common-protos": ["google/api/__init__.py"],
        "protobuf": ["google/protobuf/__init__.py"],
    }.get(name, []),
)

# The workspace template the uv image's workspaces were created from
# (aqora-template before per-notebook headers), rendered by hand.
TEMPLATE_PYPROJECT_REST = """[project]
name = "my-workspace"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = []

[dependency-groups]
dev = [
  "ruff",
  "ty",
  "watchdog",
  "nbconvert",
  "google-genai",
  "anthropic",
  "openai",
]

[build-system]
requires = ["pdm-backend"]
build-backend = "pdm.backend"

"""
TEMPLATE_PYPROJECT = (
    TEMPLATE_PYPROJECT_REST
    + """[tool.marimo.venv]
path = "/home/me/venv"
# marimo is already installed here; setting this true reinstalls it on every
# kernel start. `uv add` works either way.
writable = false
"""
)
TEMPLATE_README = '''import marimo

__generated_with = "0.23.4"
app = marimo.App(width="full", auto_download=["html", "markdown", "ipynb"])


@app.cell
def _():
    import marimo as mo

    return (mo,)


@app.cell(hide_code=True)
def _(mo):
    mo.md(r"""
    # My Workspace v1

    This notebook has been successfully created and is ready for you to start working.
    To learn how to edit and run it, check out the [marimo documentation](https://docs.marimo.io/guides/).

    Whether you're analyzing data, testing algorithms, or exploring new ideas, this is your space.
    Once you're done, don't forget to click the **Publish** button to share your work. Happy coding! 🚀
    """)
    return


if __name__ == "__main__":
    app.run()
'''

# A hosted dataset notebook (aqora-template's dataset_marimo readme before
# per-notebook headers), rendered by hand.
DATASET_README = '''import marimo

__generated_with = "0.23.4"
app = marimo.App(width="full", auto_download=["html", "markdown", "ipynb"])


@app.cell(hide_code=True)
def _(data, mo, sql_editor):
    _ = data
    _df = mo.sql(
        f"""
        SET disabled_filesystems = 'LocalFileSystem';
        {sql_editor.value}
        """
    )
    return


@app.cell(hide_code=True)
def _(mo):
    sql_editor = mo.ui.code_editor(value="SELECT * FROM data", language="sql", debounce=1000)
    sql_editor
    return (sql_editor,)


@app.cell(hide_code=True)
def _():
    from aqora.pyarrow import dataset
    data = dataset("aqora/penguins", "v1.0.0")
    return (data,)


@app.cell(hide_code=True)
def _():
    import marimo as mo
    return (mo,)


if __name__ == "__main__":
    app.run()
'''

ANALYSIS_PYPROJECT = """[project]
name = "analysis"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = [
    "marimo[recommended,lsp]>=0.23",
    "polars>=1.0",
    "requests",
    "httpx[http2] ; sys_platform == 'linux'",
    "attrs @ https://example.com/attrs-25.1.0-py3-none-any.whl",
]

[dependency-groups]
dev = [
  "ruff",
  "google-genai",
  "anthropic",
]

[tool.marimo.venv]
path = "/home/me/venv"
writable = false

[tool.marimo.package_management]
manager = "uv"
"""
ANALYSIS_LOCK = """version = 1
revision = 3
requires-python = ">=3.12"

[[package]]
name = "analysis"
version = "0.1.0"
source = { editable = "." }

[[package]]
name = "anthropic"
version = "0.40.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "attrs"
version = "25.1.0"
source = { url = "https://example.com/attrs-25.1.0-py3-none-any.whl" }

[[package]]
name = "google-genai"
version = "1.2.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "httpx"
version = "0.28.1"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "numpy"
version = "2.2.6"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "polars"
version = "1.9.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "requests"
version = "2.32.3"
source = { registry = "https://pypi.org/simple" }
"""
ANALYSIS_NOTEBOOK = """from __future__ import annotations

import marimo

app = marimo.App()

with app.setup:
    import os
    from pathlib import Path

    import numpy as np


@app.cell
def _():
    import dataclasses
    import json

    import anthropic
    import pandas as pd
    import polars as pl
    from aqora.pyarrow import dataset
    from google import genai

    import helpers
    import notinstalled
    import utils
    return


if __name__ == "__main__":
    app.run()
"""

# What the conda image's workspaces carried: its pyproject only pinned the
# pixi environment, which pixi.toml declared.
CONDA_PYPROJECT = """[tool.marimo.venv]
path = "/home/me/.cache/rattler/cache/envs/workspace-5509610833061971390/envs/default"
writable = false

[tool.marimo.package_management]
manager = "pixi"
"""
CONDA_PIXI = """[workspace]
authors = ["Me <me@example.com>"]
channels = ["conda-forge"]
name = "workspace"
platforms = ["linux-64"]
version = "0.1.0"

[tasks]

[dependencies]
python = "==3.12.13"
jq = "*"

[pypi-dependencies]
marimo = "==0.25.0"
six = "*"
httpx = { git = "https://github.com/encode/httpx.git", tag = "0.28.1" }
"""

# The uv image's legacy table, for a test's own pyproject.toml to end with.
UV_VENV = '[tool.marimo.venv]\npath = "/home/me/venv"\n'


def notebook(imports: str) -> str:
    return f"""import marimo

app = marimo.App()


@app.cell
def _():
    {imports}
    return


if __name__ == "__main__":
    app.run()
"""


def write(root: Path, files: dict[str, str]) -> Path:
    for name, text in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(text.encode())
    return root


def snapshot(root: Path) -> dict[str, bytes]:
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def migrate(workspace: Path, **kwargs) -> int:
    return kubimo_migrate.migrate(
        workspace,
        system_requirements=SYSTEM_REQUIREMENTS,
        system=SYSTEM,
        **kwargs,
    )


def header(path: Path) -> dict:
    return script_metadata.loads(path.read_text())


@pytest.fixture(autouse=True)
def _default_requires_python(monkeypatch):
    monkeypatch.delenv("MARIMO_DEFAULT_REQUIRES_PYTHON", raising=False)


@pytest.mark.parametrize(
    "pyproject",
    [
        None,
        b'[project]\nname = "x"\n',
        b'[tool.marimo.venv]\npath = "/opt/venv"\n',
        b"[tool.marimo.venv\n",
        b'[project]\nname = "\xff"\n',
        b'tool = "/home/me/venv"\n',
        b'[tool.marimo]\nvenv = "/home/me/venv"\n',
        b'[tool.marimo.venv]\npath = ["/home/me/venv"]\n',
    ],
    ids=[
        "no pyproject",
        "no table",
        "unknown venv",
        "invalid toml",
        "not utf-8",
        "tool not a table",
        "venv not a table",
        "path not a string",
    ],
)
def test_nothing_to_migrate_touches_nothing(tmp_path, pyproject):
    workspace = write(tmp_path, {"readme.py": TEMPLATE_README})
    if pyproject is not None:
        (workspace / "pyproject.toml").write_bytes(pyproject)
    before = snapshot(workspace)

    assert migrate(workspace) == 0
    assert snapshot(workspace) == before


def test_template_readme_gets_the_seed_notebook_header(tmp_path):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )

    assert migrate(workspace) == 0
    # Laid out as marimo saves a notebook, so its first save changes nothing.
    assert (workspace / "readme.py").read_bytes() == (
        f"{SEED_HEADER}\n\n{TEMPLATE_README}".encode()
    )
    assert (workspace / "pyproject.toml").read_bytes() == (
        TEMPLATE_PYPROJECT_REST.encode()
    )


def test_uv_header_declares_what_the_notebook_imported_from_the_venv(tmp_path):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": ANALYSIS_PYPROJECT,
            "uv.lock": ANALYSIS_LOCK,
            "helpers/__init__.py": "",
            "analysis/utils.py": "",
            # data, not the notebook's own pandas
            "analysis/pandas/prices.csv": "day,price\n",
            "analysis/nb.py": ANALYSIS_NOTEBOOK,
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "analysis/nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": [
            "marimo",
            # declared, marimo's own entry dropped, bare ones pinned to uv.lock
            "polars>=1.0",
            "requests==2.32.3",
            "httpx[http2]==0.28.1 ; sys_platform == 'linux'",
            "attrs @ https://example.com/attrs-25.1.0-py3-none-any.whl",
            # imported: the dev group and the image, pinned to uv.lock
            # (the venv's copy shadowed the image's) or else the image
            "anthropic==0.40.0",
            "aqora[pyarrow]==0.20.0",
            "google-genai==1.2.0",
            "numpy==2.2.6",
            "pandas==2.3.3",
        ],
    }
    assert "[tool.marimo" not in (workspace / "pyproject.toml").read_text()
    assert (workspace / "uv.lock").read_text() == ANALYSIS_LOCK


def test_unparsable_lock_pins_the_image_versions(tmp_path, caplog):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": ANALYSIS_PYPROJECT,
            "uv.lock": f"<<<<<<< ours\n{ANALYSIS_LOCK}=======\n>>>>>>> theirs\n",
            "nb.py": notebook("import numpy"),
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py")["dependencies"] == [
        "marimo",
        "polars>=1.0",
        "requests",
        "httpx[http2] ; sys_platform == 'linux'",
        "attrs @ https://example.com/attrs-25.1.0-py3-none-any.whl",
        "numpy==2.3.4",
    ]
    assert "[tool.marimo" not in (workspace / "pyproject.toml").read_text()
    assert "uv.lock" in caplog.text


@pytest.mark.parametrize("backend", ["pixi", "uv"])
def test_uv_sources_are_carried_or_left_out(tmp_path, caplog, backend):
    pyproject = """[project]
name = "sourced"
version = "0.1.0"
requires-python = ">=3.12"
dependencies = [
    "mylib[fast]>=1.0",
    "localpkg",
    "wheelpkg",
    "private",
    "member",
    "platformpkg",
    "winlib",
]

[dependency-groups]
dev = ["internal-tools"]

[tool.uv.sources]
mylib = { git = "https://github.com/org/mylib", tag = "v1.0", subdirectory = "py" }
localpkg = { path = "./libs/localpkg", editable = true }
wheelpkg = { url = "https://example.com/wheelpkg-1.0-py3-none-any.whl" }
private = { index = "internal" }
member = { workspace = true }
platformpkg = [
    { index = "internal", marker = "sys_platform == 'linux'" },
    { path = "./platformpkg", marker = "sys_platform != 'linux'" },
]
internal-tools = { index = "internal" }
winlib = { git = "https://github.com/org/winlib", marker = "sys_platform == 'win32'" }

[[tool.uv.index]]
name = "internal"
url = "https://pypi.internal.example/simple"
explicit = true

[tool.marimo.venv]
path = "/home/me/venv"
writable = false
"""
    lock = """version = 1

[[package]]
name = "internal-tools"
version = "3.0.0"
source = { registry = "https://pypi.internal.example/simple" }

[[package]]
name = "localpkg"
version = "0.1.0"
source = { editable = "libs/localpkg" }
"""
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": pyproject,
            "uv.lock": lock,
            "analysis/nb.py": notebook("import internal_tools, private"),
        },
    )

    assert migrate(workspace, backend=backend) == 0
    sources = {
        "mylib": {
            "git": "https://github.com/org/mylib",
            "tag": "v1.0",
            "subdirectory": "py",
        },
        "localpkg": {"path": "../libs/localpkg", "editable": True},
        "wheelpkg": {"url": "https://example.com/wheelpkg-1.0-py3-none-any.whl"},
    }
    if backend == "pixi":
        # pixi declares a sourced requirement in its pypi-dependencies table,
        # and has no package indexes, workspace members or marker lists.
        sources["mylib"]["extras"] = ["fast"]
        expected = {
            "dependencies": ["marimo"],
            "tool": {"pixi": {"pypi-dependencies": sources}},
        }
        left_out = {"private", "member", "platformpkg", "internal-tools", "winlib"}
    else:
        # uv keeps the requirement and reads its source apart, package indexes
        # included; only a workspace member means nothing outside the workspace.
        sources |= {
            "private": {"index": "internal"},
            "platformpkg": [
                {"index": "internal", "marker": "sys_platform == 'linux'"},
                {"path": "../platformpkg", "marker": "sys_platform != 'linux'"},
            ],
            "internal-tools": {"index": "internal"},
            "winlib": {
                "git": "https://github.com/org/winlib",
                "marker": "sys_platform == 'win32'",
            },
        }
        expected = {
            "dependencies": [
                "marimo",
                "mylib[fast]>=1.0",
                "localpkg==0.1.0",
                "wheelpkg",
                "private",
                "platformpkg",
                "winlib",
                "internal-tools==3.0.0",
            ],
            "tool": {
                "uv": {
                    "index": [
                        {
                            "name": "internal",
                            "url": "https://pypi.internal.example/simple",
                            "explicit": True,
                        }
                    ],
                    "sources": sources,
                }
            },
        }
        left_out = {"member"}
    assert header(workspace / "analysis/nb.py") == {
        "requires-python": "==3.12.*",
        **expected,
    }
    # Never resolved from PyPI instead: left out, and said so.
    for name in ("private", "member", "platformpkg", "internal-tools", "winlib"):
        assert (f"Leaving {name} out" in caplog.text) == (name in left_out)
    assert ("[tool.uv] package indexes" in caplog.text) == (backend == "pixi")


def test_imports_of_a_notebook_that_does_not_parse_are_still_found(tmp_path):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": TEMPLATE_PYPROJECT,
            "nb.py": notebook("import pandas as pd\n    unclosed = ("),
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py")["dependencies"] == ["marimo", "pandas==2.3.3"]


def test_hosted_dataset_notebook_declares_the_image_packages_it_used(tmp_path):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": DATASET_README}
    )

    assert migrate(workspace) == 0
    assert header(workspace / "readme.py")["dependencies"] == [
        "marimo",
        "aqora[pyarrow]==0.20.0",
        "duckdb==1.4.1",
        "polars==1.34.0",
        "pyarrow==21.0.0",
        "sqlglot==27.28.1",
    ]


def test_conda_notebook_carries_the_pixi_manifest(tmp_path):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": CONDA_PIXI,
            "readme.py": notebook("import numpy"),
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "readme.py") == {
        "requires-python": "==3.12.*",
        "dependencies": ["marimo", "six"],
        "tool": {
            "pixi": {
                "workspace": {"channels": ["conda-forge"]},
                "dependencies": {"jq": "*"},
                "pypi-dependencies": {
                    "httpx": {
                        "git": "https://github.com/encode/httpx.git",
                        "tag": "0.28.1",
                    }
                },
            }
        },
    }
    assert not (workspace / "pyproject.toml").exists()
    assert (workspace / "pixi.toml").read_text() == CONDA_PIXI


def test_conda_specs_convert_relative_to_the_notebook(tmp_path, caplog):
    pixi = """[project]
channels = ["conda-forge", { channel = "pytorch", priority = 1 }]
platforms = ["linux-64", "osx-arm64"]

[dependencies]
python = ">=3.11"
gdal = { version = ">=3.8", channel = "conda-forge" }

[pypi-dependencies]
requests = ">=2.31"
httpx = { version = ">=0.27", extras = ["http2", "socks"] }
rich = { extras = ["jupyter"] }
mylib = { path = "./libs/mylib", editable = true }

[target.linux-64.dependencies]
ripgrep = "*"

[target.osx-arm64.activation]
scripts = ["setup.sh"]

[pypi-options]
extra-index-urls = ["https://pypi.internal.example/simple"]
"""
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": pixi,
            "readme.py": notebook("pass"),
            "sub/nb.py": notebook("pass"),
        },
    )

    assert migrate(workspace) == 0
    in_subdirectory = header(workspace / "sub/nb.py")
    # The workspace's own Python pin stays authoritative, so no requires-python
    # contradicts it.
    assert in_subdirectory == {
        "dependencies": [
            "marimo",
            "requests>=2.31",
            "httpx[http2,socks]>=0.27",
            "rich[jupyter]",
        ],
        "tool": {
            "pixi": {
                "workspace": {
                    "channels": ["conda-forge", {"channel": "pytorch", "priority": 1}]
                },
                "dependencies": {
                    "python": ">=3.11",
                    "gdal": {"version": ">=3.8", "channel": "conda-forge"},
                },
                "pypi-dependencies": {
                    "mylib": {"path": "../libs/mylib", "editable": True}
                },
                "target": {"linux-64": {"dependencies": {"ripgrep": "*"}}},
            }
        },
    }
    at_root = header(workspace / "readme.py")["tool"]["pixi"]["pypi-dependencies"]
    assert at_root == {"mylib": {"path": "./libs/mylib", "editable": True}}
    assert "target.osx-arm64" in caplog.text
    assert "[pypi-options]" in caplog.text


@pytest.mark.parametrize(
    "pin",
    [
        '[dependencies]\npython = "3.11.*"\n',
        '[target.linux-64.dependencies]\npython = "3.11.*"\n',
    ],
    ids=["every-platform", "one-platform"],
)
def test_a_pixi_python_pin_is_not_contradicted_by_requires_python(tmp_path, pin):
    # pixi solves with its own Python pin over requires-python, and marimo
    # checks requires-python against the running kernel on every package
    # change: both in one header would ask for a restart that never helps.
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": pin,
            "readme.py": notebook("pass"),
        },
    )

    assert migrate(workspace) == 0
    migrated = header(workspace / "readme.py")
    assert "requires-python" not in migrated
    assert "python" in str(migrated["tool"]["pixi"])


def test_under_uv_requires_python_is_written_beside_a_pixi_python_pin(
    tmp_path, caplog
):
    # uv ignores [tool.pixi], so requires-python is its only Python pin; the
    # conda tables are kept, and the log says they need the Conda runtime.
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": '[dependencies]\npython = "3.11.*"\njq = "*"\n',
            "readme.py": notebook("pass"),
        },
    )

    assert migrate(workspace, backend="uv") == 0
    migrated = header(workspace / "readme.py")
    assert migrated["requires-python"] == "==3.12.*"
    assert migrated["tool"]["pixi"]["dependencies"] == {"python": "3.11.*", "jq": "*"}
    assert "need the Conda runtime" in caplog.text


@pytest.mark.parametrize(
    "index",
    [
        '[tool.uv]\nindex-url = "https://pypi.internal.example/simple"\n',
        (
            '[[tool.uv.index]]\nname = "internal"\n'
            'url = "https://pypi.internal.example/simple"\ndefault = true\n'
        ),
    ],
    ids=["index-url", "default index"],
)
def test_uv_default_index_requirements_are_left_out_under_pixi(tmp_path, caplog, index):
    pyproject = f"""[project]
dependencies = [
    "marimo>=0.23",
    "six",
    "pandas>=2",
    "mylib",
    "attrs @ https://example.com/attrs-25.1.0-py3-none-any.whl",
]

{index}
[tool.uv.sources]
mylib = {{ git = "https://github.com/org/mylib" }}

{UV_VENV}"""
    workspace = write(
        tmp_path,
        {"pyproject.toml": pyproject, "nb.py": notebook("import numpy, pandas, six")},
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": [
            "marimo",
            # a direct reference names its own source
            "attrs @ https://example.com/attrs-25.1.0-py3-none-any.whl",
            # undeclared, imported from the image
            "numpy==2.3.4",
        ],
        "tool": {
            "pixi": {
                "pypi-dependencies": {"mylib": {"git": "https://github.com/org/mylib"}}
            }
        },
    }
    # The default index served them, never PyPI: left out, even where the
    # notebook imports one the image has too, and said so.
    for name in ("six", "pandas"):
        assert f"Leaving {name} out of notebook headers" in caplog.text
    for name in ("marimo", "attrs", "mylib"):
        assert f"Leaving {name} out" not in caplog.text
    assert (
        "pyproject.toml [tool.uv] package indexes are not carried into notebook "
        "headers: declared requirements without a [tool.uv.sources] entry are "
        "left out"
    ) in caplog.text


@pytest.mark.parametrize(
    "indexes",
    [
        'extra-index-url = ["https://pypi.internal.example/simple"]\n',
        'find-links = ["https://pypi.internal.example/wheels/"]\n',
        (
            '\n[[tool.uv.index]]\nname = "internal"\n'
            'url = "https://pypi.internal.example/simple"\n'
        ),
    ],
    ids=["extra-index-url", "find-links", "non-default index"],
)
def test_uv_package_indexes_are_not_carried_under_pixi_and_said_so(
    tmp_path, caplog, indexes
):
    pyproject = f'[project]\ndependencies = ["six"]\n\n[tool.uv]\n{indexes}\n{UV_VENV}'
    workspace = write(
        tmp_path, {"pyproject.toml": pyproject, "nb.py": notebook("import six")}
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": ["marimo", "six"],
    }
    assert (
        "pyproject.toml [tool.uv] package indexes are not carried into notebook "
        "headers: requirements without a [tool.uv.sources] entry will resolve "
        "from PyPI"
    ) in caplog.text


@pytest.mark.parametrize(
    ("indexes", "carried"),
    [
        (
            '[tool.uv]\nindex-url = "https://pypi.internal.example/simple"\n',
            {"index-url": "https://pypi.internal.example/simple"},
        ),
        (
            (
                '[[tool.uv.index]]\nname = "internal"\n'
                'url = "https://pypi.internal.example/simple"\ndefault = true\n'
            ),
            {
                "index": [
                    {
                        "name": "internal",
                        "url": "https://pypi.internal.example/simple",
                        "default": True,
                    }
                ]
            },
        ),
        (
            '[tool.uv]\nextra-index-url = ["https://pypi.internal.example/simple"]\n',
            {"extra-index-url": ["https://pypi.internal.example/simple"]},
        ),
        (
            '[tool.uv]\nfind-links = ["https://pypi.internal.example/wheels/"]\n',
            {"find-links": ["https://pypi.internal.example/wheels/"]},
        ),
    ],
    ids=["index-url", "default index", "extra-index-url", "find-links"],
)
def test_uv_headers_carry_the_workspace_package_indexes(
    tmp_path, caplog, indexes, carried
):
    pyproject = (
        f'[project]\ndependencies = ["six", "pandas>=2"]\n\n{indexes}\n{UV_VENV}'
    )
    workspace = write(
        tmp_path, {"pyproject.toml": pyproject, "nb.py": notebook("import numpy, six")}
    )

    assert migrate(workspace, backend="uv") == 0
    # Every requirement resolves from the indexes it always did.
    assert header(workspace / "nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": ["marimo", "six", "pandas>=2", "numpy==2.3.4"],
        "tool": {"uv": carried},
    }
    assert "Leaving" not in caplog.text
    assert "package indexes are not carried" not in caplog.text


def test_uv_index_locations_are_rebased_onto_the_notebook(tmp_path):
    pyproject = f"""[project]
dependencies = ["tinypkg"]

[tool.uv]
find-links = ["./wheels", "https://example.com/wheels/"]

[[tool.uv.index]]
name = "local"
url = "./simple"
format = "flat"

[tool.uv.sources]
tinypkg = {{ index = "local" }}

{UV_VENV}"""
    workspace = write(
        tmp_path, {"pyproject.toml": pyproject, "analysis/nb.py": notebook("pass")}
    )

    assert migrate(workspace, backend="uv") == 0
    # uv resolves a script's relative locations from the script's directory.
    assert header(workspace / "analysis/nb.py")["tool"]["uv"] == {
        "find-links": ["../wheels", "https://example.com/wheels/"],
        "index": [{"name": "local", "url": "../simple", "format": "flat"}],
        "sources": {"tinypkg": {"index": "local"}},
    }


def test_pixi_default_index_requirements_are_left_out(tmp_path, caplog):
    pixi = """[dependencies]
jq = "*"

[pypi-dependencies]
marimo = "==0.25.0"
six = "*"
rich = { extras = ["jupyter"] }
httpx = { git = "https://github.com/encode/httpx.git", tag = "0.28.1" }

[pypi-options]
index-url = "https://pypi.internal.example/simple"
"""
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": pixi,
            "nb.py": notebook("import six"),
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": ["marimo"],
        "tool": {
            "pixi": {
                "dependencies": {"jq": "*"},
                "pypi-dependencies": {
                    "httpx": {
                        "git": "https://github.com/encode/httpx.git",
                        "tag": "0.28.1",
                    }
                },
            }
        },
    }
    # The index-url served them, never PyPI: left out, and said so.
    for name in ("six", "rich"):
        assert f"Leaving {name} out of notebook headers" in caplog.text
    for name in ("marimo", "httpx"):
        assert f"Leaving {name} out" not in caplog.text
    assert (
        "pixi.toml [pypi-options] is not carried into notebook headers: "
        "[pypi-dependencies] without a source of their own are left out"
    ) in caplog.text


@pytest.mark.parametrize(
    "options",
    [
        'extra-index-urls = ["https://pypi.internal.example/simple"]\n',
        'find-links = [{ url = "https://pypi.internal.example/wheels/" }]\n',
    ],
    ids=["extra-index-urls", "find-links"],
)
def test_pixi_pypi_options_are_not_carried_and_said_so(tmp_path, caplog, options):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": f'[pypi-dependencies]\nsix = "*"\n\n[pypi-options]\n{options}',
            "nb.py": notebook("import six"),
        },
    )

    assert migrate(workspace) == 0
    assert header(workspace / "nb.py") == {
        "requires-python": "==3.12.*",
        "dependencies": ["marimo", "six"],
    }
    assert (
        "pixi.toml [pypi-options] is not carried into notebook headers: "
        "[pypi-dependencies] without a source of their own will resolve from PyPI"
    ) in caplog.text


def test_unparsable_pixi_manifest_still_migrates(tmp_path, caplog):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": CONDA_PYPROJECT,
            "pixi.toml": "[dependencies\n",
            "readme.py": TEMPLATE_README,
        },
    )

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == f"{SEED_HEADER}\n\n{TEMPLATE_README}"
    assert not (workspace / "pyproject.toml").exists()
    assert "pixi.toml" in caplog.text


def test_notebook_with_a_header_is_left_alone(tmp_path):
    pinned = '# /// script\n# dependencies = ["marimo", "numpy==2.0.0"]\n# ///\n'
    pinned += notebook("import numpy")
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": TEMPLATE_PYPROJECT,
            "pinned.py": pinned,
            "readme.py": TEMPLATE_README,
        },
    )

    assert migrate(workspace) == 0
    assert (workspace / "pinned.py").read_bytes() == pinned.encode()
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)


def test_markdown_ignored_and_symlinked_notebooks_are_left_alone(
    tmp_path, capsys, caplog
):
    markdown = (
        "---\ntitle: Notes\nmarimo-version: 0.23.4\n---\n\n"
        "```python {.marimo}\nimport marimo as mo\n```\n"
    )
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": TEMPLATE_PYPROJECT,
            "readme.py": TEMPLATE_README,
            "notes.md": markdown,
            ".ignore": "drafts/\n",
            "drafts/nb.py": TEMPLATE_README,
        },
    )
    (workspace / "link.py").symlink_to(workspace / "readme.py")

    assert migrate(workspace) == 0
    assert (workspace / "notes.md").read_text() == markdown
    assert (workspace / "drafts/nb.py").read_text() == TEMPLATE_README
    assert (workspace / "link.py").is_symlink()
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)
    # The tables still go, so the markdown notebook is left without the
    # workspace's dependencies: said for it in the log and in the summary.
    assert (
        "Skipping notes.md: markdown notebooks are not migrated, so the "
        "workspace's dependencies are not carried into it"
    ) in caplog.text
    assert "notes.md (markdown, dependencies not carried)" in capsys.readouterr().err
    assert "link.py" in caplog.text


def test_legacy_tables_are_removed_even_if_a_notebook_fails(tmp_path):
    broken = b"import marimo\napp = marimo.App()\nname = '\xff'\n"
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    (workspace / "broken.py").write_bytes(broken)

    assert migrate(workspace) == 0
    assert (workspace / "broken.py").read_bytes() == broken
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)
    assert (workspace / "pyproject.toml").read_text() == TEMPLATE_PYPROJECT_REST


@pytest.mark.parametrize(
    ("files", "error"),
    [
        (
            {"pyproject.toml": f'[tool.uv]\nsources = "oops"\n\n{UV_VENV}'},
            "AttributeError",
        ),
        (
            {"pyproject.toml": f'[project]\ndependencies = ["./lib"]\n\n{UV_VENV}'},
            "AttributeError",
        ),
        (
            {
                "pyproject.toml": f'[project]\nname = "x"\n\n{UV_VENV}',
                "uv.lock": '[[package]]\nversion = "1.0"\n',
            },
            "KeyError",
        ),
        (
            {
                "pyproject.toml": f"[tool.ruff]\nline-length = 100\n\n{CONDA_PYPROJECT}",
                "pixi.toml": 'pypi-dependencies = "oops"\n',
            },
            "AttributeError",
        ),
    ],
    ids=["uv sources", "uv requirement", "uv lock", "pixi manifest"],
)
def test_legacy_tables_are_removed_even_if_headers_cannot_be_planned(
    tmp_path, capsys, caplog, files, error
):
    workspace = write(tmp_path, {**files, "readme.py": TEMPLATE_README})

    assert migrate(workspace) == 0
    # No header: run mode serves it on the server's Python, edit mode gives it
    # a marimo-only one, and pyproject.toml keeps what it declared.
    assert (workspace / "readme.py").read_text() == TEMPLATE_README
    assert (workspace / "pyproject.toml").read_text() == (
        files["pyproject.toml"].partition("[tool.marimo")[0]
    )
    summary = capsys.readouterr().err
    assert f"none migrated, failed to plan their headers ({error}: " in summary
    assert "pyproject.toml: legacy tables removed" in summary
    assert f"Failed to plan the notebooks' headers: {error}: " in caplog.text
    assert "Traceback" not in caplog.text


def test_legacy_tables_are_removed_even_if_the_walk_fails(
    tmp_path, monkeypatch, capsys, caplog
):
    caplog.set_level(logging.DEBUG)
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )

    def find_files(*args, **kwargs):
        raise OSError(errno.EIO, "Input/output error")

    monkeypatch.setattr(kubimo_walk, "find_files", find_files)

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == TEMPLATE_README
    assert (workspace / "pyproject.toml").read_text() == TEMPLATE_PYPROJECT_REST
    assert (
        "none migrated, failed to plan their headers "
        "(OSError: [Errno 5] Input/output error)"
    ) in capsys.readouterr().err
    # The traceback too, at debug.
    assert "Traceback" in caplog.text


@pytest.mark.parametrize(
    ("module", "reported"),
    [
        # Needed to find the notebooks: none of them is migrated.
        (
            "marimo._server.files.directory_scanner",
            "none migrated, failed to plan their headers (ModuleNotFoundError: ",
        ),
        # Needed for each notebook: each fails, as a notebook can.
        ("marimo._environments.script_metadata", "1 failed, failed: readme.py"),
    ],
    ids=["is_marimo_app", "script_metadata"],
)
def test_legacy_tables_are_removed_even_if_marimo_helpers_do_not_import(
    tmp_path, monkeypatch, capsys, module, reported
):
    # As after a fork bump that moved just this one: both load first, since
    # the scanner's own imports load script_metadata too.
    for helper in (
        "marimo._server.files.directory_scanner",
        "marimo._environments.script_metadata",
    ):
        importlib.import_module(helper)
    package, _, name = module.rpartition(".")
    monkeypatch.delattr(sys.modules[package], name)
    monkeypatch.setitem(sys.modules, module, None)
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == TEMPLATE_README
    assert (workspace / "pyproject.toml").read_text() == TEMPLATE_PYPROJECT_REST
    assert reported in capsys.readouterr().err


def test_legacy_tables_split_by_another_table_are_removed(tmp_path):
    project = '[project]\nname = "x"\ndependencies = []\n\n'
    venv = '[tool.marimo.venv]\npath = "/home/me/venv"\nwritable = false\n\n'
    ruff = "[tool.ruff]\nline-length = 100\n\n"
    package_management = '[tool.marimo.package_management]\nmanager = "uv"\n'
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": project + venv + ruff + package_management,
            "readme.py": TEMPLATE_README,
        },
    )

    assert migrate(workspace) == 0
    assert (workspace / "pyproject.toml").read_text() == project + ruff


@pytest.mark.parametrize(
    "legacy",
    [
        '[tool.marimo]\nvenv = { path = "/home/me/venv", writable = false }\n',
        '[tool]\nmarimo.venv.path = "/home/me/venv"\nmarimo.venv.writable = false\n',
        '[tool.marimo]\n\n[tool.marimo.venv]\npath = "/home/me/venv"\n',
    ],
    ids=["inline", "dotted", "empty parent"],
)
def test_tool_tables_left_empty_are_removed(tmp_path, legacy):
    project = '[project]\nname = "x"\n\n'
    workspace = write(
        tmp_path, {"pyproject.toml": project + legacy, "readme.py": TEMPLATE_README}
    )

    assert migrate(workspace) == 0
    assert (workspace / "pyproject.toml").read_text() == project


def test_comments_above_the_tables_after_legacy_tables_are_kept(tmp_path):
    project = '[project]\nname = "x"\ndependencies = []\n\n'
    venv = '[tool.marimo.venv]\npath = "/home/me/venv"\n# the image\'s\nwritable = false\n\n'
    ruff = "# Lint settings: keep in sync with CI\n\n[tool.ruff]\nline-length = 100\n\n"
    package_management = '[tool.marimo.package_management]\nmanager = "uv"\n\n'
    display = '# Dark, like the editor\n[tool.marimo.display]\ntheme = "dark"\n'
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": project + venv + ruff + package_management + display,
            "readme.py": TEMPLATE_README,
        },
    )

    assert migrate(workspace) == 0
    assert (workspace / "pyproject.toml").read_text() == project + ruff + display


def test_second_run_changes_nothing(tmp_path):
    workspace = write(
        tmp_path,
        {
            "pyproject.toml": ANALYSIS_PYPROJECT,
            "uv.lock": ANALYSIS_LOCK,
            "readme.py": TEMPLATE_README,
            "nb.py": ANALYSIS_NOTEBOOK,
        },
    )
    assert migrate(workspace) == 0
    migrated = snapshot(workspace)
    assert migrated["nb.py"].startswith(b"# /// script")

    assert migrate(workspace) == 0
    assert snapshot(workspace) == migrated


def _migrate_together(barrier, workspace: Path):
    barrier.wait()
    sys.exit(migrate(workspace))


def test_concurrent_runs_end_with_a_single_runs_bytes(tmp_path):
    files = {"pyproject.toml": TEMPLATE_PYPROJECT}
    for index in range(20):
        files[f"nb{index:02}.py"] = notebook("import numpy" if index % 2 else "pass")
    single = write(tmp_path / "single", files)
    racing = write(tmp_path / "racing", files)
    assert migrate(single) == 0
    assert all(
        text.startswith(b"# /// script")
        for name, text in snapshot(single).items()
        if name.endswith(".py")
    )

    context = multiprocessing.get_context("fork")
    barrier = context.Barrier(2)
    processes = [
        context.Process(target=_migrate_together, args=(barrier, racing), daemon=True)
        for _ in range(2)
    ]
    for process in processes:
        process.start()
    for process in processes:
        process.join(timeout=60)

    assert [process.exitcode for process in processes] == [0, 0]
    assert snapshot(racing) == snapshot(single)


def test_user_edit_during_migration_is_not_overwritten(tmp_path, monkeypatch):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    edited_readme = TEMPLATE_README.replace("My Workspace", "Our Workspace")
    add_polars = ("dependencies = []", 'dependencies = ["polars"]')
    with_header = kubimo_migrate._with_header
    strip_legacy_tables = kubimo_migrate._strip_legacy_tables

    def with_header_as_the_user_saves(text, block):
        (workspace / "readme.py").write_text(edited_readme)
        return with_header(text, block)

    def strip_as_the_user_saves(text):
        if text == TEMPLATE_PYPROJECT:
            (workspace / "pyproject.toml").write_text(text.replace(*add_polars))
        return strip_legacy_tables(text)

    monkeypatch.setattr(kubimo_migrate, "_with_header", with_header_as_the_user_saves)
    monkeypatch.setattr(kubimo_migrate, "_strip_legacy_tables", strip_as_the_user_saves)

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == edited_readme
    assert (workspace / "pyproject.toml").read_text() == (
        TEMPLATE_PYPROJECT_REST.replace(*add_polars)
    )


def test_user_save_while_the_replacement_is_written_is_kept(tmp_path, monkeypatch):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    edited_readme = TEMPLATE_README.replace("My Workspace", "Our Workspace")
    fsync = os.fsync
    saves = [edited_readme]

    def fsync_as_the_user_saves_once(descriptor):
        fsync(descriptor)
        if saves:
            (workspace / "readme.py").write_text(saves.pop())

    monkeypatch.setattr(kubimo_migrate.os, "fsync", fsync_as_the_user_saves_once)

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == edited_readme
    assert (workspace / "pyproject.toml").read_text() == TEMPLATE_PYPROJECT_REST
    assert not list(workspace.glob(".*.kubimo-tmp"))


def test_exit_code_is_1_when_pyproject_keeps_changing(tmp_path, monkeypatch):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    strip_legacy_tables = kubimo_migrate._strip_legacy_tables

    def strip_as_someone_keeps_editing(text):
        (workspace / "pyproject.toml").write_text(text + "\n")
        return strip_legacy_tables(text)

    monkeypatch.setattr(
        kubimo_migrate, "_strip_legacy_tables", strip_as_someone_keeps_editing
    )

    assert migrate(workspace) == 1
    assert "[tool.marimo.venv]" in (workspace / "pyproject.toml").read_text()


@pytest.mark.parametrize(
    ("prefix", "newline"),
    [
        ("", "\r\n"),
        ("#!/usr/bin/env python\n", "\n"),
        ("# -*- coding: utf-8 -*-\n", "\n"),
        ("#!/usr/bin/env python\r\n# -*- coding: utf-8 -*-\r\n", "\r\n"),
    ],
    ids=["crlf", "shebang", "encoding", "shebang, encoding and crlf"],
)
def test_header_goes_below_shebang_and_encoding_lines(tmp_path, prefix, newline):
    body = TEMPLATE_README.replace("\n", newline)
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": prefix + body}
    )

    assert migrate(workspace) == 0
    block = SEED_HEADER.replace("\n", newline)
    assert (workspace / "readme.py").read_bytes() == (
        prefix + block + newline + newline + body
    ).encode()


def test_a_blank_line_below_the_header_is_not_doubled(tmp_path):
    workspace = write(
        tmp_path,
        {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": "\n" + TEMPLATE_README},
    )

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text() == f"{SEED_HEADER}\n\n{TEMPLATE_README}"


def test_migrated_notebook_keeps_its_mode(tmp_path):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    (workspace / "readme.py").chmod(0o754)

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)
    assert (workspace / "readme.py").stat().st_mode & 0o777 == 0o754


def test_requires_python_follows_marimos_default(tmp_path, monkeypatch):
    monkeypatch.setenv("MARIMO_DEFAULT_REQUIRES_PYTHON", ">=3.13")
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )

    assert migrate(workspace) == 0
    assert header(workspace / "readme.py")["requires-python"] == ">=3.13"


def test_a_symlink_loop_does_not_cost_the_workspace_its_migration(tmp_path):
    # The migration runs once: failing to plan still drops the legacy tables,
    # so a walk that trips over a loop would leave the notebooks header-less
    # for good.
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    (workspace / "sub").mkdir()
    (workspace / "sub" / "a").symlink_to("..")
    (workspace / "sub" / "b").symlink_to("..")

    assert migrate(workspace) == 0
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)


def test_wait_for_migrates_once_the_marker_appears(tmp_path):
    workspace = write(
        tmp_path / "workspace",
        {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README},
    )
    marker = tmp_path / "claimed"
    report = tmp_path / "migration"
    before = snapshot(workspace)
    env = {k: v for k, v in os.environ.items() if k != "MARIMO_DEFAULT_REQUIRES_PYTHON"}
    process = subprocess.Popen(
        [
            sys.executable,
            str(MIGRATE_SCRIPT),
            "--log-level",
            "warn",
            "--wait-for",
            str(marker),
            "--report",
            str(report),
            "--system-requirements",
            str(SYSTEM_REQUIREMENTS),
            str(workspace),
        ],
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    try:
        time.sleep(1.5)
        assert process.poll() is None
        assert snapshot(workspace) == before
        assert not report.exists()

        # The agent names the claimed pod in the marker.
        marker.write_text("uid-1")
        _, stderr = process.communicate(timeout=60)
    finally:
        process.kill()
        process.wait()
    assert process.returncode == 0, stderr
    assert (workspace / "readme.py").read_text().startswith(SEED_HEADER)
    # The summary even at --log-level warn.
    assert "legacy uv workspace notebooks: 1 migrated" in stderr
    # Named back once the headers are written: the agent acks on that.
    assert report.read_text() == "uid-1"


def test_the_report_names_the_claim_with_nothing_to_migrate(tmp_path):
    workspace = write(tmp_path / "workspace", {"readme.py": TEMPLATE_README})
    marker = tmp_path / "claimed"
    marker.write_text("uid-1\n")
    report = tmp_path / "migration"
    report.write_text("pending")

    argv = ["--wait-for", str(marker), "--report", str(report), str(workspace)]
    assert kubimo_migrate.main(argv) == 0
    assert report.read_text() == "uid-1"


def test_the_report_names_the_claim_even_if_the_migration_fails(tmp_path, monkeypatch):
    def fail(*args, **kwargs):
        raise RuntimeError("boom")

    monkeypatch.setattr(kubimo_migrate, "migrate", fail)
    marker = tmp_path / "claimed"
    marker.write_text("uid-1")
    report = tmp_path / "migration"

    argv = ["--wait-for", str(marker), "--report", str(report), str(tmp_path)]
    with pytest.raises(RuntimeError, match="boom"):
        kubimo_migrate.main(argv)
    # A migration that failed is over all the same: the claim must not wait on it.
    assert report.read_text() == "uid-1"


def test_waiting_for_a_claim_prepares_the_migration_ahead_of_it(tmp_path, monkeypatch):
    prepared = object()
    monkeypatch.setattr(kubimo_migrate.SystemPackages, "installed", lambda: prepared)
    received = []

    def migrate(workspace, **kwargs):
        received.append(kwargs["system"])
        return 0

    monkeypatch.setattr(kubimo_migrate, "migrate", migrate)
    marker = tmp_path / "claimed"
    claim = threading.Timer(0.5, marker.write_text, ["uid-1"])
    claim.start()
    try:
        argv = ["--wait-for", str(marker), str(tmp_path)]
        assert kubimo_migrate.main(argv) == 0
    finally:
        claim.cancel()
    # Scanned before the claim, not after it.
    assert received == [prepared]


def test_report_needs_wait_for(tmp_path):
    report = tmp_path / "migration"
    with pytest.raises(SystemExit):
        kubimo_migrate.main(["--report", str(report), str(tmp_path)])
    assert not report.exists()


def test_dry_run_prints_the_plan_and_writes_nothing(tmp_path, capsys):
    workspace = write(
        tmp_path, {"pyproject.toml": TEMPLATE_PYPROJECT, "readme.py": TEMPLATE_README}
    )
    before = snapshot(workspace)

    assert (
        kubimo_migrate.main(
            [
                "--dry-run",
                "--system-requirements",
                str(SYSTEM_REQUIREMENTS),
                str(workspace),
            ]
        )
        == 0
    )
    assert snapshot(workspace) == before
    planned = capsys.readouterr().out
    assert SEED_HEADER in planned
    assert '-path = "/home/me/venv"' in planned

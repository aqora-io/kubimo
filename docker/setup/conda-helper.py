#!/usr/bin/env -S /usr/local/bin/uv run --script --quiet --python 3.14
# /// script
# requires-python = ">=3.14"
# dependencies = ["click", "pyyaml"]
# ///
from collections.abc import Iterable
from os import X_OK, PathLike, access, environ, getcwd, uname
from subprocess import PIPE, Popen
from tempfile import NamedTemporaryFile
from pathlib import Path
from time import monotonic
from typing import Any, AnyStr, cast, final

import click
import yaml


type CondaEnvironmentData = dict[Any, Any]


MAMBA_BIN = Path("/usr/local/bin/micromamba")
UV_BIN = Path("/usr/local/bin/uv")


@click.group()
def main():
    pass


@main.command()
@click.option(
    "--file",
    type=click.Path(exists=True, file_okay=True, readable=True, allow_dash=True),
    default="environment.yaml",
)
@click.option(
    "--prefix",
    envvar="CONDA_PREFIX",
    type=click.Path(file_okay=False, allow_dash=False),
    required=True,
)
@click.option(
    "-v",
    "--verbose",
    is_flag=True,
)
@click.option(
    "-x",
    "--print-executed",
    is_flag=True,
)
@click.option(
    "--dry-run",
    is_flag=True,
)
@click.option(
    "--unlocked",
    is_flag=True,
)
@click.option(
    "-C",
    "--directory",
    type=click.Path(exists=True, dir_okay=True, readable=True, allow_dash=False),
    default=getcwd(),
)
def sync(
    file: str | PathLike[str],
    prefix: str | PathLike[str],
    verbose: bool,
    print_executed: bool,
    dry_run: bool,
    unlocked: bool,
    directory: str | PathLike[str],
):
    clock_start = monotonic()
    directory = Path(directory)
    conda_lock = directory / f"conda-lock.{uname().machine}.txt"
    pylock = directory / "pylock.toml"
    prefix = Path(prefix)
    bin_mamba = MAMBA_BIN
    bin_uv = prefix / "bin" / "uv"

    if dry_run:
        verbose = True

    if not access(bin_mamba, X_OK):
        raise click.ClickException(f"Mamba not installed: {bin_mamba}")
    if not access(bin_uv, X_OK) and not access(UV_BIN, X_OK):
        raise click.ClickException(f"Uv not installed: {UV_BIN}, {bin_uv}")

    expected_lockfiles = set([conda_lock, pylock])
    actual_lockfiles = set(filter(Path.is_file, expected_lockfiles))
    if expected_lockfiles == actual_lockfiles:
        locked = not unlocked
    elif prefix.is_dir() and not unlocked:
        missing_lockfiles = (
            str(path.relative_to(directory)) for path in expected_lockfiles
        )
        missing_lockfiles = ", ".join(missing_lockfiles)
        raise click.ClickException(f"Missing lockfiles: {missing_lockfiles}")
    else:
        locked = False

    # Read environment.yaml
    with click.open_file(file) as fd_env:
        data_env = yaml.load(fd_env, yaml.Loader)

    # Split pip dependencies away
    if ret_update := _split_conda_environment(data_env):
        data_env, pip_dependencies = ret_update
        del ret_update
    else:
        click.echo("No dependencies to sync")
        return

    if locked:
        if prefix.is_dir():
            _mamba_env_update(
                bin_mamba,
                conda_lock,
                prefix,
                verbose=verbose,
                print_executed=print_executed,
                dry_run=dry_run,
            )
        else:
            _mamba_env_create(
                bin_mamba,
                conda_lock,
                prefix,
                verbose=verbose,
                print_executed=print_executed,
                dry_run=dry_run,
            )
    else:
        with NamedTemporaryFile(
            mode="w",
            prefix="environment.",
            suffix=".yaml",
            delete_on_close=False,
        ) as fd_env:
            yaml.dump(data_env, fd_env)
            fd_env.flush()
            fd_env.close()

            if prefix.is_dir():
                _mamba_env_update(
                    bin_mamba,
                    fd_env.name,
                    prefix,
                    verbose=verbose,
                    print_executed=print_executed,
                    dry_run=dry_run,
                )
            else:
                _mamba_env_create(
                    bin_mamba,
                    fd_env.name,
                    prefix,
                    verbose=verbose,
                    print_executed=print_executed,
                    dry_run=dry_run,
                )

        # Lock conda dependencies
        if not dry_run:
            with conda_lock.open("w") as fd_condalock:
                _ = fd_condalock.write(
                    _mamba_env_export(bin_mamba, prefix, print_executed=print_executed)
                )

    # Uv may not have been installed yet if dry sync
    if not bin_uv.is_file():
        bin_uv = UV_BIN

    if locked:
        # Update Conda prefix with locked pip dependencies
        _uv_pip_install(
            bin_uv,
            prefix,
            pylock,
            verbose=verbose,
            print_executed=print_executed,
            dry_run=dry_run,
        )
    else:
        # Compile actual pylock.toml contents
        pylock_current = _uv_pip_compile(
            bin_uv,
            prefix,
            pip_dependencies,
            verbose=verbose,
            print_executed=print_executed,
        )

        # Update Conda prefix with up-to-date pip dependencies
        if dry_run:
            with NamedTemporaryFile(
                mode="w", prefix="pylock.", suffix=".toml", delete_on_close=False
            ) as fd_pylock:
                _ = fd_pylock.write(pylock_current)
                fd_pylock.flush()
                fd_pylock.close()

                _uv_pip_install(
                    bin_uv,
                    prefix,
                    fd_pylock.name,
                    verbose=verbose,
                    print_executed=print_executed,
                    dry_run=True,
                )
        else:
            with pylock.open("w") as fd_pylock:
                _ = fd_pylock.write(pylock_current)
            _uv_pip_install(
                bin_uv,
                prefix,
                pylock,
                verbose=verbose,
                print_executed=print_executed,
                dry_run=False,
            )

    clock_elapsed = round(monotonic() - clock_start, 1)
    if verbose:
        click.echo(f"Done in {clock_elapsed}s")


def _split_conda_environment(data_env: CondaEnvironmentData):
    env_dependencies = data_env.get("dependencies")
    if env_dependencies is None:
        return

    # FIXME: Partially validate it
    if not isinstance(env_dependencies, list):
        raise ValueError(
            f"Expected a list at `dependencies` key but got: {type(env_dependencies)!r}"
        )

    conda_dependencies = set()
    pip_dependencies = set()
    for dependency in env_dependencies:
        if isinstance(dependency, dict) and (
            dependencies := dependency.get("pip") or dependency.get("uv")
        ):
            pip_dependencies.update(dependencies)
        else:
            conda_dependencies.add(dependency)

    if pip_dependencies:
        conda_dependencies.add("uv")
        data_env = data_env.copy()
        data_env["dependencies"] = list(conda_dependencies)
        return data_env, pip_dependencies
    else:
        return data_env, set()


def _mamba_env_update(
    bin_mamba: str | PathLike[str],
    specs_file: str | PathLike[str],
    prefix: Path,
    *,
    verbose: bool,
    print_executed: bool,
    dry_run: bool,
) -> None:
    cmd_mamba = [
        bin_mamba,
        "env",
        "update",
        "--yes",
        *([] if verbose else ["--quiet"]),
        *(["--dry-run"] if dry_run else []),
        "--prefix",
        prefix,
        "--file",
        specs_file,
    ]

    if print_executed:
        click.echo(cmd_mamba)

    with Popen(
        cmd_mamba,
        executable=bin_mamba,
        shell=False,
    ) as proc_mamba:
        ret_mamba = proc_mamba.wait()
    if ret_mamba != 0:
        raise MambaError(proc_mamba)


def _mamba_env_create(
    bin_mamba: str | PathLike[str],
    specs_file: str | PathLike[str],
    prefix: Path,
    *,
    verbose: bool,
    print_executed: bool,
    dry_run: bool,
) -> None:
    cmd_mamba = [
        bin_mamba,
        "create",
        "--yes",
        *([] if verbose else ["--quiet"]),
        *(["--dry-run"] if dry_run else []),
        "--prefix",
        prefix,
        "--file",
        specs_file,
    ]

    if print_executed:
        click.echo(cmd_mamba)

    with Popen(
        cmd_mamba,
        executable=bin_mamba,
        shell=False,
    ) as proc_mamba:
        ret_mamba = proc_mamba.wait()
    if ret_mamba != 0:
        raise MambaError(proc_mamba)


def _mamba_env_export(
    bin_mamba: str | PathLike[str],
    prefix: str | PathLike[str],
    *,
    print_executed: bool,
) -> str:
    cmd_mamba = [bin_mamba, "env", "export", "--explicit", "--prefix", prefix]
    if print_executed:
        click.echo(cmd_mamba)
    with Popen(
        cmd_mamba,
        executable=bin_mamba,
        stdout=PIPE,
        shell=False,
        text=True,
    ) as proc_mamba:
        assert proc_mamba.stdout is not None
        data_condalock = cast(str, proc_mamba.stdout.read())
    if proc_mamba.returncode != 0:
        raise MambaError(proc_mamba)
    return data_condalock


def _uv_pip_compile(
    bin_uv: str | PathLike[str],
    prefix: str | PathLike[str],
    pip_dependencies: Iterable[str],
    *,
    verbose: bool,
    print_executed: bool,
) -> str:
    with NamedTemporaryFile(
        mode="w",
        prefix="requirements.",
        suffix=".txt",
        delete_on_close=False,
    ) as fd_pip:
        fd_pip.writelines(pip_dependencies)
        fd_pip.flush()
        fd_pip.close()

        cmd_uv = [
            bin_uv,
            "pip",
            "compile",
            *([] if verbose else ["--quiet"]),
            "--format",
            "pylock.toml",
            fd_pip.name,
        ]
        if print_executed:
            click.echo(cmd_uv)
        cmd_env = environ.copy()
        if virtual_env := cmd_env.pop("VIRTUAL_ENV", None):
            cmd_env["PATH"] = ":".join(
                path
                for path in cmd_env["PATH"].split(":")
                if path != f"{virtual_env}/bin"
            )
        cmd_env["VIRTUAL_ENV"] = str(prefix)

        with Popen(
            cmd_uv,
            executable=bin_uv,
            env=cmd_env,
            stdout=PIPE,
            shell=False,
            text=True,
        ) as proc_uv:
            assert proc_uv.stdout is not None
            data_pylock = cast(str, proc_uv.stdout.read())
        if proc_uv.returncode != 0:
            raise UvError(proc_uv)

    return data_pylock


def _uv_pip_install(
    bin_uv: str | PathLike[str],
    prefix: str | PathLike[str],
    pylock: str | PathLike[str],
    *,
    verbose: bool,
    print_executed: bool,
    dry_run: bool,
):
    cmd_uv = [
        bin_uv,
        "pip",
        "install",
        *([] if verbose else ["--quiet"]),
        *(["--dry-run"] if dry_run else []),
        "-r",
        pylock,
    ]
    if print_executed:
        click.echo(cmd_uv)

    cmd_env = environ.copy()
    if virtual_env := cmd_env.pop("VIRTUAL_ENV", None):
        cmd_env["PATH"] = ":".join(
            path for path in cmd_env["PATH"].split(":") if path != f"{virtual_env}/bin"
        )
    cmd_env["VIRTUAL_ENV"] = str(prefix)

    with Popen(
        cmd_uv,
        executable=bin_uv,
        env=cmd_env,
        shell=False,
    ) as proc_uv:
        ret_uv = proc_uv.wait()
    if ret_uv != 0:
        raise UvError(proc_uv)


@final
class MambaError(click.ClickException):
    exit_code = 10

    def __init__(self, proc: Popen[AnyStr]):
        super().__init__(f"Command failed {proc.returncode}: {proc.args}")


@final
class UvError(click.ClickException):
    exit_code = 11

    def __init__(self, proc: Popen[AnyStr]):
        super().__init__(f"Command failed {proc.returncode}: {proc.args}")


if __name__ == "__main__":
    main()

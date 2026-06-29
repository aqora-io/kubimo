#!/usr/bin/env -S uv run --script
#
# /// script
# dependencies = [
#   "click ~= 8.4.1",
#   "ruamel.yaml ~= 0.19.1",
#   "semver ~= 3.0.4",
#   "tomlkit ~= 0.15.0",
# ]
# ///


from pathlib import Path
from subprocess import check_call

import click
from ruamel.yaml import YAML
import semver
import tomlkit


@click.command()
@click.option("--version", required=True)
@click.option("--directory", "-d")
def main(version: str, directory: str | None = None) -> None:
    version: semver.Version = semver.Version.parse(version)
    directory: Path = Path(directory) if directory else Path.cwd()

    cargo_path = directory / "Cargo.toml"
    helm_chart_path = directory / "charts" / "kubimo-controller" / "Chart.yaml"

    if not cargo_path.is_file():
        raise ValueError(f"Cannot find {cargo_path=!r}")

    if not helm_chart_path.is_file():
        raise ValueError(f"Cannot find {helm_chart_path=!r}")

    cargo_bump(cargo_path, version)
    helm_bump(helm_chart_path, version)


def cargo_bump(path: Path, version: semver.Version) -> None:
    with path.open("r+") as cargo_file:
        cargo_doc = tomlkit.load(cargo_file)

        cargo_doc["workspace"]["package"]["version"] = str(version)

        _ = cargo_file.seek(0)
        _ = cargo_file.truncate()
        tomlkit.dump(cargo_doc, cargo_file)

    _ = check_call(["cargo", "update", "--workspace", "--offline"])


def helm_bump(path: Path, version: semver.Version) -> None:
    yaml = YAML()
    yaml.preserve_quotes = True

    with path.open("r+") as chart_file:
        chart_doc = yaml.load(chart_file)

        app_version = semver.Version.parse(chart_doc["appVersion"])
        chart_version = semver.Version.parse(chart_doc["version"])
        chart_doc["version"] = str(
            replicate_semver_change(app_version, version, chart_version)
        )
        chart_doc["appVersion"] = str(version)

        _ = chart_file.seek(0)
        _ = chart_file.truncate()
        yaml.dump(chart_doc, chart_file)


def replicate_semver_change(
    left: semver.Version, right: semver.Version, version: semver.Version
) -> semver.Version:
    return version.replace(
        major=(version.major - left.major + right.major),
        minor=(version.minor - left.minor + right.minor),
        patch=(version.patch - left.patch + right.patch),
        prerelease=right.prerelease,
        build=right.build,
    )


if __name__ == "__main__":
    main()

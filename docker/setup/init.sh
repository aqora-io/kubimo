#!/bin/sh
set -ex
root=$(dirname $(realpath "$0"))
[ -f pyproject.toml ] || cp $root/pyproject.toml pyproject.toml
[ -f .gitignore ] || cp $root/gitignore .gitignore
git init
uv venv --allow-existing
uv sync || echo "Failed to sync dependencies"
uv pip install marimo

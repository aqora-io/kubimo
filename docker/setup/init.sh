#!/bin/bash

set -e
root=$(dirname $(realpath "$0"))

is_empty_or_empty_git() {
  # Empty directory
  if [ -z "$(ls -A)" ]; then
    return 0
  fi
  # Git repo with only .git present
  if [ -d ".git" ] && [ "$(ls -A | wc -l)" -eq 1 ]; then
    return 0
  fi
  return 1
}

init_template() {
  cp "$root/pyproject.toml" pyproject.toml
  echo "Added pyproject.toml"
  cp "$root/uv.lock" uv.lock
  echo "Added uv.lock"
  cp "$root/gitignore" .gitignore
  echo "Added .gitignore"
  git init --initial-branch main
}

init_marimo() {
  marimo_home="$HOME/.config/marimo"
  if [ -f "$marimo_home/marimo.toml" ]; then
    echo "Found $marimo_home/marimo.toml"
  else
    mkdir -p "$marimo_home"
    cp "$root/marimo.toml" "$marimo_home/marimo.toml"
    echo "Added $marimo_home/marimo.toml"
  fi
  if [ -d "__marimo__" ]; then
    echo "Found __marimo__"
  else
    mkdir "__marimo__"
    echo "Created __marimo__"
  fi
}

init_venv() {
  if [ ! -d ".venv" ]; then
    cp -R "$root/venv" .venv
    echo "Added .venv"
  fi
  if [ -z "$(uv pip list | grep '^marimo')" ]; then
    uv pip install marimo
  fi
}

create_trigger() {
  mkdir -p "$HOME/.kubimo"
  touch "$HOME/.kubimo/trigger"
  echo "Created $HOME/.kubimo/trigger"
}

while [[ $# -gt 0 ]]; do
  case $1 in
  -* | --*)
    echo "Unknown option $1"
    exit 1
    ;;
  *)
    # POSITIONAL_ARGS+=("$1")
    # shift
    echo "Unknown positional arg $1"
    exit 1
    ;;
  esac
done

if is_empty_or_empty_git; then
  init_template
fi
init_marimo
init_venv
create_trigger

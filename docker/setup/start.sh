#!/bin/bash

set -e

while [[ $# -gt 0 ]]; do
  case $1 in
  --base-url)
    BASE_URL="$2"
    shift
    shift
    ;;
  -* | --*)
    echo "Unknown option $1"
    exit 1
    ;;
  *)
    if [ -z "$CMD" ]; then
      CMD="$1"
    else
      echo "Unknown positional arg $1"
      exit 1
    fi
    shift
    ;;
  esac
done

set -x

uv venv --allow-existing
uv pip install marimo

host="0.0.0.0"
port="80"
base_url="${BASE_URL:-/}"

if [[ "$CMD" == "edit" ]]; then
  uv run marimo \
    --log-level=info \
    --yes \
    edit \
    --headless \
    --watch \
    "--host=$host" \
    "--port=$port" \
    "--base-url=$base_url" \
    --allow-origins='*' \
    --no-token
elif [[ "$CMD" == "run" ]]; then
  uv run /app/server.py \
    --include-code \
    "--host=$host" \
    "--port=$port" \
    "--base-url=$base_url"
else
  echo "Unknown command $CMD"
  exit 1
fi

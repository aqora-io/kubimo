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

if [ ! -d ".venv" ]; then
  cp -R "$root/venv" .venv
  echo "Added .venv"
fi
if [ -z "$(uv pip list | grep '^marimo')" ]; then
  uv pip install marimo
fi
uv sync &
sync_pid=$!

host="0.0.0.0"
port="80"

if [ -z "$BASE_URL" ]; then
  base_url_flag=""
else
  base_url_flag="--base-url=$BASE_URL"
fi

if [[ "$CMD" == "edit" ]]; then
  uv run --no-sync marimo \
    --log-level=info \
    --yes \
    edit \
    --headless \
    --watch \
    "--host=$host" \
    "--port=$port" \
    $base_url_flag \
    --allow-origins='*' \
    --no-token
elif [[ "$CMD" == "run" ]]; then
  uv run --no-sync /app/server.py \
    --include-code \
    "--host=$host" \
    "--port=$port" \
    $base_url_flag
else
  echo "Unknown command $CMD"
fi

echo "Run failed"
kill $sync_pid
exit 1

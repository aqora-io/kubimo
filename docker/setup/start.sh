#!/bin/bash

set -e

while [[ $# -gt 0 ]]; do
  case $1 in
  --base-url)
    BASE_URL="$2"
    shift
    shift
    ;;
  --token)
    TOKEN="$2"
    shift
    shift
    ;;
  --log-level)
    LOG_LEVEL="$2"
    shift
    shift
    ;;
  --host)
    HOST="$2"
    shift
    shift
    ;;
  --port)
    PORT="$2"
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

# Unset kubernetes env vars
for name in $(env | sed -n 's/^\(KUBERNETES[^=]*\)=.*/\1/p'); do
  unset "$name"
done

common_flags=("--log-level=${LOG_LEVEL:-info}")
marimo_flags=("--host=${HOST:-0.0.0.0}" "--port=${PORT:-80}")
directory=$(pwd)

if [ -n "$BASE_URL" ]; then
  marimo_flags+=("--base-url=$BASE_URL")
fi

if [ -z "$TOKEN" ]; then
  marimo_flags+=("--no-token")
else
  marimo_flags+=("--token-password=$TOKEN")
fi

if [[ "$CMD" == "edit" ]]; then
  uv sync
  exec "$VIRTUAL_ENV/bin/python3" /setup/launch.py \
    "${common_flags[@]}" \
    --yes \
    edit \
    --skip-update-check \
    --headless \
    --watch \
    --allow-origins='*' \
    "${marimo_flags[@]}" \
    "$directory"

elif [[ "$CMD" == "run" ]]; then
  uv sync
  exec "$VIRTUAL_ENV/bin/python3" /setup/launch.py \
    "${common_flags[@]}" \
    --yes \
    run \
    --headless \
    --watch \
    --allow-origins='*' \
    "${marimo_flags[@]}" \
    --include-code \
    "$directory"

elif [[ "$CMD" == "render" ]]; then
  argv=(
    --host "${HOST:-0.0.0.0}"
    --port "${PORT:-80}"
  )

  if [ -n "$BASE_URL" ]; then
    argv+=(--base-path "$BASE_URL")
  fi
  if [ -n "$TOKEN" ]; then
    argv+=(--token "$TOKEN")
  fi

  uv sync
  exec marimo-ssr serve \
    --marimo /setup/launch.py \
    --venv "$VIRTUAL_ENV" \
    "${argv[@]}" \
    "$directory"

elif [[ "$CMD" == "cache" ]]; then
  uv sync
  exec "$VIRTUAL_ENV/bin/python3" /app/cache.py \
    --include-code "${common_flags[@]}"

else
  echo "Unknown command $CMD"
fi

echo "Run failed"
exit 1

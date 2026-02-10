#!/bin/bash

set -e

wait_for_file() {
  local file="$1"
  local timeout="${2:-30}" # default timeout (seconds) if not provided
  local interval=0.1       # polling interval (seconds)
  local start_time=$(date +%s.%N)

  while [ ! -f "$file" ]; do
    local now=$(date +%s.%N)
    # Compute elapsed time with awk (fast, no external bc)
    local elapsed=$(awk -v n="$now" -v s="$start_time" 'BEGIN {print n - s}')
    if awk -v e="$elapsed" -v t="$timeout" 'BEGIN {exit (e>=t)?0:1}'; then
      return 1 # timed out
    fi
    sleep "$interval"
  done

  return 0 # file found
}

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

file="$HOME/.kubimo/trigger"
timeout="${INIT_TIMEOUT:-30}"
if wait_for_file "$file" "$timeout"; then
  echo "$file detected"
else
  echo "Error: Timed out waiting for $file after $timeout seconds." >&2
  exit 1
fi

if [ ! -d ".venv" ]; then
  cp -R "$root/venv" .venv
  echo "Added .venv"
fi
if [ -z "$(uv pip list | grep '^marimo')" ]; then
  uv pip install marimo
fi
uv sync &
sync_pid=$!

host="${HOST:-0.0.0.0}"
port="${PORT:-80}"
log_level="${LOG_LEVEL:-info}"

host_flag="--host=$host"
port_flag="--port=$port"
log_level_flag="--log-level=$log_level"

if [ -z "$BASE_URL" ]; then
  base_url_flag=""
else
  base_url_flag="--base-url=$BASE_URL"
fi

if [ -z "$TOKEN" ]; then
  token_flag="--no-token"
else
  token_flag="--token-password=$TOKEN"
fi

if [[ "$CMD" == "edit" ]]; then
  uv run --no-sync marimo \
    $log_level_flag \
    --yes \
    edit \
    --skip-update-check \
    --headless \
    --watch \
    --allow-origins='*' \
    $host_flag \
    $port_flag \
    $base_url_flag \
    $token_flag
elif [[ "$CMD" == "run" ]]; then
  uv run --no-sync /app/server.py \
    --include-code \
    --allow-origins='*' \
    $log_level_flag \
    $host_flag \
    $port_flag \
    $base_url_flag \
    $token_flag
elif [[ "$CMD" == "cache" ]]; then
  uv run --no-sync /app/cache.py --include-code $log_level_flag
  kill $sync_pid || echo "No sync process to kill"
  exit 0
else
  echo "Unknown command $CMD"
fi

echo "Run failed"
kill $sync_pid || echo "No sync process to kill"
exit 1

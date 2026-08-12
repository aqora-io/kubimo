#!/bin/bash

set -xe

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
  --origin)
    ORIGIN="$2"
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

TOKEN="${TOKEN:-$MARIMO_TOKEN}"
if [ -z "$TOKEN" ]; then
  marimo_flags+=("--no-token")
else
  marimo_flags+=("--token-password=$TOKEN")
fi

# Workspace content created before the template included [tool.marimo.venv]
# needs the table, otherwise --sandbox builds an ephemeral sandbox per
# notebook instead of using the workspace venv.
ensure_marimo_venv_config() {
  if [ -f pyproject.toml ] && ! is_marimo_venv_configured; then
    /bin/cat >>pyproject.toml <<TOML

[tool.marimo.venv]
path = "$VIRTUAL_ENV"
writable = false
TOML
  fi
}

# Sync the workspace venv, but never install marimo into it.
#
# marimo is already importable from the image's system site-packages, and that
# is the *fork's* build. Workspaces created before this change still declare
# `marimo[recommended,lsp]` in their own pyproject.toml — user data we do not
# rewrite — and installing it would put a PyPI copy in the venv that shadows
# the system one. Kernels would then run a different marimo from the server,
# which marimo only warns about while the two talk over ZeroMQ.
#
# `--no-install-package` skips it without touching the user's declared
# dependencies or their lockfile.
uv_sync_workspace() {
  /usr/local/bin/uv sync --no-install-package marimo
}

pixi_install_workspace() {
  /usr/local/bin/pixi install
}

is_marimo_venv_configured() {
  /usr/local/bin/python3 -c '
import sys, tomllib
with open("pyproject.toml", "rb") as f:
    data = tomllib.load(f)
# Check for the table itself, not a key inside it: appending a duplicate
# [tool.marimo.venv] header would be invalid TOML.
sys.exit(0 if "venv" in data.get("tool", {}).get("marimo", {}) else 1)
'
}

# --sandbox on a directory makes marimo spawn each kernel as an IPC subprocess
# on the workspace venv (see [tool.marimo.venv] in the template pyproject), so
# user-installed packages aren't shadowed by the image's system site-packages
# that launch.py prioritizes for the server.
if [[ "$CMD" == "edit" ]]; then
  # Backgrounded: marimo's --sandbox resolves each kernel's environment lazily
  # at websocket connect, and /health is a constant response, so nothing about
  # serving depends on this finishing. The venv itself already exists and is
  # populated (from the image, via init-dirs or the agent's reflink template),
  # which is what makes this safe — marimo hard-errors on a *missing* venv but
  # never on a stale one.
  #
  # The trade: a workspace that has added dependencies has a window where they
  # are not yet installed, and opening a notebook in it shows marimo's
  # "Install packages" banner. Recoverable, and the same banner users already
  # get for genuinely missing packages.
  if [[ -n "$UV_PROJECT" ]]; then
    uv_sync_workspace &
  elif [[ -x /usr/local/bin/pixi ]]; then
    pixi_install_workspace
    export SHELL=/usr/local/bin/pixi-shell
  fi
  ensure_marimo_venv_config
  export MARIMO_IN_SECURE_ENVIRONMENT=true
  export MARIMO_SESSION_COOKIE_SECURE=true
  exec /usr/local/bin/marimo \
    "${common_flags[@]}" \
    --yes \
    edit \
    --sandbox \
    --skip-update-check \
    --headless \
    --watch \
    --allow-origins='*' \
    "${marimo_flags[@]}" \
    "$directory"

elif [[ "$CMD" == "run" ]]; then
  # Same reasoning as `edit` above.
  if [[ -n "$UV_PROJECT" ]]; then
    uv_sync_workspace &
  elif [[ -x /usr/local/bin/pixi ]]; then
    pixi_install_workspace
  fi
  ensure_marimo_venv_config
  export MARIMO_IN_SECURE_ENVIRONMENT=true
  export MARIMO_SESSION_COOKIE_SECURE=true
  exec /usr/local/bin/marimo \
    "${common_flags[@]}" \
    --yes \
    run \
    --sandbox \
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

  if [ -n "$ORIGIN" ]; then
    argv+=(--origin "$ORIGIN")
  fi
  if [ -n "$BASE_URL" ]; then
    argv+=(--base-path "$BASE_URL")
  fi
  if [ -n "$TOKEN" ]; then
    argv+=(--token "$TOKEN")
  fi

  exec /usr/local/bin/marimo-ssr serve "${argv[@]}" "$directory"

elif [[ "$CMD" == "cache" ]]; then
  if [[ -n "$UV_PROJECT" ]]; then
    uv_sync_workspace
  elif [[ -x /usr/local/bin/pixi ]]; then
    pixi_install_workspace
    export VIRTUAL_ENV="$CONDA_PREFIX"
  fi
  exec "$VIRTUAL_ENV/bin/python3" /app/cache.py \
    --include-code "${common_flags[@]}"

else
  echo "Unknown command $CMD"
fi

echo "Run failed"
exit 1

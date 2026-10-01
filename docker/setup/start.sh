#!/bin/bash

# Never `set -x`: it would trace the token into the pod log.
set -euo pipefail

# Declared unexported before it ever holds the token: bash exports every
# variable it imports from the environment, and an assignment keeps the mark,
# so a same-named variable in the pod's env would hand the token to marimo
# and every kernel.
declare +x kubimo_token=""
base_url=""
log="info"
host="0.0.0.0"
port="80"
origin=""
cmd=""

while [[ $# -gt 0 ]]; do
  case $1 in
  --base-url)
    base_url="$2"
    shift
    shift
    ;;
  --token)
    kubimo_token="$2"
    shift
    shift
    ;;
  --log-level)
    log="$2"
    shift
    shift
    ;;
  --host)
    host="$2"
    shift
    shift
    ;;
  --port)
    port="$2"
    shift
    shift
    ;;
  --origin)
    origin="$2"
    shift
    shift
    ;;
  -*)
    echo "Unknown option $1"
    exit 1
    ;;
  *)
    if [ -z "$cmd" ]; then
      cmd="$1"
    else
      echo "Unknown positional arg $1"
      exit 1
    fi
    shift
    ;;
  esac
done

# Out of the environment before anything forks, so no child ever inherits it.
# marimo gets the token on stdin alone (see exec_marimo).
kubimo_token="${kubimo_token:-${MARIMO_TOKEN:-}}"
unset MARIMO_TOKEN

# The sandbox backend this pod's notebooks run with, from its workspace's
# runtime (its pool's, on a warm pod). An env var, like KUBIMO_ASSET_URL. The
# controller always sets it; unset is uv, as an absent runtime is. It also
# picks the backend a legacy workspace is migrated for, once and for good.
sandbox="${KUBIMO_SANDBOX:-uv}"
if [[ "$sandbox" != "uv" && "$sandbox" != "pixi" ]]; then
  echo "Unknown KUBIMO_SANDBOX $sandbox" >&2
  exit 1
fi

# Unset kubernetes env vars
for name in $(env | sed -n 's/^\(KUBERNETES[^=]*\)=.*/\1/p'); do
  unset "$name"
done

ws=$(pwd)
marimo_flags=("--host=$host" "--port=$port")

if [ -n "$base_url" ]; then
  marimo_flags+=("--base-url=$base_url")
fi

# Shared static-asset origin: rewrites the served HTML's ./assets/ references
# to a runner-independent URL so browsers cache marimo's frontend across
# runners and warm-pod claims. An env var, never a flag, for the same reason
# as the claim marker below.
if [ -n "${KUBIMO_ASSET_URL:-}" ]; then
  marimo_flags+=("--asset-url=$KUBIMO_ASSET_URL")
fi

if [ -n "$kubimo_token" ]; then
  marimo_flags+=("--token-password-file=-")
else
  marimo_flags+=("--no-token")
fi

# The token reaches marimo on stdin: as an argument it would show in every
# process listing, and in the environment it would reach every kernel.
# --quiet because marimo's startup banner prints its URL with
# ?access_token=<token>; it silences marimo's other console notices too (such
# as sandbox syncs), never its log lines.
exec_marimo() {
  if [ -n "$kubimo_token" ]; then
    exec /usr/local/bin/marimo --log-level="$log" --quiet --yes "$@" <<<"$kubimo_token"
  fi
  exec /usr/local/bin/marimo --log-level="$log" --yes "$@" </dev/null
}

# Workspaces created for the uv or conda images pin an environment this image
# no longer has; kubimo_migrate.py gives their notebooks PEP 723 headers once
# and exits early for every other workspace. A failure is logged and does not
# stop the runner.
migrate_workspace() {
  /usr/local/bin/python3 /app/kubimo_migrate.py --log-level "$log" \
    --backend "$sandbox" "$@" "$ws" ||
    echo "Migrating the legacy workspace failed, starting anyway" >&2
}

# marimo's experimental isolate_apps serves every session of a notebook from
# one process in that notebook's environment, instead of a kernel process per
# session: a viewer joining a running notebook skips the kernel start, and
# eight sessions took 0.34 GB instead of 1.68 GB. On for Run runners unless
# their env sets KUBIMO_ISOLATE_APPS=false. marimo only reads it from config,
# so it is written into the slot's user config, which stays on the node rather
# than in the workspace archive, and always explicitly: the slot outlives the
# runner, so an opt-out must overwrite an earlier true.
isolate_apps() {
  /usr/local/bin/python3 - "$1" <<'PY' ||
import sys

import tomlkit
from marimo._utils.xdg import marimo_config_path

path = marimo_config_path()
doc = tomlkit.parse(path.read_text()) if path.exists() else tomlkit.document()
doc.setdefault("experimental", tomlkit.table())["isolate_apps"] = sys.argv[1] == "true"
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(tomlkit.dumps(doc))
PY
    echo "Setting isolate_apps failed, starting with marimo's config as it is" >&2
}

# Warm-pool pre-boot. Set (as an env var, never a flag — an older image must
# start normally rather than crash on an unknown argument) when this pod was
# minted for a pool: the workspace is only the node template until a claim
# hydrates the tenant's files and the agent drops the marker file, which
# kubimo_migrate.py waits for before migrating them.
#
# Backgrounded as a direct command, never a `( ... ) &` subshell: a forked
# bash keeps this script's argv, --token included, for as long as it runs. It
# survives the exec below; marimo stays PID 1 so signal handling is unchanged.
# A probe restart re-runs this script with the marker already present and
# migrates in the foreground like any cold runner — idempotent by construction.
#
# The agent holds its ack, and with it every session, until the migration
# reports done through KUBIMO_MIGRATION_MARKER; a session planned first would
# give a legacy notebook the server's Python for as long as it stays open.
# "pending" goes down before the marker check, so whichever comes first, this
# line or the claim, either the agent waits for the report or the migration
# runs in the foreground below, before marimo serves anything.
if [[ "$cmd" == "edit" || "$cmd" == "run" ]]; then
  if [[ -n "${KUBIMO_CLAIM_MARKER:-}" ]]; then
    claim_args=(--wait-for "$KUBIMO_CLAIM_MARKER")
    if [[ -n "${KUBIMO_MIGRATION_MARKER:-}" ]]; then
      printf pending >"$KUBIMO_MIGRATION_MARKER.tmp" &&
        mv -f "$KUBIMO_MIGRATION_MARKER.tmp" "$KUBIMO_MIGRATION_MARKER" ||
        echo "Could not report the migration pending; a claim will not wait for it" >&2
      claim_args+=(--report "$KUBIMO_MIGRATION_MARKER")
    fi
    if [[ ! -e "$KUBIMO_CLAIM_MARKER" ]]; then
      /usr/local/bin/python3 /app/kubimo_migrate.py --log-level "$log" \
        --backend "$sandbox" "${claim_args[@]}" "$ws" &
    else
      migrate_workspace "${claim_args[@]}"
    fi
  else
    migrate_workspace
  fi
fi

# --sandbox on a directory runs each notebook's kernel in the environment its
# PEP 723 header describes, built by $sandbox, with this image's marimo wheel
# overlaid (MARIMO_RUNTIME_WHEEL); nothing is synced before marimo starts.
if [[ "$cmd" == "edit" ]]; then
  export MARIMO_IN_SECURE_ENVIRONMENT=true
  export MARIMO_SESSION_COOKIE_SECURE=true
  # Otherwise marimo relaunches its server through `uv run` to overlay its
  # editor tools on this interpreter, which already has them. marimo pops the
  # variable, so kernels do not inherit it.
  export MARIMO_SERVER_OVERLAY=1
  exec_marimo \
    edit \
    --sandbox="$sandbox" \
    --skip-update-check \
    --headless \
    --watch \
    --allow-origins='*' \
    "${marimo_flags[@]}" \
    "$ws"

elif [[ "$cmd" == "run" ]]; then
  export MARIMO_IN_SECURE_ENVIRONMENT=true
  export MARIMO_SESSION_COOKIE_SECURE=true
  if [[ "${KUBIMO_ISOLATE_APPS:-}" == "false" ]]; then
    isolate_apps false
  else
    isolate_apps true
  fi
  exec_marimo \
    run \
    --sandbox="$sandbox" \
    --headless \
    --watch \
    --allow-origins='*' \
    "${marimo_flags[@]}" \
    --include-code \
    "$ws"

elif [[ "$cmd" == "render" ]]; then
  argv=(
    --host "$host"
    --port "$port"
  )

  if [ -n "$origin" ]; then
    argv+=(--origin "$origin")
  fi
  if [ -n "$base_url" ]; then
    argv+=(--base-path "$base_url")
  fi
  if [ -n "$kubimo_token" ]; then
    argv+=(--token "$kubimo_token")
  fi

  # React, which marimo-ssr loads from node_modules, picks its build from
  # this at runtime; unset, pages render with the slower development build.
  export NODE_ENV=production
  exec /usr/local/bin/marimo-ssr serve "${argv[@]}" "$ws"

elif [[ "$cmd" == "cache" ]]; then
  migrate_workspace
  exec /usr/local/bin/python3 /app/cache.py --include-code --log-level="$log" \
    --backend "$sandbox" "$ws"

else
  echo "Unknown command $cmd"
fi

echo "Run failed"
exit 1

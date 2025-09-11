#!/bin/bash

set -e
root=$(dirname $(realpath "$0"))

add_ssh_known_hosts() {
  if [ -z "$1" ]; then
    echo "No SSH host provided"
  else
    ssh-keyscan -p "${2:-22}" "$1" >>"$HOME/.ssh/known_hosts"
    echo "Added $1 to known_hosts"
  fi
}

add_ssh_key() {
  SSH_KEY_NAME="id_kubimo"
  if [ -z "$1" ]; then
    echo "No SSH key provided"
  else
    echo "$1" >"$HOME/.ssh/$SSH_KEY_NAME"
    chmod 600 "$HOME/.ssh/$SSH_KEY_NAME"
    echo "IdentityFile $HOME/.ssh/$SSH_KEY_NAME" >>"$HOME/.ssh/config"
    chmod 600 "$HOME/.ssh/config"
    echo "Added SSH key $SSH_KEY_NAME"
  fi
}

add_git_name() {
  if [ -z "$1" ]; then
    echo "No git name"
  else
    git config --global user.name "$1"
    echo "Git name set to $1"
  fi
}

add_git_email() {
  if [ -z "$1" ]; then
    echo "No git email"
  else
    git config --global user.email "$1"
    echo "Git email set to $1"
  fi
}

s3_download() {
  if [ -z "$1" ]; then
    echo "No S3 url provided"
  else
    s3-tar download "$1" .
    echo "$1 unpacked"
  fi
}

git_clone() {
  if [ -z "$1" ]; then
    echo "No repo provided"
  else
    if [ ! -z "$2" ]; then
      local branch="--branch $2"
    fi
    if [ ! -z "$3" ]; then
      local revision="--revision $3"
    fi
    git clone --depth 1 --recurse-submodules $branch $revision $1 .
  fi
}

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
  cp "$root/gitignore" .gitignore
  echo "Added .gitignore"
  git init --initial-branch main
}

init_venv() {
  if [ ! -d ".venv" ]; then
    cp -R "$root/venv" .venv
    echo "Added .venv"
  fi
  if [ -z "$(uv pip list | grep '^marimo')" ]; then
    uv pip install "$DEFAULT_MARIMO_VERSION"
  fi
}

while [[ $# -gt 0 ]]; do
  case $1 in
  --repo)
    REPO="$2"
    shift
    shift
    ;;
  --branch)
    BRANCH="$2"
    shift
    shift
    ;;
  --revision)
    REVISION="$2"
    shift
    shift
    ;;
  --ssh-key)
    SSH_KEY="$2"
    shift
    shift
    ;;
  --ssh-host)
    SSH_HOST="$2"
    shift
    shift
    ;;
  --ssh-port)
    SSH_PORT="$2"
    shift
    shift
    ;;
  --git-name)
    GIT_NAME="$2"
    shift
    shift
    ;;
  --git-email)
    GIT_EMAIL="$2"
    shift
    shift
    ;;
  --s3-url)
    S3_URL="$2"
    shift
    shift
    ;;
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

add_ssh_known_hosts "$SSH_HOST" "$SSH_PORT"
add_ssh_key "$SSH_KEY"
add_git_name "$GIT_NAME"
add_git_email "$GIT_EMAIL"
s3_download "$S3_URL"
git_clone "$REPO" "$BRANCH" "$REVISION"
if is_empty_or_empty_git; then
  init_template
fi
init_venv

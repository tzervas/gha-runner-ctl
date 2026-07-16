#!/usr/bin/env bash
# Verify rootless Podman for the current user (expect gha-agent).
# Prefer invoking via setup-rootless phase 05 or a fresh user shell.

SCRIPT_DIR="$(dirname "$0")"

# Resolve debug lib: sibling lib/, or ~/lib from setup-rootless copy.
if [[ -f "${SCRIPT_DIR}/lib/shell-debug.sh" ]]
then
  GHA_SHELL_DEBUG_LIB="${SCRIPT_DIR}/lib/shell-debug.sh"
elif [[ -f "${HOME}/lib/shell-debug.sh" ]]
then
  GHA_SHELL_DEBUG_LIB="${HOME}/lib/shell-debug.sh"
elif [[ -f "${SCRIPT_DIR}/../scripts/lib/shell-debug.sh" ]]
then
  GHA_SHELL_DEBUG_LIB="${SCRIPT_DIR}/../scripts/lib/shell-debug.sh"
fi

if [[ -n "${GHA_SHELL_DEBUG_LIB:-}" ]]
then
  # shellcheck source=lib/shell-debug.sh
  . "${GHA_SHELL_DEBUG_LIB}"
  gha_shell_debug_init
else
  set -euo pipefail
fi

if [[ "$(id -u)" -eq 0 ]]
then
  echo "verify-rootless: FAIL — running as root. Use: sudo -u gha-agent -H …" >&2
  exit 1
fi

if [[ -z "${HOME:-}" ]]
then
  HOME="$(getent passwd "$(id -un)" | cut -d: -f6)"
  export HOME
fi

if [[ -z "${XDG_RUNTIME_DIR:-}" ]]
then
  XDG_RUNTIME_DIR="/run/user/$(id -u)"
  export XDG_RUNTIME_DIR
fi

mkdir -p "$XDG_RUNTIME_DIR"
cd "$HOME"

echo "verify-rootless: user=$(id -un) uid=$(id -u) home=$HOME"

if command -v sudo >/dev/null 2>&1
then
  if sudo -n true 2>/dev/null
  then
    echo "verify-rootless: FAIL — current user can sudo (passwordless)." >&2
    exit 1
  fi
fi
echo "verify-rootless: sudo not available to this user (good)"

ROOTLESS="false"
if ROOTLESS_OUT="$(podman info --format '{{.Host.Security.Rootless}}' 2>/dev/null)"
then
  ROOTLESS="$ROOTLESS_OUT"
fi

if [[ "$ROOTLESS" != "true" ]]
then
  echo "verify-rootless: FAIL — podman not rootless (got: $ROOTLESS)" >&2
  if ! podman info 2>&1 \
    | head -30 \
    >&2
  then
    :
  fi
  exit 1
fi
echo "verify-rootless: rootless=true"

if [[ "${GHA_SKIP_PULL_TEST:-}" != "1" ]]
then
  podman run --rm docker.io/library/alpine:3.20 \
    sh -c 'echo "container-ok host_mapped_as=$(id -u)"; test ! -u /bin/su; test ! -e /usr/bin/sudo; echo no-sudo-in-image-ok'
else
  echo "verify-rootless: skipping alpine pull test (GHA_SKIP_PULL_TEST=1)"
fi

ROOTFUL_SOCK="unix:///run/podman/podman.sock"
if [[ "${CONTAINER_HOST:-}" == "$ROOTFUL_SOCK" ]]
then
  if podman info &>/dev/null
  then
    echo "verify-rootless: FAIL — CONTAINER_HOST is rootful system socket and podman info succeeds." >&2
    exit 1
  fi
fi

if [[ -S /run/podman/podman.sock ]]
then
  if CONTAINER_HOST=unix:///run/podman/podman.sock podman info &>/dev/null
  then
    echo "verify-rootless: FAIL — can talk to rootful system socket; tighten socket perms." >&2
    exit 1
  else
    echo "verify-rootless: cannot use rootful /run/podman/podman.sock (good)"
  fi
fi

echo "verify-rootless: PASS"

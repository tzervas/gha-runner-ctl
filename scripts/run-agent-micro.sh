#!/usr/bin/env bash
# Run the micro-agent container with maximum lockdown.
#
# Intentionally *unable* to spawn work containers:
#   - no --privileged
#   - no Podman/Docker socket mount
#   - all capabilities dropped
#   - read-only rootfs, no-new-privileges
#   - nonroot (65532)
#
# Full listen/warm/up: run host binary as gha-agent (rootless), not this image.
# Debug: GHA_DEBUG=1 (trace)  GHA_DEBUG_ON_ERR=1 (default dump on failure).

SCRIPT_DIR="$(dirname "$0")"
# shellcheck source=lib/shell-debug.sh
. "${SCRIPT_DIR}/lib/shell-debug.sh"
gha_shell_debug_init

TAG="${GHA_AGENT_IMAGE:-localhost/gha-runner-ctl-agent:latest}"
NAME="${GHA_AGENT_NAME:-gha-fleet-agent}"

if [[ -n "${CONTAINER_HOST:-}" ]]
then
  echo "run-agent-micro: refuse CONTAINER_HOST (no runtime sockets in micro-agent)" >&2
  exit 1
fi

# Do not pass -t when not a TTY (CI / scripted).
TTY_ARGS=()
if [[ -t 0 ]]
then
  if [[ -t 1 ]]
  then
    TTY_ARGS=(-it)
  else
    TTY_ARGS=(-i)
  fi
else
  TTY_ARGS=(-i)
fi

exec podman run --rm "${TTY_ARGS[@]}" \
  --name "$NAME" \
  --read-only \
  --tmpfs /tmp:rw,size=16m,mode=1777 \
  --cap-drop=ALL \
  --security-opt no-new-privileges \
  --user 65532:65534 \
  --network slirp4netns \
  --memory 256m \
  --cpus 0.5 \
  --pids-limit 64 \
  "${TAG}" \
  "$@"

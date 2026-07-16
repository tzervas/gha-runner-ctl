#!/usr/bin/env bash
# Build the fleet-agent micro image (control plane only).
#
# Debug: GHA_DEBUG=1 (trace)  GHA_DEBUG_ON_ERR=1 (default dump on failure).

SCRIPT_DIR="$(dirname "$0")"
# shellcheck source=lib/shell-debug.sh
. "${SCRIPT_DIR}/lib/shell-debug.sh"
gha_shell_debug_init

cd "$SCRIPT_DIR"
cd ..
ROOT="$(pwd)"

echo "build-agent-image: cargo build --release (LTO + strip)…"
cargo build --release

BIN="$ROOT/target/release/gha-runner-ctl"
if [[ ! -x "$BIN" ]]
then
  echo "build-agent-image: missing executable $BIN" >&2
  exit 1
fi
echo "build-agent-image: binary $(du -h "$BIN" | awk '{print $1}')"

TAG="${GHA_AGENT_IMAGE:-localhost/gha-runner-ctl-agent:latest}"
echo "build-agent-image: podman build → $TAG"
podman build -f packaging/Containerfile.agent -t "$TAG" .

echo "build-agent-image: done — $TAG"
echo "  Prefer host binary as gha-agent; micro-agent has no Podman socket."
echo "  Work jobs still use: localhost/gha-runner-ctl:latest (packaging/Containerfile)"

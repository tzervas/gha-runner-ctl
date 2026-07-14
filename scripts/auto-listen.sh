#!/bin/bash
# Fully automated listen helper.
#
# Usage:
#   # Current checkout → register that repo only
#   bash scripts/auto-listen.sh
#
#   # All personal (owned) repos for the logged-in user (batch)
#   bash scripts/auto-listen.sh --batch
#   bash scripts/auto-listen.sh --batch --user tzervas
#
#   # Organization runner
#   bash scripts/auto-listen.sh --org vectorweighttechnologies
#
# Env: GHA_INTERVAL (default 30), GHA_IDLE (default 180), PATH with gha-runner-ctl
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CTL="${GHA_CTL:-}"
if [[ -z "$CTL" ]]; then
    for c in "$HOME/.local/bin/gha-runner-ctl" "$ROOT/target/release/gha-runner-ctl"; do
        [[ -x "$c" ]] && CTL="$c" && break
    done
fi
if [[ -z "$CTL" ]]; then
    # Prefer release install; fall back to cargo if present
    if command -v gha-runner-ctl >/dev/null 2>&1; then
        CTL="$(command -v gha-runner-ctl)"
    else
        echo "gha-runner-ctl not found. Install from GitHub Release or: cargo build --release" >&2
        exit 1
    fi
fi

INTERVAL="${GHA_INTERVAL:-30}"
IDLE="${GHA_IDLE:-180}"
MODE="auto" # auto | batch | org
USER_LOGIN=""
ORG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --batch) MODE=batch; shift ;;
        --user) USER_LOGIN="${2:-}"; shift 2 ;;
        --org) MODE=org; ORG="${2:-}"; shift 2 ;;
        --interval) INTERVAL="${2:-30}"; shift 2 ;;
        --idle) IDLE="${2:-180}"; shift 2 ;;
        -h | --help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

# Ensure snapshot exists
if ! podman volume exists gha-runner-ctl-data 2>/dev/null; then
    echo "auto-listen: prepare snapshot…"
    "$CTL" prepare
fi

case "$MODE" in
    auto)
        echo "auto-listen: current repo (--auto) interval=${INTERVAL}s idle=${IDLE}s"
        exec "$CTL" --scope repo --auto --mode ephemeral \
            listen --interval "$INTERVAL" --idle-secs "$IDLE"
        ;;
    batch)
        args=(--scope user --mode ephemeral)
        [[ -n "$USER_LOGIN" ]] && args+=(--user "$USER_LOGIN")
        echo "auto-listen: batch personal repos interval=${INTERVAL}s idle=${IDLE}s"
        exec "$CTL" "${args[@]}" listen --interval "$INTERVAL" --idle-secs "$IDLE"
        ;;
    org)
        [[ -n "$ORG" ]] || { echo "--org NAME required"; exit 2; }
        echo "auto-listen: org=${ORG} interval=${INTERVAL}s idle=${IDLE}s"
        exec "$CTL" --scope org --owner "$ORG" --mode ephemeral \
            listen --interval "$INTERVAL" --idle-secs "$IDLE"
        ;;
esac

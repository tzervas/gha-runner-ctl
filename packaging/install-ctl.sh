#!/bin/bash
# Install gha-runner-ctl to ~/.local/bin (or --prefix).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${HOME}/.local"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix) PREFIX="${2:-}"; shift 2 ;;
        -h | --help)
            printf 'Usage: %s [--prefix DIR]\n' "$0"
            exit 0
            ;;
        *) printf 'unknown arg: %s\n' "$1" >&2; exit 2 ;;
    esac
done

command -v cargo >/dev/null || { echo "cargo required (Rust 1.96+)"; exit 1; }
command -v podman >/dev/null || { echo "podman required"; exit 1; }

echo "Building gha-runner-ctl (release)…"
(cd "$ROOT" && cargo build --release)

mkdir -p "${PREFIX}/bin"
install -m 755 "$ROOT/target/release/gha-runner-ctl" "${PREFIX}/bin/gha-runner-ctl"
echo "Installed: ${PREFIX}/bin/gha-runner-ctl"

if ! command -v gha-runner-ctl >/dev/null 2>&1; then
    echo "Add to PATH: export PATH=\"${PREFIX}/bin:\$PATH\""
fi

echo
echo "Next:"
echo "  gha-runner-ctl prepare"
echo "  # repo:  GHA_SCOPE=repo GHA_REPO=owner/name gha-runner-ctl listen"
echo "  # org:   GHA_SCOPE=org  GHA_OWNER=my-org   gha-runner-ctl listen"

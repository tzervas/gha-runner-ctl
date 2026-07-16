#!/bin/bash
# Install gha-runner-ctl to ~/.local/bin (or --prefix).
# Debug: GHA_DEBUG=1 (trace)  GHA_DEBUG_ON_ERR=1 (default dump on failure).

SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"
# Optional shared debug (repo checkout only).
if [[ -f "${SCRIPT_DIR}/../scripts/lib/shell-debug.sh" ]]
then
  # shellcheck source=../scripts/lib/shell-debug.sh
  . "${SCRIPT_DIR}/../scripts/lib/shell-debug.sh"
  gha_shell_debug_init
else
  set -euo pipefail
fi

cd "$SCRIPT_DIR"
cd ..
ROOT="$(pwd)"

PREFIX="${HOME}/.local"
while [[ $# -gt 0 ]]
do
  case "$1" in
    --prefix)
      PREFIX="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'Usage: %s [--prefix DIR]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown arg: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if ! command -v cargo >/dev/null
then
  echo "cargo required (Rust 1.96+)"
  exit 1
fi

if ! command -v podman >/dev/null
then
  echo "podman required"
  exit 1
fi

echo "Building gha-runner-ctl (release)…"
cd "$ROOT"
cargo build --release

mkdir -p "${PREFIX}/bin"
install -m 755 \
  "$ROOT/target/release/gha-runner-ctl" \
  "${PREFIX}/bin/gha-runner-ctl"
echo "Installed: ${PREFIX}/bin/gha-runner-ctl"

if ! command -v gha-runner-ctl >/dev/null 2>&1
then
  echo "Add to PATH: export PATH=\"${PREFIX}/bin:\$PATH\""
fi

echo
echo "Next:"
echo "  gha-runner-ctl prepare"
echo "  # 1-click: detect cwd repo or fall back to personal user batch"
echo "  gha-runner-ctl --full-auto"
echo "  # user batch:  gha-runner-ctl --scope user --user YOUR_LOGIN listen"
echo "  # repo:        gha-runner-ctl --scope repo --repo owner/name listen"
echo "  # org:         gha-runner-ctl --scope org  --owner my-org listen"

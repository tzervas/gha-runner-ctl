#!/bin/bash
# gha-runner-ctl automated listen wrapper
# Fully rewritten into the Rust controller binary via the --full-auto flag.
set -euo pipefail

# Ensure gha-runner-ctl is in PATH, if not try local release build
if ! command -v gha-runner-ctl >/dev/null 2>&1; then
    ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if [[ -x "$ROOT/target/release/gha-runner-ctl" ]]; then
        export PATH="$ROOT/target/release:$PATH"
    fi
fi

exec gha-runner-ctl --full-auto "$@"

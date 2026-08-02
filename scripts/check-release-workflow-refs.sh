#!/usr/bin/env bash
# Guards against the bug class #114 shipped: release-dispatch.yml hardcoded the
# pre-rename package name (`gha-runner-ctl`) in the crates.io preflight URL and in
# `cargo update -p`, and hardcoded the pre-rename *binary* name in the archive step.
# The rename (#114) landed without touching any of those three literals, so the
# crates.io preflight silently and permanently read "not published" (it was asking
# about a crate name that was never the real one), and the release archive shipped
# the wrong binary.
#
# The fix makes the workflow resolve the package name and every [[bin]] name from
# Cargo.toml at run time ($PKG / $PRIMARY_BIN / $BINS — see the "Resolve the version
# to release" step) instead of pasting a literal. This script keeps that true: it
# fails if the specific spots that must stay dynamic ever regain a hardcoded crate
# or binary name literal, so the next rename can't silently reopen the same bug.
#
# Usage: scripts/check-release-workflow-refs.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$ROOT/.github/workflows/release-dispatch.yml"

status=0

fail() {
    echo "::error::$*"
    status=1
}

if [[ ! -f "$WORKFLOW" ]]; then
    fail "expected to find $WORKFLOW"
    exit 1
fi

# 1. The crates.io preflight must build its URL from $PKG, never a literal crate name.
bad_crate_url=$(grep -nE 'crates\.io/api/v1/crates/[A-Za-z0-9_-]' "$WORKFLOW" | grep -v '\$PKG' || true)
if [[ -n "$bad_crate_url" ]]; then
    fail "release-dispatch.yml queries crates.io with a literal crate name instead of \$PKG (resolved from Cargo.toml):"
    printf '%s\n' "$bad_crate_url" >&2
fi

# 2. `cargo update -p` must reference $PKG, never a literal package name.
bad_cargo_update=$(grep -nE 'cargo update -p [A-Za-z0-9_"'"'"'-]' "$WORKFLOW" | grep -v '\-p "\$PKG"' || true)
if [[ -n "$bad_cargo_update" ]]; then
    fail "release-dispatch.yml's 'cargo update -p' targets a literal package name instead of \"\$PKG\" (resolved from Cargo.toml):"
    printf '%s\n' "$bad_cargo_update" >&2
fi

# 3. The archive step must package binaries from $BINS, never a literal binary name —
#    otherwise a [[bin]] rename silently ships the wrong (or a stale) binary.
bad_tar=$(grep -nE '^\s*tar -czf "\$A" -C target/release [A-Za-z0-9_-]' "$WORKFLOW" | grep -v '\$BINS' || true)
if [[ -n "$bad_tar" ]]; then
    fail "release-dispatch.yml's release archive step packages a literal binary name instead of \$BINS (resolved from Cargo.toml):"
    printf '%s\n' "$bad_tar" >&2
fi

if [[ "$status" -eq 0 ]]; then
    echo "OK: release-dispatch.yml has no dangling literal crate/binary name references."
fi

exit "$status"

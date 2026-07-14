#!/bin/bash
# Run local security scanners. Fail on HIGH/CRITICAL vulns where tools support it.
#
# Usage: bash scripts/security-scan.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="${HOME}/.cargo/bin:/usr/local/bin:${PATH}"

FAIL=0
note() { printf '\n== %s ==\n' "$1"; }

note "cargo audit (RustSec CVEs)"
if command -v cargo-audit >/dev/null || cargo install cargo-audit --locked >/dev/null 2>&1; then
    cargo audit || FAIL=1
else
    echo "cargo-audit unavailable"; FAIL=1
fi

note "cargo deny (advisories + licenses + sources)"
if command -v cargo-deny >/dev/null; then
    cargo deny check || FAIL=1
else
    echo "cargo-deny unavailable (optional)"
fi

note "gitleaks (secrets)"
if command -v gitleaks >/dev/null; then
    gitleaks detect --source . --no-git -v || FAIL=1
else
    echo "gitleaks unavailable (optional)"
fi

note "trivy fs (vuln + secret + misconfig)"
if command -v trivy >/dev/null; then
    # Exclude dist/ noise; scan source packaging only
    trivy fs --scanners vuln,secret,misconfig \
        --severity HIGH,CRITICAL \
        --skip-dirs dist,target \
        --exit-code 1 \
        . || FAIL=1
else
    echo "trivy unavailable (optional)"
fi

if (( FAIL != 0 )); then
    echo
    echo "security-scan: FAILED"
    exit 1
fi
echo
echo "security-scan: OK"

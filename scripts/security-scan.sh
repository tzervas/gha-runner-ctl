#!/bin/bash
# Run local security scanners. Fail on HIGH/CRITICAL vulns where tools support it.
#
# Usage: bash scripts/security-scan.sh
# Debug: GHA_DEBUG=1  GHA_DEBUG_ON_ERR=1 (default)

SCRIPT_DIR="$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/shell-debug.sh
. "${SCRIPT_DIR}/lib/shell-debug.sh"
gha_shell_debug_init

cd "$SCRIPT_DIR"
cd ..
ROOT="$(pwd)"
export PATH="${HOME}/.cargo/bin:/usr/local/bin:${PATH}"

FAIL=0
note() {
  printf '\n== %s ==\n' "$1"
}

note "cargo audit (RustSec CVEs)"
if command -v cargo-audit >/dev/null
then
  if ! cargo audit
  then
    FAIL=1
  fi
elif cargo install cargo-audit --locked >/dev/null 2>&1
then
  if ! cargo audit
  then
    FAIL=1
  fi
else
  echo "cargo-audit unavailable"
  FAIL=1
fi

note "cargo deny (advisories + licenses + sources)"
if command -v cargo-deny >/dev/null
then
  if ! cargo deny check
  then
    FAIL=1
  fi
else
  echo "cargo-deny unavailable (optional)"
fi

note "gitleaks (secrets)"
if command -v gitleaks >/dev/null
then
  if ! gitleaks detect --source . --no-git -v
  then
    FAIL=1
  fi
else
  echo "gitleaks unavailable (optional)"
fi

note "trivy fs (vuln + secret + misconfig)"
if command -v trivy >/dev/null
then
  if ! trivy fs --scanners vuln,secret,misconfig \
    --severity HIGH,CRITICAL \
    --skip-dirs dist,target \
    --exit-code 1 \
    .
  then
    FAIL=1
  fi
else
  echo "trivy unavailable (optional)"
fi

if (( FAIL != 0 ))
then
  echo
  echo "security-scan: FAILED"
  exit 1
fi
echo
echo "security-scan: OK"

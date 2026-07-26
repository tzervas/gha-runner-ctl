#!/usr/bin/env bash
# Build the current tree and adopt it as the host fleet binary, idempotently.
#
# WHY THIS EXISTS
# The host binary drifted three days behind main, and the drift was invisible
# until it broke something: `gha-fleet-recover.service` calls
# `gha-runner-ctl recover`, a subcommand the installed build predates, so it
# failed 91 times in 24 hours. That service frees orphan pool claims and exited
# workers *so new jobs can be picked up*, so the fleet was quietly losing its
# own recovery path.
#
# Upgrading by hand is what let that happen. This makes it one reviewable,
# idempotent command with a backup and a verification step.
#
#   bash scripts/adopt-local.sh            # show what would change
#   APPLY=1 bash scripts/adopt-local.sh    # build, back up, install, restart
#   APPLY=1 RESTART=0 bash scripts/adopt-local.sh   # install without restarting
#
# Safe to re-run: if the built binary is byte-identical to the installed one it
# reports "already current" and does nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AGENT_USER="${AGENT_USER:-gha-agent}"
AGENT_HOME="${AGENT_HOME:-/home/${AGENT_USER}}"
DEST="${DEST:-${AGENT_HOME}/.local/bin/gha-runner-ctl}"
APPLY="${APPLY:-0}"
RESTART="${RESTART:-1}"
INSTANCES="${INSTANCES:-cpu}"

need() { command -v "$1" >/dev/null 2>&1 || { echo "error: $1 required" >&2; exit 2; }; }
need cargo
need install

cd "$ROOT"

echo "==> building release binary from $(git rev-parse --short HEAD 2>/dev/null || echo '(not a git tree)')"
cargo build --release
src="${ROOT}/target/release/gha-runner-ctl"
[ -x "$src" ] || { echo "error: $src missing after build" >&2; exit 2; }

# Verify before installing: an unusable binary must never reach the fleet path.
echo "==> verifying the built binary"
if ! GHA_ALLOW_ROOT=1 "$src" --help >/dev/null 2>&1; then
  echo "error: built binary does not run --help; refusing to install" >&2
  exit 2
fi
for sub in listen up down status detect warm recover; do
  if GHA_ALLOW_ROOT=1 "$src" --help 2>&1 | grep -qE "^[[:space:]]*${sub}\b"; then
    printf '    %-8s present\n' "$sub"
  else
    # recover is the one that was missing on the stale host build, so an absent
    # subcommand is reported loudly rather than shrugged off.
    echo "    WARNING: subcommand '${sub}' not found in the built binary" >&2
  fi
done

if [ -f "$DEST" ] && cmp -s "$src" "$DEST"; then
  echo "==> already current: $DEST is byte-identical to the build. Nothing to do."
  exit 0
fi

echo "==> installed: $( [ -f "$DEST" ] && date -u -r "$DEST" +%Y-%m-%dT%H:%M:%SZ || echo '(absent)' )"
echo "==> built    : $(date -u -r "$src" +%Y-%m-%dT%H:%M:%SZ)"

if [ "$APPLY" != "1" ]; then
  echo
  echo "dry run. Re-run with APPLY=1 to back up, install, and restart."
  echo "  APPLY=1 bash scripts/adopt-local.sh"
  exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "error: installing to ${DEST} needs root (it is owned by ${AGENT_USER})." >&2
  echo "       re-run with sudo." >&2
  exit 2
fi

if [ -f "$DEST" ]; then
  bk="${DEST}.bak-$(date -u +%Y%m%dT%H%M%SZ)"
  cp -p "$DEST" "$bk"
  echo "==> backup: $bk"
fi

install -o "$AGENT_USER" -g "$AGENT_USER" -m 0755 "$src" "$DEST"
echo "==> installed -> $DEST"

if [ "$RESTART" = "1" ]; then
  uid="$(id -u "$AGENT_USER")"
  for inst in ${INSTANCES//,/ }; do
    unit="gha-runner-ctl@${inst}.service"
    echo "==> restarting ${unit}"
    sudo -u "$AGENT_USER" XDG_RUNTIME_DIR="/run/user/${uid}" \
      systemctl --user restart "$unit" || echo "    warn: could not restart ${unit}" >&2
  done
  sleep 5
  for inst in ${INSTANCES//,/ }; do
    unit="gha-runner-ctl@${inst}.service"
    state="$(sudo -u "$AGENT_USER" XDG_RUNTIME_DIR="/run/user/${uid}" \
      systemctl --user is-active "$unit" 2>&1 || true)"
    printf '    %-32s %s\n' "$unit" "$state"
  done
  # The stale-binary failure mode was a broken timer, so check it explicitly.
  rec="$(sudo -u "$AGENT_USER" XDG_RUNTIME_DIR="/run/user/${uid}" \
    systemctl --user is-active gha-fleet-recover.timer 2>&1 || true)"
  printf '    %-32s %s\n' "gha-fleet-recover.timer" "$rec"
else
  echo "==> RESTART=0: the new binary is installed but the running listener still"
  echo "    has the old one loaded. Restart to activate."
fi

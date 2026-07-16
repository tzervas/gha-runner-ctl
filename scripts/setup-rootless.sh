#!/usr/bin/env bash
# Idempotent rootless Podman + dedicated fleet-agent OS user.
#
# Sequential fresh shells only:
#   open phase → work → phase shell exits (closed) → open next phase
# Never parallel; never leave the previous phase shell open.
# Package/PATH/subuid changes apply without logout/reboot.
#
# Usage (privileged WSL/dev bootstrap shell):
#   sudo bash scripts/setup-rootless.sh
#
# Debug: GHA_DEBUG=1  GHA_DEBUG_ON_ERR=1 (default)

SCRIPT_DIR="$(dirname "$0")"
cd "$SCRIPT_DIR"
SCRIPT_DIR="$(pwd)"
cd ..
REPO_ROOT="$(pwd)"

export GHA_SHELL_DEBUG_LIB="${SCRIPT_DIR}/lib/shell-debug.sh"
# shellcheck source=lib/shell-debug.sh
. "${GHA_SHELL_DEBUG_LIB}"
gha_shell_debug_init
gha_assert_serial_orchestrator

export GHA_AGENT_USER="${GHA_AGENT_USER:-gha-agent}"
export GHA_AGENT_HOME="${GHA_AGENT_HOME:-/home/${GHA_AGENT_USER}}"
export GHA_SUBUID_START="${GHA_SUBUID_START:-100000}"
export GHA_SUBUID_COUNT="${GHA_SUBUID_COUNT:-65536}"

if [[ "$(id -u)" -ne 0 ]]
then
  echo "setup-rootless: must run as root once to create user / subuid (WSL bootstrap)." >&2
  exit 1
fi

echo "setup-rootless: serial orchestrator (one phase shell at a time)"

# ── 01 packages (fresh shell after apt so new binaries are visible) ──────────
gha_run_phase "01-packages" -c "
export DEBIAN_FRONTEND=noninteractive
echo 'setup-rootless: packages (best-effort)…'
if ! apt-get update -qq 2>/dev/null
then
  echo 'setup-rootless: apt-get update failed (continuing)'
fi
if ! apt-get install -y -qq \
  podman uidmap slirp4netns fuse-overlayfs crun \
  dbus-user-session passt aardvark-dns netavark \
  2>/dev/null
then
  echo 'setup-rootless: apt incomplete — checking binaries…'
  for b in podman newuidmap newgidmap fuse-overlayfs crun slirp4netns
  do
    if ! command -v \"\$b\" >/dev/null
    then
      echo \"missing \$b\" >&2
      exit 1
    fi
  done
fi
chmod u+s /usr/bin/newuidmap /usr/bin/newgidmap 2>/dev/null
true
mount --make-rshared / 2>/dev/null
true
command -v podman
command -v newuidmap
echo 'setup-rootless: packages phase OK'
"

# ── 02 agent OS user + subuid (then next phase reloads identity maps) ────────
gha_run_phase "02-agent-user" -c "
AGENT_USER=\"${GHA_AGENT_USER}\"
AGENT_HOME=\"${GHA_AGENT_HOME}\"
SUBUID_START=\"${GHA_SUBUID_START}\"
SUBUID_COUNT=\"${GHA_SUBUID_COUNT}\"

if ! id \"\$AGENT_USER\" &>/dev/null
then
  useradd --system --create-home --home-dir \"\$AGENT_HOME\" \
    --shell /usr/sbin/nologin \
    --comment 'gha-runner-ctl fleet agent (rootless)' \
    \"\$AGENT_USER\"
  echo \"setup-rootless: created user \$AGENT_USER\"
else
  echo \"setup-rootless: user \$AGENT_USER already exists\"
fi

rm -f \"/etc/sudoers.d/\${AGENT_USER}\" 2>/dev/null
true
if grep -R \"^\\s*\${AGENT_USER}\\s\" /etc/sudoers /etc/sudoers.d 2>/dev/null
then
  echo \"setup-rootless: ERROR: \${AGENT_USER} appears in sudoers — remove it.\" >&2
  exit 1
fi
echo \"setup-rootless: confirmed \${AGENT_USER} is not a sudoer\"

if ! grep -q \"^\${AGENT_USER}:\" /etc/subuid 2>/dev/null
then
  echo \"\${AGENT_USER}:\${SUBUID_START}:\${SUBUID_COUNT}\" >> /etc/subuid
fi
if ! grep -q \"^\${AGENT_USER}:\" /etc/subgid 2>/dev/null
then
  echo \"\${AGENT_USER}:\${SUBUID_START}:\${SUBUID_COUNT}\" >> /etc/subgid
fi

AGENT_UID=\"\$(id -u \"\$AGENT_USER\")\"
AGENT_GID=\"\$(id -g \"\$AGENT_USER\")\"
mkdir -p \"\$AGENT_HOME\" \"/run/user/\${AGENT_UID}\"
chown -R \"\${AGENT_UID}:\${AGENT_GID}\" \"\$AGENT_HOME\" \"/run/user/\${AGENT_UID}\"
chmod 700 \"\$AGENT_HOME\" \"/run/user/\${AGENT_UID}\"

install -d -o \"\$AGENT_UID\" -g \"\$AGENT_GID\" -m 700 \
  \"\$AGENT_HOME/.config/containers\" \
  \"\$AGENT_HOME/.local/share/containers\" \
  \"\$AGENT_HOME/.local/share/gha-runner-ctl/instances\" \
  \"\$AGENT_HOME/.local/share/gha-runner-ctl/logs\" \
  \"\$AGENT_HOME/.local/bin\"

loginctl enable-linger \"\$AGENT_USER\" 2>/dev/null
true
echo \"setup-rootless: agent-user phase OK (uid=\$AGENT_UID)\"
"

# ── 03 rootless config as gha-agent (fresh user shell, new XDG_RUNTIME_DIR) ──
gha_run_phase "03-rootless-config" --user "${GHA_AGENT_USER}" -c '
set -euo pipefail
export XDG_CONFIG_HOME="${HOME}/.config"
export XDG_DATA_HOME="${HOME}/.local/share"
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME/containers" "$XDG_DATA_HOME/containers"
cd "$HOME"
cat > "$XDG_CONFIG_HOME/containers/storage.conf" <<EOF
[storage]
driver = "overlay"
runroot = "${XDG_RUNTIME_DIR}/containers"
graphroot = "${HOME}/.local/share/containers/storage"
[storage.options]
mount_program = "/usr/bin/fuse-overlayfs"
EOF
cat > "$XDG_CONFIG_HOME/containers/containers.conf" <<EOF
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
[containers]
default_sysctls = []
EOF
chmod 600 "$XDG_CONFIG_HOME/containers/"*.conf
echo "setup-rootless: wrote rootless containers.conf as $(id -un)"
'

# ── 04 install binary into agent home (fresh root shell) ─────────────────────
gha_run_phase "04-install-binary" -c "
AGENT_USER=\"${GHA_AGENT_USER}\"
AGENT_HOME=\"${GHA_AGENT_HOME}\"
AGENT_UID=\"\$(id -u \"\$AGENT_USER\")\"
AGENT_GID=\"\$(id -g \"\$AGENT_USER\")\"
REPO_BIN=\"${REPO_ROOT}/target/release/gha-runner-ctl\"
ROOT_BIN=\"/root/.local/bin/gha-runner-ctl\"
if [[ -x \"\$REPO_BIN\" ]]
then
  install -o \"\$AGENT_UID\" -g \"\$AGENT_GID\" -m 0755 \
    \"\$REPO_BIN\" \
    \"\$AGENT_HOME/.local/bin/gha-runner-ctl\"
  echo \"setup-rootless: installed \$REPO_BIN\"
elif [[ -x \"\$ROOT_BIN\" ]]
then
  install -o \"\$AGENT_UID\" -g \"\$AGENT_GID\" -m 0755 \
    \"\$ROOT_BIN\" \
    \"\$AGENT_HOME/.local/bin/gha-runner-ctl\"
  echo \"setup-rootless: installed \$ROOT_BIN\"
else
  echo \"setup-rootless: no binary yet (build/release later)\"
fi
"

# ── 05 verify in a clean gha-agent shell (post-config reload) ────────────────
VERIFY_SCRIPT="${SCRIPT_DIR}/verify-rootless.sh"
if [[ -r "$VERIFY_SCRIPT" ]]
then
  # Copy into agent-readable path if repo is under /root.
  AGENT_VERIFY="${GHA_AGENT_HOME}/verify-rootless.sh"
  install -o "$(id -u "${GHA_AGENT_USER}")" -g "$(id -g "${GHA_AGENT_USER}")" -m 0755 \
    "$VERIFY_SCRIPT" \
    "$AGENT_VERIFY"
  # Also ship shell-debug next to it for sourcing by relative path fallback.
  mkdir -p "${GHA_AGENT_HOME}/lib"
  install -o "$(id -u "${GHA_AGENT_USER}")" -g "$(id -g "${GHA_AGENT_USER}")" -m 0644 \
    "${SCRIPT_DIR}/lib/shell-debug.sh" \
    "${GHA_AGENT_HOME}/lib/shell-debug.sh"

  gha_run_phase "05-verify-rootless" --user "${GHA_AGENT_USER}" -c "
export GHA_SHELL_DEBUG_LIB=\"\${HOME}/lib/shell-debug.sh\"
export XDG_RUNTIME_DIR=\"/run/user/\$(id -u)\"
bash \"\${HOME}/verify-rootless.sh\"
"
else
  echo "setup-rootless: verify script missing — skip phase 05"
fi

AGENT_UID="$(id -u "${GHA_AGENT_USER}")"
echo
echo "setup-rootless: OK"
echo "  user:     ${GHA_AGENT_USER} (uid=${AGENT_UID})  shell=nologin  sudo=no"
echo "  home:     ${GHA_AGENT_HOME}"
echo "  subuid:   $(grep "^${GHA_AGENT_USER}:" /etc/subuid)"
echo
echo "  Phases: sequential open→work→close→next (no leftover phase shells)."
echo "  Run agent:"
echo "    sudo -u ${GHA_AGENT_USER} -H env XDG_RUNTIME_DIR=/run/user/${AGENT_UID} \\"
echo "      ${GHA_AGENT_HOME}/.local/bin/gha-runner-ctl --help"
echo
echo "  Ephemeral WSL/dev as root: GHA_ALLOW_ROOT=1 only for bootstrap."
echo "  Debug: GHA_DEBUG=1  GHA_DEBUG_ON_ERR=0  GHA_PHASE_MAX=16  GHA_PHASE_LOGIN=1"

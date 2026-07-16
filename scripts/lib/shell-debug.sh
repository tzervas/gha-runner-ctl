#!/usr/bin/env bash
# Shared helpers for gha-runner-ctl host scripts.
#
# Conventions:
#   - No command-chaining with && — set -e, newlines, \, explicit if/return
#   - Work runs as a sequence of fresh shells: start phase N, wait until it
#     fully exits (shell/TTY closed), only then start phase N+1
#   - Never background phases; never fan-out thousands of shells
#   - Cap nesting with GHA_PHASE_MAX (default 16)
#
# Debug on error (until the stack is stable):
#   GHA_DEBUG=1          — trace every command (set -x)
#   GHA_DEBUG_ON_ERR=1   — dump context when a command fails (default on)
#   GHA_DEBUG_ON_ERR=0   — silence ERR trap
#
# Phase shells:
#   GHA_PHASE_LOGIN=1    — bash -l (default): re-read profile after installs
#   GHA_PHASE_LOGIN=0    — bash --noprofile --norc (still a new process)
#   GHA_PHASE_MAX=16     — max nested gha_run_phase depth
#
# shellcheck shell=bash

gha_shell_debug_init() {
  set -euo pipefail

  case "${GHA_DEBUG:-0}" in
    1|true|yes|YES)
      export PS4='+ ${BASH_SOURCE[0]##*/}:${LINENO}:${FUNCNAME[0]:-main}: '
      set -x
      ;;
  esac

  case "${GHA_DEBUG_ON_ERR:-1}" in
    0|false|no|NO)
      return 0
      ;;
  esac

  if [[ -n "${_GHA_ERR_TRAP_INSTALLED:-}" ]]
  then
    return 0
  fi
  _GHA_ERR_TRAP_INSTALLED=1
  trap 'gha_on_err $?' ERR
}

gha_on_err() {
  local ec="${1:-$?}"
  if ! set +x 2>/dev/null
  then
    :
  fi
  {
    echo
    echo "========== gha-runner-ctl DEBUG ON ERROR =========="
    echo "exit_code:  $ec"
    echo "phase:      ${GHA_PHASE_NAME:-"(top-level)"} depth=${GHA_PHASE_DEPTH:-0}"
    if NOW="$(date -Is 2>/dev/null)"
    then
      echo "time:       $NOW"
    else
      echo "time:       $(date)"
    fi
    if UNAME="$(id -un 2>/dev/null)"
    then
      echo "user:       $UNAME uid=$(id -u 2>/dev/null)"
    else
      echo "user:       ?"
    fi
    echo "pwd:        $PWD"
    echo "script:     ${BASH_SOURCE[1]:-${0:-?}}"
    echo "line:       ${BASH_LINENO[0]:-?}"
    echo "function:   ${FUNCNAME[1]:-main}"
    echo "command:    ${BASH_COMMAND:-?}"
    echo "--- env (selected) ---"
    echo "HOME=${HOME:-}"
    echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-}"
    echo "CONTAINER_HOST=${CONTAINER_HOST:-}"
    echo "GHA_ALLOW_ROOT=${GHA_ALLOW_ROOT:-}"
    echo "GHA_PREFER_REPOS=${GHA_PREFER_REPOS:-}"
    echo "--- podman (best-effort) ---"
    if command -v podman >/dev/null 2>&1
    then
      if ! podman info --format 'rootless={{.Host.Security.Rootless}} runtime={{.Host.OCIRuntime.Name}}' 2>&1 \
        | head -5
      then
        echo "podman info failed"
      fi
      if ! podman ps -a --format 'table {{.Names}}\t{{.Status}}\t{{.Image}}' 2>&1 \
        | head -20
      then
        echo "podman ps failed"
      fi
    else
      echo "podman not in PATH"
    fi
    echo "==================================================="
  } >&2
}

# Reap any leftover jobs in *this* shell before the process exits.
# Ensures a phase does not leave orphans when the shell/TTY closes.
gha_phase_close() {
  local ec="${1:-0}"
  # Stop tracing for clean close log.
  if ! set +x 2>/dev/null
  then
    :
  fi
  # Best-effort: kill background jobs started in this phase shell only.
  local j
  j="$(jobs -p 2>/dev/null || true)"
  if [[ -n "${j}" ]]
  then
    echo "── phase close: reaping leftover jobs: ${j} ──" >&2
    # shellcheck disable=SC2086
    kill ${j} 2>/dev/null \
      || true
    wait 2>/dev/null \
      || true
  fi
  echo "── phase closed: ${GHA_PHASE_NAME:-?} exit=${ec} (shell exiting) ──"
  return "${ec}"
}

# Run one named phase in a *new* bash process, wait for it to fully exit, then
# return. The orchestrator must call this sequentially — never with &.
#
# Flow:
#   parent ──spawn──▶ phase shell (fresh login/env)
#   parent ◀─wait── phase shell exits (TTY/process closed)
#   parent ──spawn──▶ next phase shell …
#
# Usage:
#   gha_run_phase "01-packages" -c 'apt-get install …'
#   gha_run_phase "02-verify"   /path/to/script.sh
#   gha_run_phase "03-as-agent" --user gha-agent -c 'podman info'
gha_run_phase() {
  local name="${1:-}"
  if [[ -z "$name" ]]
  then
    echo "gha_run_phase: name required" >&2
    return 2
  fi
  shift

  # Refuse accidental parallel use: if caller backgrounds us, still only one
  # child — but document that & is unsupported.
  if [[ -n "${GHA_PHASE_BUSY:-}" ]]
  then
    echo "gha_run_phase: another phase is still open (${GHA_PHASE_BUSY}); sequential only" >&2
    return 1
  fi

  local depth="${GHA_PHASE_DEPTH:-0}"
  local max="${GHA_PHASE_MAX:-16}"
  if ! [[ "$depth" =~ ^[0-9]+$ ]]
  then
    depth=0
  fi
  if ! [[ "$max" =~ ^[0-9]+$ ]]
  then
    max=16
  fi
  if (( depth >= max ))
  then
    echo "gha_run_phase: refusing nest depth ${depth} (GHA_PHASE_MAX=${max})" >&2
    return 1
  fi

  local run_user=""
  if [[ "${1:-}" == "--user" ]]
  then
    run_user="${2:-}"
    shift 2
    if [[ -z "$run_user" ]]
    then
      echo "gha_run_phase: --user needs a name" >&2
      return 2
    fi
  fi

  local next_depth=$((depth + 1))
  echo "── phase open [${depth}→${next_depth}]: ${name}${run_user:+ (user=${run_user})} ──"

  local -a env_pass=(
    "GHA_PHASE_DEPTH=${next_depth}"
    "GHA_PHASE_NAME=${name}"
    "GHA_PHASE_MAX=${max}"
    "GHA_DEBUG=${GHA_DEBUG:-0}"
    "GHA_DEBUG_ON_ERR=${GHA_DEBUG_ON_ERR:-1}"
    "GHA_PHASE_LOGIN=${GHA_PHASE_LOGIN:-1}"
    "GHA_ALLOW_ROOT=${GHA_ALLOW_ROOT:-}"
    "GHA_AGENT_USER=${GHA_AGENT_USER:-}"
    "GHA_AGENT_HOME=${GHA_AGENT_HOME:-}"
    "GHA_SUBUID_START=${GHA_SUBUID_START:-}"
    "GHA_SUBUID_COUNT=${GHA_SUBUID_COUNT:-}"
    "GHA_AGENT_IMAGE=${GHA_AGENT_IMAGE:-}"
    "DEBIAN_FRONTEND=${DEBIAN_FRONTEND:-noninteractive}"
    "PATH=${PATH:-/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin}"
    "HOME=${HOME:-/root}"
    "USER=${USER:-root}"
    "TERM=${TERM:-dumb}"
    "LANG=${LANG:-C.UTF-8}"
  )
  if [[ -n "${XDG_RUNTIME_DIR:-}" ]]
  then
    env_pass+=("XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR}")
  fi
  if [[ -n "${SSH_AUTH_SOCK:-}" ]]
  then
    env_pass+=("SSH_AUTH_SOCK=${SSH_AUTH_SOCK}")
  fi
  if [[ -n "${GHA_SHELL_DEBUG_LIB:-}" ]]
  then
    env_pass+=("GHA_SHELL_DEBUG_LIB=${GHA_SHELL_DEBUG_LIB}")
  fi

  local -a bash_inv
  case "${GHA_PHASE_LOGIN:-1}" in
    0|false|no|NO)
      bash_inv=(bash --noprofile --norc)
      ;;
    *)
      bash_inv=(bash -l)
      ;;
  esac

  # Body always ends by closing the phase shell cleanly (reap jobs, log, exit).
  local body_prefix
  body_prefix='
set -euo pipefail
if [[ -n "${GHA_SHELL_DEBUG_LIB:-}" ]]
then
  # shellcheck disable=SC1090
  . "${GHA_SHELL_DEBUG_LIB}"
  gha_shell_debug_init
fi
# On any exit path, reap jobs and announce shell close before process dies.
trap '\''ec=$?; gha_phase_close "$ec"; exit "$ec"'\'' EXIT
'

  local phase_inner
  local -a cmd
  if [[ "${1:-}" == "-c" ]]
  then
    shift
    phase_inner="${body_prefix}
${1}
"
    cmd=(
      "${bash_inv[@]}"
      -c
      "${phase_inner}"
    )
  else
    # Script path form: wrap so EXIT trap still runs.
    local script_q
    script_q="$(printf '%q ' "$@")"
    phase_inner="${body_prefix}
${script_q}
"
    cmd=(
      "${bash_inv[@]}"
      -c
      "${phase_inner}"
    )
  fi

  # Mark busy so nested accidental re-entry from the *same* parent is visible.
  # Child has its own process; parent clears busy only after wait completes.
  GHA_PHASE_BUSY="${name}"
  export GHA_PHASE_BUSY

  local ec=0
  if [[ -n "$run_user" ]]
  then
    local uhome uuid
    uhome="$(getent passwd "$run_user" | cut -d: -f6)"
    uuid="$(id -u "$run_user")"
    env_pass+=("HOME=${uhome}")
    env_pass+=("USER=${run_user}")
    env_pass+=("LOGNAME=${run_user}")
    env_pass+=("XDG_RUNTIME_DIR=/run/user/${uuid}")

    # Write body to a temp script so su does not mangle quoting; remove after wait.
    local tmp tmp_q
    tmp="$(mktemp /tmp/gha-phase.XXXXXX)"
    printf '%s\n' "${phase_inner}" >"${tmp}"
    chmod 0700 "${tmp}"
    chown "${run_user}:" "${tmp}" 2>/dev/null \
      || true
    tmp_q="$(printf '%q' "$tmp")"

    local -a run_bash
    case "${GHA_PHASE_LOGIN:-1}" in
      0|false|no|NO)
        run_bash=(bash --noprofile --norc)
        ;;
      *)
        run_bash=(bash -l)
        ;;
    esac

    if ! env "${env_pass[@]}" \
      su -s /bin/bash "$run_user" -c "exec ${run_bash[*]} ${tmp_q}"
    then
      ec=$?
    fi
    rm -f "${tmp}" 2>/dev/null \
      || true
  else
    # Synchronous: parent blocks until child process tree exits (shell closed).
    if ! env -i "${env_pass[@]}" "${cmd[@]}"
    then
      ec=$?
    fi
  fi

  unset GHA_PHASE_BUSY
  echo "── phase reaped: ${name} (next shell may open) exit=${ec} ──"
  return "${ec}"
}

# Run many phases strictly one-after-another. Stops on first failure.
# Args: pairs are not used — call gha_run_phase in order from the orchestrator.
# This helper only documents the contract for top-level scripts.
gha_assert_serial_orchestrator() {
  if [[ -n "${GHA_PHASE_NAME:-}" ]]
  then
    echo "gha_assert_serial_orchestrator: refuse — already inside phase ${GHA_PHASE_NAME}" >&2
    return 1
  fi
  return 0
}

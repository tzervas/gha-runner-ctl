#!/bin/bash
# Single-runner entrypoint: seed snapshot if needed, register, run.
# Tokens are only accepted from env (injected by gha-runner-ctl); never logged.
set -euo pipefail

SEED="${RUNNER_SEED:-/opt/actions-runner-seed}"
HOME_DIR="${RUNNER_HOME:-/opt/actions-runner}"

log() { printf 'runner: %s\n' "$*" >&2; }

if [[ ! -x "${HOME_DIR}/run.sh" ]]; then
    if [[ -x "${SEED}/run.sh" ]]; then
        log "seeding runner binaries from image snapshot"
        cp -a "${SEED}/." "${HOME_DIR}/"
    else
        log "ERROR: runner binaries missing"
        exit 1
    fi
fi

cd "$HOME_DIR"

REPO_URL="${REPO_URL:-}"
RUNNER_TOKEN="${RUNNER_TOKEN:-}"
RUNNER_NAME="${RUNNER_NAME:-tg-agent-relay-podman}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,linux,x64,podman}"
RUNNER_GROUP="${RUNNER_GROUP:-Default}"
WORK_DIR="${RUNNER_WORK_DIR:-_work}"
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-true}"
RUNNER_RETAIN="${RUNNER_RETAIN:-false}"

if [[ -z "$REPO_URL" ]]; then
    log "ERROR: REPO_URL required"
    exit 1
fi

need_register=0
if [[ "$RUNNER_EPHEMERAL" == "true" || "$RUNNER_EPHEMERAL" == "1" ]]; then
    need_register=1
    # Ephemeral: never reuse stale registration files
    rm -f .runner .credentials .credentials_rsaparams 2>/dev/null || true
elif [[ ! -f .runner ]]; then
    need_register=1
elif [[ "$RUNNER_RETAIN" != "true" && "$RUNNER_RETAIN" != "1" ]]; then
    need_register=1
fi

if (( need_register == 1 )); then
    if [[ -z "$RUNNER_TOKEN" ]]; then
        log "ERROR: RUNNER_TOKEN required to register (controller injects a short-lived token)"
        exit 1
    fi
    config_args=(
        --unattended
        --url "$REPO_URL"
        --token "$RUNNER_TOKEN"
        --name "$RUNNER_NAME"
        --labels "$RUNNER_LABELS"
        --work "$WORK_DIR"
        --runnergroup "$RUNNER_GROUP"
        --replace
    )
    if [[ "$RUNNER_EPHEMERAL" == "true" || "$RUNNER_EPHEMERAL" == "1" ]]; then
        config_args+=(--ephemeral)
        log "registering ephemeral runner name=${RUNNER_NAME}"
    else
        log "registering retained runner name=${RUNNER_NAME}"
    fi
    ./config.sh "${config_args[@]}"
    # Drop token from environment for child processes
    unset RUNNER_TOKEN
else
    log "using retained registration on snapshot volume"
    unset RUNNER_TOKEN
fi

exec ./run.sh

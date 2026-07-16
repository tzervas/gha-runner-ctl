#!/bin/bash
# Single-runner entrypoint: seed snapshot, register, run.
# Tokens only from env (controller); never logged. Fail closed on bad identity.
set -euo pipefail

SEED="${RUNNER_SEED:-/opt/actions-runner-seed}"
HOME_DIR="${RUNNER_HOME:-/opt/actions-runner}"

log() { printf 'runner: %s\n' "$*" >&2; }

# Reject shell metacharacters / traversal in controller-supplied identity fields.
safe_ident() {
    local s="${1:-}"
    if [[ -z "$s" ]]
    then
        return 1
    fi
    if [[ ${#s} -gt 128 ]]
    then
        return 1
    fi
    case "$s" in
        *..* | *[!A-Za-z0-9._-]* ) return 1 ;;
    esac
    return 0
}

safe_url() {
    # Only github.com over HTTPS (repo or org path).
    local u="${1:-}"
    case "$u" in
        https://github.com/*)
            local path="${u#https://github.com/}"
            if [[ -z "$path" ]]
            then
                return 1
            fi
            case "$path" in
                *..* | *[!A-Za-z0-9._/-]* | /* | */) return 1 ;;
            esac
            return 0
            ;;
        *) return 1 ;;
    esac
}

if [[ ! -x "${HOME_DIR}/run.sh" ]]
then
    if [[ -x "${SEED}/run.sh" ]]
    then
        log "seeding runner binaries from image snapshot"
        cp -a "${SEED}/." "${HOME_DIR}/"
        if ! chmod -R go-w "${HOME_DIR}" 2>/dev/null
        then
            :
        fi
    else
        log "ERROR: runner binaries missing"
        exit 1
    fi
fi

cd "$HOME_DIR"

REPO_URL="${REPO_URL:-}"
RUNNER_TOKEN="${RUNNER_TOKEN:-}"
RUNNER_NAME="${RUNNER_NAME:-shared-podman-1}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,linux,x64,podman}"
RUNNER_GROUP="${RUNNER_GROUP:-Default}"
WORK_DIR="${RUNNER_WORK_DIR:-_work}"
RUNNER_EPHEMERAL="${RUNNER_EPHEMERAL:-true}"
RUNNER_RETAIN="${RUNNER_RETAIN:-false}"

if ! safe_url "$REPO_URL"; then
    log "ERROR: REPO_URL must be https://github.com/<owner>[/<repo>] with safe charset"
    exit 1
fi
if ! safe_ident "$RUNNER_NAME"; then
    log "ERROR: invalid RUNNER_NAME"
    exit 1
fi
# Labels: comma-separated safe idents
IFS=',' read -r -a _labs <<<"$RUNNER_LABELS"
for lab in "${_labs[@]}"
do
    lab="${lab// /}"
    if [[ -z "$lab" ]]
    then
        continue
    fi
    if ! safe_ident "$lab"
    then
        log "ERROR: invalid label"
        exit 1
    fi
done
if ! safe_ident "$RUNNER_GROUP"; then
    log "ERROR: invalid RUNNER_GROUP"
    exit 1
fi
if ! safe_ident "$WORK_DIR"; then
    # work dir is relative under runner home
    log "ERROR: invalid WORK_DIR"
    exit 1
fi

need_register=0
# REUSE: controller says volume already has a retain registration for this REPO_URL.
if [[ "${RUNNER_TOKEN:-}" == "REUSE" ]]
then
    if [[ -f .runner ]]
    then
        log "reusing retained registration (.runner present) — no config.sh"
        need_register=0
        unset RUNNER_TOKEN
    else
        log "REUSE requested but .runner missing — will require a real token"
        need_register=1
    fi
elif [[ "${RUNNER_TOKEN:-}" == "reuse" ]]
then
    if [[ -f .runner ]]
    then
        log "reusing retained registration (.runner present) — no config.sh"
        need_register=0
        unset RUNNER_TOKEN
    else
        log "REUSE requested but .runner missing — will require a real token"
        need_register=1
    fi
elif [[ "$RUNNER_EPHEMERAL" == "true" ]]
then
    need_register=1
    rm -f .runner .credentials .credentials_rsaparams 2>/dev/null \
      || true
elif [[ "$RUNNER_EPHEMERAL" == "1" ]]
then
    need_register=1
    rm -f .runner .credentials .credentials_rsaparams 2>/dev/null \
      || true
elif [[ ! -f .runner ]]
then
    need_register=1
elif [[ "$RUNNER_RETAIN" != "true" ]]
then
    if [[ "$RUNNER_RETAIN" != "1" ]]
    then
        need_register=1
    fi
fi

if (( need_register == 1 ))
then
    if [[ -z "${RUNNER_TOKEN:-}" ]]
    then
        log "ERROR: RUNNER_TOKEN required to register"
        exit 1
    fi
    if [[ "$RUNNER_TOKEN" == "REUSE" ]]
    then
        log "ERROR: RUNNER_TOKEN required to register"
        exit 1
    fi
    if [[ "$RUNNER_TOKEN" == "reuse" ]]
    then
        log "ERROR: RUNNER_TOKEN required to register"
        exit 1
    fi
    # Never print token or length that could leak into shared logs.
    log "registering runner name=${RUNNER_NAME} ephemeral=${RUNNER_EPHEMERAL}"
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
    if [[ "$RUNNER_EPHEMERAL" == "true" ]]
    then
        config_args+=(--ephemeral)
    elif [[ "$RUNNER_EPHEMERAL" == "1" ]]
    then
        config_args+=(--ephemeral)
    fi
    ./config.sh "${config_args[@]}"
    # Best-effort wipe of token from this shell
    if RUNNER_TOKEN="$(head -c 64 /dev/urandom | base64 2>/dev/null)"
    then
        :
    else
        RUNNER_TOKEN=""
    fi
    unset RUNNER_TOKEN
else
    log "using retained registration on snapshot volume"
    unset RUNNER_TOKEN
fi

exec ./run.sh

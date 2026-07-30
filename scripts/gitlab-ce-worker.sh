#!/usr/bin/env bash
# gitlab-ce-worker.sh — mint glrt, run one capped gitlab-runner container, always DELETE.
#
# Secure integration MVP (not full listen loop):
#   - PAT only via secret exec (gitlab/api-token)
#   - glrt never printed; shredded after inject
#   - no docker.sock; shell executor inside worker
#   - glrt register: only --url/--token/--executor/--description (tags set at mint)
#   - resource caps; run_untagged=false tags
#   - DELETE runner on every exit path
#
# Usage:
#   ./scripts/gitlab-ce-worker.sh                 # full cycle on HOMELAB via SSH if needed
#   ./scripts/gitlab-ce-worker.sh --online-only   # register, wait until online or timeout, delete
#   GHA_GITLAB_URL=https://git.vectorweight.com ./scripts/gitlab-ce-worker.sh
#
# Remote worker (recommended — forge on LAN):
#   HOMELAB_SSH=kang@HOST HOMELAB_SSH_KEY=/path/to/key ./scripts/gitlab-ce-worker.sh
set -euo pipefail

GHA_GITLAB_URL="${GHA_GITLAB_URL:-https://git.vectorweight.com}"
SECRET_KEY="${GITLAB_SECRET_KEY:-gitlab/api-token}"
RUNNER_IMAGE="${GITLAB_RUNNER_IMAGE:-docker.io/gitlab/gitlab-runner:alpine-v17.5.3}"
ONLINE_TIMEOUT="${GITLAB_ONLINE_TIMEOUT:-90}"
MODE="online-only"
MEMORY="${GITLAB_WORKER_MEMORY:-512m}"
CPUS="${GITLAB_WORKER_CPUS:-1}"
TAGS="${GITLAB_RUNNER_TAGS:-self-hosted,linux,x64,podman,tier-micro,gha-runner-ctl}"
DESC_PREFIX="${GITLAB_RUNNER_DESC:-gha-runner-ctl-worker}"

for a in "$@"; do
  case "$a" in
    --online-only) MODE=online-only ;;
    --help|-h)
      sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

die() { echo "gitlab-ce-worker: $*" >&2; exit 1; }
log() { echo "gitlab-ce-worker: $*" >&2; }

command -v secret >/dev/null || die "secret CLI not found"
command -v curl >/dev/null || die "curl not found"
command -v python3 >/dev/null || die "python3 not found"
secret ls 2>/dev/null | grep -qx "$SECRET_KEY" || die "missing vault key $SECRET_KEY"

HOST_HDR="$(python3 -c "from urllib.parse import urlparse; print(urlparse('''${GHA_GITLAB_URL}''').hostname or '')")"
[ -n "$HOST_HDR" ] || die "bad GHA_GITLAB_URL"

WORKDIR="${TMPDIR:-/tmp}/gl-worker-$$"
mkdir -p "$WORKDIR"
chmod 700 "$WORKDIR"
RUNNER_ID=""
API_BASE=""
TUNNEL_PID=""
REMOTE_CONFIG=""

cleanup() {
  set +e
  if [ -n "${RUNNER_ID:-}" ] && [ -n "${API_BASE:-}" ]; then
    log "DELETE /runners/${RUNNER_ID}"
    secret exec GITLAB_TOKEN="$SECRET_KEY" -- bash -c \
      'curl -sk -o /dev/null -w "delete_http=%{http_code}\n" --max-time 20 \
        -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" \
        -H "Host: '"${HOST_HDR}"'" \
        -X DELETE "'"${API_BASE}"'/runners/'"${RUNNER_ID}"'"' 2>/dev/null || true
  fi
  if [ -n "${HOMELAB_SSH:-}" ] && [ -n "${REMOTE_CONFIG:-}" ]; then
    local -a ssh_opts=(-o BatchMode=yes -o IdentitiesOnly=yes)
    [ -n "${HOMELAB_SSH_KEY:-}" ] && ssh_opts+=(-i "$HOMELAB_SSH_KEY")
    ssh "${ssh_opts[@]}" "$HOMELAB_SSH" \
      "sudo -n podman rm -f gl-ctl-worker 2>/dev/null; sudo -n rm -rf ${REMOTE_CONFIG}" 2>/dev/null || true
  fi
  if [ -n "${TUNNEL_PID:-}" ] && kill -0 "$TUNNEL_PID" 2>/dev/null; then
    kill "$TUNNEL_PID" 2>/dev/null || true
  fi
  if [ -d "$WORKDIR" ]; then
    find "$WORKDIR" -type f -exec shred -u {} \; 2>/dev/null || rm -rf "$WORKDIR"
    rm -rf "$WORKDIR"
  fi
}
trap cleanup EXIT

resolve_api_base() {
  local code
  code="$(curl -sk -o /dev/null -w "%{http_code}" --max-time 8 \
    -H "Host: ${HOST_HDR}" "${GHA_GITLAB_URL}/users/sign_in" 2>/dev/null || echo 000)"
  if [ "$code" = "200" ] || [ "$code" = "302" ]; then
    API_BASE="${GHA_GITLAB_URL%/}/api/v4"
    log "API direct (sign_in=${code})"
    return 0
  fi
  [ -n "${HOMELAB_SSH:-}" ] || die "cannot reach forge (sign_in=${code}); set HOMELAB_SSH"
  local -a ssh_opts=(-o BatchMode=yes -o ExitOnForwardFailure=yes -o IdentitiesOnly=yes)
  [ -n "${HOMELAB_SSH_KEY:-}" ] && ssh_opts+=(-i "$HOMELAB_SSH_KEY")
  local port="${GITLAB_SPIKE_LOCAL_PORT:-18443}"
  fuser -k "${port}/tcp" 2>/dev/null || true
  sleep 0.2
  ssh "${ssh_opts[@]}" -f -N -L "127.0.0.1:${port}:127.0.0.1:443" "$HOMELAB_SSH" \
    || die "ssh tunnel failed"
  TUNNEL_PID="$(ss -ltnp 2>/dev/null | awk -v p=":${port}" '
    $4 ~ p { if (match($0, /pid=([0-9]+)/, a)) { print a[1]; exit } }')"
  API_BASE="https://127.0.0.1:${port}/api/v4"
  code="$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    -H "Host: ${HOST_HDR}" "https://127.0.0.1:${port}/users/sign_in" || echo 000)"
  [ "$code" = "200" ] || [ "$code" = "302" ] || die "tunnel sign_in=${code}"
  log "API via tunnel :${port} (sign_in=${code})"
}

mint_runner() {
  secret exec GITLAB_TOKEN="$SECRET_KEY" -- env \
    WORKDIR="$WORKDIR" API_BASE="$API_BASE" HOST_HDR="$HOST_HDR" \
    SPIKE_DESC="${DESC_PREFIX}-$(hostname -s 2>/dev/null || echo host)-$$" \
    TAGS="$TAGS" \
    python3 - <<'PY'
import json, os, pathlib, urllib.parse
from urllib.request import Request, urlopen
import ssl

api = os.environ["API_BASE"]
host = os.environ["HOST_HDR"]
token = os.environ["GITLAB_TOKEN"]
wd = pathlib.Path(os.environ["WORKDIR"])
desc = os.environ["SPIKE_DESC"]
tags = [t.strip() for t in os.environ["TAGS"].split(",") if t.strip()]
ctx = ssl._create_unverified_context()

def api_call(method, path, body=None):
    data = None
    headers = {
        "PRIVATE-TOKEN": token,
        "Host": host,
        "User-Agent": "gha-runner-ctl-gitlab-worker/0.1",
        "Accept": "application/json",
    }
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = Request(f"{api}{path}", data=data, headers=headers, method=method)
    with urlopen(req, context=ctx, timeout=30) as r:
        raw = r.read()
        code = r.status
    return code, json.loads(raw.decode() or "{}") if raw else {}

code, body = api_call("POST", "/user/runners", {
    "runner_type": "instance_type",
    "description": desc,
    "tag_list": tags,
    "run_untagged": False,
    "locked": False,
    "maximum_timeout": 3600,
    "access_level": "not_protected",
})
if code not in (200, 201):
    raise SystemExit(f"mint failed http={code} body_keys={list(body)}")
rid = body.get("id")
glrt = body.get("token")
if not rid or not glrt:
    raise SystemExit("mint missing id/token")
(wd / "runner_id").write_text(str(rid))
(wd / "glrt").write_text(glrt)
os.chmod(wd / "glrt", 0o600)
# never print glrt
print(f"mint_ok id={rid} token_len={len(glrt)} tags={tags}", flush=True)
PY
  RUNNER_ID="$(cat "$WORKDIR/runner_id")"
  log "minted runner_id=${RUNNER_ID}"
}

wait_status() {
  local want="$1" deadline=$((SECONDS + ONLINE_TIMEOUT)) st
  while [ "$SECONDS" -lt "$deadline" ]; do
    st="$(secret exec GITLAB_TOKEN="$SECRET_KEY" -- bash -c \
      'curl -sk --max-time 15 -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" -H "Host: '"${HOST_HDR}"'" \
        "'"${API_BASE}"'/runners/'"${RUNNER_ID}"'"' \
      | python3 -c 'import sys,json; print(json.load(sys.stdin).get("status",""))' 2>/dev/null || true)"
    log "runner status=${st:-unknown}"
    [ "$st" = "$want" ] && return 0
    sleep 5
  done
  return 1
}

run_worker_remote() {
  [ -n "${HOMELAB_SSH:-}" ] || die "HOMELAB_SSH required for remote podman worker"
  local -a ssh_opts=(-o BatchMode=yes -o IdentitiesOnly=yes)
  [ -n "${HOMELAB_SSH_KEY:-}" ] && ssh_opts+=(-i "$HOMELAB_SSH_KEY")

  REMOTE_CONFIG="/tmp/gl-ctl-cfg-$$"
  # ship glrt over ssh stdin into remote file (not argv)
  ssh "${ssh_opts[@]}" "$HOMELAB_SSH" "sudo -n mkdir -p ${REMOTE_CONFIG} && sudo -n chmod 700 ${REMOTE_CONFIG}"
  # copy token
  ssh "${ssh_opts[@]}" "$HOMELAB_SSH" "sudo -n tee ${REMOTE_CONFIG}/token >/dev/null && sudo -n chmod 600 ${REMOTE_CONFIG}/token" \
    < "$WORKDIR/glrt"
  shred -u "$WORKDIR/glrt" 2>/dev/null || rm -f "$WORKDIR/glrt"

  local forge_url="$GHA_GITLAB_URL"
  # from inside homelab, use public hostname (caddy) or http://gitlab if on podman net — use HTTPS hostname
  log "starting capped gitlab-runner on ${HOMELAB_SSH}"
  ssh "${ssh_opts[@]}" "$HOMELAB_SSH" bash -s <<REMOTE
set -euo pipefail
IMG="${RUNNER_IMAGE}"
CFG="${REMOTE_CONFIG}"
URL="${forge_url}"
MEM="${MEMORY}"
CPUS="${CPUS}"
# pull if missing
sudo -n podman pull "\$IMG" >/dev/null
# register non-interactive
sudo -n podman run --rm \
  --name gl-ctl-register \
  --memory "\$MEM" --cpus "\$CPUS" \
  -v "\$CFG:/etc/gitlab-runner:Z" \
  # New glrt workflow: tags/run_untagged/locked are set at POST /user/runners only.
  "\$IMG" register --non-interactive \
    --url "\$URL" \
    --token "\$(sudo -n cat \$CFG/token)" \
    --executor shell \
    --description "gha-runner-ctl-shell"
# shred token file after register (config.toml holds auth)
sudo -n shred -u "\$CFG/token" 2>/dev/null || sudo -n rm -f "\$CFG/token"
# run in background briefly so agent contacts GitLab
sudo -n podman rm -f gl-ctl-worker 2>/dev/null || true
sudo -n podman run -d \
  --name gl-ctl-worker \
  --memory "\$MEM" --cpus "\$CPUS" \
  --restart=no \
  -v "\$CFG:/etc/gitlab-runner:Z" \
  "\$IMG" run --max-builds 1 --working-directory /home/gitlab-runner
echo "worker_started"
REMOTE

  if wait_status "online"; then
    log "PROOF: runner online"
  else
    log "WARN: did not reach online within ${ONLINE_TIMEOUT}s (still cleaning up)"
    # dump remote logs (no secrets)
    ssh "${ssh_opts[@]}" "$HOMELAB_SSH" \
      'sudo -n podman logs gl-ctl-worker 2>&1 | tail -40' || true
  fi

  # stop worker
  ssh "${ssh_opts[@]}" "$HOMELAB_SSH" \
    'sudo -n podman stop -t 15 gl-ctl-worker 2>/dev/null; sudo -n podman rm -f gl-ctl-worker 2>/dev/null; true'
  log "worker stopped"
}

# --- main ---
resolve_api_base
export API_BASE
mint_runner
if [ -z "${HOMELAB_SSH:-}" ]; then
  die "set HOMELAB_SSH to run the capped podman worker on the forge host"
fi
run_worker_remote
# final status probe
wait_status "online" 2>/dev/null || true
st="$(secret exec GITLAB_TOKEN="$SECRET_KEY" -- bash -c \
  'curl -sk --max-time 15 -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" -H "Host: '"${HOST_HDR}"'" \
    "'"${API_BASE}"'/runners/'"${RUNNER_ID}"'"' \
  | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("status"), d.get("online"))' 2>/dev/null || echo unknown)"
log "final_status_before_delete=${st}"
log "OK cycle complete (cleanup will DELETE runner)"

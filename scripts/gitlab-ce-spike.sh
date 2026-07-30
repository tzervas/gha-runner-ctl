#!/usr/bin/env bash
# gitlab-ce-spike.sh — live proof: mint + inspect + delete one GitLab CE runner.
#
# Does NOT start gitlab-runner. Proves the control-plane API path only.
# Never prints PAT or glrt values.
#
# Prerequisites: secret CLI (key gitlab/api-token), curl, python3.
#
# Usage:
#   ./scripts/gitlab-ce-spike.sh
#   GHA_GITLAB_URL=https://git.vectorweight.com ./scripts/gitlab-ce-spike.sh
#
# If this host cannot open :443 to the forge (common WSL→LAN firewall):
#   HOMELAB_SSH=kang@192.168.x.x
#   HOMELAB_SSH_KEY=/path/to/deploy-key
# script forwards 127.0.0.1:18443 → remote 127.0.0.1:443.
set -euo pipefail

GHA_GITLAB_URL="${GHA_GITLAB_URL:-https://git.vectorweight.com}"
SECRET_KEY="${GITLAB_SECRET_KEY:-gitlab/api-token}"
SPIKE_DESC="${SPIKE_DESC:-gha-runner-ctl-spike-ephemeral}"
LOCAL_PORT="${GITLAB_SPIKE_LOCAL_PORT:-18443}"
WORKDIR="${TMPDIR:-/tmp}/gl-spike-$$"
mkdir -p "$WORKDIR"
chmod 700 "$WORKDIR"

die() { echo "gitlab-ce-spike: $*" >&2; exit 1; }
log() { echo "gitlab-ce-spike: $*" >&2; }

command -v secret >/dev/null || die "secret CLI not found"
command -v curl >/dev/null || die "curl not found"
command -v python3 >/dev/null || die "python3 not found"
secret ls 2>/dev/null | grep -qx "$SECRET_KEY" || die "missing vault key $SECRET_KEY"

HOST_HDR="$(python3 -c "from urllib.parse import urlparse; print(urlparse('''${GHA_GITLAB_URL}''').hostname or '')")"
[ -n "$HOST_HDR" ] || die "could not parse host from GHA_GITLAB_URL"

API_BASE=""
TUNNEL_PID=""
RUNNER_ID=""

cleanup() {
  set +e
  if [ -n "${RUNNER_ID:-}" ] && [ -n "${API_BASE:-}" ]; then
    log "cleanup DELETE /runners/${RUNNER_ID}"
    # Token must be expanded inside the secret-exec child, never in this shell.
    secret exec GITLAB_TOKEN="$SECRET_KEY" -- bash -c \
      'curl -sk -o /dev/null -w "cleanup_http=%{http_code}\n" --max-time 15 \
        -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" \
        -H "Host: '"${HOST_HDR}"'" \
        -X DELETE "'"${API_BASE}"'/runners/'"${RUNNER_ID}"'"' 2>/dev/null || true
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
    log "direct reachability ok (sign_in=${code})"
    return 0
  fi
  if [ -z "${HOMELAB_SSH:-}" ]; then
    die "cannot reach ${GHA_GITLAB_URL} (sign_in=${code}). Set HOMELAB_SSH=user@host for tunnel."
  fi
  local -a ssh_opts=(-o BatchMode=yes -o ExitOnForwardFailure=yes -o IdentitiesOnly=yes)
  if [ -n "${HOMELAB_SSH_KEY:-}" ]; then
    ssh_opts+=(-i "$HOMELAB_SSH_KEY")
  fi
  fuser -k "${LOCAL_PORT}/tcp" 2>/dev/null || true
  sleep 0.2
  ssh "${ssh_opts[@]}" -f -N -L "127.0.0.1:${LOCAL_PORT}:127.0.0.1:443" "$HOMELAB_SSH" \
    || die "ssh tunnel failed"
  TUNNEL_PID="$(ss -ltnp 2>/dev/null | awk -v p=":${LOCAL_PORT}" '
    $4 ~ p {
      if (match($0, /pid=([0-9]+)/, a)) { print a[1]; exit }
    }')"
  API_BASE="https://127.0.0.1:${LOCAL_PORT}/api/v4"
  code="$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    -H "Host: ${HOST_HDR}" "https://127.0.0.1:${LOCAL_PORT}/users/sign_in" || echo 000)"
  [ "$code" = "200" ] || [ "$code" = "302" ] || die "tunnel up but sign_in=${code}"
  log "via SSH tunnel 127.0.0.1:${LOCAL_PORT} (sign_in=${code})"
}

# All API calls run inside secret exec so GITLAB_TOKEN is never exported to this shell.
# State files under WORKDIR (runner id only — never the glrt after proof step).
export WORKDIR API_BASE HOST_HDR SPIKE_DESC SECRET_KEY

resolve_api_base
export API_BASE

secret exec GITLAB_TOKEN="$SECRET_KEY" -- env \
  WORKDIR="$WORKDIR" \
  API_BASE="$API_BASE" \
  HOST_HDR="$HOST_HDR" \
  SPIKE_DESC="$SPIKE_DESC" \
  python3 - <<'PY'
import json, os, subprocess, sys, pathlib

api_base = os.environ["API_BASE"]
host = os.environ["HOST_HDR"]
token = os.environ["GITLAB_TOKEN"]
workdir = pathlib.Path(os.environ["WORKDIR"])
desc = os.environ["SPIKE_DESC"]
rid_path = workdir / "runner_id"

def api(method, path, body=None):
    url = f"{api_base}{path}"
    cmd = [
        "curl", "-sk", "-o", str(workdir / "body.json"),
        "-w", "%{http_code}", "--max-time", "20",
        "-H", f"PRIVATE-TOKEN: {token}",
        "-H", f"Host: {host}",
        "-H", "Content-Type: application/json",
        "-X", method, url,
    ]
    if body is not None:
        cmd.extend(["-d", json.dumps(body)])
    out = subprocess.check_output(cmd, text=True).strip()
    code = int(out)
    raw = (workdir / "body.json").read_text() if (workdir / "body.json").exists() else ""
    data = None
    if raw:
        try:
            data = json.loads(raw)
        except json.JSONDecodeError:
            data = raw
    return code, data

def show(label, code):
    print(f"=== {label} ===")
    print(f"http={code}")

code, data = api("GET", "/version")
show("GET /version", code)
assert code == 200, data
print({k: data.get(k) for k in ("version", "revision", "enterprise")})

code, data = api("GET", "/user")
show("GET /user", code)
assert code == 200, data
print(f"username={data.get('username')} id={data.get('id')} is_admin={data.get('is_admin')}")

code, data = api("GET", "/runners/all")
show("GET /runners/all (before)", code)
assert code == 200, data
print(f"count={len(data) if isinstance(data, list) else data}")

code, data = api("POST", "/user/runners", {
    "runner_type": "instance_type",
    "description": desc,
    "tag_list": ["self-hosted", "linux", "x64", "podman", "tier-micro"],
    "run_untagged": False,
    "locked": False,
    "maximum_timeout": 3600,
    "access_level": "not_protected",
})
show("POST /user/runners", code)
if code != 201:
    print("error=", data if not isinstance(data, dict) else data.get("message") or data, file=sys.stderr)
    sys.exit(1)
tok = (data or {}).get("token") or ""
rid = (data or {}).get("id")
rid_path.write_text(str(rid))
print(f"id={rid}")
print(f"token_present={bool(tok)}")
print(f"token_prefix={(tok[:5] + '…') if tok else None}")
print(f"token_len={len(tok)}")
print(f"token_expires_at={(data or {}).get('token_expires_at')}")
# Drop glrt from memory and body file immediately.
del tok
del data
body = workdir / "body.json"
if body.exists():
    body.write_bytes(b"\0" * body.stat().st_size)
    body.unlink()

code, data = api("GET", f"/runners/{rid}")
show(f"GET /runners/{rid}", code)
assert code == 200, data
keys = ["id", "description", "active", "status", "runner_type", "tag_list", "run_untagged"]
print({k: data.get(k) for k in keys})

code, data = api("DELETE", f"/runners/{rid}")
show(f"DELETE /runners/{rid}", code)
assert code in (200, 204), data
rid_path.unlink(missing_ok=True)

code, data = api("GET", "/runners/all")
show("GET /runners/all (after)", code)
assert code == 200, data
print(f"count={len(data) if isinstance(data, list) else data}")
print("PROOF_OK")
PY

# success path: clear RUNNER_ID so cleanup does not double-delete
RUNNER_ID=""
# If python left a runner_id (failed after mint), cleanup trap will use it —
# but cleanup needs token via secret exec; export id for trap via file.
if [ -f "$WORKDIR/runner_id" ]; then
  RUNNER_ID="$(cat "$WORKDIR/runner_id")"
fi

if [ -z "${RUNNER_ID:-}" ]; then
  trap - EXIT
  if [ -n "${TUNNEL_PID:-}" ] && kill -0 "$TUNNEL_PID" 2>/dev/null; then
    kill "$TUNNEL_PID" 2>/dev/null || true
  fi
  find "$WORKDIR" -type f -exec shred -u {} \; 2>/dev/null || true
  rm -rf "$WORKDIR"
  log "done"
  exit 0
fi

# runner_id still present → proof failed after mint; let EXIT trap delete
die "spike left runner_id=${RUNNER_ID} (cleanup will DELETE)"

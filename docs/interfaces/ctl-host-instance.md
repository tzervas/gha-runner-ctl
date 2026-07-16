# Interface Bulletin: ctl/host-instance

**Contract ID:** `ctl/host-instance`  
**Producer:** W2 (host/env)  
**Status:** STABLE  
**Version:** 0.2.6  
**Consumers:** operators, W3 (PR test plan), narrative docs  
**Depends-on:** `ctl/cli-env@STABLE`, `ctl/security-identity@STABLE`, `ctl/phase-runner` (via `scripts/lib/shell-debug.sh`)

## Fleet agent identity (this host)

| Field | Value |
|-------|--------|
| OS user | `gha-agent` (uid **997**, shell `/usr/sbin/nologin`, **no sudo**) |
| Home | `/home/gha-agent` |
| Rootless runtime | `XDG_RUNTIME_DIR=/run/user/997` |
| Control binary | `/home/gha-agent/.local/bin/gha-runner-ctl` (from `target/release` in repo) |
| subuid/subgid | `gha-agent:100000:65536` (`/etc/subuid`, `/etc/subgid`) |
| Linger | `loginctl enable-linger gha-agent` → **yes** |

Bootstrap (privileged once, serial phases):

```bash
sudo bash /root/work/gha-runner-ctl/scripts/setup-rootless.sh   # exit 0
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/997 \
  bash /home/gha-agent/verify-rootless.sh                     # PASS
```

## Instance layout

| Path | Purpose |
|------|---------|
| `/home/gha-agent/.local/share/gha-runner-ctl/instances/cpu.env` | CPU fleet `EnvironmentFile` (global `GHA_*` per `ctl/cli-env`) |
| `/home/gha-agent/.local/share/gha-runner-ctl/instances/warm-auth.env` | Optional `GH_TOKEN` only, mode **0600**, owner `gha-agent` (bootstrap from host `gh auth token`; prefer `gh auth login` as agent long-term) |
| `/home/gha-agent/.local/share/gha-runner-ctl/logs/` | Operator logs |
| `/home/gha-agent/.config/systemd/user/gha-runner-ctl@.service` | User unit template (`%i` → instance name, e.g. `cpu`) |
| `/home/gha-agent/.config/containers/*.conf` | Rootless Podman storage/engine (from `setup-rootless` phase 03) |

### `cpu.env` (STABLE fields)

| Variable | Value (this host) |
|----------|-------------------|
| `GHA_SCOPE` | `user` |
| `GHA_USER` | `tzervas` |
| `GHA_PREFER_REPOS` | `tzervas/gha-runner-ctl,tzervas/tg-agent-relay,tzervas/agent-harness` |
| `GHA_CONTAINER` | `gha-runner-cpu` |
| `GHA_VOLUME` | `gha-runner-cpu-data` |
| `GHA_RUNNER_NAME` | `wsl-cpu-1` |
| `GHA_LABELS` | `self-hosted,linux,x64,podman` |
| `GHA_IMAGE` | `localhost/gha-runner-ctl:latest` |
| `GHA_CPUS` / `GHA_MEMORY` | `4` / `4g` |
| `GHA_MODE` | `ephemeral` (required for `user` + multi-entry `GHA_PREFER_REPOS` at CLI validate; **`warm` forces `retain` per repo**) |
| `GHA_DEMAND_EXCLUDE_LABELS` | `gpu` |
| `GHA_SKIP_HOST_UPDATE` | `1` |
| `GHA_BUILD_DIR` | `/root/work/gha-runner-ctl/packaging` |

GPU instance env files and `gha-runner-ctl@gpu-*` units are **not** enabled on this host (C4).

## systemd user units

| Unit | State | Exec |
|------|-------|------|
| `gha-runner-ctl@cpu.service` | **enabled, active** | `gha-runner-ctl listen --interval 150 --idle-secs 500` + `EnvironmentFile=…/instances/cpu.env` |

Operator commands (as root wrapper for user bus):

```bash
AGENT_UID=$(id -u gha-agent)
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/${AGENT_UID} \
  DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/${AGENT_UID}/bus \
  systemctl --user {start|stop|status} gha-runner-ctl@cpu
```

Manual listen (no systemd):

```bash
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/997 bash -lc '
  set -a; source ~/.local/share/gha-runner-ctl/instances/cpu.env; set +a
  ~/.local/bin/gha-runner-ctl listen --interval 150 --idle-secs 500
'
```

## Warm retain path (allowlist)

Paced registration (no parallel podman storm):

```bash
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/997 bash -lc '
  set -a
  source ~/.local/share/gha-runner-ctl/instances/cpu.env
  source ~/.local/share/gha-runner-ctl/instances/warm-auth.env   # if agent has no gh credentials
  set +a
  ~/.local/bin/gha-runner-ctl warm --gap-secs 8
'
```

**This host (2026-07-15):** `warm` **PASS** — three retain runners registered and started:

| Repo | Container |
|------|-----------|
| `tzervas/gha-runner-ctl` | `gha-runner-cpu-tzervas-gha-runner-ctl` |
| `tzervas/tg-agent-relay` | `gha-runner-cpu-tzervas-tg-agent-relay` |
| `tzervas/agent-harness` | `gha-runner-cpu-tzervas-agent-harness` |

Per-repo volumes: `gha-runner-cpu-data-<owner>-<repo-slug>` (truncation rules per `ctl/cli-env`).

## Runners / locks (agent XDG)

| Artifact | Pattern |
|----------|---------|
| Registration pace lock | `$XDG_RUNTIME_DIR/gha-runner-ctl-reg-pace.lock` (+ `.exclusive`) |
| Listen lock | `$XDG_RUNTIME_DIR/gha-runner-ctl-listen-{container}.lock` |
| Up lock | `$XDG_RUNTIME_DIR/gha-runner-ctl-up-{container}.lock` |

## Invariants

- Production control plane runs as **gha-agent**, not root (`GHA_ALLOW_ROOT` not used for agent).
- Work containers use **rootless** Podman graph under `/home/gha-agent/.local/share/containers/storage`.
- Micro-agent image path has **no** Podman socket mount; host binary path used for `prepare` / `warm` / `listen` / `up`.
- `verify-rootless` **PASS** confirms non-root, no passwordless sudo, `podman info` rootless=true.

## WSL / dbus notes

- `loginctl enable-linger gha-agent` is set; user systemd **worked** on this host with `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/997/bus`.
- If user bus is missing after reboot: start a user session for `gha-agent` before `systemctl --user`.

## Delta

- **0.2.6 STABLE:** First host-instance bulletin for WSL fleet agent `gha-agent`, `cpu.env`, warm retain allowlist, `gha-runner-ctl@cpu` user unit.
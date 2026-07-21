# Host residual operations

These steps are intentional and human-gated where they can change the workstation
package set or base image. The fleet agent never registers an organization runner for
you without explicit `--scope org` flags.

## Rootless fleet agent (required for production)

WSL / ephemeral dev often opens a root shell. Bootstrap from there, then drop:

```bash
# packages + user gha-agent (nologin, no sudo) + subuid + rootless config
sudo bash scripts/setup-rootless.sh

# must PASS as gha-agent (fails if root or passwordless sudo)
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  bash scripts/verify-rootless.sh

# agent binary (refuses euid 0 without GHA_ALLOW_ROOT=1; refuses rootful CONTAINER_HOST sockets without GHA_ALLOW_ROOTFUL_SOCKET=1)
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  /home/gha-agent/.local/bin/gha-runner-ctl --help
```

Micro-agent container (cannot spawn work containers — no socket):

```bash
bash scripts/build-agent-image.sh
bash scripts/run-agent-micro.sh --help
```

## Sequential fresh shells (no logout / reboot)

Host scripts are a serial orchestrator: one phase shell at a time.

```text
  open phase 01  →  work  →  phase shell exits (closed / jobs reaped)
  open phase 02  →  work  →  phase shell exits
  open phase 03  →  …
```

* Previous shell is fully closed before the next opens (no leftover phase TTY).
* After `apt install`, the next phase is a login shell (`bash -l`) so
  `profile.d` / PATH hooks apply without logging out.
* After creating `gha-agent` + subuid, config/verify run as that user in a new
  shell with a clean `XDG_RUNTIME_DIR`.
* Never background phases (`&`). Depth-capped (`GHA_PHASE_MAX`, default 16).

```bash
# setup-rootless (serial):
#   01-packages → close → 02-agent-user → close → 03-rootless-config → …
sudo bash scripts/setup-rootless.sh
```

| Env | Effect |
|-----|--------|
| `GHA_PHASE_LOGIN=1` | Default: child phases use `bash -l` (reload profile). |
| `GHA_PHASE_LOGIN=0` | Minimal `bash --noprofile --norc` (still a new process). |
| `GHA_PHASE_MAX=16` | Refuse nesting beyond this depth. |

## Debug on error (until stable)

Scripts source `scripts/lib/shell-debug.sh`. The Rust binary dumps context after failures.

| Env | Effect |
|-----|--------|
| `GHA_DEBUG=1` | Shell: `set -x` full trace. Binary: richer dump on error. |
| `GHA_DEBUG_ON_ERR` unset or `1` | Default on: dump user/cwd/phase/podman snapshot when a command or agent call fails. |
| `GHA_DEBUG_ON_ERR=0` | Silence error dumps once the stack is stable. |

Never dumps tokens (`GH_TOKEN` / registration secrets are redacted or skipped).

## Install 0.2.6

Release binary:

```bash
VER=0.2.6
TARGET=x86_64-unknown-linux-gnu
BASE="https://github.com/tzervas/gha-runner-ctl/releases/download/v${VER}"
curl -fsSL -o "gha-runner-ctl-${VER}-${TARGET}.tar.gz" \
  "${BASE}/gha-runner-ctl-${VER}-${TARGET}.tar.gz"
curl -fsSL -o "SHA256SUMS-${VER}.txt" "${BASE}/SHA256SUMS-${VER}.txt"
sha256sum -c "SHA256SUMS-${VER}.txt"
tar xzf "gha-runner-ctl-${VER}-${TARGET}.tar.gz"
cd "gha-runner-ctl-${VER}-${TARGET}"
bash install.sh
export PATH="$HOME/.local/bin:$PATH"
```

From source: `bash packaging/install-ctl.sh` then ensure `~/.local/bin` is on `PATH`.

## Prepare (snapshot + optional host update)

```bash
# Default stock path: host apt/dnf upgrade, build packaging image, seed volume
gha-runner-ctl prepare

# Skip host package refresh only
gha-runner-ctl prepare --skip-host-update
# equivalent: GHA_SKIP_HOST_UPDATE=1 gha-runner-ctl prepare

# Any OCI image as job rootfs (auto → external when GHA_IMAGE is not the stock tag)
GHA_IMAGE=docker.io/library/ubuntu:24.04 gha-runner-ctl prepare --skip-host-update
GHA_IMAGE=ghcr.io/my-org/ci:latest GHA_PULL_POLICY=always gha-runner-ctl prepare --skip-host-update
```

Host package upgrades require privileges and operator intent. Do not automate
unattended host `apt`/`dnf` upgrade without an explicit human decision for that host.

**After pulling packaging changes** (e.g. work image tools: gitleaks, Rust/cargo),
re-run `gha-runner-ctl prepare` (or `prepare --skip-host-update`) so live work
containers pick up the rebuilt image + reseeding snapshot. Hot-path `up` uses
the configured pull policy (`never` by default for **build** mode) and will not
rebuild packaging for you.

Full image reference: [WORK_IMAGES.md](WORK_IMAGES.md).

## User listen (personal batch)

```bash
# Public personal repos (default visibility when no filter is set)
gha-runner-ctl --scope user --user YOUR_LOGIN listen --interval 180 --idle-secs 180

# Or 1-click outside a git checkout (defaults to user batch)
gha-runner-ctl --full-auto
```

Repo-scoped from a checkout:

```bash
cd ~/work/your-repo
gha-runner-ctl --scope repo --auto listen --interval 30 --idle-secs 180
```

## Residual checklist

- [ ] Binary 0.2.6 installed; `gha-runner-ctl --help` works
- [ ] Auth path chosen (`gh` / GCM / `GH_TOKEN` / interactive) with least privilege
- [ ] Human approved host package refresh or used `--skip-host-update`
- [ ] `prepare` completed (image + volume present)
- [ ] `listen` running for intended scope (`repo` / `user` / `org`)
- [ ] Consumer workflows use labels `self-hosted,linux,x64,podman` (or your custom set)

## Multi-instance (CPU + GPU) on WSL

Locks are per `--container`, so two `listen` processes can run:

| Instance | Labels | Resources | GPU |
|----------|--------|-----------|-----|
| cpu | `self-hosted,linux,x64,podman` | 4 CPU / 4g | no |
| gpu | `self-hosted,linux,x64,podman,gpu` | 4 CPU / 4g | `--gpu` |

Canonical host layout: `~/.local/share/gha-runner-ctl/` + `systemctl --user start gha-runner-ctl@cpu gha-runner-ctl@gpu`.

```bash
# GPU jobs only
runs-on: [self-hosted, linux, x64, podman, gpu]
```

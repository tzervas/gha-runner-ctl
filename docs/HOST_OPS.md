# Host residual operations

These steps are intentional and **human-gated** where they can change the workstation
package set or base image. The controller never registers an organization runner for
you without explicit `--scope org` flags.

## Install 0.2.2

**Release binary:**

```bash
VER=0.2.2
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

**From source:** `bash packaging/install-ctl.sh` then ensure `~/.local/bin` is on `PATH`.

## Prepare (snapshot + optional host update)

```bash
# Default: host apt/dnf upgrade, then image build --pull=always, then seed volume
gha-runner-ctl prepare

# Skip host package refresh only
gha-runner-ctl prepare --skip-host-update
# equivalent: GHA_SKIP_HOST_UPDATE=1 gha-runner-ctl prepare
```

Host package upgrades require privileges and operator intent. Do **not** automate
unattended host `apt`/`dnf` upgrade without an explicit human decision for that host.

## User listen (personal batch)

```bash
# Public personal repos (default visibility when no filter is set)
gha-runner-ctl --scope user --user YOUR_LOGIN listen --interval 30 --idle-secs 180

# Or 1-click outside a git checkout (defaults to user batch)
gha-runner-ctl --full-auto
```

Repo-scoped from a checkout:

```bash
cd ~/work/your-repo
gha-runner-ctl --scope repo --auto listen --interval 30 --idle-secs 180
```

## Residual checklist

- [ ] Binary 0.2.2 installed; `gha-runner-ctl --help` works
- [ ] Auth path chosen (`gh` / GCM / `GH_TOKEN` / interactive) with least privilege
- [ ] Human approved host package refresh **or** used `--skip-host-update`
- [ ] `prepare` completed (image + volume present)
- [ ] `listen` running for intended scope (`repo` / `user` / `org`)
- [ ] Consumer workflows use labels `self-hosted,linux,x64,podman` (or your custom set)

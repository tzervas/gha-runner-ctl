# Quickstart — WSL dual CPU + dual GPU-slice runners

Target layout on this workstation (WSL2):

| Piece | Path / unit |
|-------|-------------|
| Binary | `~/.local/bin/gha-runner-ctl` |
| Instance env | `~/.local/share/gha-runner-ctl/instances/{cpu,gpu-a,gpu-b}.env` |
| Logs | `~/.local/share/gha-runner-ctl/logs/` |
| systemd | `systemctl --user status 'gha-runner-ctl@*'` |

## Architecture

```text
  GitHub Actions (runs-on labels)
           │
           ├─ self-hosted,linux,x64,podman          ──►  wsl-cpu-1   (4 CPU / 4g, no GPU)
           ├─ …,gpu  or  …,gpu,gpu-slice-a         ──►  wsl-gpu-a   (4 CPU / 4g + soft GPU slice A)
           └─ …,gpu  or  …,gpu,gpu-slice-b         ──►  wsl-gpu-b   (4 CPU / 4g + soft GPU slice B)

  Listeners (always on, cheap):  gha-runner-ctl listen  ×3
  Heavy containers:              Podman up only when demand; down on idle → GPU free
```

**GPU slices (consumer GeForce / WSL):** there is no hardware MIG. Two workers **time-share** the same GPU with labels `gpu-slice-a` / `gpu-slice-b` and env `GHA_GPU_SLICE`. When **both** GPU containers are down, the device is free for the Windows host / other apps.

## 1. Clean old installs

```bash
# stop controllers (avoid pkill -f self-match)
systemctl --user stop 'gha-runner-ctl@*' 2>/dev/null || true
podman rm -f gha-runner-cpu gha-runner-gpu gha-runner-gpu-a gha-runner-gpu-b 2>/dev/null || true
# optional: archive old /root/running tarballs → ~/.local/share/gha-runner-ctl/archive/
```

## 2. Install binary (0.2.4+)

```bash
# from repo after cargo build --release, or release tarball:
install -m 0755 target/release/gha-runner-ctl ~/.local/bin/gha-runner-ctl
export PATH="$HOME/.local/bin:$PATH"
```

## 3. Prepare snapshot volumes

```bash
gha-runner-ctl --scope user --user YOUR_LOGIN --all-repos \
  --build-dir /path/to/gha-runner-ctl/packaging \
  --volume gha-runner-cpu-data --container gha-runner-cpu \
  prepare --skip-host-update   # omit skip to apt/dnf upgrade host first
# repeat for gha-runner-gpu-a-data and gha-runner-gpu-b-data
```

## 4. Instance env files

**CPU** (`instances/cpu.env`) — ignores GPU jobs:

```bash
GHA_SCOPE=user
GHA_USER=tzervas
GHA_ALL_REPOS=true
GHA_CONTAINER=gha-runner-cpu
GHA_VOLUME=gha-runner-cpu-data
GHA_RUNNER_NAME=wsl-cpu-1
GHA_LABELS=self-hosted,linux,x64,podman
GHA_CPUS=4
GHA_MEMORY=4g
GHA_MODE=ephemeral
GHA_DEMAND_EXCLUDE_LABELS=gpu
```

**GPU-A** — only jobs with `gpu`:

```bash
GHA_GPU=true
GHA_GPU_SLICE=a
GHA_CONTAINER=gha-runner-gpu-a
GHA_VOLUME=gha-runner-gpu-a-data
GHA_RUNNER_NAME=wsl-gpu-a
GHA_LABELS=self-hosted,linux,x64,podman,gpu,gpu-slice-a
GHA_DEMAND_REQUIRE_LABELS=gpu
# + same scope/user/cpus/memory as cpu
```

**GPU-B** — same with `slice=b`, `gpu-slice-b`, container/volume/name `…-b`.

## 5. systemd user units

```bash
# Template: ~/.config/systemd/user/gha-runner-ctl@.service
# EnvironmentFile=…/instances/%i.env
# ExecStart=…/gha-runner-ctl listen --interval 30 --idle-secs 180

systemctl --user daemon-reload
systemctl --user enable --now gha-runner-ctl@cpu gha-runner-ctl@gpu-a gha-runner-ctl@gpu-b
loginctl enable-linger "$USER"   # keep running after logout
```

## 6. Consumer workflows (self-hosted only)

```yaml
jobs:
  build:
    runs-on: [self-hosted, linux, x64, podman]
  train:
    runs-on: [self-hosted, linux, x64, podman, gpu]           # either slice
  train_a:
    runs-on: [self-hosted, linux, x64, podman, gpu, gpu-slice-a]
```

Registration is **per-repo** (user batch re-registers to whichever owned repo has demand). Personal accounts cannot use a single “user-wide” runner object.

## 7. Verify

```bash
systemctl --user is-active gha-runner-ctl@cpu gha-runner-ctl@gpu-a gha-runner-ctl@gpu-b
tail -f ~/.local/share/gha-runner-ctl/logs/cpu.log
gh api repos/OWNER/REPO/actions/runners --jq '.runners[]|{name,status,busy,labels:[.labels[].name]}'
gh workflow run ci.yml -R OWNER/REPO
```

When idle, containers exit — `down: no GPU runner containers running — GPU returned to host (idle)`.

## Ops notes

| Topic | Guidance |
|-------|----------|
| Thrashing | 0.2.4+ sticky registration: will not recycle mid-job when demand moves repos |
| Ephemeral race | If logs say “registration has been deleted”, wait ~5s and `up` again (or let `listen` retry) |
| Private repos | `GHA_ALL_REPOS=true` + token with repo admin |
| Host upgrades | `prepare` without `--skip-host-update` (human-gated apt/dnf) |
| **API rate limits** | **Never** poll all owned repos every few seconds. Set `GHA_PREFER_REPOS` to an **allowlist** of CI repos only (e.g. three products). Core REST quota can look fine while Actions list endpoints return 403 secondary limits after a scan storm. Use `listen --interval 60`+ and at most a few listeners. |
| Registration without list | `up` (registration-token) often still works when `actions/runs` is secondary-limited — force `up` to pick queued jobs |

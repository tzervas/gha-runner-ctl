# Quickstart — WSL dual CPU + dual GPU-slice runners

Target layout on this workstation (WSL2):

| Piece | Path / unit |
|-------|-------------|
| Fleet agent (binary) | `~/.local/bin/gha-runner-ctl` |
| Instance env | `~/.local/share/gha-runner-ctl/instances/{cpu,gpu-a,gpu-b}.env` |
| Logs | `~/.local/share/gha-runner-ctl/logs/` |
| systemd | `systemctl --user status 'gha-runner-ctl@*'` |
| Work image | `localhost/gha-runner-ctl:latest` (Ubuntu + actions/runner) |
| Agent image (optional) | `localhost/gha-runner-ctl-agent:latest` (Ubuntu-minimal stripped control plane) |

## Architecture: fleet agent + work endpoints

Call the long-lived Rust process the fleet agent. It is not the fat CI image.

| Plane | Lifetime | Role |
|-------|----------|------|
| Fleet agent | Always on | Registration intelligence, pacing, allocate/tear down work containers |
| Work endpoint | Job / warm retain | Official `actions/runner` + toolchains inside a Podman container |

Deploy the agent two ways (same binary):

1. Host binary + systemd (recommended on single-user WSL) — this quickstart.
2. Micro-agent container — Ubuntu 24.04 base stripped to binary + CA certs only (`packaging/Containerfile.agent`). No Podman socket — cannot spawn work containers; full `listen`/`warm`/`up` use the host binary as `gha-agent` (see [DESIGN](DESIGN.md) / [SECURITY](SECURITY.md)).

Once a work runner is registered and online, GitHub pushes jobs. You do not need aggressive API polling for assignment.

| Approach | API cost | When to use |
|----------|----------|-------------|
| `warm` + retain (recommended) | One registration-token POST per repo, paced | Steady CI for a small allowlist |
| User-batch ephemeral + listen | New registration-token every scale-up / repo switch | Ad-hoc many repos (use allowlist + pacing) |
| Org runner | One registration for the org | Best if CI lives under an org |

```text
  Preferred fleet (personal repos):
    [fleet agent] warm --prefer-repos a/r1,a/r2,a/r3
         → paced registration-token POSTs
         → one work container per repo (retain online)
         → GitHub pushes jobs; almost no further registration API

  Demand listen (optional scale / recovery):
    [fleet agent] poll allowlist every 2–5 min, paced GETs
         → only mint tokens / spin work containers if needed
```

GPU slices (consumer GeForce / WSL): no hardware MIG. Soft slices `gpu-slice-a|b` time-share; idle tear-down frees the GPU for the host.

## 1. Clean old installs

```bash
# stop fleet agent units (avoid pkill -f self-match)
systemctl --user stop 'gha-runner-ctl@*' 2>/dev/null || true
podman rm -f gha-runner-cpu gha-runner-gpu gha-runner-gpu-a gha-runner-gpu-b 2>/dev/null || true
# optional: archive old /root/running tarballs → ~/.local/share/gha-runner-ctl/archive/
```

## 2. Install binary (0.2.6)

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

CPU (`instances/cpu.env`) — ignores GPU jobs:

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

GPU-A — only jobs with `gpu`:

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

GPU-B — same with `slice=b`, `gpu-slice-b`, container/volume/name `…-b`.

## 5. systemd user units

```bash
# Template: ~/.config/systemd/user/gha-runner-ctl@.service
# EnvironmentFile=…/instances/%i.env
# ExecStart=…/gha-runner-ctl listen --interval 180 --idle-secs 180
# scope=user: interval is floored at 120s even if you pass a lower value

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

Registration is per-repo (user batch re-registers to whichever owned repo has demand). Personal accounts cannot use a single “user-wide” runner object.

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
| API rate limits | Always set `GHA_PREFER_REPOS` (allowlist). Calls are paced (`GHA_API_MIN_GAP_MS=1000` default) and budgeted (`GHA_API_MAX_PER_POLL=12`). On 403/429 the process backs off (starts at `GHA_API_BACKOFF_SECS=90`, doubles to 15m) and honors `Retry-After` / rate-limit reset. For `scope=user`, prefer `listen --interval 180` (default) or at least 120s; only start the listeners you need (e.g. CPU only unless GPU jobs exist). |
| Registration without list | `up` (registration-token) often still works when `actions/runs` is secondary-limited — force `up` to pick queued jobs |
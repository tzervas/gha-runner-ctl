# Work container images (any OCI rootfs)

`gha-runner-ctl` runs GitHub Actions jobs inside a **work container**. You choose
the image; the fleet agent injects the runner kit and entrypoint when needed.

Nothing is locked to `localhost/gha-runner-ctl` except the **default** convenience
tag used by `image-mode=auto`.

## Workflow-selectable image + arch (issue #28)

Fleet runners are Podman containers with **no in-container engine**. Jobs must
**not** nest `podman`/`docker` (e.g. mycelium-lang `draw-in-container.sh` fails
with `need podman or docker`). Instead, select the target distro/arch at
**spawn** via `runs-on` labels so the job runs **natively inside** that runner.

### Label → image map

| Source | Role |
|--------|------|
| **Built-in defaults** | Common distro tags (`ubuntu-24.04`, `debian-bookworm`, `rocky-9`, …) → Docker Hub library images |
| **`GHA_IMAGE_MAP` / `--image-map`** | JSON or minimal TOML file; **overrides/extends** builtins |

Example workflow cell (mycelium-lang draw-in / multi-distro CI):

```yaml
jobs:
  draw-in-ubuntu:
    runs-on: [self-hosted, linux, x64, podman, ubuntu-24.04]
    steps:
      - uses: actions/checkout@v4
      - run: uname -a && cat /etc/os-release
  draw-in-arm64:
    # Requires QEMU/binfmt on the fleet host (see below)
    runs-on: [self-hosted, linux, arm64, podman, ubuntu-24.04]
    steps:
      - run: uname -m   # aarch64 inside emulated runner
```

When the listen pool sees `ubuntu-24.04` on the job, it sets the work image to
the mapped OCI ref (`docker.io/library/ubuntu:24.04` by default), forces
`image-mode=external`, pulls per policy, and registers the runner **with that
label** so GitHub routes the job. No nested container.

#### Config file format

**JSON** (`packaging/image-map.example.json`):

```json
{
  "images": {
    "ubuntu-24.04": "docker.io/library/ubuntu:24.04",
    "custom-ci": "ghcr.io/org/ci:1"
  },
  "arches": {
    "arm64": "linux/arm64"
  }
}
```

**TOML** (`packaging/image-map.example.toml`):

```toml
[images]
ubuntu-24.04 = "docker.io/library/ubuntu:24.04"
custom-ci = "ghcr.io/org/ci:1"

[arches]
arm64 = "linux/arm64"
```

```bash
# Fleet host env (instance .env or systemd)
export GHA_IMAGE_MAP=/etc/gha-runner-ctl/image-map.json
# or: --image-map /path/to/image-map.toml
```

#### Built-in image labels (subset)

| Label | Default image |
|-------|----------------|
| `ubuntu-24.04` | `docker.io/library/ubuntu:24.04` |
| `ubuntu-22.04` | `docker.io/library/ubuntu:22.04` |
| `debian-bookworm` / `debian-12` | `docker.io/library/debian:bookworm` |
| `rocky-9` | `docker.io/library/rockylinux:9` |
| `fedora-40` | `docker.io/library/fedora:40` |
| `alpine-3.20` | `docker.io/library/alpine:3.20` |

If **no** image label matches, behavior is unchanged: `GHA_IMAGE` / stock packaging image.

### Cross-arch emulation (`--platform` / arch labels)

| `runs-on` arch token | Podman `--platform` | notes |
|----------------------|---------------------|--------|
| `x64` / `amd64` / `x86_64` | (native on amd64 hosts — no flag) | default fleet |
| `arm64` / `aarch64` | `linux/arm64` | needs binfmt on non-arm hosts |
| `riscv64` | `linux/riscv64` | experimental; runner kit may need custom seed |
| `x86` / `i386` | `linux/386` | |
| `arm` / `armv7` | `linux/arm/v7` | |

CLI override: `GHA_PLATFORM=linux/arm64` / `--platform linux/arm64` on single-container `up`.

#### Fleet-host prerequisite: binfmt_misc / QEMU

Cross-arch spawn **checks** `/proc/sys/fs/binfmt_misc` for a QEMU handler matching
the target. If missing, spawn **fails with a clear error** (never a silent
wrong-arch run):

```text
cannot spawn arm64 runner (platform linux/arm64): QEMU/binfmt_misc is not registered …
```

Register handlers on the **fleet host** (once per boot, or via systemd):

```bash
# Privileged one-shot (common with Podman):
podman run --privileged --rm tonistiigi/binfmt --install all

# Or distro packages, e.g. Debian/Ubuntu:
# sudo apt-get install -y qemu-user-static
# sudo systemctl restart systemd-binfmt
```

The actions/runner kit in the volume is still the host-arch seed by default.
For production multi-arch, prefer matching `GHA_RUNNER_ARCH` / `GHA_RUNNER_SHA256`
(or a custom `GHA_RUNNER_SEED_URL`) to the emulated arch, or use a multi-arch
aware seed pipeline. Draft behavior sets `runner_arch` from the arch label when
known (`x64`/`arm64`/`arm`).

## Modes (`GHA_IMAGE_MODE` / `--image-mode`)

| Mode | Behavior |
|------|----------|
| **auto** (default) | Stock default tag → **build** packaging `Containerfile`. Any other OCI ref → **external**. |
| **build** | Always `podman build -t $GHA_IMAGE` from `GHA_BUILD_DIR` / `packaging/`. |
| **external** | Use `$GHA_IMAGE` as-is (pull per policy). Seed **actions/runner** into the work volume; bind-mount host entrypoint. |

## Pull policy (`GHA_PULL_POLICY` / `--pull-policy`)

| Policy | When unset (default) | Meaning |
|--------|----------------------|---------|
| **never** | build mode | Do not pull; image must exist locally (or was just built). |
| **missing** | external mode | Pull only if missing. |
| **always** | — | Always pull/refresh. |

Set explicitly any time, e.g. `GHA_PULL_POLICY=always` for weekly refresh of a distro base.

## Ergonomic examples

### Stock packaging image (unchanged)

```bash
# defaults: GHA_IMAGE=localhost/gha-runner-ctl:latest → auto → build
gha-runner-ctl prepare --skip-host-update
```

### Any Linux distro as the job rootfs

```bash
export GHA_IMAGE=docker.io/library/ubuntu:24.04
# auto → external; pull=missing; inject runner into volume
gha-runner-ctl prepare --skip-host-update

# or Fedora / Alpine / Debian / Amazon Linux / custom CI images:
export GHA_IMAGE=docker.io/library/fedora:40
export GHA_IMAGE=docker.io/library/alpine:3.20
export GHA_IMAGE=ghcr.io/my-org/ci-base:2026.07
export GHA_IMAGE=registry.internal:5000/team/builder@sha256:…
gha-runner-ctl prepare --skip-host-update
```

### Pin runner kit (not hard-coded forever)

Defaults match `packaging/Containerfile` but are overridable:

```bash
export GHA_RUNNER_VERSION=2.335.1
export GHA_RUNNER_ARCH=x64
export GHA_RUNNER_SHA256=4ef2f25285f0ae4477f1fe1e346db76d2f3ebf03824e2ddd1973a2819bf6c8cf
# or a fully custom tarball URL (still SHA256-checked):
export GHA_RUNNER_SEED_URL=https://example.com/my-actions-runner.tar.gz
export GHA_RUNNER_SHA256=<64-hex>
```

### User / UID inside the work container

```bash
export GHA_RUNNER_USER=1001:1001   # stock packaging default
export GHA_RUNNER_USER=0:0         # root (some minimal images)
```

### Seed helper (only used to unpack the runner into the volume)

```bash
# default: docker.io/library/ubuntu:24.04
export GHA_SEED_HELPER_IMAGE=docker.io/library/debian:bookworm-slim
```

### Custom entrypoint

```bash
export GHA_ENTRYPOINT=/path/to/my-entrypoint.sh
# default: packaging/entrypoint.sh beside Containerfile (GHA_BUILD_DIR)
```

### Instance env (multi-manager)

```bash
# ~/.local/share/gha-runner-ctl/instances/cpu.env
GHA_IMAGE=ghcr.io/my-org/rust-ci:1.96
GHA_IMAGE_MODE=external
GHA_PULL_POLICY=missing
GHA_RUNNER_USER=1001:1001
GHA_LABELS=self-hosted,linux,x64,podman
# …
```

Then `prepare` once per volume and start `listen` as usual.

## What “any *nix image” means

| Layer | Source |
|-------|--------|
| **Rootfs / tools** | Your OCI image (`GHA_IMAGE`) — Ubuntu, Fedora, Alpine, custom org images, etc. |
| **Runner binaries** | Injected into the **volume** from the official (or custom-URL) actions/runner tarball |
| **Register/run loop** | Host `entrypoint.sh` (bind-mounted for external mode) |

The official **actions/runner** release used by default is a **Linux** userspace binary
(glibc-oriented). It runs inside whatever Linux OCI rootfs you pick **if** that rootfs
can execute it and supply dependencies (`libicu`, `git`, `curl`, … — or you install them
in your image / job steps).

FreeBSD/OpenBSD/**named** container images on a Linux Podman host are only useful when
they are still Linux ABI-compatible rootfs images (or you pre-seed a custom runner kit
that matches the image ABI). True FreeBSD/OpenBSD kernels are outside Podman-on-Linux;
bring your own seed volume (`run.sh` already present → prepare skips re-download).

## Security notes

- Image refs are validated (no shell metacharacters; length/charset limits).
- External mode still uses `no-new-privileges`, `--cap-drop ALL`, and configurable `--user`.
- Prefer digests (`@sha256:…`) or immutable tags for production images.
- `--pull=never` remains the safe hot-path default for **build** mode after prepare.

## Related

- [HOST_OPS.md](HOST_OPS.md) — prepare / re-seed after packaging changes  
- [SECURITY.md](SECURITY.md) — work vs agent plane  
- [ctl/cli-env](interfaces/ctl-cli-env.md) — full flag/env contract  

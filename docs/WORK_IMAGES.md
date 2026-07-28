# Work container images (any OCI rootfs)

`gha-runner-ctl` runs GitHub Actions jobs inside a **work container**. You choose
the image; the fleet agent injects the runner kit and entrypoint when needed.

Nothing is locked to `localhost/gha-runner-ctl` except the **default** convenience
tag used by `image-mode=auto`.

## Cold vs hot

Fleet runners have **no general egress** (measured 2026-07-26: gha-runner-ctl#51's
standards gate failed at "Ensure PyYAML" because the image lacked it and the job could not
fetch it; `tzervas/fleet-ops`'s render-backlog workflow cannot succeed on any self-hosted
runner today because `gh` is on no runner image either). That single fact drives a hard
split between what may be baked into an image and what must never be:

| | COLD | HOT |
|---|---|---|
| **What** | Tooling + pre-resolved dependencies (uv, uv-managed CPython, ruff, pytest, gitleaks, gh, and — via the warm-image pattern below — a repo's exact locked Python/Rust dependency set) | Application secrets, and the repo checkout itself |
| **Built with** | **No application secret**, ever | Injected at container **instantiation**, via the fleet's `secret exec` wrapper |
| **Where it lives** | Baked into an image layer, published to GHCR, content-addressed | Never in a layer — mounted/exported into the running container only |
| **Files** | `packaging/Containerfile`, `packaging/Containerfile.runner-{base,shell,python}`, `packaging/Containerfile.warm-{python,rust}` | `entrypoint.sh` (registration token), the fleet's secret-injection path, `actions/checkout` |

**Why this is mandatory, not an optimization:** with no egress at job time, a job-time `uv
sync` or `cargo fetch`/`build` cannot resolve or download a single package — it will fail
every time, not just run slowly. Pre-resolving dependencies at image-build time (where
egress does exist, the same assumption every pinned-binary `RUN curl ...` step in these
Containerfiles already depends on) is the only way the dependency set can ever be present
when the job actually executes. Caching is a side effect of this split, not its purpose.

**Content addressing (warm images):** `packaging/Containerfile.warm-python` and
`Containerfile.warm-rust` bake ONE consuming repo's exact locked dependency set
(`uv sync --frozen` / `cargo fetch --locked` — frozen/locked so the image can never
silently drift from the lock the repo actually committed). Because the tag is derived from
the lock's own hash, a cache hit is provable and a stale image can never be mistaken for a
fresh one:

```bash
# Python
TAG="fleet-warm-python:<repo>-$(sha256sum uv.lock | cut -c1-12)"
# Rust
TAG="fleet-warm-rust:<repo>-$(sha256sum Cargo.lock | cut -c1-12)"
```

### Operator runbook — warm images

**Rebuild a warm image after a lock changes:**

```bash
# Python — build context is ONLY the two manifest files, never the repo tree/secrets
mkdir -p /tmp/warm-ctx && cp pyproject.toml uv.lock /tmp/warm-ctx/
TAG="fleet-warm-python:<repo>-$(sha256sum uv.lock | cut -c1-12)"
podman build -f packaging/Containerfile.warm-python -t "$TAG" /tmp/warm-ctx
podman tag "$TAG" "ghcr.io/tzervas/$TAG"
podman push "ghcr.io/tzervas/$TAG"

# Rust — same idea, Cargo.toml + Cargo.lock only
mkdir -p /tmp/warm-ctx && cp Cargo.toml Cargo.lock /tmp/warm-ctx/
TAG="fleet-warm-rust:<repo>-$(sha256sum Cargo.lock | cut -c1-12)"
podman build -f packaging/Containerfile.warm-rust -t "$TAG" /tmp/warm-ctx
```

A repo whose lock hasn't changed never needs a rebuild: the tag it already points at
(`GHA_IMAGE=ghcr.io/tzervas/fleet-warm-python:<repo>-<hash>`) is still exactly right,
because the hash is the lock.

**Verify a cache hit (i.e. that the job actually used the baked venv/registry and did not
silently fall back to a live resolve):**

```bash
# Python — job-time `uv sync --frozen` inside the warm image should report 0 to
# install/resolve; anything else means the venv wasn't actually pre-populated.
uv sync --frozen --python 3.13.14  # inside the running container
# Rust — job-time build should succeed fully offline:
cargo build --offline
```

If either command needs network access to complete, the warm image is stale, was built
from a different lock than the one now committed, or was never built at all — check the
tag's lock-hash suffix against `sha256sum uv.lock`/`sha256sum Cargo.lock` in the repo the
job actually checked out.

### Quadlet — not used, and why

Podman >= 4.4 ships Quadlet (`.container`/`.image`/`.build`/`.volume`/`.kube` units under
`~/.config/containers/systemd/`), which would be a natural fit for declaring these image
builds and their target volumes declaratively. It is **deliberately not used anywhere in
this pipeline**: the target host is measured as Debian GNU/Linux 12 (bookworm) running
podman `4.3.1+ds1-8+deb12u1+b3`, with no backports repository configured (apt candidate is
still 4.3.1) and no Quadlet generator present anywhere (`find /usr /opt -name 'quadlet*'`
is empty). A Quadlet unit written today would be silently inert on this host — no error,
no unit, nothing — which is this fleet's single worst failure shape (see Honest CI,
`docs/HONEST_CI.md`). Every build/push mechanism in this pipeline (`packaging/Containerfile.warm-*`,
`.github/workflows/publish-images.yml`) therefore uses plain `podman build` / `podman
push` / `podman pull`, all confirmed present in 4.3.1. Revisit Quadlet once the host's
podman is upgraded to >= 4.4.

As a complementary, degrade-gracefully cache that DOES work on 4.3.1 today: a named
`podman volume` (e.g. `fleet-uv-cache`, `fleet-cargo-registry`) mounted into warm-image
builds and job containers (`-v fleet-uv-cache:/home/runner/.cache/uv`) speeds up repeat
resolves across different repos/locks without depending on any baked layer — it just has
no content-addressing or zero-egress guarantee of its own, unlike the warm images above.

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

### Fleet runner-base (ap-workflows) — preferred once published

Shared CI rootfs with rootless-friendly tools (`gh`, `trivy`, `gitleaks`, …) built from
`ap-workflows` as `ghcr.io/tzervas/ap-workflows/runner-base` (tags: `latest` and
`<git-sha>`). After that image is pushed to GHCR, fleet hosts / seed units should set:

```bash
export GHA_IMAGE=ghcr.io/tzervas/ap-workflows/runner-base:latest
export GHA_IMAGE_MODE=external
# optional: pin a digest after first pull for reproducibility
gha-runner-ctl prepare --skip-host-update
```

Until publish, workflows must not assume these tools exist via `sudo apt-get` (rootless
podman + `no_new_privs` cannot escalate). Prefer `command -v` short-circuit, then a
checksum-verified user-writable install into `$RUNNER_TEMP/bin`, else fail loudly.

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

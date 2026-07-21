# Work container images (any OCI rootfs)

`gha-runner-ctl` runs GitHub Actions jobs inside a **work container**. You choose
the image; the fleet agent injects the runner kit and entrypoint when needed.

Nothing is locked to `localhost/gha-runner-ctl` except the **default** convenience
tag used by `image-mode=auto`.

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

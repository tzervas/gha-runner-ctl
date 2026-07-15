# Changelog

## 0.2.4

- **Demand filters:** `--demand-require-labels` / `--demand-exclude-labels` so CPU listeners ignore GPU jobs and GPU listeners only wake on `gpu`
- **Sticky user-batch:** do not recycle registration while active repo still has matching work
- **GPU soft-slices:** `--gpu-slice a|b` for dual workers on one consumer GPU (time-share; no MIG); idle `down` frees GPU
- Labels convention: `gpu` + optional `gpu-slice-a` / `gpu-slice-b`

## 0.2.3

- **Multi-instance locks:** `up`/`listen` PID locks are namespaced by `--container`, so two controllers (e.g. CPU + GPU) can run side-by-side
- **`--gpu` / `GHA_GPU`:** Podman GPU attach for WSL2 (`--gpus all`, `/dev/dxg`, `/usr/lib/wsl` mount + `LD_LIBRARY_PATH`)
- Pair GPU instance with an extra runner label (e.g. `gpu`) so only GPU jobs schedule there

## 0.2.2

- **`prepare` updates the host first** (`apt-get`/`dnf` upgrade) before building
  the snapshot; skip with `--skip-host-update` / `GHA_SKIP_HOST_UPDATE=1`
- Image build uses `--pull=always` so the Ubuntu base is not stale
- Containerfile runs `apt-get upgrade` before package install

## 0.2.1

- Security tooling: `scripts/security-scan.sh` (cargo-audit, cargo-deny, gitleaks, trivy)
- `deny.toml` license/advisory policy for MIT-compatible deps
- Container runs as non-root UID **1001** (trivy DS-0002); prepare seeds volume ownership
- No RustSec CVEs in lockfile at release time

## 0.2.0

- **`--auto`**: detect `owner/repo` from cwd (`gh repo view` / `git remote`)
- **`scope=user`**: batch poll all personal owned repos; ephemeral re-register
  to whichever has self-hosted demand (one process, not one per repo)
- **`detect`** command and **`scripts/auto-listen.sh`** (thin `--full-auto` shim;
  pass-through args only — not a separate `--batch`/`--org` driver)
- Distributed release still via `scripts/dist.sh --upload` (local build)

## 0.1.1

- Fail-closed validation for repo/owner/labels/names/image/cpus/memory
- Secret redaction on errors; registration env file overwrite+unlink
- HTTP timeouts; single-instance flock on `up` / `listen`
- Podman: `no-new-privileges`, `--pull=never` on hot path
- Wake server requires `GHA_WAKE_TOKEN`; constant-time compare
- Entrypoint validates `https://github.com/…` only; never logs tokens
- SECURITY.md operator checklist
- **Distributed release assets**: Linux x86_64 tarball + SHA256SUMS via
  `scripts/dist.sh` and tag workflow (required for host updates without cargo)

## 0.1.0

- Initial release: one Podman runner, snapshot `prepare`, auto-registration, `listen` up/down
- Repo and org registration scopes
- MIT license; NOTICE cites [actions/runner](https://github.com/actions/runner) (MIT)

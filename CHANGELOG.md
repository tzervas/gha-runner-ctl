# Changelog

## 0.2.6

- **Architecture (fleet agent):** long-lived Rust **fleet agent** owns registration + container lifecycle; work endpoints are separate job containers (see `docs/DESIGN.md`)
- **Fresh shell phases:** `gha_run_phase` runs each setup step in a new bash (login by default) so package/PATH/subuid changes apply without logout; sequential + `GHA_PHASE_MAX` cap
- **Debug on error:** `GHA_DEBUG` / `GHA_DEBUG_ON_ERR` (default on) dumps safe context from scripts (`scripts/lib/shell-debug.sh`) and the binary after failures
- **Shell style:** scripts/Containerfiles avoid `&&` chains — `set -e`, newlines, `\` continuations, explicit `if`
- **Rootless Podman path:** dedicated OS user `gha-agent` (no sudo, nologin); `scripts/setup-rootless.sh` + `verify-rootless.sh`
- **Refuse root:** agent exits unless unprivileged, or `GHA_ALLOW_ROOT=1` for ephemeral WSL/dev bootstrap only
- **Refuse rootful sockets:** block `CONTAINER_HOST` system podman/docker sock unless `GHA_ALLOW_ROOTFUL_SOCKET=1`
- **Dual agent deploy:** host binary as `gha-agent` (full control plane) **or** micro-agent image with **no** Podman socket (cannot spawn if compromised)
- **Work containers:** `--cap-drop ALL`, no runtime socket mounts
- **Hardened release profile:** LTO, single codegen unit, `panic=abort`, strip
- **Intelligent registration:** host-wide paced registration-token POSTs (`GHA_REG_MIN_GAP_SECS`, `GHA_REG_MAX_PER_HOUR`), backoff on 403/429
- **Retain reuse:** skip registration-token when volume already registered for same repo (GitHub pushes jobs)
- **`warm` command:** batch-register retain runners for allowlist, one container per repo, gentle gaps
- User+retain allowed for sticky single-repo units; multi-repo → prefer `warm` fleet

## 0.2.5

- **Paced GitHub API client:** min gap between calls (`GHA_API_MIN_GAP_MS`, default 500ms), per-poll GET budget (`GHA_API_MAX_PER_POLL`, default 24), honor `X-RateLimit-*` / `Retry-After`, exponential cool-down on 403/429
- User-batch floor poll interval 30s; warn when `GHA_PREFER_REPOS` unset
- Cap job lookups per repo; smaller runs pages; repo-list page cap

## 0.2.4
- **Rate-limit safe user-batch:** `GHA_PREFER_REPOS` is an allowlist (only those repos polled); soft-skip 403/404 on Actions APIs


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

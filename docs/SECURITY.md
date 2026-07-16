# Security model

## Planes

| Plane | Trust surface | Notes |
|---|---|---|
| **Fleet agent** (`gha-runner-ctl`) | Small: TLS to GitHub, rootless Podman as `gha-agent` | Long-lived; no sudo; refuses euid 0 unless `GHA_ALLOW_ROOT=1` (ephemeral WSL/dev); refuses rootful `CONTAINER_HOST` sockets unless `GHA_ALLOW_ROOTFUL_SOCKET=1` |
| **Work endpoint** (runner image) | Large: CI tools, build deps, job code | UID 1001, `--cap-drop ALL`, **no** runtime socket mount |
| **Micro-agent image** | Binary + CA only | Distroless nonroot; **no shell, no sudo, no Podman socket** — cannot spawn containers if compromised |

Design goal: agent is purpose-built; work is disposable; compromise of the micro-agent does not yield a container runtime.

## Identity & rootless Podman

WSL / ephemeral dev shells often land you as **root**. That is a bootstrap accident, not the agent identity.

| Role | OS user | Podman | sudo |
|---|---|---|---|
| Bootstrap (this shell) | root (WSL default) | rootful OK only for setup | n/a |
| **Fleet agent (production)** | `gha-agent` (shell=nologin) | **rootless** only | **never** |
| Micro-agent container | distroless `nonroot` (65532) | **none** (no socket, no CLI) | none |

```bash
# once, from privileged bootstrap:
sudo bash scripts/setup-rootless.sh
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  bash scripts/verify-rootless.sh

# run agent:
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  /home/gha-agent/.local/bin/gha-runner-ctl …

# ephemeral WSL/dev only (not production):
GHA_ALLOW_ROOT=1 gha-runner-ctl …
# rootful CONTAINER_HOST only if intentional (rare):
GHA_ALLOW_ROOTFUL_SOCKET=1 gha-runner-ctl …
```

**If someone breaks into the micro-agent container:** no shell, no root, no sudoer, no Podman/Docker socket → they cannot spin host/work containers. Token theft is still a risk if `GH_TOKEN` was injected — use short-lived / least-privilege tokens.

**If someone compromises the host agent process as `gha-agent`:** they can manage **that user’s** rootless containers only — not host root, not other users’ processes.

## Threats we design for

| Threat | Mitigation |
|---|---|
| Shell injection via labels/names/repo | Allowlist charset validation before Podman/API |
| Registration / PAT leakage in logs | Redact secrets; never print tokens; scrub API errors |
| Env-file residue | `0600` under `XDG_RUNTIME_DIR`, overwrite + unlink after `up` |
| Rootful runtime socket on fleet agent host | Refuse system `CONTAINER_HOST` podman/docker sockets unless `GHA_ALLOW_ROOTFUL_SOCKET=1`; production runs as `gha-agent` rootless |
| Privilege escalation in work container | `--security-opt no-new-privileges`, no docker.sock mount on work |
| Surprise image pulls on `up` | `--pull=never` after `prepare` |
| Unauthenticated wake endpoint | Loopback only; **requires** `GHA_WAKE_TOKEN` (≥16 chars) |
| Twin fleet agents racing | Exclusive **PID/instance lock file** on `listen` / `up` (`create_new` + live-PID check; not `flock(2)`) |
| Public fork abuse of self-hosted | Prefer private repos; documented warning on `up` |
| Stale registration after ephemeral job | Wipe `.runner` / credentials on `down` in ephemeral mode |
| Registration / Actions API thrash | Allowlist `GHA_PREFER_REPOS`; API + registration budgets; soft-skip 403 |
| Bloated agent attack surface | Micro-agent image = binary + CA certs only; host binary preferred on single user |

## Org vs personal repos

Organization runners only serve **repositories in that org**. Personal
`user/repo` workflows cannot use `vectorweighttechnologies` runners while
staying outside the org. See README.

## Operator checklist

- [ ] `gh auth` / `GH_TOKEN` with least privilege for registration only  
- [ ] Runner groups in org UI limited to intended repos  
- [ ] Prefer **private** repos on self-hosted compute  
- [ ] Do not commit registration tokens or `GHA_WAKE_TOKEN`  
- [ ] Keep `gha-runner-ctl` and the **work** image pin current (runner sha256 in Containerfile)  
- [ ] Prefer **host binary as `gha-agent`** for full `listen`/`warm`/`up`; micro-agent has **no** Podman socket and cannot spawn work containers
- [ ] Always set `GHA_PREFER_REPOS` allowlist for personal multi-repo  
- [ ] Run `bash scripts/security-scan.sh` before each release  

## Hardening follow-ons

See the checklist at the end of [DESIGN](DESIGN.md) §7 (read-only agent rootfs, dropped caps, fine-grained PATs, signed releases / SBOM).

## Local scanners

```bash
bash scripts/security-scan.sh
# cargo audit          — RustSec CVEs in Cargo.lock
# cargo deny check     — advisories + licenses + sources
# gitleaks detect      — secrets in tree
# trivy fs             — vulns/secrets/misconfig (Containerfile, etc.)
```

## Host + snapshot freshness

`gha-runner-ctl prepare` **updates host packages first** (apt/dnf), then rebuilds
the image with `--pull=always` and reseeds the volume. That keeps the long-lived
snapshot from freezing known CVEs on the host or base image. Skip only when
intentional: `--skip-host-update` or `GHA_SKIP_HOST_UPDATE=1`.

## Reporting

Open a private security advisory on the GitHub repo if you find a vulnerability.

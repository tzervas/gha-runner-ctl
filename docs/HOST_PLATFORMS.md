# Host platforms

`gha-runner-ctl` is a **fleet manager for any Podman-capable Unix host.** The
primary, best-tested target is **native Linux** (bare metal, VM, or cloud
instance). Everything else — FreeBSD/OpenBSD, generic Unix, and Windows via
WSL2 — is a variant of that same host contract: a POSIX shell, a Rust
1.96+ toolchain (only if building from source), and a working Podman
(rootless preferred).

**WSL2 is one optional deployment path, not a requirement.** Nothing in the
binary, the install scripts, or `prepare`/`up`/`listen` assumes Windows or
WSL. Docs that read like a WSL bootstrap transcript describe one operator's
workstation, not a hard dependency — see [Notes on WSL-flavored
docs](#notes-on-wsl-flavored-docs) below.

## Supported hosts

| OS family | Status | Notes |
|---|---|---|
| **Linux (native)** | Primary, fully supported | Any distro with rootless Podman ≥4.x, cgroup v2, and `newuidmap`/`newgidmap` (user namespaces). This is the target `ci.yml`/`fleet-ci.yml` run against (`runs-on: [self-hosted, linux, x64, podman]`). |
| **FreeBSD / OpenBSD** | Supported via a Linux VM or Linux-ABI container runtime | Podman's container runtime (runc/crun) needs a Linux kernel. Run the fleet agent inside a Linux `bhyve`/`vm-bhyve` guest (FreeBSD) or a Linux VM under `vmm(4)` (OpenBSD), or use a Linux-ABI compatibility layer if your Podman build supports it. The agent binary itself is portable Rust and builds on BSD; what it drives (Podman work containers) needs a Linux kernel underneath. See [WORK_IMAGES.md](WORK_IMAGES.md#what-any-nix-image-means) for the same constraint on job rootfs images. |
| **Generic Unix (illumos, other POSIX)** | Best-effort | Builds if Rust 1.96+ and Podman (or a Podman-compatible OCI runtime) are available. Not in CI; treat as community-supported. |
| **Windows + WSL2** | Optional, supported | See [WSL2 (optional)](#wsl2-optional) below. Useful when the fleet host is a Windows workstation (e.g. to expose a consumer GPU to soft-sliced work containers). Not required for any core feature. |

Podman is the reference container engine throughout this repo; anything
sufficiently Podman-API-compatible (matching CLI + rootless semantics) is
expected to work but is untested.

## Prerequisites by OS family

| Requirement | Linux (native) | FreeBSD/OpenBSD (via Linux VM) | WSL2 |
|---|---|---|---|
| Podman | ≥4.x, rootless | ≥4.x, rootless, inside the Linux guest | ≥4.x, rootless, inside the WSL2 distro |
| `gh` auth / `GH_TOKEN` | `gh auth login`, GCM, or masked prompt | same, inside the guest | same, inside the WSL2 distro |
| cgroup | v2 (systemd or cgroupfs) | v2, inside the Linux guest | v2, inside the WSL2 distro (WSL2's kernel provides this) |
| User namespaces | `newuidmap`/`newgidmap`, `/etc/subuid` + `/etc/subgid` entries | same, inside the guest | same, inside the WSL2 distro |
| systemd (optional, for `--user` units) | Most distros | Depends on guest distro | Ubuntu WSL ≥ 22.04 with `systemd=true` in `wsl.conf`, or run the binary directly without systemd |
| Rust 1.96+ (source builds only) | rustup or distro package | rustup, inside the guest | rustup, inside the WSL2 distro |

`scripts/setup-rootless.sh` provisions the dedicated `gha-agent` OS user,
subuid/subgid ranges, and rootless Podman config. It is host-agnostic: run it
from whatever privileged bootstrap shell you have (a fresh cloud VM's root
shell, a WSL2 root shell, a BSD Linux-guest shell) — it does not check for or
require WSL.

## Optional: containers/VMs for testing the fleet manager itself

`gha-runner-ctl`'s own CI (`ci.yml`, `fleet-ci.yml`) currently runs on a
single self-hosted Linux/Podman runner. A useful follow-on for
cross-platform confidence is a **test matrix** that exercises the same
`cargo test` / `scripts/verify-rootless.sh` path across the host families
above, e.g.:

```yaml
strategy:
  matrix:
    include:
      - target: native-linux
        runs-on: [self-hosted, linux, x64, podman]
      - target: freebsd-vm
        runs-on: [self-hosted, freebsd, podman]   # Linux guest under bhyve
      - target: wsl2
        runs-on: [self-hosted, windows, wsl2, podman]
```

This is a proposal, not yet wired up — none of these runner labels exist
today. If you stand up a FreeBSD/OpenBSD or WSL2 self-hosted runner and want
it in the matrix, open a PR; the binary has no platform-specific code paths
that would block it.

## Quickstart: native Linux

This is the default, shortest path — no VM, no Windows layer.

```bash
# 1. Install (release tarball or from source; see README.md#install)
curl -fsSL -o gha-runner-ctl.tar.gz \
  https://github.com/tzervas/gha-runner-ctl/releases/latest/download/gha-runner-ctl-x86_64-unknown-linux-gnu.tar.gz
tar xzf gha-runner-ctl.tar.gz && cd gha-runner-ctl-*/ && bash install.sh
export PATH="$HOME/.local/bin:$PATH"

# 2. Rootless bootstrap (once, from a root shell on this host)
sudo bash scripts/setup-rootless.sh
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  bash scripts/verify-rootless.sh

# 3. Auth + run as gha-agent
sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) \
  gha-runner-ctl --full-auto
```

See [QUICKSTART.md](QUICKSTART.md) for the fuller multi-instance CPU+GPU
systemd walkthrough and [HOST_OPS.md](HOST_OPS.md) for host residual ops.

## Appendix: WSL2 (optional)

Use this only if the fleet host happens to be a Windows machine — most
commonly to reach a consumer GPU for soft-sliced GPU work containers.

1. Enable WSL2 with a Linux distro (Ubuntu recommended) and install Podman
   inside that distro, same as any native Linux host.
2. Rootless Podman inside WSL2 works the same way as bare-metal Linux
   (`scripts/setup-rootless.sh`, `scripts/verify-rootless.sh`) — WSL2's
   kernel provides cgroup v2 and user namespaces.
3. Caveats specific to WSL2:
   - The WSL2 root shell you land in by default is a **bootstrap
     convenience**, not the agent's runtime identity — still bootstrap once,
     then drop to `gha-agent` (rootless), same as native Linux. Treat
     `GHA_ALLOW_ROOT=1` as ephemeral-dev-only, never production, on WSL2
     exactly as on any other host.
   - GPU passthrough to soft-sliced work containers depends on your NVIDIA
     WSL driver setup; there is no hardware MIG on consumer GeForce, so GPU
     "slices" are time-shared (`--gpu-slice a|b`), not partitioned — same
     behavior as on native Linux with a consumer GPU.
   - `systemd --user` units need `systemd=true` under `[boot]` in
     `/etc/wsl.conf` (Ubuntu WSL ≥ 22.04); otherwise run the binary directly
     or under a process supervisor of your choice.
   - Networking/loopback for the wake endpoint (`GHA_WAKE_TOKEN`,
     `--wake-port`) behaves like native Linux inside the WSL2 distro; it is
     not automatically reachable from the Windows host without a port proxy,
     which is out of scope here.

Full dual CPU + dual GPU-slice systemd walkthrough (written from a WSL2
workstation, but every step is plain rootless-Podman-on-Linux):
[QUICKSTART.md](QUICKSTART.md).

## Notes on WSL-flavored docs

Several existing docs ([QUICKSTART.md](QUICKSTART.md),
[HOST_OPS.md](HOST_OPS.md), [DESIGN.md](DESIGN.md), [SECURITY.md](SECURITY.md))
were originally written against one operator's WSL2 workstation and describe
that environment concretely (paths, instance names, GPU slices). None of the
underlying mechanisms — rootless Podman, `gha-agent` user, systemd `--user`
units, GPU soft-slices — are WSL-specific; they apply verbatim to a bare
native Linux box. Where those docs say "WSL bootstrap shell" or similar, read
it as "the privileged shell you bootstrap from" — which on a cloud VM or bare
metal box is just `root` or `sudo`, no WSL involved.

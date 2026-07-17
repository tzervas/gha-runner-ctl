# Dynamic ephemeral runner pool

## Host budget (default)

| Resource | Cap |
|----------|-----|
| CPU | **8** cores total (all ephemeral workers) |
| Memory | **8 GiB** total |
| Max workers | **16–24** (process + host caps) |

Shared across CPU + GPU soft-slice managers via  
`$XDG_DATA_HOME/gha-runner-ctl/pool/state.json` (file lock).

## Automatic sizing (no workflow changes)

| Tier | When (job name / labels) | Default size |
|------|--------------------------|--------------|
| **micro** | gitleaks, trivy, lint, ruff, fmt, docs, security… | 0.25 CPU / 512 MiB |
| **small** | fleet-ci detect, light checks | 0.5 CPU / 1 GiB |
| **medium** | test, cargo, pytest, ci (default) | 1 CPU / 2 GiB |
| **large** | train, release, e2e, build-image, benchmark… | 2 CPU / 4 GiB |
| **gpu** | label `gpu` / `gpu-slice-*` | 2 CPU / 4 GiB + GPU device |

If the preferred size does not fit free budget, the allocator **shrinks** toward the free remainder (floor 0.25c / 256 MiB) or skips until a worker finishes.

## Horizontal scale

On each listen tick (pool mode `dynamic`):

1. Reap exited worker containers → release budget  
2. List matching queued/in-progress jobs  
3. Spawn `container-w{N}` workers until local max or pool full  

Example: eight micro jobs → up to ~8× (0.25c/512m) under 8c/8g.

## Config (instance env)

```bash
GHA_POOL_MODE=dynamic
GHA_POOL_CPUS=8
GHA_POOL_MEMORY=8g
GHA_POOL_MAX_WORKERS=16
# optional override fixed single-runner legacy:
# GHA_POOL_MODE=off
```

Legacy single-container listen: `GHA_POOL_MODE=off` (still auto-sizes `GHA_CPUS`/`GHA_MEMORY` from the first matching job).

## GPU slices

GPU listeners still require `gpu` labels; they claim **large/gpu** CPU+RAM from the **same** host pool and attach the device. Soft slices `a`/`b` remain time-share on one consumer GPU.

## Policy

- Ephemeral only (no retain warm fleet of work endpoints)  
- Managers (`listen`) stay warm via systemd  
- Devs **do not** set runner CPU/RAM in workflows — only `runs-on: [self-hosted, linux, x64, podman]` (+ `gpu` when needed)

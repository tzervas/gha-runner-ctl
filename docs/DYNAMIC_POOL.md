# Dynamic ephemeral runner pool

## Host budget (default)

| Resource | Cap |
|----------|-----|
| CPU | **16** cores total (all ephemeral workers) |
| Memory | **16 GiB** total |
| Max workers | **16–24** (process + host caps) |
| Single worker ceiling | **20 CPU / 28 GiB** (`xlarge`) or **8 CPU / 16 GiB** (`gpu`) |

The 16c/16 GiB figures above are the built-in *fallback* default for hosts
that don't set `GHA_POOL_CPUS`/`GHA_POOL_MEMORY` — they are deliberately small
so an unconfigured pool fails safe on a modest box. They are **not** big enough
to grant a `large` or `xlarge` job its preferred size (see table below); on a
fallback-sized pool those jobs shrink toward the free remainder instead. Set
`GHA_POOL_CPUS`/`GHA_POOL_MEMORY` to match the real host — the tiers below are
sized for a many-core self-hosted box, not a laptop.

Shared across CPU + GPU soft-slice managers via  
`$XDG_DATA_HOME/gha-runner-ctl/pool/state.json` (file lock).

Override per host:

```bash
GHA_POOL_MODE=dynamic
GHA_POOL_CPUS=16
GHA_POOL_MEMORY=16g
GHA_POOL_MAX_WORKERS=16
```

## Automatic sizing

| Tier | When | Size |
|------|------|------|
| **micro** | gitleaks, trivy, lint, ruff, fmt, security, commitizen, quadlet-generate, capture-diff, registry-check, secret-keymap, policy-check, adversarial, yamllint, shellcheck, notify… | 1 CPU / 1 GiB |
| **small** | fleet-ci detect, light checks, **unrecognised job names (catch-all default)** | 2 CPU / 2 GiB |
| **medium** | cargo/pytest/test/check/docs | 4 CPU / 4 GiB |
| **large** | release, e2e, build-image, “build and test”, local-parity… | 12 CPU / 16 GiB |
| **xlarge** | workspace-build, chromium, all-features, label `xlarge` | 20 CPU / 28 GiB |
| **gpu** | label `gpu` / `gpu-slice-*` / `cuda` | 8 CPU / 16 GiB + GPU device |

If the preferred size does not fit free budget, the allocator **shrinks** toward the free remainder (floor 0.25c / 256 MiB) or skips until a worker finishes.

### Explicit size labels (preferred for justified heavy jobs)

Put a size token on `runs-on` **only when justified**. The worker re-registers with that label so GitHub routes the job correctly.

```yaml
# Default — medium (4c/4g). No size label required.
runs-on: [self-hosted, linux, x64, podman]

# Heavy Rust / multi-crate — 12c/16g
runs-on: [self-hosted, linux, x64, podman, large]

# Max single worker — 20c/28g (must be justified: full workspace, chromium, etc.)
runs-on: [self-hosted, linux, x64, podman, xlarge]

# GPU (5080 soft-slice listeners) — must include gpu
runs-on: [self-hosted, linux, x64, podman, gpu]
# optional soft slice:
# runs-on: [self-hosted, linux, x64, podman, gpu, gpu-slice-a]
```

**Do not** label everything `xlarge`. The fleet manager treats size labels as hard demand; bloated labels starve micro/security jobs.

## Horizontal scale

On each listen tick (pool mode `dynamic`):

1. Reap exited worker containers → release budget  
2. List matching queued/in-progress jobs  
3. Spawn `container-w{N}` workers until local max or pool full  

Example: eight micro jobs (1c/1g each = 8c/8g) fill the default 16c/16g pool;
size `GHA_POOL_CPUS`/`GHA_POOL_MEMORY` to the real host for actual concurrency.

## GPU slices

GPU listeners require `gpu` on the job; they claim **gpu** CPU+RAM from the **same** host pool and attach the device. Soft slices `a`/`b` time-share one consumer GPU (e.g. RTX 5080). Full-device jobs still use `gpu` without a slice preference unless you pin `gpu-slice-a|b`.

## Policy

- Ephemeral workers (warm retain is opt-in)  
- Managers (`listen`) stay warm via systemd  
- Workflows set **labels**, not raw CPU/RAM env vars  
- Resource choices must be **justified** (compile weight, matrix width, GPU training)  

### Post-job-exit reclaim vs. spawn grace (issue #127)

The scale planner reclaims idle (`running && !busy`) ephemeral workers
immediately so they can't pin a pool slot to a repo they aren't converting on
(`post-job-exit`, see `plan_scale` in `src/pool.rs`). `busy=0` is ambiguous by
itself, though: it's also what a **freshly-spawned, never-dispatched** worker
looks like before GitHub has had a chance to schedule a job onto it. Reading
that as "already finished" reclaimed workers 40-70s after `up`, before they
could ever take a job — a total stall regardless of queue depth.

The fix: a `running && !busy` worker is only reclaimable when it is
[`post_job_exit_eligible`](../../src/pool.rs) — either it carries a positive
completion signal, or it has been running at least `GHA_POOL_SPAWN_GRACE_SECS`
(default `90`) since spawn. A worker whose job genuinely finishes quickly is
still reclaimed promptly in ephemeral mode: the runner process exits (it's
`exec`'d as the container's PID 1), the container stops, and
`reap_pool_workers` reaps it on the very next tick — independent of the grace
window, so completed workers never leak capacity waiting one out.

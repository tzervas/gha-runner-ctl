# Fleet manager pre-drain debug capture

| Field | Value |
|-------|--------|
| **Stamp (UTC)** | 2026-07-22T03:10:27Z (capture start) |
| **Host** | WSL/desktop fleet (`gha-agent` uid 997) |
| **Binary** | `/home/gha-agent/.local/bin/gha-runner-ctl` (md5 in `00-identity.txt`) |
| **Purpose** | Snapshot **before intentional queue drain**, for `gha-runner-ctl` product improvements |
| **Drain status** | **NOT performed in this capture** |

Artifacts in this directory: `00-`…`10-` files listed in `INDEX.md`.

---

## Executive snapshot

| Signal | Observation |
|--------|-------------|
| Listen unit | **active/running** (`interval=45s`, `idle-secs=300` via drop-in) |
| Pool claims | 2 short-lived workers claimed for `mycelium-cli-common` / `-myc` at capture mid-point; many workers exit within seconds |
| GitHub work | **~197 queued runs** across 64 repos with work; **0 in_progress** aggregate at sample time |
| Rate limit | Healthy (`core 4973/5000`) — **not** API exhaustion |
| Prefer list | **236** repos (all active non-fork), mycelium-first (~94 myc) |
| Zombie retain | **8+ containers Up ~7h**, GH runners **online + idle** on non-mycelium repos — contradicts FLEET_POLICY ephemeral-only |
| warm-boot unit | `gha-runner-warm-boot.service` **active/exited**, and **Required** by listen unit |
| Logging | No live listen stdout in journal usefully; `cpu.log` is historical **root-refusal spam** only |

**Bottom line:** The control plane is up and occasionally claims workers, but the **fleet is not draining backlog**. Primary choke is architectural (per-repo ephemeral registration × multi-job workflows × huge prefer list + zombie retain runners), not GitHub rate limit.

---

## Findings for `gha-runner-ctl` (actionable product bugs / gaps)

### P0 — Zombie retain runners after warm-boot / policy violation

**Evidence:** `05-containers.txt`, `08-runners-online.jsonl`

- Containers such as `gha-runner-cpu-tzervas-webpuppet-rs-mcp` **Up 7 hours**
- Matching GitHub runners: `status=online`, `busy=false`
- Host policy (`FLEET_POLICY.txt`) forbids retain multi-repo warm work endpoints
- Yet `gha-runner-warm-boot.service` still exists and is **Required** by `gha-runner-ctl@.service`

**Impact:** Idle online runners on the **wrong repos** do not help `mycelium-lang` (23 queued). They confuse operators (“fleet is online”) and may consume Podman / cgroup budget.

**Product fixes (suggested):**
1. Split **warm-boot** from **listen** (`Wants=` not `Requires=`; or delete warm-boot in ephemeral mode).
2. On `listen` start with `GHA_MODE=ephemeral`, **reap** containers older than N minutes with retain config, or runners online+idle > idle-secs.
3. `gha-runner-ctl status` should list **stale online idle** runners and containers.
4. Enforce: `warm` command refused when policy file says ephemeral-only.

### P0 — Huge prefer-list + round-robin starves hot queues

**Evidence:** `02-config.txt`, `02-prefer-repos.list`, `06-github-queues.tsv`

- `GHA_PREFER_REPOS` length **236**
- `GHA_REPOS_PER_TICK=12` but pool demand path caps scan (~6 in code)
- Work concentrated: `cabal-devmelopner=50`, `mycelium-lang=23`, `gha-runner-ctl=14`, many myc components 1–2 each
- `sum_in_progress=0` while hundreds queued → demand/up not matching demand volume

**Product fixes:**
1. Support **priority tiers** in prefer list (or `GHA_PRIORITY_REPOS` polled **every tick** before RR).
2. Demand score: prefer repos with **queued job count** (single Actions API query pattern or cache), not pure RR.
3. `GHA_PREFER_REPOS_FILE=` to avoid env size / reload issues; validate length with warnings.
4. Dedicated multi-instance: `listen@myc` (myc-only) + `listen@tooling` without competing one RR cursor.

### P0 — Ephemeral multi-job workflows need N registrations per workflow

**Evidence:** `07-mycelium-lang-detail.txt` — `fleet-ci` single job queued with labels `self-hosted,linux,x64,podman`; historical multi-job detect→noop required 2 registrations.

**Impact:** `GHA_REG_MAX_PER_HOUR=180` can be burned by matrix × components × multi-job CI. Backlog grows faster than reg budget.

**Product fixes:**
1. Document **single-job-per-workflow** as fleet-friendly (umbrella already moved noop into detect).
2. Optional **sticky ephemeral**: keep registration for `idle-secs` after job if same repo still has queued jobs (check before `config.sh remove` / container exit).
3. Batch: one worker container that re-registers in-process without full container recycle when demand stays on same repo.

### P1 — Dual RR state files can diverge

**Evidence:** `04-pool-rr-state.txt`

```
gha-runner-ctl-rr-gha-runner-cpu-gha-agent.txt = 16
gha-runner-ctl-rr-gha-runner-cpu.txt = 0
```

**Product fix:** Single canonical RR path; or migrate/delete legacy file on start; log active offset each tick.

### P1 — Pool claim lifecycle vs container exit

**Evidence:** Claims for `w0`/`w1` existed while containers already exited (seconds later in `05-containers.txt`).

**Product fix:** Reap claims when container not running **before** budget accounting; metrics for claim_orphan_seconds.

### P1 — Observability black hole

**Evidence:** `09-logs.txt` — listen produces almost no operator-visible log; `cpu.log` is only historical root-refusal loops.

**Product fixes:**
1. Structured log to `$XDG_DATA_HOME/gha-runner-ctl/logs/listen-%i.jsonl` every tick: `{offset, scanned, demand, ups, reg_budget_left, pool_free}`.
2. `gha-runner-ctl status --json` for systemd healthchecks.
3. Prometheus textfile or `--metrics-addr` optional.

### P2 — Systemd template defaults fight drop-ins

**Evidence:** Unit `ExecStart=... --interval 150` overridden by drop-in `45`.

**Product fix:** Ship template with env `GHA_LISTEN_INTERVAL` only; no hardcoded interval in unit.

### P2 — Label case

GitHub returns labels `Linux`,`X64` (capitalized) on zombie runners; workflows request `linux`,`x64`. GitHub matches case-insensitively for custom labels usually — note for debugging only.

---

## Config at capture (knobs)

| Knob | Value |
|------|-------|
| GHA_MODE | ephemeral |
| GHA_POOL_MODE | dynamic |
| GHA_POOL_CPUS / MEMORY / MAX_WORKERS | 16 / 16g / 16 |
| GHA_REPOS_PER_TICK | 12 |
| GHA_API_MIN_GAP_MS | 400 |
| GHA_API_MAX_PER_POLL | 80 |
| GHA_REG_MAX_PER_HOUR | 180 |
| Listen interval | **45s** (drop-in) |
| Prefer list | 236 repos, mycelium-first |

---

## Queue pressure (sample)

From `06-github-queues.tsv` (mycelium prefer subset + tooling):

- **repos_with_work=64**, **sum_queued≈197**, **sum_in_progress=0**
- Worst: cabal-devmelopner 50, mycelium-lang 23, gha-runner-ctl 14, agent-harness 9
- Many `mycelium-*` component repos with 1–2 queued (fleet-ci/security from bulk badge apply)

This is exactly the load multi-OS **component** draw-in will amplify unless sticky-ephemeral + priority polling land.

---

## Recommended ops (after this capture; not executed here)

1. Stop/remove **idle online** retain containers (7h Up list).
2. Disable or de-`Require` **warm-boot** for ephemeral fleets.
3. Temporarily shrink prefer to **mycelium-hot (~106)** during draw-in campaigns.
4. Reset RR offset to 0 after prefer rewrite.
5. Cancel or concurrency-group stale PR workflow storms on umbrellas.
6. Prefer **single-job** product workflows; matrix on GH-hosted only for experimental OS.

---

## File index

| File | Contents |
|------|----------|
| `00-identity.txt` | Host, binary path, help header |
| `01-systemd.txt` | Unit status, drop-ins, related units |
| `02-config.txt` | Redacted env + knobs |
| `02-prefer-repos.list` | Full prefer list one-per-line |
| `03-process.txt` | Processes / listen cmdline |
| `04-pool-rr-state.txt` | Pool claims + RR offsets + data tree |
| `05-containers.txt` | Podman ps/images |
| `06-github-queues.tsv` | Queued/in_progress by repo |
| `07-mycelium-lang-detail.txt` | Recent runs + sample job labels |
| `08-runners-online.jsonl` | GH runners online/busy sample |
| `09-logs.txt` | Log dirs + cpu.log tail + journal attempts |
| `10-rate-limit.txt` | API remaining |
| `ANALYSIS.md` | This document |

---

## Suggested gha-runner-ctl issues (copy-paste)

1. **reap-stale-online-runners** — ephemeral mode must not leave 7h idle online retain containers  
2. **priority-prefer-repos** — poll priority set every tick before RR  
3. **sticky-ephemeral-same-repo** — if repo still has queued jobs, skip full teardown  
4. **listen structured tick logs** — demand/up/budget metrics  
5. **warm-boot decoupling** — do not `Requires=` warm-boot for ephemeral listen  
6. **single RR state file** — eliminate dual `*-cpu` / `*-cpu-gha-agent` offsets  
7. **status --json** — operator health for swarm automation  

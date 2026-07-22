# Fleet recovery (safe, queue-preserving)

## Problem

Ephemeral workers exit after one job (or OOM), but **pool claims** can linger
until the next listen tick. That leaves GitHub Actions jobs **queued** while
local capacity looks “full” — operators may be tempted to cancel runs.

**Do not cancel the queue as a recovery step.** Queued jobs are the backlog to
drain. Recovery is **local capacity hygiene** so listen can register fresh
ephemeral runners and pick those jobs up.

## What `recover` does

```bash
gha-runner-ctl recover
gha-runner-ctl recover --json
gha-runner-ctl recover --prune-exited=false   # claims only
```

| Action | Safe? |
|--------|--------|
| Release pool claims whose containers are **not running** | Yes |
| Force-rm **exited/created** fleet containers not in claims | Yes (local only) |
| Reap stale retain leftovers (`GHA_REAP_STALE_SECS`) in ephemeral mode | Yes |
| Cancel GitHub workflow runs / delete queues | **Never** |

After recover, **leave listen running** (or restart `gha-runner-ctl@cpu`). The
next demand poll claims queued jobs for **priority** then prefer-list repos.

## Listen-loop hygiene (0.2.12+)

1. Reap finished claims **before** each poll  
2. Reap again after API cool-down / before spawn batch  
3. On spawn failure, reap + **one retry** (budget leak recovery)  
4. Job name `build` → **large** tier (avoids rustup OOM 137 on micro/small)  
5. Product `ci.yml` advertises `large` on `runs-on` for the build job  

## Host priority for primary backlog

Set hot queues so they are polled **every tick** (never wait full RR):

```bash
# Example: showcase + ctl + myc umbrellas
export GHA_PRIORITY_REPOS=tzervas/gha-runner-ctl,tzervas/mycelium-lang,tzervas/mycelium-lang-myc,tzervas/tg-agent-relay,tzervas/peft-rs,tzervas/tero-mcp,tzervas/memory-gate-rs,tzervas/cabal-devmelopner,tzervas/agent-harness
export GHA_PREFER_REPOS_FILE=$HOME/.local/share/gha-runner-ctl/allowlists/all-active-mycelium-first.list
export GHA_POOL_SCAN_PER_TICK=16
export GHA_REAP_STALE_SECS=3600
export GHA_TICK_LOG=auto
```

Or write `allowlists/priority-hot.list` and expand into `GHA_PRIORITY_REPOS`.

## Automated safe recovery (host)

Timer/oneshot (gha-agent user) — **no** `gh run cancel`:

```bash
# /home/gha-agent/.local/bin/gha-fleet-recover.sh
#!/bin/bash
set -euo pipefail
export XDG_RUNTIME_DIR=/run/user/$(id -u)
export XDG_DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
# shellcheck disable=SC1091
set -a
source "$XDG_DATA_HOME/gha-runner-ctl/instances/cpu.env"
set +a
exec "$HOME/.local/bin/gha-runner-ctl" recover --json
```

Optional systemd user timer every 10–15 minutes. Listen still owns continuous
reap; timer is belt-and-suspenders after crashes.

## Operator runbook

1. `gha-runner-ctl recover --json` — free local budget  
2. Confirm listen active: `systemctl --user status gha-runner-ctl@cpu`  
3. Watch tick log: `$XDG_DATA_HOME/gha-runner-ctl/logs/listen-ticks.jsonl`  
4. Confirm priority repos appear in `pool: spawned=… repo=…` journal lines  
5. **Only** cancel a GitHub run if the **commit is obsolete**, not to “fix” the fleet  

## Failure classes (see HONEST_CI.md)

| Symptom | Class |
|---------|--------|
| Jobs queued, runners busy elsewhere / no capacity | `FAIL_RUNNER` until recover + priority |
| rustup Killed / exit 137 | `FAIL_ENV` undersized worker — use large tier |
| Product test fail | `FAIL_PRODUCT` |

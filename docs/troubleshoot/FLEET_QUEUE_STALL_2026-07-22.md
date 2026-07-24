# Fleet queue stall — pre-drain capture (2026-07-22)

Operator capture **before** draining queues, while multi-OS / component CI
backlog was large.

**Bundle:** [captures/gha-fleet-debug-20260722T031027Z/](./captures/gha-fleet-debug-20260722T031027Z/)  
**Analysis:** [captures/gha-fleet-debug-20260722T031027Z/ANALYSIS.md](./captures/gha-fleet-debug-20260722T031027Z/ANALYSIS.md)

## Headline

- ~197 queued workflow runs (sample), **0 in_progress**
- Listen active (45s interval), rate limit healthy
- **8+ idle online retain runners** ~7h (policy violation / warm-boot residue)
- Prefer list **236** repos; RR dual state files; weak tick observability

## Product work items → 0.2.12

| Finding | Fix in 0.2.12 |
|---------|----------------|
| Huge prefer-list + RR starves hot queues | `GHA_PRIORITY_REPOS` every tick before RR |
| Env-sized 236-repo CSV fragile | `GHA_PREFER_REPOS_FILE` |
| Pool scan hard-cap 6 | `GHA_POOL_SCAN_PER_TICK` (default 12) |
| User-batch floor 120s | `GHA_LISTEN_MIN_INTERVAL` (default 45) |
| Warm-boot / retain zombies | `GHA_REAP_STALE_SECS` on listen start (ephemeral) |
| No tick observability | `GHA_TICK_LOG=auto` → `listen-ticks.jsonl` |

## Host apply (cpu instance)

```bash
# Prefer file (durable allowlist)
export GHA_PREFER_REPOS_FILE=$HOME/.local/share/gha-runner-ctl/allowlists/all-active-mycelium-first.list

# Hot queues polled every tick (never wait full RR)
export GHA_PRIORITY_REPOS=tzervas/mycelium-lang,tzervas/mycelium-lang-myc,tzervas/cabal-devmelopner,tzervas/gha-runner-ctl,tzervas/agent-harness,tzervas/mycelium

export GHA_POOL_SCAN_PER_TICK=16
export GHA_REPOS_PER_TICK=12
export GHA_LISTEN_MIN_INTERVAL=45
export GHA_REAP_STALE_SECS=3600
export GHA_TICK_LOG=auto

# Soft-bind warm-boot (Wants=, not Requires=); prefer disable warm-boot under ephemeral-only policy
# systemctl --user disable --now gha-runner-warm-boot.service
```

After restart, confirm:

1. Journal / stderr: `prefer=N priority=M scan/tick=… reap_stale=…`
2. Tick log: `$XDG_DATA_HOME/gha-runner-ctl/logs/listen-ticks.jsonl`
3. `podman ps` no multi-hour unclaimed retain workers
4. Queued→in_progress on priority repos within a few ticks

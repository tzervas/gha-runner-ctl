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

## Product work items

See ANALYSIS.md § "Findings for gha-runner-ctl".

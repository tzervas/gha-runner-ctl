# Orchestration model (cost-aware)

## Roles

| Role | Model | Responsibility |
|------|--------|----------------|
| **Orchestrator** | frontier (e.g. grok-4.5) | Plan, interfaces, sequencing, merge, security decisions, write PM briefs |
| **Workers / forks** | **Cheapest model currently available** | Execute one tightly scoped brief only |

Worker model selection: always pick the lowest-cost model from `grok models` (today: `grok-composer-2.5-fast`). Never fan work to frontier unless the task is inherently planning/merge. Config: `fork_secondary_model` in `~/.grok/config.toml`.

Workers must **not** invent new cross-cutting APIs. They implement against contracts below (or fail and report; do not redesign).

## Parallelism rules

1. **Split only on disjoint file/ownership sets** (or pure read/verify).
2. **Shared interfaces first** — orchestrator writes the contract; workers fill bodies.
3. **One worker = one PM brief** (see template). No multi-epic forks.
4. **No drive-by refactors** outside In scope.
5. Prefer **serial host mutations** (apt, useradd, warm register) on a single worker; parallelize pure code/docs/tests.

## Shared contracts (0.2.6+)

### A. Planes

- **Fleet agent** = long-lived `gha-runner-ctl` control plane (host binary as `gha-agent` preferred).
- **Work endpoint** = Podman job container (`actions/runner` image).
- **Micro-agent image** = binary + certs only; **no** Podman socket; cannot spawn.

### B. Identity

- Production agent: OS user `gha-agent`, nologin, **no sudo**, rootless Podman.
- Refuses euid 0 unless `GHA_ALLOW_ROOT=1` (ephemeral WSL/dev only).
- Refuses rootful `CONTAINER_HOST` system sockets unless `GHA_ALLOW_ROOTFUL_SOCKET=1`.

### C. Shell phases

- `gha_run_phase` in `scripts/lib/shell-debug.sh`.
- **Sequential only**: open → work → **process exits/closed** → next open.
- Never background phases. Cap: `GHA_PHASE_MAX` (default 16).
- Login shell by default (`GHA_PHASE_LOGIN=1`) so installs apply without logout.

### D. Debug

- `GHA_DEBUG=1` — full shell trace / richer binary dump.
- `GHA_DEBUG_ON_ERR` default **on** until stable; set `0` to silence.
- Never dump tokens.

### E. Shell style

- No `&&` command chains; use `set -e`, newlines, `\`, explicit `if`.
- Containerfiles: one logical step per `RUN` where practical.

---

## Mandatory worker brief (project-management style)

Every fork/subagent prompt **must** include all sections below. Incomplete briefs are invalid — orchestrator rewrites before launch.

```text
# WORKER BRIEF
Repo: <path>
Model: <cheapest available>
Branch / tree notes: <e.g. dirty OK, no commit>

## User story
As a <role>,
I want <capability>,
so that <business/ops outcome>.

## Background (context only)
<3–8 lines max; link contracts A–E; no architecture redesign>

## Task
- T1: <atomic action>
- T2: …
(ordered; each testable)

## Requirements
### Functional
- FR1: …
### Non-functional
- NFR1: (security, idempotency, no token logs, …)
### Constraints
- C1: In scope files only: <paths>
- C2: Out of scope: <paths / actions>
- C3: Contract refs: ORCHESTRATION.md §A–E (or specific)

## Deliverables
- D1: <artifact: code change / script output / report section>
- D2: …

## Success criteria
- SC1: <measurable, e.g. `cargo test` exit 0>
- SC2: …

## Definition of done (DoD)
- [ ] All tasks T* completed or explicitly blocked with reason
- [ ] All success criteria SC* met (evidence: command + exit code or snippet)
- [ ] Deliverables D* present
- [ ] No changes outside In scope
- [ ] No secrets in logs/output
- [ ] Final report uses the template below

## Explicit non-goals
- …

## Final report format (worker must return)
### Status: PASS | FAIL | BLOCKED
### User story: addressed? yes/no
### Tasks: T1 … (done/skipped)
### Evidence: commands + results
### Files changed: list or "none"
### Risks / follow-ups: for orchestrator only
```

## Orchestrator checklist before fork

- [ ] One user story (not a theme)
- [ ] Disjoint ownership vs other in-flight workers
- [ ] Requirements + DoD are binary (pass/fail)
- [ ] Cheapest available model selected
- [ ] Contracts cited; no “figure out architecture”
```

---

## Contracts framework (summary)

Full portable text: `~/documents/agent-dev-methodology.md` §6.

A **contract** is a named, bounded, testable, versioned, owned, published agreement at a boundary.

**Layers:** Framework (process) · Domain (product) · Instance (host/env).

**Lifecycle:** PROPOSE → DRAFT bulletin → STABLE → consume. Missing → consumer BLOCKED.

**IDs this ship:** `framework/pm-brief`, `framework/fractal-depth`, `framework/bulletin`, `framework/git-semver-pr`, `ctl/cli-env`, `ctl/security-identity`, `ctl/phase-runner`, `ctl/work-endpoint`, `ctl/micro-agent`, `ctl/reg-api-budget`, `ctl/host-instance`.

Bulletin graph: `docs/interfaces/BULLETIN_GRAPH.md`.

Consumers list `Depends-on: <id>@STABLE` in Requirements. Producers emit Interface Bulletins. Do not invent APIs.

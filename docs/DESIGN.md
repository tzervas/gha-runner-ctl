# Architecture & Design

This document is the source of truth for how **gha-runner-ctl** is shaped.
Product name stays `gha-runner-ctl`; the long-lived process is the **fleet agent**.

---

## 1. Two planes (preferred model)

```text
  ┌─────────────────────────────────────────────────────────────┐
  │  FLEET AGENT  (long-lived)                                  │
  │  gha-runner-ctl binary — hardened control plane             │
  │                                                             │
  │  • GitHub API (paced, allowlisted, registration budget)     │
  │  • Owns registration lifecycle + REUSE / warm               │
  │  • Allocates / tears down work containers                   │
  │  • systemd unit, bare host, or micro-agent image            │
  └───────────────────────────┬─────────────────────────────────┘
                              │ podman run / stop / rm
                              ▼
  ┌─────────────────────────────────────────────────────────────┐
  │  WORK ENDPOINT  (ephemeral or warm-retain)                  │
  │  Official actions/runner inside a job container             │
  │                                                             │
  │  • Heavy surface: toolchains, checkout, build, GPU          │
  │  • Spun for real job allocation                             │
  │  • Not the control plane; not where PATs live long-term     │
  └─────────────────────────────────────────────────────────────┘
```

| Piece | Lifetime | What it is | Deploy as |
|-------|----------|------------|-----------|
| **Fleet agent** | Long-lived | Rust binary; intelligence + lifecycle | Host install **or** hardened micro-container |
| **Work endpoint** | Job-scoped (or warm retain) | `actions/runner` + CI tools image | Fat work image / volume snapshot |

**Why this split helps security and ops**

* The agent surface is tiny and purpose-built (no compiler, no npm, no broad package set).
* Work containers stay the large, mutable attack surface — and they can die with the job.
* Registration tokens and PAT material stay under agent control (short-lived env files, shred, redaction).
* GitHub **pushes** jobs once a runner is registered and online; the agent’s job is to keep that path intelligent (warm retain / REUSE), not to hammer the API.

### Honest protocol boundary

GitHub Actions still speaks to the **official `actions/runner`** process for job assignment.
The fleet agent does **not** replace that wire protocol. It **owns** when that process is registered, where it runs, and when containers exist:

* Agent mints / reuses registration; paces POSTs.
* Work container runs `config.sh` / `run.sh` (or reuses `.runner` on the volume).
* Agent decides up/down, labels, GPU attach, multi-instance locks.

Future hardening can shrink the work image further or add nested isolation; the agent remains the durable control plane either way.

---

## 2. Dual deployment of the fleet agent

Both are first-class. Same binary; different packaging.

### A. Host binary as `gha-agent` + rootless Podman (**production default**)

```text
OS user gha-agent (nologin, no sudo)
  └── gha-runner-ctl (fleet agent binary)
        └── rootless podman → work containers (mapped UIDs, not host root)
```

* Bootstrap once from the privileged WSL/dev shell: `scripts/setup-rootless.sh`.
* Agent refuses euid 0 unless `GHA_ALLOW_ROOT=1` (ephemeral WSL/dev only).
* Refuses rootful `CONTAINER_HOST` system sockets unless `GHA_ALLOW_ROOTFUL_SOCKET=1`.
* Instance env under `/home/gha-agent/.local/share/gha-runner-ctl/`.
* See [SECURITY](SECURITY.md) and [HOST_OPS](HOST_OPS.md).

### B. Micro-agent container (Alpine-style surface, **cannot spawn**)

```text
podman run --read-only --cap-drop=ALL  gha-runner-ctl-agent
  = binary + CA certs only
  = no shell, no sudo, no podman CLI, no runtime socket
  → can talk to GitHub API if given a token
  → cannot allocate work containers (by design)
```

* Image: [packaging/Containerfile.agent](../packaging/Containerfile.agent).
* Runner: [scripts/run-agent-micro.sh](../scripts/run-agent-micro.sh) (drops all caps, read-only, refuses socket env).
* **Never** mount `podman.sock` / `docker.sock` into this image — that would undo the model.
* Full `listen`/`warm`/`up` that need Podman: use path A (host binary as `gha-agent`).

Build:

```bash
bash scripts/build-agent-image.sh
```

---

## 3. Registration intelligence (not demand thrash)

Preferred steady state for a small personal allowlist:

```text
warm --prefer-repos a/r1,a/r2,a/r3
  → one retain registration per repo (paced registration-token POSTs)
  → runners online; GitHub pushes jobs
  → REUSE skips new POSTs when volume already holds .runner for that repo
```

| Mode | API cost | Use when |
|------|----------|----------|
| **warm + retain** | One paced POST per repo; then almost none | Steady CI on allowlist (recommended) |
| **listen + demand** | Gentle GETs (2–5+ min); POST only if work needs up | Recovery / scale / GPU wake |
| **user-batch ephemeral re-target** | New registration when demand switches repos | Ad-hoc many repos; still allowlist + pace |

Host-wide registration budget: `GHA_REG_MIN_GAP_SECS`, `GHA_REG_MAX_PER_HOUR`.
API pacing: `GHA_API_MIN_GAP_MS`, `GHA_API_MAX_PER_POLL`, backoff on 403/429.

---

## 4. Work containers (the endpoint)

* **Image:** Ubuntu-based snapshot with official runner + common CI tools ([packaging/Containerfile](../packaging/Containerfile)).
* **Ephemeral mode:** register with `--ephemeral`, die after one job, wipe credentials on down.
* **Retain mode:** stay registered; container can restart with `RUNNER_TOKEN=REUSE` if `.runner` is on the volume for the same `REPO_URL`.
* **GPU soft-slices:** labels + demand filters; idle down returns GPU to the host (no MIG on consumer GeForce/WSL).

The agent allocates these containers; they are not the long-lived control plane.

---

## 5. Token security & process isolation

* Reject raw `ghp_` / `github_pat_` on CLI argv (history / `ps`).
* Resolve tokens via GCM, `gh`, config `0600`, env, or masked prompt.
* Registration env file: `XDG_RUNTIME_DIR`, `0600`, shred + unlink after `up`.
* Entrypoint never logs tokens; identity fields charset-validated.
* Podman: `no-new-privileges`, non-root UID 1001 in work image, `--pull=never` on hot path.

See [SECURITY](SECURITY.md).

---

## 6. Multi-repo personal accounts

Personal accounts cannot register one user-wide runner for all repos.

* **Preferred:** `warm` fleet — one retain work endpoint per allowlisted repo; agent stays one process (or one agent unit) managing many containers.
* **Legacy / ad-hoc:** `scope=user` listen re-targets registration (ephemeral) when demand moves — higher registration cost; always set `GHA_PREFER_REPOS`.

---

## 7. Hardening roadmap (agent + model)

Already in tree or in progress:

* [x] Fail-closed validation, redaction, instance locks
* [x] API + registration pacing / budgets
* [x] REUSE retain registration; `warm` batch
* [x] Dual packaging path: host binary + micro-agent image scaffold
* [x] Release profile: LTO, strip, single codegen unit

Follow-on (security model work):

* [ ] Tighten agent image: read-only root, dropped caps, no socket when not needed
* [ ] Explicit agent subcommand / `--role agent` messaging and health endpoint (auth’d)
* [ ] Separate “thin online listener” vs “fat job” images if push + isolation need both
* [ ] Rootless-only path docs; deny docker.sock; seccomp/apparmor profiles
* [ ] Audit token scopes and prefer fine-grained PATs / short-lived credentials
* [ ] Optional: sign agent releases; SBOM for agent + work images

The binary must stay **stout**: small dependency set, `unsafe_code = forbid`, audited release path (`scripts/security-scan.sh`).

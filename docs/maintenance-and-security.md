# Maintenance and validated security scanning

This document describes the right-sized maintenance workflow, the self-hosted FOSS
security validation workflow, the `sec` branch, and the fleet-security cancel trap
fix. Scope is **gha-runner-ctl only**.

Companion: [branch-sync.md](./branch-sync.md) (owned by the branch-sync worker;
linked here even if that file lands in a parallel branch).

---

## Exit contract (every job)

| Situation | Outcome |
|-----------|---------|
| The thing it checks is broken | **FAIL** |
| The work completed successfully | **PASS** |
| There was nothing to do (empty) | **PASS** (exit 0) |
| Could not tell (API error, missing tool, crash) | **FAIL** loudly — **UNKNOWN**, never a quiet pass |

Empty and unknown are different code paths. Collapsing them is how a gate silently
stops gating.

---

## (A) Maintenance — `.github/workflows/maintenance.yml`

**Triggers:** weekly `schedule` (Mondays 08:17 UTC) + `workflow_dispatch`.

**Concurrency:** group only. **No** `cancel-in-progress` — this workflow has a
`schedule:` trigger. On self-hosted fleets, a queued scheduled run that is cancelled
by the next schedule reports `cancelled` rather than `failed`, so the job silently
never runs and nothing alerts.

### Jobs and runner tiers

| Job | `runs-on` tier | Basis |
|-----|----------------|-------|
| `rust fmt and clippy` | **large** (4 cpu / 8 GiB) | Compiles. Measured: `cargo clippy --all-targets` peaked at **1225 MiB**. Micro caps at 512 MiB → SIGKILL 137 and a multi-minute stall that looks like a hang. Job names containing `fmt` / `clippy` / `lint` silently force **micro** under the name heuristic, so the tier label is **explicit** and must not be removed. |
| `dependency staleness report` | **small** (0.5 cpu / 1 GiB) | `cargo update --dry-run` only — parses the graph, does not compile this crate. Report-only: outdated packages print and the job still exits 0. Tool failure → FAIL (unknown). |
| `stale and merged branch report` | **micro** (0.25 cpu / 512 MiB) | Pure `git` + `gh` API. No compile. Report-only; **DELETE NOTHING**. Empty stale/merged sets → PASS. API/fetch failure → FAIL (unknown). |

### What each job does

1. **rust fmt and clippy** — `cargo fmt --all -- --check` then
   `cargo clippy --all-targets -- -D warnings`. Failure means the tree is broken.
2. **dependency staleness report** — captures `cargo update --dry-run` into the
   step summary. Informational only.
3. **stale and merged branch report** — lists remote branches whose tip is older
   than 90 days (via `git for-each-ref`), and merged PR head branches that still
   exist (sample of recent closed PRs). Operators may delete by hand; the job never
   does.

---

## (B) Security validation — `.github/workflows/security-validate.yml`

**Triggers:** weekly `schedule` (Tuesdays 07:17 UTC) + `workflow_dispatch`.

**Concurrency:** group only — **no** `cancel-in-progress` (same schedule trap).

**Self-hosted FOSS only.** No SaaS (no Snyk, Codecov, CodeQL cloud, or other
vendor phone-home scanners).

### Tools, pins, and tiers

| Job | Tool | Pin | Tier | Basis |
|-----|------|-----|------|-------|
| `gitleaks validated` | [gitleaks](https://github.com/gitleaks/gitleaks) | **8.30.1** + SHA256 `551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb` | **micro** | Proven at micro on this fleet (`fleet-security.yml`). Explicit micro so a future rename cannot silently re-tier. **Critical:** Debian/Ubuntu apt ships **8.16.0**, a different incompatible scanner — always download the pinned GitHub release and verify the checksum (same pattern as `fleet-security.yml`). |
| `trivy filesystem validated` | [trivy](https://github.com/aquasecurity/trivy) | **0.72.0** + SHA256 `bbb64b9695866ce4a7a8f5c9592002c5961cab378577fa3f8a040df362b9b2ea` | **micro** | Proven at micro. Scanners: `vuln,secret,license`. Severity gate: HIGH,CRITICAL. |
| `rust advisory validate` | [cargo-audit](https://github.com/rustsec/rustsec) **0.22.2** (prebuilt musl) + [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) **0.20.2** (prebuilt musl) | checksum-pinned in the workflow | **medium** (2 cpu / 4 GiB) | **No `cargo install` compile** — prebuilt binaries only. Still runs `cargo tree` / metadata (dep graph, not a full crate compile). Explicit medium because a name containing “security” would otherwise force micro (512 MiB). If a future change switches to `cargo install`, that job **must** move to **large**. |
| `publish report to sec` | git + artifacts | n/a | **micro** | Pure git push of generated reports. No compile. |

Semgrep is **not** included: optional in the brief; keeping the set small and
honest preferred over an unvalidated SAST wall.

### Validation methodology

A wall of unvalidated advisories trains people to ignore red. Every
`cargo-audit` vulnerability is classified:

| Class | Meaning | Gate |
|-------|---------|------|
| **RUNTIME** | `cargo tree --invert <crate> --edges normal,build` shows a path | **APPLICABLE** — FAIL |
| **DEV_ONLY** | Present only on dev-dependency edges | Documented; does not fail the gate by itself |
| **NOT_IN_GRAPH** | `cargo tree --invert` finds no package | Documented; not applicable |
| **UNKNOWN** | Tree/audit parse failed or could not determine reachability | **FAIL** (unknown is not clean) |

Rules:

- Never silently drop a finding.
- If reachability cannot be determined → **UNKNOWN** → fail.
- gitleaks hits are always **APPLICABLE** (a secret is reachable by definition).
- trivy HIGH/CRITICAL filesystem hits in this tree are treated as **APPLICABLE**
  unless they are clearly a documented test fixture (called out in the report).

### Remediation output format

Every real finding must include a concrete fix line, for example:

```text
remediation: cargo update -p <crate> --precise <patched-version>
```

Or, when no patched version is listed: a link to the RustSec advisory plus the
config/code change required (`deny.toml`, replace the crate, rotate the secret).
A finding with no stated fix is not done.

### Report output

- Job summary: `$GITHUB_STEP_SUMMARY` (markdown).
- Artifacts: raw JSON from gitleaks, trivy, cargo-audit, cargo-deny, plus
  `validation.md` and `validation-summary.json`.
- On `schedule` / `workflow_dispatch`, the **publish report to sec** job commits
  the bundle under `security-reports/<timestamp>/` on the **`sec`** branch and
  refreshes `security-reports/LATEST.md`. It does **not** push to `main` or `dev`.

Direct commit to `sec` (rather than a PR) is intentional: reports are
machine-generated audit artifacts, not product code. `sec` is the security home
so feature flow on `dev`/`main` stays unblocked.

---

## (C) `sec` branch

### Creation

This repo had no `sec` branch. It was created **non-destructively** from the
current default-branch tip:

```bash
git push origin origin/main:refs/heads/sec
```

That only creates a new ref; it destroys nothing. Confirm with:

```bash
git ls-remote origin refs/heads/sec
```

### Sync policy

`sec` stays aligned with `main` by the **same non-destructive merge discipline
used for `dev`**:

- **MERGE from `main` into `sec`** — never force-push, never reset.
- Sync automation lives in `.github/workflows/branch-sync.yml` and is documented
  in [docs/branch-sync.md](./branch-sync.md) (owned by a separate worker; do not
  edit those files from the maintenance/security lane).

Security scan output targets `sec`. Opening a PR from a scan-results branch
**into** `sec` is also acceptable; product work continues to PR into `dev`.

---

## (D) fleet-security cancel trap fix

`.github/workflows/fleet-security.yml` had **both** a `schedule:` trigger and
`concurrency.cancel-in-progress: true`. On this self-hosted fleet that is the
silent-failure trap: the scheduled run queues with no runner, the next schedule
cancels it, GitHub reports `cancelled` (not `failed`), so the scan never runs and
nothing alerts.

**Fix:** remove `cancel-in-progress` (keep the concurrency group). **Nothing else**
in that file was changed — the job names `gitleaks` and
`trivy filesystem (vuln+secret+license)` are required status contexts on `main`
and must not be renamed or removed.

---

## Sizing reference (fleet facts)

| Tier | Caps |
|------|------|
| micro | 0.25 cpu / 512 MiB |
| small | 0.5 cpu / 1 GiB |
| medium | 2 cpu / 4 GiB |
| large | 4 cpu / 8 GiB |

`runs-on: [self-hosted, linux, x64, podman, <tier>]`. An explicit tier label
**takes precedence** over the job-name heuristic in `gha-runner-ctl`. Name
substrings that force micro when unlabelled include: `gitleaks`, `trivy`,
`lint`, `fmt`, `clippy` (without build/test), `security`, and others — see
fleet sizing docs / `size_for_job`.

**Undersizing is the #1 cause of false CI failures on this fleet.** Any job that
compiles must carry an explicit tier of at least **large**.

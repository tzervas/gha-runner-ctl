# Honest CI — outcomes and failure classes

**Normative for all fleet-managed repos org-wide** (not mycelium-only). Green
means real gates cleared. Missing toolchains, half-ported platforms, and broken
runners must **never** look like product success.

**Minimum showcase bar:** GitHub **profile-pinned** repos must comply first
(visitor-facing). Current pin set is tracked in the workstation pack as
`SHOWCASE_REPOS.md` / operator notes (peft-rs, gha-runner-ctl, memory-gate-rs,
tero-mcp, tg-agent-relay, mycelium-lang as of 2026-07-21). Pins change → update
that list in the same change that re-pins.

Companion: [SUPPORT_MATRIX](https://github.com/tzervas/mycelium-lang/blob/main/docs/SUPPORT_MATRIX.md)
pattern (required / experimental / planned), [FLEET_STANDARDS.md](./FLEET_STANDARDS.md).

---

## Three legitimate outcomes

| Outcome | When | Job result | Badge / gate |
|---------|------|------------|--------------|
| **PASS** | Implementation is complete for this cell **and** all required checks succeeded | success | May block merge/release |
| **FAIL** | Something that was supposed to work did not | failure | Blocks required gates; message must classify why |
| **SKIP** | Cell is intentionally not run yet (stub, planned OS, feature flag off, path filter) | skipped / neutral | Must **not** count as green for that claim |

There is **no fourth outcome** of “ran a bit, tools missing, exit 0.” That is a **lie**.

---

## Failure classes (must be distinct)

When a job **fails**, the **first actionable line** of the failing step (and a
GitHub annotation when possible) MUST name exactly one primary class:

| Class | Code | Meaning | Operator action |
|-------|------|---------|-----------------|
| **Product** | `FAIL_PRODUCT` | Code, tests, lints, security findings, contract checks — the repo under test is wrong or incomplete *as claimed* | Fix the product / tests |
| **Not implemented** | `FAIL_NOT_IMPLEMENTED` | Workflow or matrix cell claims work that is not built yet (no stub skip was used) | Either implement, or convert job to **SKIP** with an explicit stub reason — do not soft-pass |
| **Runner / image** | `FAIL_RUNNER` | Self-hosted runner, labels, registration, Podman, disk, cgroup, image pull, entrypoint — fleet host path broken | Fix fleet / `gha-runner-ctl` / image; **not** a product PR |
| **Environment / toolchain** | `FAIL_ENV` | Expected tools missing or wrong version *on a path that claims to provide them* (rustc, myc, gitleaks, trivy, uv, …) | Fix image/`prepare`, pin, or PATH; **not** “skip and green” |
| **Infrastructure / API** | `FAIL_INFRA` | GitHub API, auth, rate limit, network to registry — external dependency | Retry / tokens / backoff; do not reclassify as product |

### Why this split

- A red **product** run means “don’t ship this commit.”
- A red **runner/env** run means “don’t blame the commit; fix the machine/image.”
- A red **not implemented** run means “workflow is lying about coverage — fix the workflow.”
- A **skip** means “we are not claiming this yet.”

If you cannot tell which class failed from the log in **one screen**, the workflow is defective.

---

## How to signal (concrete)

### 1. Banner line (required on fail and on intentional skip)

```bash
# fail
echo "::error title=FAIL_ENV::rustc not on PATH after image setup (expected 1.85+ on draw-in image)"
echo "HONEST_CI class=FAIL_ENV detail=rustc_missing image=$IMAGE"
exit 1

# intentional skip (job-level if: or early step that marks skip — never exit 0 after claiming a gate)
echo "::notice title=SKIP_STUB::Windows draw-in not implemented; cell is planned-only"
echo "HONEST_CI class=SKIP_STUB detail=windows_draw_in_planned"
# Prefer job-level `if: false` / path filters / matrix exclusion over exit 0 mid-job.
exit 0   # only allowed when the *job name and matrix id* already say stub/planned and gates ignore this job
```

### 2. Preflight before product steps

Required order for install/draw-in style jobs:

1. **Runner preflight** — labels, podman, disk, `id`, image present → `FAIL_RUNNER` on miss  
2. **Toolchain preflight** — `command -v` + version pins for every tool the job will use → `FAIL_ENV` on miss  
3. **Product steps** — build/test/scan → `FAIL_PRODUCT` on miss  
4. Never run product steps if (1) or (2) failed.

### 3. Job naming

| Bad | Good |
|-----|------|
| `test` (secretly skips without cargo) | `test (linux-x64)` / `test stub (windows — planned)` |
| `security` green when trivy missing | `security/trivy` fails `FAIL_ENV` **or** separate `security/trivy (optional)` job that is **not** a required check |
| `multi-os` | only list OSes that actually run |

### 4. Required vs experimental vs planned

| Tier | Failure | Skip |
|------|---------|------|
| **required** | Any class fails the gate | Skip only via path filters that remove the job entirely — not soft success |
| **experimental** | May use `continue-on-error: true` **only** if job name contains `experimental` and summary still prints `HONEST_CI class=…` | Preferred: keep experimental out of branch protection |
| **planned** | Must not run as a green empty job | Matrix omit, or `if: false` with stub name |

`continue-on-error: true` without `experimental` / `advisory` in the **job name** is forbidden for fleet templates.

### 5. Forbidden patterns (fake green)

| Pattern | Why banned |
|---------|------------|
| `cargo test \|\| true` | Product failure hidden |
| `command -v trivy \|\| exit 0` on a required security job | Env failure hidden as pass |
| Empty job with only `echo ok` named as full CI | Fake pass |
| Matrix OS legs that never execute but show success | Multi-OS lie |
| “Always green gate” for product workflows | Allowed only for pure meta automation with **honest job names** (e.g. issue reopen), never for build/test/security |

---

## Runner vs product: operator checklist

When a run is red, read the first `HONEST_CI class=` / `::error title=FAIL_*`:

| Class | Look at |
|-------|---------|
| `FAIL_RUNNER` | `gha-runner-ctl` tick log, pool claims, `podman ps`, online runners, reg budget |
| `FAIL_ENV` | Work image (`GHA_IMAGE`), `prepare`, rustup in container, tool pins |
| `FAIL_PRODUCT` | Commit, tests, lockfiles |
| `FAIL_NOT_IMPLEMENTED` | Workflow YAML / SUPPORT_MATRIX — fix claims |
| `FAIL_INFRA` | `gh api rate_limit`, tokens, registry |

---

## Migration (fleet pack)

1. Replace “missing tool → exit 0” with **`FAIL_ENV`** on required jobs; move advisory scans to **named optional** jobs.
2. Strip multi-OS matrix lies; planned OS → omit or stub-skip, not green.
3. Add preflight steps to `fleet-ci` / draw-in templates.
4. Branch protection only on jobs that can produce `FAIL_PRODUCT` or real required env (not on advisory).

---

## Summary one-liner

**Pass only when the claimed work truly ran and cleared gates; fail loudly with a class (`PRODUCT` / `NOT_IMPLEMENTED` / `RUNNER` / `ENV` / `INFRA`); skip only when we are not claiming the work yet — never green on silence.**

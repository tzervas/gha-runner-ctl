# fleet-runner CRITICAL + HIGH CVE posture

Scan gates (publish blocking):

```bash
# Fail only when a CRITICAL/HIGH still has an available fix (FixedVersion non-empty)
trivy image --severity CRITICAL --ignore-unfixed --exit-code 1 --scanners vuln <image>
trivy image --severity HIGH --ignore-unfixed --exit-code 1 --scanners vuln <image>
```

Interpretation used by this fleet:

- **Fail publish** if any CRITICAL or HIGH finding has a non-empty `FixedVersion`
  (a fix exists and we have not applied it in the image). Use `--ignore-unfixed`
  so residual unfixed CVEs do not fail the gate by themselves.
- **Allow publish** when residual findings remain **only** because
  `FixedVersion` is empty (no Debian/upstream package fix yet). Those residuals
  are documented below. **Do not claim CRITICAL/HIGH-clear** while residuals exist.

## Rust-first tooling policy (HIGH pass, `0.1.2` / `sec-20260728-high`)

Owner preference for this fleet:

1. Prefer **Rust release binaries** for fleet utilities (ripgrep, fd). Do **not**
   solve CVEs by expanding `npm`/`node_modules`.
2. Node exists only because actions/runner ships it — do not `npm install` as a
   primary strategy. Allowed: drop unused trees, or a **one-line** checksummed
   tarball version bump inside the official runner tree when a HIGH is otherwise
   unblockable.
3. gitleaks (Go): multi-stage rebuild with **latest stable Go image**
   (`golang:1.26-bookworm` / Go **1.26.5**), not old 1.24.
4. .NET HIGH in runner: try latest actions/runner first (still **2.336.0**); if
   still vulnerable, install **latest stable .NET 8 runtime** from Microsoft
   (official runtime tarball = same bits as `dotnet-runtime-8.0` **8.0.29**)
   **over** the bundled self-contained runtime. No npm.
5. Optional: latest stable **ripgrep** + **fd** musl static bins (checksum from
   GitHub releases).

Security patch tags: `sec-20260728-high`, semver `0.1.2` (patch over `0.1.1`),
floating `dev`. Prior CRITICAL-only tag: `sec-20260728` / `0.1.1`.

| Image | GHCR names |
| --- | --- |
| base | `ghcr.io/tzervas/fleet-runner-base`, `ghcr.io/tzervas/gha-runner-ctl/runner-base` |
| shell | `ghcr.io/tzervas/fleet-runner-shell`, `ghcr.io/tzervas/gha-runner-ctl/runner-shell` |
| python | `ghcr.io/tzervas/fleet-runner-python`, `ghcr.io/tzervas/gha-runner-ctl/runner-python` |

## Hardening applied in `0.1.2` / `sec-20260728-high`

| Control | Detail |
| --- | --- |
| Base OS | `debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818` (unchanged) |
| `apt-get upgrade -y` | Retained on every base build |
| actions/runner | Still **2.336.0** (latest as of 2026-07-28) |
| gitleaks | **v8.30.1** rebuilt with `golang:1.26-bookworm@sha256:1ecb7edf…eb651` (Go **1.26.5**) + `golang.org/x/crypto@v0.54.0` |
| .NET runtime | Overlay Microsoft `dotnet-runtime` **8.0.29** (`dba346c5…fba81`) onto self-contained runner bin; rewrite runtime version pins only |
| node20 tree | **Removed** (`FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true`) — avoids npm expansion for legacy HIGH surface |
| node24 npm | Checksummed tarball bumps only: `tar` **7.5.19**, `brace-expansion` **5.0.8**, `undici` **6.27.0** (+ tar@7 peers). **No `npm install`** |
| shfmt (shell class) | **v3.13.1** rebuilt with `golang:1.26-bookworm` (Go **1.26.5**). Upstream release asset was Go 1.26.1 (12 HIGH stdlib FixedVersions) |
| ripgrep | **15.2.0** musl static (`33e15bcf…149c`) → `/usr/local/bin/rg` |
| fd | **10.4.2** musl static (`e3257d48…bdde`) → `/usr/local/bin/fd` |

## Hardening retained from `0.1.1` / `sec-20260728` (CRITICAL)

| Control | Detail |
| --- | --- |
| node-tar CRITICAL | Still patched to **7.5.19** (now only on node24 after node20 drop) |
| gitleaks CRITICAL stdlib | Superseded by Go 1.26.5 rebuild (also clears HIGH stdlib) |

## Baseline vs hardened (fleet-runner-base) — HIGH fixable

| Metric | `0.1.1` / `sec-20260728` | `0.1.2` / `sec-20260728-high` |
| --- | ---: | ---: |
| HIGH total (base) | 133 | 75 (all FixedVersion empty) |
| HIGH with FixedVersion (base) | **58** | **0** (gate pass) |
| HIGH with FixedVersion (shell) | (+ shfmt Go 1.26.1 = 12) | **0** (shfmt rebuilt) |
| HIGH with FixedVersion (python) | inherits base | **0** (gate pass) |
| HIGH primary clusters (pre) | gitleaks stdlib+x/crypto (22), .NET 8.0.28×5 CVEs×5 deps (25), node npm (11) | cleared |

### `0.1.1` fixable HIGHs (for history)

| Package / path | CVEs (summary) | Installed | FixedVersion |
| --- | --- | --- | --- |
| `stdlib` (gitleaks gobinary) | CVE-2026-25679 … 42504 (12) | v1.24.13 | 1.25.x / **1.26.5** |
| `golang.org/x/crypto` (gitleaks) | CVE-2025-47913, CVE-2026-39828…46597 (10) | v0.35.0 | 0.43.0 / **0.52.0+** |
| `Microsoft.NETCore.App.Runtime.linux-x64` | CVE-2026-47302, 50524, 50528, 50651, 57108 | 8.0.28 | **8.0.29** |
| `brace-expansion` / `cross-spawn` / `glob` / `minimatch` / `sigstore` / `undici` | various | (node20+node24 npm) | patch/minor (or drop node20) |

## CRITICAL gate (still pass)

| Metric | `0.1.1` | `0.1.2` |
| --- | ---: | ---: |
| CRITICAL with FixedVersion | **0** | **0** |
| CRITICAL FixedVersion empty | 18 residual | 18 residual (perl ×16, zlib ×1, sqlite ×1) |

## Residual CRITICAL — FixedVersion empty (no claim of zero)

These are accepted residual risk until Debian (or a base-image refresh that drops
the package) publishes a fix. They are **not** treated as gate failures.

**Re-scan evidence (2026-07-28):** `trivy image ghcr.io/tzervas/fleet-runner-base:sec-20260728-high --severity CRITICAL --format json` → **18** CRITICAL, all FixedVersion empty, **0** fixable. Full investigation: [`docs/sec-unfixed-critical.md`](../docs/sec-unfixed-critical.md) (branch `sec/unfixed-perl-bookworm`, issue #74).

### perl family (bookworm)

| Packages | Installed | CVEs |
| --- | --- | --- |
| `perl`, `perl-base`, `perl-modules-5.36`, `libperl5.36` | `5.36.0-7+deb12u3` | CVE-2026-13221, CVE-2026-42496, CVE-2026-57433, CVE-2026-8376 (×4 pkgs = 16 rows) |

| Path | Result (2026-07-28) |
| --- | --- |
| bookworm-security newer? | **No** — security suite only has older `…+deb12u2` |
| trixie fixed? | **No** for these CVEs (`5.40.1-6` still vulnerable; some fixes only forky/sid; CVE-2026-13221 unfixed even on sid) |
| removable? | **No** — `perl-base` is Essential; bookworm `git` Depends: perl; dry-run remove drops git |

**Why present:** `git` (hard dep) + Essential `perl-base`. `installdependencies.sh` does **not** require perl, but the fleet base requires git.

**Mitigation:** keep `apt-get upgrade -y`; re-scan on each rebuild; track Debian security tracker. **Do not** mixed-suite pin trixie/sid perl onto bookworm.

### zlib

| Package | Installed | CVE | Trivy status |
| --- | --- | --- | --- |
| `zlib1g` | `1:1.2.13.dfsg-1` | CVE-2023-45853 | `will_not_fix` on bookworm |

| Path | Result |
| --- | --- |
| bookworm-security newer? | **No** |
| trixie fixed? | **Yes** (`1:1.3.dfsg+really1.3.1-1`) — Debian fixed on trixie; bookworm ignored (minizip not shipped from src:zlib) |
| removable? | **No** — required by `installdependencies.sh` (`libkrb5-3 zlib1g`) and dpkg/curl/git/python |

### sqlite (bookworm)

| Package | Installed | CVE | Notes |
| --- | --- | --- | --- |
| `libsqlite3-0` | `3.40.1-2+deb12u2` | CVE-2025-7458 | bookworm `<no-dsa>`; FixedVersion empty |

| Path | Result |
| --- | --- |
| bookworm-security newer? | **No** |
| trixie fixed? | **Yes** (`3.46.1-7+deb13u1`) |
| removable? | **No** without dropping `libpython3.11-stdlib` / standards-gate `python3-yaml` stack |

### Recommendation (issue #74)

**Stay on bookworm residual.** A trixie base probe cleared zlib+sqlite but left the **16-row perl CRITICAL cluster** and introduced **openssh-client** CRITICAL (net ~17 residual). Suite move is not justified solely by these residuals until perl is fixed on the target suite. No safe apt pin or package removal lands on this train.

## Residual HIGH — FixedVersion empty

Document any HIGH with empty FixedVersion discovered after the harden pass in
the scan log for the `sec-20260728-high` image. As of the harden build, the
`--ignore-unfixed` HIGH gate is expected to be **clean** (0 fixable). Sample
unfixed HIGH families (curl, expat, util-linux, …) are tracked on issue #74 for
a separate `sec/unfixed-curl-bookworm` investigation — not cleared by this
CRITICAL residual write-up.

## Gate commands

```bash
# Fail if any CRITICAL still has a fix available
trivy image --severity CRITICAL --ignore-unfixed --exit-code 1 --scanners vuln \
  ghcr.io/tzervas/fleet-runner-base:sec-20260728-high

# Fail if any HIGH still has a fix available
trivy image --severity HIGH --ignore-unfixed --exit-code 1 --scanners vuln \
  ghcr.io/tzervas/fleet-runner-base:sec-20260728-high

# Secret scan must remain clean (no secrets baked into layers)
trivy image --scanners secret --exit-code 1 \
  ghcr.io/tzervas/fleet-runner-base:sec-20260728-high
```

## What we deliberately do **not** claim

- We do **not** claim CRITICAL count is zero while perl/zlib/sqlite residuals remain.
- We do **not** claim the upstream gitleaks or actions/runner release assets alone
  clear go-stdlib / x/crypto / node-tar / .NET findings without the rebuild and
  overlay steps above.
- We do **not** expand npm as a CVE strategy; node20 is dropped rather than
  patched with `npm install`.
- Class images (`shell`, `python`) inherit base residuals; additional class
  toolchains may add their own findings and must be scanned independently.
- We do **not** mix Debian suites or strip Essential packages to paper over
  FixedVersion-empty findings (see `docs/sec-unfixed-critical.md`).

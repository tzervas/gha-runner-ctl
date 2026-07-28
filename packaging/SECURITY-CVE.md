# fleet-runner CRITICAL CVE posture

Scan gate (publish blocking):

```bash
# Fail only when a CRITICAL still has an available fix (FixedVersion non-empty)
trivy image --severity CRITICAL --ignore-unfixed --exit-code 1 --scanners vuln <image>
```

Interpretation used by this fleet:

- **Fail publish** if any CRITICAL finding has a non-empty `FixedVersion` (a fix
  exists and we have not applied it in the image). Use `--ignore-unfixed` so
  residual unfixed CVEs do not fail the gate by themselves.
- **Allow publish** when residual CRITICALs remain **only** because
  `FixedVersion` is empty (no Debian/upstream package fix yet). Those residuals
  are documented below. **Do not claim CRITICAL-clear** while residuals exist.

Security patch tags: `sec-20260728`, semver `0.1.1` (patch over `0.1.0`), floating
`dev`. Images:

| Image | GHCR names |
| --- | --- |
| base | `ghcr.io/tzervas/fleet-runner-base`, `ghcr.io/tzervas/gha-runner-ctl/runner-base` |
| shell | `ghcr.io/tzervas/fleet-runner-shell`, `ghcr.io/tzervas/gha-runner-ctl/runner-shell` |
| python | `ghcr.io/tzervas/fleet-runner-python`, `ghcr.io/tzervas/gha-runner-ctl/runner-python` |

## Hardening applied in `0.1.1` / `sec-20260728`

| Control | Detail |
| --- | --- |
| Base OS | `debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818` (re-resolved 2026-07-28; unchanged) |
| `apt-get upgrade -y` | Retained on every base build; pulls all available bookworm security updates |
| actions/runner | Bumped **2.335.1 → 2.336.0** (linux-x64 SHA `04cf0be1…5d5d`) |
| gitleaks | Still **v8.30.1** (latest). Upstream release binary was Go **1.24.11** (stdlib **CVE-2025-68121**, FixedVersion 1.24.13). Rebuilt from tag `v8.30.1` with `golang:1.24-bookworm@sha256:1a6d4452…77ac` (Go **1.24.13**) |
| node-tar | actions/runner 2.336.0 still shipped npm `tar` **6.2.1** (node20) / **7.5.15** (node24). Patched both trees to **7.5.19** (+ tar@7 peer deps) for **CVE-2026-59873** |

## Baseline vs hardened (fleet-runner-base)

| Metric | `0.1.0` / pre-harden | `0.1.1` / `sec-20260728` |
| --- | ---: | ---: |
| CRITICAL total | 20 | **18** |
| CRITICAL with FixedVersion | 3 (node-tar ×2, gitleaks stdlib) | **0** (gate pass) |
| CRITICAL FixedVersion empty | 17 (perl family ×16, zlib ×1) | **18** residual (perl ×16, zlib ×1, sqlite ×1) |

`0.1.0` fixable CRITICALs (for history):

| Package / path | CVE | Installed | FixedVersion |
| --- | --- | --- | --- |
| `tar` (node20 npm) | CVE-2026-59873 | 6.2.1 | 7.5.19 |
| `tar` (node24 npm) | CVE-2026-59873 | 7.5.13–7.5.15 | 7.5.19 |
| `stdlib` (gitleaks gobinary) | CVE-2025-68121 | v1.24.11 | 1.24.13, 1.25.7, … |

## Residual CRITICAL — FixedVersion empty (no claim of zero)

These are accepted residual risk until Debian (or a base-image refresh that drops
the package) publishes a fix. They are **not** treated as gate failures.

### perl family (bookworm)

No Debian fixed version as of 2026-07-28. Trivy reports the same CVEs on each
perl-related package:

| Packages | CVEs |
| --- | --- |
| `perl`, `perl-base`, `perl-modules-5.36`, `libperl5.36` | CVE-2026-13221, CVE-2026-42496, CVE-2026-57433, CVE-2026-8376 |

**Why present:** `git` and other bookworm packages pull perl as a dependency of
the runner base. Removing perl would break `git` packaging on Debian.

**Mitigation:** keep `apt-get upgrade -y`; re-scan on each rebuild; track Debian
security tracker for bookworm perl updates.

### zlib

| Package | CVE | Notes |
| --- | --- | --- |
| `zlib1g` | CVE-2023-45853 | FixedVersion empty on bookworm; widely flagged residual |

**Mitigation:** same as perl — upgrade-at-build + re-scan. No local backport in
this image set.

### sqlite (bookworm)

| Package | CVE | Notes |
| --- | --- | --- |
| `libsqlite3-0` | CVE-2025-7458 | FixedVersion empty on bookworm as of 2026-07-28 |

**Why present:** pulled as a dependency of the runner base stack (python3/git
ecosystem). No Debian fixed version yet.

**Mitigation:** keep `apt-get upgrade -y`; re-scan on rebuild.

## Gate commands

```bash
# Fail if any CRITICAL still has a fix available
trivy image --severity CRITICAL --ignore-unfixed --exit-code 1 --scanners vuln \
  ghcr.io/tzervas/fleet-runner-base:sec-20260728

# Secret scan must remain clean (no secrets baked into layers)
trivy image --scanners secret --exit-code 1 \
  ghcr.io/tzervas/fleet-runner-base:sec-20260728
```

## What we deliberately do **not** claim

- We do **not** claim CRITICAL count is zero while perl/zlib residuals remain.
- We do **not** claim the upstream gitleaks or actions/runner release assets alone
  clear go-stdlib / node-tar findings without the rebuild and patch steps above.
- Class images (`shell`, `python`) inherit base residuals; additional class
  toolchains may add their own findings and must be scanned independently.

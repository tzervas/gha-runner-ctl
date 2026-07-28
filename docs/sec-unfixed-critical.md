# Residual unfixed CRITICAL CVEs — investigation (`sec/unfixed-perl-bookworm`)

**Issue:** https://github.com/tzervas/gha-runner-ctl/issues/74  
**Branch:** `sec/unfixed-perl-bookworm` (from `origin/dev`)  
**Image under test:** `ghcr.io/tzervas/fleet-runner-base:sec-20260728-high`  
**Scan date:** 2026-07-28  
**Gate policy (unchanged):** fail only when `FixedVersion` is non-empty (`trivy --ignore-unfixed`).

## 1. Residual CRITICAL inventory (FixedVersion empty)

Command:

```bash
trivy image ghcr.io/tzervas/fleet-runner-base:sec-20260728-high \
  --severity CRITICAL --format json
```

| Package | Installed | CVEs | Trivy status | Count |
| --- | --- | --- | --- | ---: |
| `perl` | `5.36.0-7+deb12u3` | CVE-2026-13221, CVE-2026-42496, CVE-2026-57433, CVE-2026-8376 | affected / fix_deferred | 4 |
| `perl-base` | same | same | same | 4 |
| `perl-modules-5.36` | same | same | same | 4 |
| `libperl5.36` | same | same | same | 4 |
| `zlib1g` | `1:1.2.13.dfsg-1` | CVE-2023-45853 | **will_not_fix** | 1 |
| `libsqlite3-0` | `3.40.1-2+deb12u2` | CVE-2025-7458 | affected | 1 |
| **Total** | | | | **18** |

CRITICAL with non-empty `FixedVersion`: **0** (publish gate still green).

## 2. Per-package investigation

### 2a. perl family

| Question | Finding |
| --- | --- |
| **bookworm-security newer?** | **No.** Candidate is `5.36.0-7+deb12u3` from `bookworm/main`. Security suite only has older `5.36.0-7+deb12u2`. No apt pin can clear these CVEs on bookworm today. |
| **trixie has a fix?** | **Partial / mostly no.** madison: trixie ships `perl 5.40.1-6`. Debian security tracker (2026-07-28): all four CVEs still **vulnerable on trixie**. `CVE-2026-57433` / `CVE-2026-8376` fixed only from **forky** `5.40.1-8` / **sid** `5.42.2-3`. `CVE-2026-13221` still unfixed even on sid. `CVE-2026-42496` is `fix_deferred` / postponed on bookworm and trixie. |
| **Can perl be removed?** | **No (not safely).** (1) Debian marks `perl-base` as **Essential**. (2) Bookworm `git` hard-`Depends: perl` (+ `liberror-perl`). Removing perl dry-run removes `git` + whole perl family. (3) `actions/runner` `bin/installdependencies.sh` does **not** install perl — but the fleet base **requires git** for checkout / work. Hundreds of `/usr/lib/git-core/*` scripts reference perl. |
| **Mixed-suite pin (trixie/sid perl on bookworm)?** | **Rejected.** Perl is ABI-coupled (`libperl5.36` vs `libperl5.40`); Essential package swaps across suites break dpkg/debconf. Half-measure per issue policy. |

**Live reverse-deps (image):** `git`, `debconf`, `adduser`, and the perl package set itself.

### 2b. zlib (`zlib1g`)

| Question | Finding |
| --- | --- |
| **bookworm-security newer?** | **No.** Only `1:1.2.13.dfsg-1` on bookworm. |
| **trixie has a fix?** | **Yes.** trixie `1:1.3.dfsg+really1.3.1-1(+b1)`; tracker marks CVE-2023-45853 **fixed** on trixie/sid. On bookworm Debian sets status **ignored** / Trivy `will_not_fix` (contrib minizip path; src:zlib does not ship the vulnerable minizip binary on bookworm). |
| **Can zlib be removed?** | **No.** Required by `installdependencies.sh` (`apt-get install -y libkrb5-3 zlib1g` on Debian) and by `dpkg`, `curl`, `git`, `python3.11-minimal`, `util-linux`, `sudo`, apt, etc. |

### 2c. sqlite (`libsqlite3-0`)

| Question | Finding |
| --- | --- |
| **bookworm-security newer?** | **No.** `3.40.1-2+deb12u2` is current; no security-suite bump. Tracker: bookworm `<no-dsa>` (minor). |
| **trixie has a fix?** | **Yes.** `3.46.1-7+deb13u1` marked fixed for CVE-2025-7458. |
| **Can sqlite be removed?** | **No without breaking the standards gate.** Reverse-depends: `libpython3.11-stdlib`. Fleet base installs `python3-yaml` so `standards_check.py` can `import yaml` (and stdlib pulls `_sqlite3` → `libsqlite3-0`). Confirmed in-image: `python3 -c 'import sqlite3'` links `libsqlite3.so.0`. |

## 3. Empirical: move base to trixie?

Probed `debian:trixie-slim` + `perl` + `zlib1g` + `libsqlite3-0` + `curl` + `git` + `python3-yaml`, then Trivy CRITICAL:

| Residual family on trixie | Count | Notes |
| --- | ---: | --- |
| perl family (same 4 CVEs × 4 pkgs) | 16 | **Not cleared** |
| zlib | 0 | cleared |
| sqlite | 0 | cleared |
| **openssh-client** (new) | 1 | CVE-2026-60002 FixedVersion empty |
| **Net residual CRITICAL** | **17** | worse or equal vs bookworm’s 18 for the three families; adds OpenSSH; still 16 perl |

Moving the runner base to trixie is a **large blast radius** (glibc, openssl, python 3.13 vs 3.11, actions/runner deps, class images) and **does not fix the perl CRITICAL cluster** that dominates the residual count.

Moving to **sid/forky** only for perl is operationally unacceptable for fleet runners.

## 4. Options considered and rejected

| Option | Verdict |
| --- | --- |
| `apt-get upgrade` / re-pin bookworm-security | Already applied; no newer packages |
| Remove perl after `installdependencies.sh` | Breaks Essential + git |
| Replace Debian `git` with static/musl git to drop perl | Possible research later; not a “safe one-line” fix; loses distro security updates for git |
| Mixed-suite apt pin of trixie zlib/sqlite/perl | Unsafe ABI/deps; policy forbids half-measures |
| Rebuild base on trixie now | Clears zlib+sqlite only; perl remains; new CRITICAL surface; high migration cost |

## 5. Recommendation

**Stay on Debian bookworm residual.** Continue shipping with:

```bash
trivy image --severity CRITICAL --ignore-unfixed --exit-code 1 --scanners vuln IMAGE
trivy image --severity HIGH --ignore-unfixed --exit-code 1 --scanners vuln IMAGE
```

Document residual risk in `packaging/SECURITY-CVE.md`. Do **not** claim CRITICAL-zero.

### Revisit triggers

1. Debian bookworm-security (or point release) publishes fixed `perl` / `zlib1g` / `libsqlite3-0` with non-empty Trivy `FixedVersion` → rebuild base, prove gate, bump sec tag.
2. Perl CVEs fixed on **trixie** *and* a planned suite migration is funded (class images + runner + python pin review).
3. Optional later spike: vendor a perl-free git binary **if** checkout workflows never need perl git helpers (must be proven, not assumed).

### What not to do

- Do not invent FixedVersions or disable scanners to greenwash residuals.
- Do not expand npm to “fix” OS CVEs.
- Do not mix bookworm + trixie packages via apt preferences for Essential stacks.

## 6. Implementation on this branch

No Containerfile change: **no safe removal or apt pin exists** that clears residual CRITICAL without a deliberate suite migration.

This document + `packaging/SECURITY-CVE.md` update only.

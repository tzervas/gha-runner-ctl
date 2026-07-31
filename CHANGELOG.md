## Unreleased

### Fixed — pool reclaimed freshly-spawned workers before GitHub could dispatch a job (#127)

The dynamic-pool scale planner (`plan_scale` in `src/pool.rs`) reclaimed any
`running && !busy` ephemeral worker immediately under the `post-job-exit`
scale-in path, with no age check. `busy=0` is ambiguous: it's true both for a
worker whose job genuinely finished *and* for one that was never dispatched a
job at all. Live evidence: workers were torn down ~42s after `up` — well
inside the time GitHub needs to schedule a job onto a newly-registered
runner — leaving queue depth flat at 3 across 22+ minutes with zero jobs
claimed.

Fixed by adding a grace window: a `running && !busy` worker is now reclaimable
only when it carries a positive completion signal, or has been running at
least `GHA_POOL_SPAWN_GRACE_SECS` (default `90`, chosen with margin over the
40-70s observed churn) since spawn. Age comes from the container's actual
start time (`podman inspect .State.StartedAt`); unknown age fails closed
(never reclaimed without proof). A worker whose job genuinely finishes
quickly is unaffected — in ephemeral mode the runner process exits as the
container's PID 1, the container stops, and the existing `reap_pool_workers`
sweep (every tick, independent of this grace window) reclaims it promptly, so
completed workers never leak capacity waiting out the window.

Also extended the wrong-repo preempt fallback (used when `ephemeral_post_job_exit`
is off) with the same eligibility gate, and made grace-protected idle workers
count as covering demand on their own repo so the planner doesn't double-spawn
while a freshly-spawned worker is still waiting to be dispatched.

Tests added in `src/pool.rs` (`issue127_*`) reproduce the churn against
pre-fix code — reverting `post_job_exit_eligible` to unconditional `true`
(the prior behavior) fails 3 of the 5 new tests, all in the "not reclaimed
within grace window" direction; the other 2 (prompt reclaim of a genuinely
completed worker, and eventual reclaim once grace expires) continue to pass
either way, as expected, since pre-fix code over-reclaims rather than
under-reclaims.

Note on PR #97 (`feat/retain-mode-worker-reuse`, open/draft): that PR ties
`ephemeral_post_job_exit` to `GHA_MODE` (currently hardcoded `true` at the
`listen()` call site) so `GHA_MODE=retain` workers aren't swept by this same
mechanism. It is a real, independent fix for a different bug — retain-mode
idle workers being torn down between jobs — and does not touch the grace/
eligibility question at all: with or without #97, a default (ephemeral-mode)
worker hits `ephemeral_post_job_exit=true` and needs this grace window. This
change does not modify or supersede #97's call site; the two are additive and
can land in either order.

### Changed — rebalance per-tier memory grants and lower the sizing catch-all (memory-bound pool)

Real controller output under load showed the pool memory-bound, not CPU-bound:
`skip_cap` climbing while `free_left=2.00c/0MiB` — cores still free, memory at
zero. Two compounding causes, both fixed here:

1. **`resources_for_tier()` over-granted memory per tier.** `micro` 2g→1g,
   `small` 4g→2g, `medium` 8g→4g, `large` 24g→16g, `xlarge` 40g→28g (`gpu`
   unchanged at 16g). CPU is left untouched — cores were measured idle, not the
   bottleneck. At the 114 GiB pool cap this roughly *doubles* concurrency
   (`medium` 14→28 workers, `large` 4→7 workers). The OOM history is
   respected: the original exit-137 kills were `cargo test`/`cargo build
   --release` on the OLD `large` tier's **4 GiB**; the new `large` (16 GiB) is
   still 4x that headroom, not a return to the failing configuration.
2. **`size_for_job()`'s catch-all fell through to `Medium` (now 4g).** Traced
   concretely: `quadlet-generate`, a trivial generator job, matched none of
   the classifier's branches and silently landed on the 8g-then-4g default —
   8 GiB for a job that needs a fraction of that. The catch-all now defaults
   to `Small` instead of `Medium` (heavy work is already caught explicitly by
   the cargo/build/release/xlarge branches above the catch-all, so lowering
   the fallback does not put compiles at risk), and observed light job names
   (`quadlet-generate`, `capture-diff`, `registry-check`, `secret-keymap`,
   `policy-check`, `adversarial`, `yamllint`, `shellcheck`, `notify`) are now
   classified `Micro` directly instead of relying on the fallback at all.

Updated every place that mirrors these numbers: `src/pool.rs` unit tests,
`docs/DYNAMIC_POOL.md`, and `mycelium-port/gha_pool.myc`'s hand-encoded
differential oracle (`tier_cpus_milli`/`tier_mem_mib` binary literals +
`check_all` assertions).

**Follow-up (not implemented here):** the real fix is genuinely dynamic
sizing — cores, RAM, and disk from each job's actual measured usage (peak
RSS/CPU from cgroup stats at reap time, persisted per `(repo, job_name)`,
falling back to the name heuristic only when there's no history) — which
supersedes name-based tiers entirely. Podman also supports per-container disk
limits (`--storage-opt size=`) that the controller does not currently set.
Also unaddressed: the Medium-default keyword list matches the bare substring
`"ci"`, which fires on any job name containing that fragment, not just CI
jobs; left as-is pending a full inventory of job names across every
consuming repo.

### Added — GitHub App installation-token authentication as a first-class CLI feature (opt-in, closes #41)

`listen` re-scans every `GHA_PRIORITY_REPOS` repo every tick — measured on the homelab
instance at ~4,800 GETs/hour against a classic PAT's 5,000/hour cap, which is why
`listen: list_demand_jobs: budget exhausted mid-scan` fired on nearly every tick.
`--app-id`/`GHA_APP_ID` + `--app-private-key`/`GHA_APP_PRIVATE_KEY` mint short-lived
installation tokens instead — measured 12,500 requests/hour on this fleet's installation
(2.5x the PAT budget; installation limits scale with installation size, never a
hardcoded figure — check the live number with `doctor` or `GET /rate_limit`). Purely
additive: absent App flags/env fall back to the existing `GH_TOKEN`/PAT discovery
unchanged, so existing deployments need zero config change; *partially*-set App config
is now a hard error rather than a silent fall-back, so a typo can't quietly masquerade
as a working PAT setup. RS256 JWT signing shells out to `openssl` (already required on
the host) instead of adding a crate dependency.

Three additions beyond the original opt-in mint:

- **Real CLI parameters.** `--app-id`, `--app-installation-id`, `--app-private-key`
  (with `GHA_APP_ID`/`GHA_APP_INSTALLATION_ID`/`GHA_APP_PRIVATE_KEY` env, flag-over-env
  precedence, all visible in `--help`) — matching the `#[arg(long, env = "...", global =
  true)]` convention every other option in this tool already follows, instead of the
  env-var-only mechanism this started as.
- **Installation auto-discovery.** `--app-installation-id` is now optional: omitted, it's
  resolved via `GET /app/installations`, preferring an owner-matched installation, then
  the sole installation, then failing with every candidate `id`/account listed (zero
  installations names the install URL) — removing a manual copy-paste-prone lookup step.
- **`doctor` subcommand.** Reports the active auth path, App identity/installation/
  granted permissions (checked against the documented set), and the live
  `GET /rate_limit` budget — all without ever printing a token, JWT, or key. Every
  failure names its fix.
- **Vault-integrated key handling.** `--app-private-key` accepts `secret:<group>/<key>`
  (shells out to the existing `secret get` — never reimplements SOPS/age), `file:<path>`,
  or a bare path. Vault content is auto-detected as raw or base64-encoded PEM. Inline PEM
  content is refused on both the flag and the env var (and now also blocked at the argv
  level by `prevent_raw_token_args`, alongside raw PAT patterns). Key files (including
  the vault form's tmpfs-materialised copy) must be `0600`/`0400` or the run refuses with
  the offending mode and a `chmod 600` fix; the vault form's temp file is shredded
  immediately after each signing operation, including on error paths.

See `src/appauth.rs` and [docs/GITHUB_APP_AUTH.md](docs/GITHUB_APP_AUTH.md) for the full
setup steps (App permissions, installation target, key storage) and sample `doctor`
output.

### Changed — pool tier resource grants sized for the actual homelab host, not a laptop

Controller pool logs showed ~39 of 48 pool cores sitting idle immediately after
scale-out (`free_left=38.75c/76288MiB`) while CI jobs crawled, on a host with
56 cores / 125 GB RAM and a load average around 4.58 (roughly 8% utilised).
The cause: `resources_for_tier()` in `src/pool.rs` granted each job a sliver of
the machine — `large` (cargo test/release) got 4 of 56 cores; `micro` lint jobs
got a quarter of one core.

| Tier | Old | New |
|------|-----|-----|
| micro | 0.25c / 512m | 1c / 2g |
| small | 0.5c / 1g | 2c / 4g |
| medium | 2c / 4g | 4c / 8g |
| large | 4c / 8g | 12c / 24g |
| xlarge | 8c / 16g | 20c / 40g |
| gpu | 4c / 8g | 8c / 16g |

`fit_to_budget` still shrinks a job's grant toward the free remainder (floor
0.25c/256 MiB) when the pool is tight, so smaller hosts degrade gracefully
instead of failing outright — these are *preferred* sizes, not hard minimums.

Tests and docs that asserted the old numbers (`src/pool.rs` unit tests,
`docs/DYNAMIC_POOL.md`, the `gha_pool` Mycelium port's differential oracle in
`mycelium-port/gha_pool.myc`) were updated to match.

**Deployment required — this change alone does nothing.** The deployed
`gha-runner-ctl` binary on both the homelab and WSL controllers must be
rebuilt from this source and redeployed before pool workers see the new
grants; the currently-running binary is already stale in the other direction
(`large` grants 4c/4g in production vs. 4c/8g already in pre-change source).
Restart the controller only when the pool is idle — per issue #95, restarting
while jobs are in flight kills them.

Pool caps should be widened alongside this change (not applied here — homelab
env-file edit is a separate follow-up): current live caps are
`GHA_POOL_CPUS=48` / `GHA_POOL_MEMORY=85g`. Four concurrent `large` jobs alone
now want 48c/96g, so at the new tier sizes memory caps the pool before CPU
does. Recommended: `GHA_POOL_CPUS=48→52`, `GHA_POOL_MEMORY=85g→100g`, leaving
~4 cores / ~25 GB headroom for the host on the 56c/125 GB box.

### Fixed — `GHA_MODE=retain` worker reuse was silently defeated by a hardcoded flag

The dynamic pool's scale-decision call site (`listen()` in `src/lib.rs`) hardcoded
`ephemeral_post_job_exit: true` on every tick, independent of `cli.mode`. In
`src/pool.rs`, that flag makes an idle-but-registered worker (a) not count as
covering demand and (b) get reclaimed on the very next tick — so even with
`GHA_MODE=retain` set, a retained worker was torn down before it could pick up
a second job. Retain mode registered once, then behaved exactly like ephemeral
mode anyway. The flag is now derived from `effective_ephemeral(&cli)`, so
retain's idle workers are left alone (subject to the existing wrong-repo
preempt fallback and the new bounded-retirement window below) instead of being
swept every tick.

**Default mode is unchanged** (`GHA_MODE` still defaults to `ephemeral`) — this
fix is inert until an instance is explicitly opted into `GHA_MODE=retain`.

### Added — bounded retain lifetime (`GHA_RETAIN_MAX_AGE_SECS`, `GHA_RETAIN_MAX_JOBS`)

Clarifying the credential model first: the registration token minted for
`config.sh` is single-use and expires in ~1 hour if unused, but once
`config.sh` succeeds the runner holds its own durable credentials and does not
need another token to keep serving jobs — retained runners are **not** limited
to a 1-hour lifetime by anything credential-related.

What bounded retirement *is* for: workspace hygiene and drift control. A
long-lived retained container accumulates `_work` directory state and job
history, so retirement is now capped by two independent, env-tunable bounds:

- `GHA_RETAIN_MAX_AGE_SECS` (default `3000`, i.e. 50 minutes) — wall-clock age
  of the registration since it was last freshly minted.
- `GHA_RETAIN_MAX_JOBS` (default `25`) — number of times the registration has
  been reused (i.e. how many container restarts have ridden the same
  registration) since it was last freshly minted.

The on-disk retain marker (`gha-runner-ctl-retain-{container}-{user}.ok`) now
records a creation timestamp and a reuse counter alongside the target repo URL,
instead of just the URL. Once either bound is exceeded, `volume_has_runner_config()`
returns `false` and `up()` falls back to minting a fresh registration token, as
if nothing had been retained. **Backward compatible:** a marker written by a
prior build (bare URL, no recorded age) parses as unknown age and is treated
as not-reusable — the safe direction — rather than assumed fresh.

## v0.3.0 (2026-07-25)

### Fixed — release automation: a crates.io failure no longer blocks the release

`v0.3.0` was merged to `main` on 2026-07-24 and **never shipped**. Both
`release-on-merge` runs failed at `cargo publish` (exit 101), and because the
publish ran *before* tagging and was fatal, the `v0.3.0` tag and GitHub release
were never created. `main` carried an untagged, unreleased 0.3.0 while the
latest release stayed `v0.2.11`.

- **Reordered:** tag + GitHub release now run **before** the crates.io publish.
  A GitHub Release records what shipped from this repository; a registry
  publication is a separate artifact in a separate system with its own failure
  domain. One must not be able to take out the other.
- **Publish is no longer gated on the tag.** It relies on `cargo publish` being
  idempotent ("already exists" -> success), so a publish that failed for a
  registry-side reason is retried by simply re-running the workflow. Under the
  old gating, once the tag existed the publish would have been skipped forever.
- **Failure is still loud:** a failed publish still fails the job and now writes
  an actionable job summary naming the likely cause (this crate has never been
  published; a scoped crates.io token needs `publish-new`, not just
  `publish-update`).

Known outstanding: the crates.io publish for this version is still expected to
fail until the token carries `publish-new`. That is a credential-scope issue
outside this repository and no longer blocks the GitHub release.

### Changed — `dev` restored as the single integration branch

Release PR #37 was squash-merged, leaving `dev` and `main` with identical trees
but disjoint histories (merge base three days stale). `docs/RELEASE.md` already
required all feature/fix PRs to base `dev` and required a merge-back after each
promote; neither was happening. `dev` has been back-merged from `main` and open
PRs retargeted. Release promotes must use a **merge commit, not a squash**.


### Fixed — capacity-safe idle scale-in (demand-driven autoscaler)

Idle scale-in no longer kills mid-job pool workers when the partial prefer-repo
round-robin demand sample looks empty.

- **Per-worker busy detection:** `WorkerSnapshot.busy` / `is_busy(worker)` from the
  local actions/runner process tree (`Runner.Worker` via `podman top`) — **not**
  the demand scan. `plan_scale` only scales in `running && !busy` workers; a
  busy worker on an un-scanned prefer-repo is held. Fail-closed if process
  inspection fails.
- **Demand-empty full-sweep gate:** `idle_secs` starts only after consecutive empty
  ticks cover a full prefer-list sweep:
  `empty_sweep_ticks = ceil(prefer_len / max(scan_per_tick, 1))`
  (`demand_empty_confirmed`). One empty partial RR tick is never enough.
- Scale-out clamp math (`max_workers` / CPU / mem / `max_spawn_per_tick` +
  `try_claim`) unchanged.
- Regression tests: busy worker not scaled in; empty gate requires full sweep.
### Added — workflow-selectable image + cross-arch spawn (issue #28, draft)

Fleet runners have no nested container engine; distro/arch jobs must select the
work rootfs at **spawn** (mycelium-lang draw-in / multi-OS CI).

- **Label → image map:** built-in distro labels (`ubuntu-24.04`, `debian-bookworm`,
  `rocky-9`, …) plus optional `GHA_IMAGE_MAP` / `--image-map` (JSON or minimal TOML).
  Dynamic pool resolves job `runs-on` labels → OCI ref, forces external image mode,
  and re-registers the worker with those labels.
- **Arch / platform:** arch labels (`arm64`, `riscv64`, …) → `podman --platform`;
  CLI `GHA_PLATFORM` / `--platform` for single-container `up`.
- **binfmt guard:** when target arch ≠ host, require QEMU/`binfmt_misc` registration
  or fail with a clear error (no silent wrong-arch run).
- Docs: [docs/WORK_IMAGES.md](docs/WORK_IMAGES.md); examples
  `packaging/image-map.example.json` / `.toml`.
- Unit tests: label→image resolution, arch→platform args, binfmt-missing guard.

### Host prerequisite (cross-arch only)

```bash
podman run --privileged --rm tonistiigi/binfmt --install all
```

## 0.2.12

### Added — safe recovery (queue-preserving)

- **`gha-runner-ctl recover`**: release orphan pool claims + prune exited fleet
  containers so listen can pick up **queued** Actions jobs. **Never** cancels
  GitHub workflow runs.
- Listen: reap finished claims before poll **and** mid-spawn (retry once on
  budget failure).
- Job sizing: bare `build` jobs → **large** tier; product `ci.yml` uses
  `runs-on: …, large` to avoid rustup OOM (exit 137).
- Docs: [docs/RECOVERY.md](docs/RECOVERY.md).

### Fixed — robust queue drain (fleet stall 2026-07-22)

Listen no longer starves hot repos under a large prefer-list + ephemeral multi-job load.

- **Priority repos every tick:** `GHA_PRIORITY_REPOS` / `--priority-repos` polled before round-robin so `mycelium-lang`, cabal, etc. never wait a full RR cycle.
- **Prefer-repos file:** `GHA_PREFER_REPOS_FILE` / `--prefer-repos-file` (one `owner/repo` per line and/or CSV). Merged with `GHA_PREFER_REPOS`; avoids huge env strings and reload pain.
- **Higher pool scan default:** `GHA_POOL_SCAN_PER_TICK` (default **12**, was hard-capped at 6) after the priority set.
- **Listen floor 45s:** `GHA_LISTEN_MIN_INTERVAL` (default **45**, was hard-coded **120**) under `scope=user`.
- **Stale container reap on listen start:** `GHA_REAP_STALE_SECS` (default **3600**) stops+rms unclaimed fleet workers older than the threshold (warm-boot / retain leftovers). `0` disables.
- **Tick metrics log:** `GHA_TICK_LOG=auto` → `$XDG_DATA_HOME/gha-runner-ctl/logs/listen-ticks.jsonl` each tick (`jobs`, `spawned`, pool usage). `off` / empty disables.
- Demand allowlist paths (`scope=user|repo`) honor prefer-file, not only `GHA_PREFER_REPOS`.

### Docs

- Pre-drain capture preserved under `docs/troubleshoot/` (PR #24); product work closed by this release.
- `docs/troubleshoot/FLEET_QUEUE_STALL_2026-07-22.md` updated with host apply knobs.

## 0.2.11

### Added
- **docs/HOST_PLATFORMS.md** — Linux-first multi-host guidance; FreeBSD/OpenBSD/Unix via Linux VM; Windows WSL2 optional (not required).

### Changed
- README: platforms summary; WSL no longer implied as primary; GPU framed as optional.

## 0.2.10 — 2026-07-21

### Changed
- Host pool defaults **16 CPU / 16 GiB**; tiers micro→xlarge (medium **2c/4g**, large **4c/8g**, xlarge **8c/16g**, gpu **4c/8g**).
- Explicit size labels on `runs-on` (`large`/`xlarge`/`gpu`) drive allocation; workers re-register matching labels.
- Docs: DYNAMIC_POOL sizing policy (justified labels only).

## 0.2.9

### Added
- **Any OCI work image:** `GHA_IMAGE` accepts arbitrary registry/refs (including host:port and `@sha256:` digests).
- **`GHA_IMAGE_MODE`:** `auto` | `build` | `external` — auto uses packaging build only for the stock default tag; any other image is external (pull + inject runner).
- **`GHA_PULL_POLICY`:** `never` | `missing` | `always` (defaults: never for build hot path, missing for external).
- **Runner kit knobs (not hard-coded):** `GHA_RUNNER_VERSION`, `GHA_RUNNER_SHA256`, `GHA_RUNNER_ARCH`, optional `GHA_RUNNER_SEED_URL`.
- **`GHA_RUNNER_USER`**, **`GHA_SEED_HELPER_IMAGE`**, **`GHA_ENTRYPOINT`** for ergonomic external rootfs setup.
- Docs: [docs/WORK_IMAGES.md](docs/WORK_IMAGES.md).

## 0.2.8 — 2026-07-20

### Fixed
- Registration hourly budget no longer freezes the listen loop (return error instead of spin-sleep).
- Default `GHA_REG_MAX_PER_HOUR` raised 30→90 (host env can set 120).
- reopen-issues meta workflow always has a green gate job (avoids zero-job red runs).
- **Listen drain under backlog:** `list_demand_jobs` returns **partial** results when the per-poll API budget is exhausted instead of failing the whole tick with zero spawns.
- Partial results on budget exhaust; prefer queued runs, light in_progress sample for multi-job matrices; RR-capped scan width so registration POSTs still fit.
- README architecture mermaid diagrams (sanitized — no hostnames, tokens, or personal paths).

## 0.2.7

### Dynamic host pool (horizontal + vertical)

- Shared budget **GHA_POOL_CPUS** / **GHA_POOL_MEMORY** (default **8 / 8g**) across all listen managers
- **Automatic job sizing** from job name + labels (`micro` … `large` / `gpu`) — workflows need not set CPU/RAM
- Multi-worker spawn: `container-w{N}` claims pool, reaps on exit; many small runners or mixed sizes within budget
- `GHA_POOL_MODE=dynamic` (default) vs `off` for legacy single-container listen
- Docs: `docs/DYNAMIC_POOL.md`

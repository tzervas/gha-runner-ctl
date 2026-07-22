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

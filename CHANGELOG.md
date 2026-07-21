## 0.2.10

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

## 0.2.8

### Fixed
- Registration hourly budget no longer freezes the listen loop (return error instead of spin-sleep).
- Default `GHA_REG_MAX_PER_HOUR` raised 30→90 (host env can set 120).
- reopen-issues meta workflow always has a green gate job (avoids zero-job red runs).
 — 2026-07-20

### Fixed
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

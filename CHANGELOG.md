## 0.2.8 — 2026-07-20

### Fixed
- **Listen drain under backlog:** `list_demand_jobs` returns **partial** results when the per-poll API budget is exhausted instead of failing the whole tick with zero spawns.
- Scan **queued** runs only (in_progress already has a runner); fewer runs per repo; cap scan width so registration POSTs still fit in budget.
- README architecture mermaid diagrams (sanitized — no hostnames, tokens, or personal paths).

## 0.2.7

### Dynamic host pool (horizontal + vertical)

- Shared budget **GHA_POOL_CPUS** / **GHA_POOL_MEMORY** (default **8 / 8g**) across all listen managers
- **Automatic job sizing** from job name + labels (`micro` … `large` / `gpu`) — workflows need not set CPU/RAM
- Multi-worker spawn: `container-w{N}` claims pool, reaps on exit; many small runners or mixed sizes within budget
- `GHA_POOL_MODE=dynamic` (default) vs `off` for legacy single-container listen
- Docs: `docs/DYNAMIC_POOL.md`

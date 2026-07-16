# Interface Bulletin: ctl/reg-api-budget

**Contract ID:** `ctl/reg-api-budget`  
**Producer:** W1b (code/contracts)  
**Status:** STABLE  
**Version:** 0.2.6  
**Consumers:** W2 (host/env), W3 (PR test plan)  
**Depends-on:** `framework/bulletin@STABLE`, `ctl/cli-env@STABLE`

## API GET pacing (per process)

Controlled by global CLI/env (see `ctl-cli-env.md`):

| Parameter | Default | Runtime clamp | Behavior |
|-----------|---------|---------------|----------|
| `GHA_API_MIN_GAP_MS` | `1000` | 50–60000 ms | Minimum spacing between GETs in `ApiPacer` |
| `GHA_API_MAX_PER_POLL` | `12` | 2–500 | Max GETs per demand poll cycle; then wait next `listen` interval |
| `GHA_API_BACKOFF_SECS` | `90` | 5–900 s | Initial backoff on 403/429; doubles up to 900 s |

On rate limit: honors `Retry-After` / `X-RateLimit-Reset`; sets cool-down until resume.  
Low remaining quota (`X-RateLimit-Remaining` < 30) triggers proactive cool-down.

Used by: `listen` demand polling (`demand`, `repo_needs_runner`, org/user repo lists).

## Registration-token POST pacing (host-wide)

| Parameter | Default | Runtime clamp | Behavior |
|-----------|---------|---------------|----------|
| `GHA_REG_MIN_GAP_SECS` | `5` | 1–600 s | Min seconds between successful POST timestamps |
| `GHA_REG_MAX_PER_HOUR` | `30` | 1–500 | Max POSTs in rolling 3600 s window |

### Artifacts

| File | Role |
|------|------|
| `{XDG_RUNTIME_DIR}/gha-runner-ctl-reg-pace.lock` | Coordination marker |
| `{XDG_RUNTIME_DIR}/gha-runner-ctl-reg-pace.exclusive` | Short-lived exclusive create |
| `{XDG_RUNTIME_DIR}/gha-runner-ctl-reg-pace.json` | `RegPaceState`: `last_unix`, `recent[]` |

Fallback when `XDG_RUNTIME_DIR` unset: system temp dir.

### Flow

1. `pace_registration()` before each registration-token POST.
2. On budget exhaustion: sleep 30 s, retry (up to 120 attempts).
3. On POST failure with rate limit: `note_registration_failure_backoff()` extends `last_unix`.

Shared across **all** `gha-runner-ctl` processes on the host.

## Invariants

- Registration POSTs never bypass `pace_registration()`.
- API GETs in a poll cycle go through `ApiPacer::get` (gap + per-poll cap + cool-down).
- Budget state is on the host filesystem under runtime dir — W2 must not delete casually during active fleet.

## Out of scope

- GitHub Apps/JWT auth (not implemented).
- Cross-host coordination (single-host file locks only).

## Delta since previous bulletin

- **0.2.6 STABLE:** Documented `ApiPacer` + `RegPaceState` from `src/lib.rs`.
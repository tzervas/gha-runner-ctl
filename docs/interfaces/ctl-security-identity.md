# Interface Bulletin: ctl/security-identity

**Contract ID:** `ctl/security-identity`  
**Producer:** W1b (code/contracts)  
**Status:** STABLE  
**Version:** 0.2.6  
**Consumers:** W1c (docs narrative), W2 (host/env), W3 (PR test plan)  
**Depends-on:** `framework/bulletin@STABLE`, `ctl/cli-env@STABLE`

## Fleet identity (production)

| Expectation | Detail |
|-------------|--------|
| OS user | Dedicated unprivileged agent (e.g. `gha-agent`), nologin, no sudo |
| Container runtime | Rootless Podman (`XDG_RUNTIME_DIR` socket) |
| Control plane | Long-lived `gha-runner-ctl` on host; work in Podman runner containers |

## Root and socket guards

| Env | When | Behavior |
|-----|------|----------|
| (none) | euid **0** | Exit **78** (`EX_CONFIG`); message directs to `scripts/setup-rootless.sh` and `gha-agent` |
| `GHA_ALLOW_ROOT=1` (or `true`/`yes`) | euid **0** | Allow with **WARNING** on stderr (ephemeral WSL/dev only) |
| `CONTAINER_HOST` contains `/run/podman/podman.sock` or `/var/run/docker.sock` | Any podman op | Error unless `GHA_ALLOW_ROOTFUL_SOCKET=1` |

## Secret handling

### CLI token prohibition

`prevent_raw_token_args()` scans `std::env::args()` for substrings:

- `ghp_`, `gho_`, `ghu_`, `ghs_`, `github_pat_`

On match: exit **127** with guidance to use env, `gh`, GCM, or interactive prompt — **not** argv.

### GitHub API token resolution order

`github_token()` (no logging of token value):

1. `GH_TOKEN`, then `GITHUB_TOKEN` (non-empty)
2. `git credential` fill for `host=github.com`
3. `gh auth token`
4. `$HOME/.config/gha-runner-ctl/config.json` field `github_token`
5. Interactive: optional GCM install prompt; optional save to config + GCM

Failure returns error string only (redacted in outer layers).

### Redaction

`redact()` masks patterns including `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `Bearer `, `RUNNER_TOKEN=`; truncates to 400 chars.

Applied to: CLI error output, podman stderr in debug dump, listen/wake errors.

### Debug dump (`debug_dump_on_error`)

| Env | Effect |
|-----|--------|
| `GHA_DEBUG=1` | Always dump on error |
| `GHA_DEBUG_ON_ERR` unset | **On** (default while stabilizing) |
| `GHA_DEBUG_ON_ERR=0` | Off unless `GHA_DEBUG=1` |

Dump includes: error, user/euid, pwd, selected **non-secret** `GHA_*` env vars, redacted `podman info` / `podman ps` (max 15 lines).  
**Never** prints env keys containing `TOKEN` or `SECRET`.

## Wake authentication

| Rule | Detail |
|------|--------|
| Minimum length | `GHA_WAKE_TOKEN` ≥ **16** when set |
| Headers | `Authorization: Bearer <secret>` (case-insensitive prefix) or `X-Wake-Token: <secret>` |
| Compare | Constant-time; secret **not** lowercased |
| Bind | `127.0.0.1` only |

## Config file

| Path | Schema | Notes |
|------|--------|-------|
| `$HOME/.config/gha-runner-ctl/config.json` | `{ "github_token": "<optional string>" }` | Created only on interactive save; consumers must not document passing tokens on CLI |

## Crate safety

| Rule | Source |
|------|--------|
| `unsafe_code = forbid` | `Cargo.toml` `[lints.rust]` |
| No token in logs | `main` uses `redact(&e)`; registration/runner tokens never `eprintln!` raw |

## Invariants

- Production path: non-root + rootless Podman; root and rootful socket are opt-in escape hatches only.
- Secrets must not appear in argv, debug env dump, or unredacted stderr.
- Wake and GitHub credentials are out-of-band from CLI flags (except `GHA_WAKE_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN` env).

## Out of scope for consumers

- Shell phase tracing (`GHA_DEBUG` in `scripts/lib/shell-debug.sh`) — see `ctl/phase-runner` / ORCHESTRATION §C–D.
- Host instance paths (W2 `ctl/host-instance`).

## Delta since previous bulletin

- **0.2.6 STABLE:** First published security/identity bulletin from `main.rs` + `lib.rs` auth/root/debug paths.
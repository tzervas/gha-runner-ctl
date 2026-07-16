# Interface Bulletin: ctl/cli-env

**Contract ID:** `ctl/cli-env`  
**Producer:** W1b (code/contracts)  
**Status:** STABLE  
**Version:** 0.2.6  
**Consumers:** W1c (docs narrative), W2 (host/env), W3 (PR test plan)  
**Depends-on:** `framework/bulletin@STABLE`

## Product identity

| Artifact | Value |
|----------|--------|
| Crate / binary | `gha-runner-ctl` |
| `VERSION` file | `0.2.6` |
| `Cargo.toml` `package.version` | `0.2.6` |
| GitHub HTTP User-Agent | `gha-runner-ctl/0.2.6` (`src/lib.rs` `UA`) |

## Entry behavior

| Condition | Behavior |
|-----------|----------|
| No subcommand, `--full-auto` unset | Exit **1**, message: use `--help` or `--full-auto` |
| No subcommand, `--full-auto` set | Auto `prepare` if image/volume missing; then implicit `listen` with `--interval 180`, `--idle-secs 500`, no wake port |
| `main` preflight | `prevent_raw_token_args()` then `refuse_root_unless_allowed()` before `run()` |
| Success | Exit **0** |
| `run()` error | Exit **1**; stderr uses `redact()`; optional `debug_dump_on_error` (see `ctl/security-identity`) |

## Commands

| Command | Purpose | Instance lock |
|---------|---------|---------------|
| `prepare` | Host package refresh (unless skipped), build image, seed volume snapshot | No |
| `up` | Registration token + register/start runner container | Yes: `{XDG_RUNTIME_DIR or tmp}/gha-runner-ctl-up-{container}.lock` |
| `down` | Stop runner; `--rm` (default **true**) removes container | No |
| `status` | Report container/volume state | No |
| `detect` | Print resolved scope/target/labels/container | No |
| `listen` | Demand poll loop; scale up/down; optional localhost wake server | Yes: `gha-runner-ctl-listen-{container}.lock` |
| `warm` | Paced retain registration per allowlist repo (or single `--repo`) | No |

### Subcommand-only flags

| Flag | Env | Default | Command | Notes |
|------|-----|---------|---------|-------|
| `--with-container` | — | **true** | `prepare` | Build/run seed container when true |
| `--skip-host-update` | `GHA_SKIP_HOST_UPDATE` | false | `prepare` | Skip apt/dnf on host |
| `--interval` | — | `180` | `listen` | Clamped **5–3600** s; `scope=user` floor **120** s |
| `--idle-secs` | — | `180` | `listen` | Clamped **30–86400** s |
| `--wake-port` | `GHA_WAKE_PORT` | unset | `listen` | Requires `GHA_WAKE_TOKEN` (≥16 chars); binds `127.0.0.1` |
| `--rm` | — | **true** | `down` | Remove container after stop |
| `--gap-secs` | — | `8` | `warm` | Effective gap `max(gap_secs, reg_min_gap_secs, 3)` |
| `--start` | — | **true** | `warm` | If false, mint registration token only (paced) |

### Wake HTTP (when `--wake-port` set)

| Method / path | Auth | Action |
|---------------|------|--------|
| `GET /health` | None | `200 ok` |
| `POST /wake` | `Authorization: Bearer <token>` or `X-Wake-Token: <token>` | Runs `up` |
| `POST /sleep` | Same | Runs `down --rm` |

Token compare is constant-time; secret case preserved (see `wake_request_line_authorized`).

## Global flags and environment

All global options accept the same name as env var (clap `env =`); boolean env vars use standard clap parsing.

| Flag | Env | Type / values | Default | Required when |
|------|-----|---------------|---------|---------------|
| `--scope` | `GHA_SCOPE` | `repo`, `org`, `user` | `repo` | — |
| `--repo` | `GHA_REPO` | `owner/repo` | — | `scope=repo` unless `--auto`, `--prefer-repos` (warm), or repo set by resolve |
| `--owner` | `GHA_OWNER` | ident | — | `scope=org` |
| `--user` | `GHA_USER` | ident | gh user if unset | `scope=user` (resolved at runtime) |
| `--auto` | `GHA_AUTO` | bool | false | — |
| `--image` | `GHA_IMAGE` | image ref | `localhost/gha-runner-ctl:latest` | — |
| `--container` | `GHA_CONTAINER` | ident | `gha-runner-ctl` | — |
| `--volume` | `GHA_VOLUME` | ident | `gha-runner-ctl-data` | — |
| `--runner-name` | `GHA_RUNNER_NAME` | ident | `shared-podman-1` | — |
| `--labels` | `GHA_LABELS` | comma-separated | `self-hosted,linux,x64,podman` | — |
| `--cpus` | `GHA_CPUS` | float string | `5` | — |
| `--memory` | `GHA_MEMORY` | size | `8g` | — |
| `--gpu` | `GHA_GPU` | bool | false | — |
| `--gpu-slice` | `GHA_GPU_SLICE` | `a` or `b` | — | Requires `--gpu` |
| `--demand-require-labels` | `GHA_DEMAND_REQUIRE_LABELS` | CSV | — | Listen demand filter |
| `--demand-exclude-labels` | `GHA_DEMAND_EXCLUDE_LABELS` | CSV | — | Listen demand filter |
| `--build-dir` | `GHA_BUILD_DIR` | path | packaging dir in crate | Must contain `Containerfile` |
| `--mode` | `GHA_MODE` | `ephemeral`, `retain` | `ephemeral` | — |
| `--wake-token` | `GHA_WAKE_TOKEN` | string | — | ≥16 chars if set; required with wake port |
| `--full-auto` | `GHA_FULL_AUTO` | bool | false | Sets `--auto`; may set `scope=user` |
| `--this-repo-only` | `GHA_THIS_REPO_ONLY` | URL or `owner/repo` | — | Forces `scope=repo` + repo |
| `--public-only` | `GHA_PUBLIC_ONLY` | bool | false | Org/user demand visibility |
| `--private-only` | `GHA_PRIVATE_ONLY` | bool | false | Org/user demand visibility |
| `--all-repos` | `GHA_ALL_REPOS` | bool | false | Public+private for demand |
| `--prefer-repos` | `GHA_PREFER_REPOS` | CSV `owner/repo` | — | User-batch allowlist; warm repo list |
| `--api-min-gap-ms` | `GHA_API_MIN_GAP_MS` | u64 | `1000` | Clamped **50–60000** in pacer |
| `--api-max-per-poll` | `GHA_API_MAX_PER_POLL` | u32 | `12` | Clamped **2–500** |
| `--api-backoff-secs` | `GHA_API_BACKOFF_SECS` | u64 | `90` | Clamped **5–900**; doubles on rate limit |
| `--repos-per-tick` | `GHA_REPOS_PER_TICK` | u32 | `1` | On CLI / wake snapshot (see help text) |
| `--reg-min-gap-secs` | `GHA_REG_MIN_GAP_SECS` | u64 | `5` | Clamped **1–600**; host-wide |
| `--reg-max-per-hour` | `GHA_REG_MAX_PER_HOUR` | u32 | `30` | Clamped **1–500**; host-wide |

### Convenience env (not clap flags)

| Env | Effect |
|-----|--------|
| `GHA_BATCH=1` | If `scope` was default `repo`, switch to `scope=user` and set user from `gh` |

## Scope and registration

| Scope | Registration API | Notes |
|-------|------------------|-------|
| `repo` | `POST /repos/{owner}/{repo}/actions/runners/registration-token` | URL `https://github.com/{owner}/{repo}` |
| `org` | `POST /orgs/{owner}/actions/runners/registration-token` | Requires `--owner` |
| `user` | Per active repo at `listen` / `up` time | Ephemeral re-target across repos; retain requires single sticky repo |

`warm` forces per-repo `scope=repo`, `mode=retain`, suffixes `container` / `volume` / `runner_name` with repo slug (truncated to 60 chars).

## Demand visibility (`listen`, org/user)

| Flags set | Private repos | Public repos |
|-----------|---------------|--------------|
| (default) | excluded | included |
| `--private-only` | included | excluded |
| `--all-repos` | included | included |
| `--public-only` | excluded | included (same as default) |

## Artifacts / paths

| Path | Purpose |
|------|---------|
| `$HOME/.config/gha-runner-ctl/config.json` | Optional `github_token` (see security bulletin) |
| `$XDG_RUNTIME_DIR/gha-runner-ctl-reg-pace.lock` (+ `.exclusive`) | Host-wide registration pacing lock |
| `$XDG_RUNTIME_DIR/gha-runner-ctl-reg-pace.json` | Registration pace state |
| `$XDG_RUNTIME_DIR/gha-runner-ctl-{up\|listen}-{container}.lock` | Per-container controller lock |
| Default build context | `{CARGO_MANIFEST_DIR}/packaging/` or `--build-dir` |

## Invariants

- `#![forbid(unsafe_code)]` on crate (`Cargo.toml` `lints.rust`).
- Global flags are **global** on all subcommands (clap `global = true`).
- Validation rejects unsafe `repo`, `image`, `container`, `volume`, `runner-name`, `labels`, `cpus`, `memory`, `gpu-slice`.
- `listen` + `up` for the same `--container` cannot run concurrently (lock).
- Registration-token POSTs share host budget (`ctl/reg-api-budget` bulletin).
- GitHub REST from this binary uses `ureq` agent with UA `gha-runner-ctl/0.2.6` and 20s timeouts.
- Errors and debug output must not print raw tokens (`redact`, debug skips `*TOKEN*` / `*SECRET*` env keys).

## Out of scope for consumers

- systemd unit contents, host user creation, live warm/listen on production host (W2).
- Semver tooling, commits, PR title (W3 / process owner).
- Narrative redesign of DESIGN/HOST_OPS (W1c) — consumers **cite** this bulletin, do not invent flags.

## Delta since previous bulletin

- **0.2.6 STABLE:** First published `ctl-cli-env.md` aligned to `src/lib.rs` clap surface, wake server, locks, and version lockstep.
//! One GitHub Actions self-hosted runner controller (Podman).
//!
//! Registration targets:
//! - **repo** — one repository (optional **--auto** from cwd / `gh repo view`)
//! - **org** — organization runner (many org repos, one registration)
//! - **user** — batch personal account: poll all owned repos; ephemeral-register
//!   the single runner to whichever repo has queued self-hosted work
//!
//! GitHub queues jobs. With **pool mode** (default), a listen process can spawn
//! multiple ephemeral workers sized from job complexity within a host budget
//! (default 8 CPU / 8 GiB shared across all managers).

mod appauth;
mod dump_redact;
mod fail_closed;
mod image_arch;
mod pool;

pub use dump_redact::{
    classify_value, is_allowlisted_key, redact_env_dump, redact_for_dump, redact_free_text,
    RedactedField, UnsafeShape, ValueVerdict, DUMP_ALLOWLIST,
};
pub use fail_closed::{check_succeeded, fail_closed, FailClosedEvent, FailClosedTracker};
pub use image_arch::{
    binfmt_lists_arch, binfmt_missing_error, ensure_binfmt_for_arch, extra_image_arch_labels,
    load_image_map, parse_image_map, podman_platform_args, resolve_arch_from_labels,
    resolve_job_image_arch, ImageMap, JobImageArch, TargetArch,
};
pub use pool::{
    demand_empty_confirmed, empty_sweep_ticks, fit_to_budget, format_cpus, format_memory_mib,
    is_busy, parse_cpus_f64, parse_memory_mib, plan_scale, resources_for_tier, size_for_job,
    DemandSignal, ResourcePool, ScaleInput, ScalePlan, SizeTier, SpawnRequest, WorkerSnapshot,
    DEFAULT_MAX_SPAWN_PER_TICK, DEFAULT_SPAWN_GRACE_SECS,
};

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_IMAGE: &str = "localhost/gha-runner-ctl:latest";
const DEFAULT_CONTAINER: &str = "gha-runner-ctl";
const DEFAULT_VOLUME: &str = "gha-runner-ctl-data";
const DEFAULT_LABELS: &str = "self-hosted,linux,x64,podman";
const DEFAULT_NAME: &str = "shared-podman-1";
/// Helper used only to extract/seed the runner kit into a volume (any image with shell+curl works).
const DEFAULT_SEED_HELPER_IMAGE: &str = "docker.io/library/ubuntu:24.04";
/// Numeric uid:gid inside work containers (stock packaging image uses 1001).
const DEFAULT_RUNNER_USER: &str = "1001:1001";
/// Official actions/runner pin (overridable via GHA_RUNNER_VERSION / GHA_RUNNER_SHA256 / GHA_RUNNER_ARCH).
const DEFAULT_RUNNER_VERSION: &str = "2.335.1";
const DEFAULT_RUNNER_SHA256: &str =
    "4ef2f25285f0ae4477f1fe1e346db76d2f3ebf03824e2ddd1973a2819bf6c8cf";
const DEFAULT_RUNNER_ARCH: &str = "x64";
const UA: &str = "ap-runner-ctl/0.3.3";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Production GitHub REST base, **without** a trailing slash. Previously inlined into
/// every `format!` that built an API URL; now the default of [`HttpConfig`].
const GITHUB_API_BASE: &str = "https://api.github.com";
const MIN_POLL_SECS: u64 = 5;
const MAX_POLL_SECS: u64 = 3600;
const MIN_IDLE_SECS: u64 = 30;
const MAX_IDLE_SECS: u64 = 86_400;
/// Default gap between GitHub API calls within one process (ms).
const DEFAULT_API_MIN_GAP_MS: u64 = 1000;
/// Default cap on API GETs per demand tick (allowlist of ~3 repos fits comfortably).
const DEFAULT_API_MAX_PER_POLL: u32 = 12;
/// Initial backoff when rate-limited (seconds).
const DEFAULT_API_BACKOFF_SECS: u64 = 90;
const MAX_API_BACKOFF_SECS: u64 = 900;
/// Default listen interval for scale-up demand polling (seconds). 2–5 min band.
const DEFAULT_LISTEN_INTERVAL_SECS: u64 = 180;
/// Floor for user-batch demand interval (seconds). Overridable via GHA_LISTEN_MIN_INTERVAL.
/// Historical 120s starved large prefer-lists under multi-job ephemeral load (see fleet debug 2026-07-22).
const USER_BATCH_MIN_INTERVAL_SECS: u64 = 45;
/// Default: check this many allowlisted repos per tick (round-robin stagger).
/// 0 = all allowlisted repos each tick (still paced by min-gap).
const DEFAULT_REPOS_PER_TICK: u32 = 1;
/// Min seconds between registration-token POSTs (shared across processes on host).
const DEFAULT_REG_MIN_GAP_SECS: u64 = 5;
/// Max registration-token POSTs per rolling hour (shared host budget).
const DEFAULT_REG_MAX_PER_HOUR: u32 = 90;
/// Bounded retain lifetime (seconds). NOT a credential-expiry workaround — the
/// registration token is single-use and consumed by `config.sh`; after that the
/// runner holds durable, non-expiring credentials (see `effective_ephemeral` doc
/// comment). This bound exists for workspace hygiene / drift control: a retained
/// runner's `_work` dir and job history accumulate over a long-lived container,
/// so we force a fresh registration (fresh volume state) periodically rather than
/// let one runner serve indefinitely. Overridable via GHA_RETAIN_MAX_AGE_SECS.
const DEFAULT_RETAIN_MAX_AGE_SECS: u64 = 3000;
/// Bounded retain lifetime (job count). Same rationale as the age bound above —
/// caps drift, not credentials. Overridable via GHA_RETAIN_MAX_JOBS.
const DEFAULT_RETAIN_MAX_JOBS: u32 = 25;

#[derive(Debug, Clone, ValueEnum)]
pub enum Mode {
    Ephemeral,
    Retain,
}

#[derive(Debug, Clone, ValueEnum, PartialEq, Eq, Copy)]
pub enum Scope {
    /// One repository. Use with --repo or --auto.
    Repo,
    /// Organization registration (repos must live in that org).
    Org,
    /// Batch all personal (owner) repos under a user login; re-register per demand.
    User,
}

/// How the work container image is obtained.
#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum ImageMode {
    /// `build` for the default stock tag when packaging is available; otherwise `external`.
    Auto,
    /// Build `packaging/Containerfile` and tag as `--image`.
    Build,
    /// Use any OCI image as-is (pull per policy); inject actions/runner via volume + entrypoint.
    External,
}

/// Podman `--pull` policy for work containers and prepare.
#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum PullPolicy {
    /// Never pull (stock hot path after prepare build).
    Never,
    /// Pull only if the image is missing locally.
    Missing,
    /// Always pull/refresh the image digest.
    Always,
}

#[derive(Debug, Parser)]
#[command(
    name = "gha-runner-ctl",
    about = "Fleet agent for self-hosted GHA on Podman: long-lived control plane, ephemeral work containers"
)]
pub struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[arg(long, env = "GHA_SCOPE", value_enum, default_value_t = Scope::Repo, global = true)]
    scope: Scope,

    /// owner/repo when scope=repo (or filled by --auto)
    #[arg(long, env = "GHA_REPO", global = true)]
    repo: Option<String>,

    /// Org login when scope=org
    #[arg(long, env = "GHA_OWNER", global = true)]
    owner: Option<String>,

    /// User login when scope=user (default: authenticated gh user)
    #[arg(long, env = "GHA_USER", global = true)]
    user: Option<String>,

    /// Infer owner/repo from the current git checkout / gh context
    #[arg(long, env = "GHA_AUTO", global = true, default_value_t = false)]
    auto: bool,

    /// Work container image (any OCI ref: distro, custom CI image, private registry, digest).
    /// Examples: `localhost/gha-runner-ctl:latest`, `docker.io/library/ubuntu:24.04`,
    /// `ghcr.io/org/ci:1.2`, `registry.example.com:5000/ci@sha256:…`
    #[arg(long, env = "GHA_IMAGE", default_value = DEFAULT_IMAGE, global = true)]
    image: String,

    /// How to obtain `--image`: auto | build | external (see `ImageMode`).
    #[arg(long, env = "GHA_IMAGE_MODE", value_enum, default_value_t = ImageMode::Auto, global = true)]
    image_mode: ImageMode,

    /// Podman pull policy for prepare/up: never | missing | always.
    /// When unset (`auto` via empty default path): `never` for build mode, `missing` for external.
    #[arg(long, env = "GHA_PULL_POLICY", value_enum, global = true)]
    pull_policy: Option<PullPolicy>,

    /// uid:gid (or user name) for work containers. Stock image: 1001:1001.
    #[arg(long, env = "GHA_RUNNER_USER", default_value = DEFAULT_RUNNER_USER, global = true)]
    runner_user: String,

    /// Helper image used only to seed/copy the runner kit into volumes (needs shell + curl/tar).
    #[arg(long, env = "GHA_SEED_HELPER_IMAGE", default_value = DEFAULT_SEED_HELPER_IMAGE, global = true)]
    seed_helper_image: String,

    /// Official actions/runner version when seeding external images (or empty volume).
    #[arg(long, env = "GHA_RUNNER_VERSION", default_value = DEFAULT_RUNNER_VERSION, global = true)]
    runner_version: String,

    /// SHA256 of the actions-runner linux tarball (must match --runner-version / --runner-arch).
    #[arg(long, env = "GHA_RUNNER_SHA256", default_value = DEFAULT_RUNNER_SHA256, global = true)]
    runner_sha256: String,

    /// actions/runner arch segment in the release asset name (`x64`, `arm64`, …).
    #[arg(long, env = "GHA_RUNNER_ARCH", default_value = DEFAULT_RUNNER_ARCH, global = true)]
    runner_arch: String,

    /// Optional full URL for the runner tarball (overrides version/arch URL construction).
    #[arg(long, env = "GHA_RUNNER_SEED_URL", global = true)]
    runner_seed_url: Option<String>,

    /// Optional host path to entrypoint.sh (default: packaging/entrypoint.sh next to Containerfile).
    #[arg(long, env = "GHA_ENTRYPOINT", global = true)]
    entrypoint: Option<PathBuf>,

    #[arg(long, env = "GHA_CONTAINER", default_value = DEFAULT_CONTAINER, global = true)]
    container: String,

    #[arg(long, env = "GHA_VOLUME", default_value = DEFAULT_VOLUME, global = true)]
    volume: String,

    #[arg(long, env = "GHA_RUNNER_NAME", default_value = DEFAULT_NAME, global = true)]
    runner_name: String,

    #[arg(long, env = "GHA_LABELS", default_value = DEFAULT_LABELS, global = true)]
    labels: String,

    #[arg(long, env = "GHA_CPUS", default_value = "5", global = true)]
    cpus: String,

    #[arg(long, env = "GHA_MEMORY", default_value = "8g", global = true)]
    memory: String,

    /// Attach WSL/host GPU into the runner container (Podman --gpus + /dev/dxg).
    /// Pair with a `gpu` runner label so only GPU jobs schedule here.
    #[arg(long, env = "GHA_GPU", default_value_t = false, global = true)]
    gpu: bool,

    /// Soft GPU share id for dual workers on one consumer GPU (`a` or `b`).
    /// Sets env markers for jobs; both may time-share the same device (no MIG on GeForce).
    /// Tear-down on idle returns the GPU (container stop frees device processes).
    #[arg(long, env = "GHA_GPU_SLICE", global = true)]
    gpu_slice: Option<String>,

    /// Only wake for jobs whose labels include **all** of these (comma-separated).
    /// Example GPU listener: `--demand-require-labels gpu`
    #[arg(long, env = "GHA_DEMAND_REQUIRE_LABELS", global = true)]
    demand_require_labels: Option<String>,

    /// Skip jobs that include **any** of these labels (comma-separated).
    /// Example CPU listener: `--demand-exclude-labels gpu`
    #[arg(long, env = "GHA_DEMAND_EXCLUDE_LABELS", global = true)]
    demand_exclude_labels: Option<String>,

    #[arg(long, env = "GHA_BUILD_DIR", global = true)]
    build_dir: Option<PathBuf>,

    #[arg(long, env = "GHA_MODE", value_enum, default_value_t = Mode::Ephemeral, global = true)]
    mode: Mode,

    #[arg(long, env = "GHA_WAKE_TOKEN", global = true)]
    wake_token: Option<String>,

    /// Automatically prepare, poll, and register (gentle demand poll ~3 min; idle 500s)
    #[arg(long, env = "GHA_FULL_AUTO", default_value_t = false, global = true)]
    full_auto: bool,

    /// Target a specific repository: [platform/]owner/name (defaults platform to github.com)
    #[arg(long, env = "GHA_THIS_REPO_ONLY", global = true)]
    this_repo_only: Option<String>,

    /// Only target public repositories (default if no visibility filter is specified)
    #[arg(long, env = "GHA_PUBLIC_ONLY", default_value_t = false, global = true)]
    public_only: bool,

    /// Only target private repositories
    #[arg(long, env = "GHA_PRIVATE_ONLY", default_value_t = false, global = true)]
    private_only: bool,

    /// Target both public and private repositories
    #[arg(long, env = "GHA_ALL_REPOS", default_value_t = false, global = true)]
    all_repos: bool,

    /// Comma-separated `owner/repo` for user-batch demand poll.
    /// When set, **only** these repos are polled (allowlist) — avoids burning the
    /// GitHub API rate limit across hundreds of owned repos.
    /// Example: `tzervas/gha-runner-ctl,tzervas/tg-agent-relay,tzervas/agent-harness`
    ///
    /// DEPRECATED (WP-09): this is a flat allowlist, not a preference ordering — the honest
    /// name is `GHA_ALLOWLIST_REPOS`. `GHA_PREFER_REPOS` still works during the deprecation
    /// window (a warning is printed) but is ignored if `GHA_ALLOWLIST_REPOS` is also set.
    /// See `Cli::effective_allowlist_repos`.
    #[arg(long, env = "GHA_PREFER_REPOS", global = true)]
    prefer_repos: Option<String>,

    /// Comma-separated `owner/repo` for user-batch demand poll (allowlist, not an ordering).
    /// Preferred name — supersedes the deprecated `GHA_PREFER_REPOS`. Same semantics: when set,
    /// only these repos are polled. Example: `tzervas/gha-runner-ctl,tzervas/tg-agent-relay`
    #[arg(long = "allowlist-repos", env = "GHA_ALLOWLIST_REPOS", global = true)]
    allowlist_repos: Option<String>,

    /// Path to the allowlist file (one `owner/repo` per line and/or CSV). Merged with
    /// GHA_ALLOWLIST_REPOS / GHA_PREFER_REPOS (inline CSV wins over the file — see
    /// `allowlist_repos_list`; a fully-inline allowlist has silently shadowed this file before).
    /// Survives large allowlists without overflowing env.
    /// Example: `$XDG_DATA_HOME/gha-runner-ctl/allowlists/active-demand.list`
    ///
    /// Preferred name — supersedes the deprecated `GHA_PREFER_REPOS_FILE`.
    #[arg(
        long = "allowlist-repos-file",
        env = "GHA_ALLOWLIST_REPOS_FILE",
        global = true
    )]
    allowlist_repos_file: Option<String>,

    /// DEPRECATED (WP-09): same meaning as `--allowlist-repos-file`. This file has always been
    /// an allowlist, never a preference ordering — the old name conflated it with
    /// `GHA_PRIORITY_REPOS`, which genuinely IS an ordering and is a separate feature.
    ///
    /// Still honored, because fleets pin this in instance env files and cannot all move on the
    /// same day. A production host currently sets
    /// `GHA_PREFER_REPOS_FILE=.../allowlists/active-demand.list`; silently dropping support would
    /// leave it polling every owned repo, exhausting its API budget mid-scan and reporting zero
    /// demand — a failure this fleet has already seen. If `GHA_ALLOWLIST_REPOS_FILE` is also set,
    /// the new name wins and this one is ignored with a warning.
    #[arg(
        long = "prefer-repos-file",
        env = "GHA_PREFER_REPOS_FILE",
        global = true
    )]
    prefer_repos_file: Option<String>,

    /// Comma-separated `owner/repo` polled **every tick before** round-robin allowlist.
    /// Use for hot queues (e.g. mycelium-lang) so they never wait a full RR cycle.
    #[arg(long, env = "GHA_PRIORITY_REPOS", global = true)]
    priority_repos: Option<String>,

    /// Floor for listen poll interval under scope=user (seconds). Default 45.
    #[arg(long, env = "GHA_LISTEN_MIN_INTERVAL", default_value_t = USER_BATCH_MIN_INTERVAL_SECS, global = true)]
    listen_min_interval: u64,

    /// Max repos scanned for demand in dynamic pool mode per tick (after priority set). Default 12.
    #[arg(
        long,
        env = "GHA_POOL_SCAN_PER_TICK",
        default_value_t = 12,
        global = true
    )]
    pool_scan_per_tick: u32,

    /// On listen start, stop+rm worker containers older than this many seconds that are not in the pool claim set.
    /// Targets stale retain/warm leftovers. `0` disables. Default 3600.
    #[arg(
        long,
        env = "GHA_REAP_STALE_SECS",
        default_value_t = 3600,
        global = true
    )]
    reap_stale_secs: u64,

    /// Append one JSON line per listen tick to this path (dir created). Empty = disabled.
    /// Default: `$XDG_DATA_HOME/gha-runner-ctl/logs/listen-ticks.jsonl` when unset via env empty string.
    #[arg(long, env = "GHA_TICK_LOG", default_value = "auto", global = true)]
    tick_log: String,

    /// Minimum milliseconds between GitHub API calls in this process (paced batching).
    #[arg(long, env = "GHA_API_MIN_GAP_MS", default_value_t = DEFAULT_API_MIN_GAP_MS, global = true)]
    api_min_gap_ms: u64,

    /// Max GitHub API GETs per demand poll cycle (then wait for next --interval).
    #[arg(long, env = "GHA_API_MAX_PER_POLL", default_value_t = DEFAULT_API_MAX_PER_POLL, global = true)]
    api_max_per_poll: u32,

    /// Initial backoff seconds after a rate-limit / secondary 403 (doubles up to 15m).
    #[arg(long, env = "GHA_API_BACKOFF_SECS", default_value_t = DEFAULT_API_BACKOFF_SECS, global = true)]
    api_backoff_secs: u64,

    /// Allowlisted repos checked **per listen tick** (round-robin). `1` = stagger one
    /// repo every interval (each of N repos ~ every N×interval). `0` = whole allowlist
    /// each tick (still paced by `api_min_gap_ms`). Default 1.
    #[arg(long, env = "GHA_REPOS_PER_TICK", default_value_t = DEFAULT_REPOS_PER_TICK, global = true)]
    repos_per_tick: u32,

    /// Min seconds between registration-token POSTs (host-wide file lock). Default 5.
    #[arg(long, env = "GHA_REG_MIN_GAP_SECS", default_value_t = DEFAULT_REG_MIN_GAP_SECS, global = true)]
    reg_min_gap_secs: u64,

    /// Max registration-token POSTs per rolling hour (host-wide). Default 90.
    #[arg(long, env = "GHA_REG_MAX_PER_HOUR", default_value_t = DEFAULT_REG_MAX_PER_HOUR, global = true)]
    reg_max_per_hour: u32,

    /// Host pool: total CPUs for all ephemeral workers (shared file lock). Default 8.
    #[arg(long, env = "GHA_POOL_CPUS", default_value = "8", global = true)]
    pool_cpus: String,

    /// Host pool: total memory for all ephemeral workers (e.g. 8g). Default 8g.
    #[arg(long, env = "GHA_POOL_MEMORY", default_value = "8g", global = true)]
    pool_memory: String,

    /// Max concurrent ephemeral workers this listen process may own. Default 16.
    #[arg(
        long,
        env = "GHA_POOL_MAX_WORKERS",
        default_value_t = 16,
        global = true
    )]
    pool_max_workers: u32,

    /// Enable dynamic multi-worker pool sizing (default true).
    #[arg(long, env = "GHA_POOL_MODE", default_value = "dynamic", global = true)]
    pool_mode: String,

    /// Path to label→image map (JSON or minimal TOML). Merged over built-in distro defaults.
    /// See docs/WORK_IMAGES.md and packaging/image-map.example.json. Issue #28.
    #[arg(long, env = "GHA_IMAGE_MAP", global = true)]
    image_map: Option<PathBuf>,

    /// Podman `--platform` for work containers (e.g. `linux/arm64`). Usually set from
    /// job arch labels at spawn; CLI/env override applies to single-container `up`.
    #[arg(long, env = "GHA_PLATFORM", global = true)]
    platform: Option<String>,

    /// GitHub App ID for installation-token auth (an alternative to GH_TOKEN/PAT with a
    /// much higher, installation-scoped rate limit). Requires --app-private-key too.
    /// See docs/GITHUB_APP_AUTH.md. Setting only some App-auth flags is a hard error,
    /// not a silent fall-back to GH_TOKEN — run `doctor` to check what's configured.
    #[arg(long, env = "GHA_APP_ID", global = true)]
    app_id: Option<String>,

    /// GitHub App installation id. Optional: when omitted (with --app-id and
    /// --app-private-key set), it is auto-discovered via `GET /app/installations` —
    /// picking an owner-matched installation if `--owner`/`--user`/`--repo`'s owner
    /// disambiguates, else the sole installation, else failing with the list of
    /// candidates. See `doctor` and docs/GITHUB_APP_AUTH.md.
    #[arg(long, env = "GHA_APP_INSTALLATION_ID", global = true)]
    app_installation_id: Option<String>,

    /// GitHub App private key: `secret:<group>/<key>` (recommended — retrieved from the
    /// vault via the `secret` CLI, decrypted only into a 0600 tmpfs file for the span of
    /// a signing operation), `file:<path>`, or a bare filesystem path. Never inline PEM
    /// content — that is refused outright (readable via `/proc/<pid>/environ`, leaks
    /// into shell history / env dumps / CI logs, can't be chmod-restricted).
    #[arg(long, env = "GHA_APP_PRIVATE_KEY", global = true)]
    app_private_key: Option<String>,
}

impl Cli {
    /// The HTTP seam configuration for this invocation.
    ///
    /// This is the **single** place production code decides which host it talks to.
    /// Today it is unconditionally GitHub; it is a method on `Cli` rather than a bare
    /// constant so that the eventual forge selection has exactly one seat to take,
    /// and so every call site already reads `cli.http()` instead of a literal.
    fn http(&self) -> HttpConfig {
        HttpConfig::github()
    }

    /// Resolves the flat repo allowlist, preferring the honestly-named `GHA_ALLOWLIST_REPOS`
    /// over the deprecated `GHA_PREFER_REPOS` (WP-09 rename: it's an allowlist, not an
    /// ordering). If both are set, `GHA_ALLOWLIST_REPOS` wins silently — no need to warn about
    /// the one being used correctly. If only the old name is set, a one-line deprecation
    /// warning goes to stderr; the value is still honored — this is a warning window, not a
    /// breaking change. Do not delete `prefer_repos` parsing without a real deprecation period;
    /// fleets pin config and won't all move on the same day.
    fn effective_allowlist_repos(&self) -> Option<String> {
        let v = self.effective_allowlist_repos_quiet()?;
        if self.allowlist_repos.is_none() {
            static WARNED: AtomicBool = AtomicBool::new(false);
            warn_deprecated_once(
                &WARNED,
                "listen: warning: GHA_PREFER_REPOS is deprecated, rename to GHA_ALLOWLIST_REPOS \
                 (identical flat-allowlist behavior; GHA_PREFER_REPOS support will be removed in \
                 a future release)",
            );
        }
        Some(v)
    }

    /// Same resolution as `effective_allowlist_repos` but silent — for presence/shape checks
    /// (e.g. `validate_cli`) that shouldn't print the deprecation warning a second time.
    fn effective_allowlist_repos_quiet(&self) -> Option<String> {
        self.allowlist_repos
            .as_ref()
            .or(self.prefer_repos.as_ref())
            .cloned()
    }

    /// Resolves the allowlist FILE path, preferring `GHA_ALLOWLIST_REPOS_FILE` over the
    /// deprecated `GHA_PREFER_REPOS_FILE`. Same precedence and same warning discipline as
    /// the inline pair above: new name wins, old name still works, and when only the old
    /// name is set the operator is told once.
    fn effective_allowlist_repos_file(&self) -> Option<&String> {
        if self.allowlist_repos_file.is_some() {
            return self.allowlist_repos_file.as_ref();
        }
        if self.prefer_repos_file.is_some() {
            static WARNED: AtomicBool = AtomicBool::new(false);
            warn_deprecated_once(
                &WARNED,
                "listen: warning: GHA_PREFER_REPOS_FILE is deprecated, rename to \
                 GHA_ALLOWLIST_REPOS_FILE (identical allowlist-file behavior; the old name will \
                 be removed in a future release)",
            );
        }
        self.prefer_repos_file.as_ref()
    }

    /// Build a GitHub App auth config from the already-clap-resolved `--app-*` fields
    /// (flag beats env, exactly like every other option — clap did that resolution when
    /// it parsed `Cli`, so this just reads the fields; it never touches `std::env::var`
    /// itself). `Ok(None)` means "nothing App-auth-shaped is set, use GH_TOKEN/PAT."
    /// `Err` means App auth was *attempted* but is incomplete or invalid — callers must
    /// treat that as a hard failure, not a silent fall-back (see `appauth` module docs).
    fn app_auth_config(&self) -> Result<Option<appauth::AppAuthConfig>, String> {
        appauth::resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => self.app_id.clone(),
            "GHA_APP_INSTALLATION_ID" => self.app_installation_id.clone(),
            "GHA_APP_PRIVATE_KEY" => self.app_private_key.clone(),
            _ => None,
        })
    }

    /// Best-effort owner login to disambiguate App-auth installation auto-discovery:
    /// `--owner` (org scope), else `--user` (user scope), else the owner half of
    /// `--repo owner/name`. `None` when nothing is resolved yet (e.g. `doctor` run with
    /// no scope flags at all) — auto-discovery still works via the sole-installation
    /// fallback, it just can't disambiguate multiple installations without this.
    fn app_auth_owner_hint(&self) -> Option<String> {
        self.owner
            .clone()
            .or_else(|| self.user.clone())
            .or_else(|| {
                self.repo
                    .as_ref()
                    .and_then(|r| r.split('/').next())
                    .map(str::to_string)
            })
    }
}

#[derive(Debug, Subcommand, Clone)]
pub enum Cmd {
    /// Obtain work image (build packaging or pull external) + seed runner volume
    Prepare {
        #[arg(long, default_value_t = true)]
        with_container: bool,
        /// Skip apt/dnf host package refresh before building the snapshot
        #[arg(long, env = "GHA_SKIP_HOST_UPDATE", default_value_t = false)]
        skip_host_update: bool,
    },
    /// Register + start for the resolved target
    Up,
    Down {
        #[arg(long, default_value_t = true)]
        rm: bool,
    },
    Status,
    /// Print resolved registration target (repo/org/user batch) without starting
    Detect,
    /// Poll for demand; up/down. With scope=user, re-targets registration per repo.
    /// Prefer retain + warm for steady state (GitHub pushes jobs; little API needed).
    Listen {
        #[arg(long, default_value_t = DEFAULT_LISTEN_INTERVAL_SECS)]
        interval: u64,
        #[arg(long, default_value_t = 180)]
        idle_secs: u64,
        #[arg(long, env = "GHA_WAKE_PORT")]
        wake_port: Option<u16>,
    },
    /// Gently batch-register **retain** runners for `GHA_PREFER_REPOS` (or one --repo).
    /// One container/volume/name per repo; paced registration-token POSTs.
    /// After warm, runners stay online and GitHub **pushes** jobs (no demand storm).
    Warm {
        /// Seconds between registration-token mints (default: max of reg_min_gap and 8).
        #[arg(long, default_value_t = 8)]
        gap_secs: u64,
        /// If true, also start containers after register (default true).
        #[arg(long, default_value_t = true)]
        start: bool,
    },
    /// Safe local recovery: free orphan pool claims + exited workers so new jobs can be picked up.
    /// **Never** cancels GitHub workflow runs or deletes the Actions queue.
    Recover {
        /// Also force-rm exited fleet containers not in the claim set (default true).
        #[arg(long, default_value_t = true)]
        prune_exited: bool,
        /// Print JSON summary to stdout (default false).
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Check auth configuration without printing secrets: which credential path is
    /// active (GitHub App vs GH_TOKEN/PAT/gh/GCM/config/interactive), App identity +
    /// installation + granted permissions when App auth is configured, and the live
    /// `GET /rate_limit` budget. A separate command (not folded into `status`, which
    /// reports container/volume/registration state and requires a resolved scope) so
    /// it runs with no other flags and exits non-zero with actionable text per failed
    /// check — the thing to run first when a repo/user says jobs aren't picking up.
    Doctor,
}

/// Shared gate for both debug dumps: `GHA_DEBUG=1` always enables; otherwise
/// `GHA_DEBUG_ON_ERR` unset/`1`/`true`/`yes` (default ON while stabilizing the fleet
/// agent / rootless path) enables, `GHA_DEBUG_ON_ERR=0` silences.
fn debug_dump_enabled() -> bool {
    let always = env_truthy("GHA_DEBUG");
    let on_err = match std::env::var("GHA_DEBUG_ON_ERR") {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "YES" | ""),
        Err(_) => true,
    };
    always || on_err
}

/// Keys dumped by [`debug_dump_on_error`] and [`debug_dump_fail_closed`]. Every one of
/// these must also be an exact entry in `dump_redact::DUMP_ALLOWLIST` — see
/// `dump_resolved_env`, which debug-asserts that on every call so a typo here fails
/// loudly in dev/test builds rather than silently starting to print an unvetted key.
const DEBUG_DUMP_ENV_KEYS: &[&str] = &[
    "HOME",
    "XDG_RUNTIME_DIR",
    "CONTAINER_HOST",
    "GHA_ALLOW_ROOT",
    "GHA_SCOPE",
    "GHA_USER",
    "GHA_REPO",
    "GHA_PREFER_REPOS",
    "GHA_ALLOWLIST_REPOS",
    "GHA_MODE",
    "GHA_CONTAINER",
    "GHA_VOLUME",
    "GHA_IMAGE",
    "GHA_GPU",
    "GHA_APP_ID",
    "GHA_APP_INSTALLATION_ID",
    "GHA_APP_PRIVATE_KEY",
];

/// Print each set env var in `keys`, redacted per [`dump_redact`] — an **allowlist** of
/// exact key names plus a value-shape check, replacing the old `key.contains("TOKEN") ||
/// key.contains("SECRET")` blocklist this function used to run. A blocklist only knows
/// what it has been taught to reject; see the `dump_redact` module docs.
fn dump_resolved_env(keys: &[&str]) {
    for key in keys {
        debug_assert!(
            is_allowlisted_key(key),
            "debug dump key {key:?} is not in dump_redact::DUMP_ALLOWLIST — add it there \
             first so its value shape is actually validated"
        );
        if let Ok(v) = std::env::var(key) {
            let field = redact_for_dump(key, &v);
            eprintln!("{}={}", field.key, field.value);
        }
    }
}

/// Text that has passed through [`redact_free_text`] and is therefore safe to print
/// verbatim. Private field, single redacting constructor — no other way to build one
/// — the same pattern as [`RedactedCommand`] and [`FailClosedEvent::redacted`], but
/// for the general case of "any free-text field a dump function is about to print",
/// not specifically a command string.
///
/// Exists because of exactly how the issue #132 third follow-up audit's finding
/// happened: [`debug_dump_fail_closed`]'s sibling [`debug_dump_on_error`] printed its
/// `err` parameter via a bare `eprintln!("error: {err}")` with **no** redaction of its
/// own, relying entirely on its one caller (`main.rs`) having pre-scrubbed it — and
/// that caller used the old, materially weaker `redact()` blocklist (now retired; see
/// its doc comment), so an AWS-shaped secret sailed through untouched. A type that can
/// only be constructed pre-redacted makes that specific failure mode structurally
/// unreachable in [`write_debug_dump_on_error`] the same way [`RedactedCommand`]
/// already made it unreachable for `command` in [`write_debug_dump_fail_closed`]: every
/// value that function prints is this type, not `&str`, so there is no raw path left
/// to accidentally reach for.
struct RedactedText(String);

impl RedactedText {
    fn new(raw: &str) -> Self {
        RedactedText(redact_free_text(raw))
    }
}

impl fmt::Display for RedactedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Dump troubleshooting context after a failure (no secrets).
///
/// Enabled when `GHA_DEBUG=1` or `GHA_DEBUG_ON_ERR` is unset/`1` (default on).
/// Disable with `GHA_DEBUG_ON_ERR=0` once the stack is stable.
///
/// Gathers `err` plus best-effort environment/subprocess context, then hands
/// everything to `write_debug_dump_on_error` for the actual printing — that
/// function, not this one, is what carries the redaction guarantee (issue #132 third
/// follow-up audit): see its doc comment.
pub fn debug_dump_on_error(err: &str) {
    if !debug_dump_enabled() {
        return;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "?".into());
    let euid_root = effective_uid_is_root();
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    dump_resolved_env(DEBUG_DUMP_ENV_KEYS);
    let podman = gather_podman_dump_snapshot();
    let mut stderr = std::io::stderr();
    // Infallible by convention (matches every other eprintln!-based dump in this
    // file): a broken stderr pipe should never turn a debug dump into a panic.
    let _ = write_debug_dump_on_error(&mut stderr, err, &user, euid_root, cwd.as_deref(), &podman);
}

/// Best-effort `podman info` + `podman ps -a` snapshot, gathered separately from
/// [`write_debug_dump_on_error`] so that function's signature stays a handful of
/// arguments instead of one-per-subprocess-output-stream (clippy's
/// `too_many_arguments`, and more importantly: fewer independent raw-string
/// parameters for a future editor to accidentally interpolate directly instead of
/// through [`RedactedText`]).
struct PodmanDumpSnapshot {
    stdout: Option<String>,
    stderr: Option<String>,
    unrunnable_err: Option<String>,
    ps_lines: Vec<String>,
}

fn gather_podman_dump_snapshot() -> PodmanDumpSnapshot {
    let (stdout, stderr, unrunnable_err) = match Command::new("podman")
        .args([
            "info",
            "--format",
            "rootless={{.Host.Security.Rootless}} runtime={{.Host.OCIRuntime.Name}}",
        ])
        .output()
    {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let stdout = (!stdout.is_empty()).then_some(stdout);
            let stderr = (!o.status.success() && !stderr.is_empty()).then_some(stderr);
            (stdout, stderr, None)
        }
        Err(e) => (None, None, Some(e.to_string())),
    };
    let ps_lines: Vec<String> = Command::new("podman")
        .args([
            "ps",
            "-a",
            "--format",
            "{{.Names}}\t{{.Status}}\t{{.Image}}",
        ])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .take(15)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    PodmanDumpSnapshot {
        stdout,
        stderr,
        unrunnable_err,
        ps_lines,
    }
}

/// Core of [`debug_dump_on_error`], factored out to write to any [`Write`] rather than
/// stderr directly — the identical reasoning and pattern as
/// [`write_debug_dump_fail_closed`] (issue #132 third follow-up audit: this is the
/// concrete test surface that was missing for the plaintext `debug_dump_on_error`
/// path, the gap that let the round-3 finding through).
///
/// EVERY parameter here is raw, caller-supplied text (an error message, subprocess
/// stdout/stderr, a `podman ps` line, `$USER`, `$PWD`) — none of it is assumed safe.
/// Each one is wrapped in [`RedactedText::new`] at the exact point it is written, never
/// interpolated directly; there is no field in this function's body that reaches `w`
/// without going through that constructor first. That is the structural guarantee this
/// function makes testable: feed it a synthetic credential in any parameter and assert
/// it never reaches the buffer.
fn write_debug_dump_on_error<W: Write>(
    w: &mut W,
    err: &str,
    user: &str,
    euid_root: bool,
    cwd: Option<&str>,
    podman: &PodmanDumpSnapshot,
) -> std::io::Result<()> {
    writeln!(w, "========== gha-runner-ctl DEBUG ON ERROR ==========")?;
    writeln!(w, "error:      {}", RedactedText::new(err))?;
    writeln!(
        w,
        "user:       {} euid_root={}",
        RedactedText::new(user),
        euid_root
    )?;
    if let Some(cwd) = cwd {
        writeln!(w, "pwd:        {}", RedactedText::new(cwd))?;
    }
    if let Some(s) = &podman.stdout {
        writeln!(w, "podman:     {}", RedactedText::new(s))?;
    }
    if let Some(e) = &podman.stderr {
        writeln!(w, "podman_err: {}", RedactedText::new(e))?;
    }
    if let Some(e) = &podman.unrunnable_err {
        writeln!(w, "podman:     not runnable ({})", RedactedText::new(e))?;
    }
    for (i, line) in podman.ps_lines.iter().take(15).enumerate() {
        if i == 0 {
            writeln!(w, "--- podman ps -a (max 15) ---")?;
        }
        writeln!(w, "{}", RedactedText::new(line))?;
    }
    writeln!(
        w,
        "hint:       GHA_DEBUG=1 for more; GHA_DEBUG_ON_ERR=0 to silence"
    )?;
    writeln!(w, "===================================================")?;
    Ok(())
}

/// A command string that has passed through [`redact_free_text`] and is therefore safe
/// to print verbatim. The only way to obtain one is [`RedactedCommand::new`], which
/// redacts on construction — no public field, no other constructor. This mirrors
/// [`FailClosedEvent::redacted`]'s pattern: [`write_debug_dump_fail_closed`] physically
/// cannot print a raw `command` because it never receives one, closing the gap
/// structurally (MEDIUM-B, issue #132 second follow-up audit) rather than relying on
/// every call site to pre-redact or on this dump remembering to call
/// [`redact_free_text`] itself before printing.
///
/// `command` is caller-supplied free text describing what was run (e.g. `"podman top
/// <container>"`) — today's single call site passes a static literal, but nothing
/// stopped (and a future caller reasonably might build) a dynamic command string that
/// embeds a resolved value, including one that turns out to hold a credential. Same
/// hostile-input treatment as [`FailClosedEvent`]'s `reason` field: scanned regardless
/// of whether the caller already redacted it.
struct RedactedCommand(String);

impl RedactedCommand {
    fn new(raw: &str) -> Self {
        RedactedCommand(redact_free_text(raw))
    }
}

impl fmt::Display for RedactedCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Developer debug dump for a single fail-closed decision (issue #132).
///
/// A fail-closed event is usually a logic or environment bug, not a transient — this
/// prints exactly what a developer needs to tell those apart in one pass: the failing
/// command, its real exit status and (redacted) stderr, the resolved inputs that fed
/// the decision, and what the code assumed as a result. Same `GHA_DEBUG`/
/// `GHA_DEBUG_ON_ERR` gate as [`debug_dump_on_error`].
///
/// `resolved_inputs` are dumped through the exact same `dump_redact` allowlist as
/// [`debug_dump_on_error`] — an unrecognised key is printed as
/// `***REDACTED(key_not_allowlisted)***`, never silently skipped, so a caller that
/// passes something new finds out immediately rather than assuming it was included.
///
/// `command` is redacted immediately via `RedactedCommand::new` (MEDIUM-B) before it
/// ever reaches `write_debug_dump_fail_closed`.
pub fn debug_dump_fail_closed(
    ev: &FailClosedEvent,
    command: &str,
    resolved_inputs: &[(&str, String)],
) {
    if !debug_dump_enabled() {
        return;
    }
    let mut stderr = std::io::stderr();
    let command = RedactedCommand::new(command);
    // Infallible by convention (matches every other eprintln!-based dump in this
    // file): a broken stderr pipe should never turn a debug dump into a panic.
    let _ = write_debug_dump_fail_closed(&mut stderr, ev, &command, resolved_inputs);
}

/// Core of [`debug_dump_fail_closed`], factored out to write to any [`Write`] rather
/// than stderr directly, so the redaction guarantee ("every field printed here comes
/// from `ev`'s already-redacted getters, never a raw field") is unit-testable against
/// an in-memory buffer instead of only provable by inspection (issue #132 follow-up
/// audit — this is the concrete test surface for the plaintext-dump half of HIGH-1 /
/// HIGH-2 / MEDIUM-3, alongside [`FailClosedEvent::to_json_line`] for the WARN-event
/// half). `command` takes [`RedactedCommand`], not `&str` (MEDIUM-B, second follow-up
/// audit) — there is structurally no `&str` overload here for a future caller to reach
/// for and accidentally print a raw command unredacted.
fn write_debug_dump_fail_closed<W: Write>(
    w: &mut W,
    ev: &FailClosedEvent,
    command: &RedactedCommand,
    resolved_inputs: &[(&str, String)],
) -> std::io::Result<()> {
    writeln!(
        w,
        "========== gha-runner-ctl DEBUG (fail-closed) =========="
    )?;
    writeln!(w, "check:       {}", ev.check())?;
    writeln!(w, "object:      {}", ev.object())?;
    writeln!(w, "assumed:     {}", ev.assumed())?;
    writeln!(w, "consecutive: {} (since {})", ev.consecutive, ev.since)?;
    writeln!(w, "command:     {command}")?;
    // ev.reason() is the real error including exit status. It is redacted
    // unconditionally on construction (FailClosedEvent::redacted, via
    // dump_redact::redact_free_text) regardless of what the caller passes — this no
    // longer depends on the caller having pre-redacted it (issue #132 follow-up audit,
    // HIGH-2).
    writeln!(w, "reason:      {}", ev.reason())?;
    writeln!(w, "resolved inputs:")?;
    let pairs: Vec<(&str, &str)> = resolved_inputs
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();
    for field in redact_env_dump(pairs) {
        writeln!(w, "  {}={}", field.key, field.value)?;
    }
    writeln!(
        w,
        "hint:        GHA_DEBUG=1 for more; GHA_DEBUG_ON_ERR=0 to silence"
    )?;
    writeln!(
        w,
        "=========================================================="
    )?;
    Ok(())
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "YES"))
        .unwrap_or(false)
}

/// Fleet agent must not run as root in production.
///
/// WSL / ephemeral dev containers often start as root — set `GHA_ALLOW_ROOT=1` only
/// for bootstrap there. Production path: dedicated `gha-agent` user + rootless Podman
/// (`scripts/setup-rootless.sh`). No sudoer, shell=nologin.
pub fn refuse_root_unless_allowed() {
    if !effective_uid_is_root() {
        return;
    }
    let allow = std::env::var("GHA_ALLOW_ROOT")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "YES"))
        .unwrap_or(false);
    if allow {
        eprintln!(
            "gha-runner-ctl: WARNING running as root (GHA_ALLOW_ROOT set) — \
             use only in ephemeral WSL/dev bootstrap; production = gha-agent + rootless"
        );
        return;
    }
    eprintln!(
        "gha-runner-ctl ERROR: refusing to run as root.\n\
         Fleet agent identity: unprivileged user (e.g. gha-agent), rootless Podman, no sudo.\n\
         Bootstrap once:  sudo bash scripts/setup-rootless.sh\n\
         Then:            sudo -u gha-agent -H env XDG_RUNTIME_DIR=/run/user/$(id -u gha-agent) …\n\
         Ephemeral dev only:  GHA_ALLOW_ROOT=1 gha-runner-ctl …"
    );
    std::process::exit(78); // EX_CONFIG
}

/// Effective UID without `unsafe` (crate forbids unsafe_code). Parses `/proc/self/status`.
fn effective_uid_is_root() -> bool {
    #[cfg(unix)]
    {
        if let Ok(s) = fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                // Uid: real effective saved fs
                if let Some(rest) = line.strip_prefix("Uid:") {
                    let mut parts = rest.split_whitespace();
                    let _real = parts.next();
                    if let Some(euid) = parts.next() {
                        return euid == "0";
                    }
                }
            }
            // Parsed status but no Uid line — fail-closed (treat as root).
            return true;
        }
        // Unreadable /proc — fail-closed: refuse unless GHA_ALLOW_ROOT.
        true
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Checks for raw token patterns in CLI arguments. If found, prints an error message and exits.
/// This prevents users from leaking secrets in shell history, process listings, or logs.
pub fn prevent_raw_token_args() {
    let token_prefixes = ["ghp_", "gho_", "ghu_", "ghs_", "github_pat_"];
    for arg in std::env::args() {
        for prefix in token_prefixes {
            if arg.contains(prefix) {
                eprintln!("gha-runner-ctl ERROR: Raw GitHub token/PAT pattern detected in command line arguments!");
                eprintln!("We take an opinionated stance on security: we do NOT allow passing secrets directly via CLI arguments to prevent history or process logs exposure.");
                eprintln!("Please run without token arguments. We will securely prompt you interactively, retrieve it via Git Credential Manager, or load it from config.");
                eprintln!("\nTo scrub this command from your shell history:");
                eprintln!("  - In Bash: history -d $(history | tail -n 2 | head -n 1 | awk '{{print $1}}') (or edit ~/.bash_history)");
                eprintln!("  - In Zsh:  fc -W && fc -R (or edit ~/.zsh_history)");
                std::process::exit(127);
            }
        }
        // Same rationale, for App private key material: `--app-private-key` only ever
        // accepts `secret:<group>/<key>`, `file:<path>`, or a bare path — inline PEM on
        // argv would land in /proc/<pid>/cmdline and shell history just like a raw PAT.
        if arg.contains("-----BEGIN") {
            eprintln!(
                "gha-runner-ctl ERROR: inline PEM key material detected in command line arguments!"
            );
            eprintln!("--app-private-key only accepts secret:<group>/<key>, file:<path>, or a bare path — never inline key content.");
            eprintln!("\nTo scrub this command from your shell history:");
            eprintln!("  - In Bash: history -d $(history | tail -n 2 | head -n 1 | awk '{{print $1}}') (or edit ~/.bash_history)");
            eprintln!("  - In Zsh:  fc -W && fc -R (or edit ~/.zsh_history)");
            std::process::exit(127);
        }
    }
}

pub fn run() -> Result<(), String> {
    let mut cli = Cli::parse();

    // `doctor` is a read-only diagnostic and deliberately bypasses scope resolution/
    // validation (which requires --repo/--owner/--user or --auto): it should run with
    // no other flags at all, since its whole point is "check auth before anything else
    // needs a resolved target."
    if matches!(cli.cmd, Some(Cmd::Doctor)) {
        return doctor(&cli);
    }

    resolve_cli(&mut cli)?;
    validate_cli(&cli)?;

    if cli.full_auto {
        let has_vol = volume_exists(&cli.volume);
        let has_img = podman(&["image", "exists", &cli.image]).is_ok();
        if !has_vol || !has_img {
            eprintln!(
                "full-auto: missing Podman volume or image. Triggering automated prepare first..."
            );
            prepare(&cli, true, false)?;
        }
    }

    let cmd = match cli.cmd.as_ref() {
        Some(c) => c.clone(),
        None => {
            if cli.full_auto {
                eprintln!("full-auto: initiating automated listener/handler...");
                Cmd::Listen {
                    interval: DEFAULT_LISTEN_INTERVAL_SECS,
                    idle_secs: 500,
                    wake_port: None,
                }
            } else {
                return Err(
                    "No command specified. Run with --help for options, or use --full-auto.".into(),
                );
            }
        }
    };

    match cmd {
        Cmd::Prepare {
            with_container,
            skip_host_update,
        } => prepare(&cli, with_container, skip_host_update),
        Cmd::Up => {
            let _lock = InstanceLock::acquire("up", &cli.container)?;
            up(&cli)
        }
        Cmd::Down { rm } => down(&cli, rm),
        Cmd::Status => status(&cli),
        Cmd::Detect => {
            print_detect(&cli);
            Ok(())
        }
        Cmd::Listen {
            interval,
            idle_secs,
            wake_port,
        } => {
            let interval = interval.clamp(MIN_POLL_SECS, MAX_POLL_SECS);
            let idle_secs = idle_secs.clamp(MIN_IDLE_SECS, MAX_IDLE_SECS);
            let _lock = InstanceLock::acquire("listen", &cli.container)?;
            listen(&cli, interval, idle_secs, wake_port)
        }
        Cmd::Warm { gap_secs, start } => warm(&cli, gap_secs, start),
        Cmd::Recover { prune_exited, json } => recover(&cli, prune_exited, json),
        Cmd::Doctor => doctor(&cli),
    }
}

// --- Resolve auto / batch context --------------------------------------------

fn get_user_login_from_token(token: &str, http: &HttpConfig) -> Result<String, String> {
    #[derive(Deserialize)]
    struct UserResponse {
        login: String,
    }

    let url = http.api_url("user");
    let resp = http_agent(http)
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("Failed to get user info from token: {e}"))?;

    if resp.status() != 200 {
        return Err(format!("GET /user returned HTTP {}", resp.status()));
    }

    let body: UserResponse = resp
        .into_json()
        .map_err(|e| format!("Failed to parse user info: {e}"))?;
    Ok(body.login)
}

fn resolve_cli(cli: &mut Cli) -> Result<(), String> {
    if let Some(ref target) = cli.this_repo_only {
        let cleaned = target
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        let parts: Vec<&str> = cleaned.split('/').collect();
        if parts.len() == 3 {
            cli.scope = Scope::Repo;
            cli.repo = Some(format!("{}/{}", parts[1], parts[2]));
        } else if parts.len() == 2 {
            cli.scope = Scope::Repo;
            cli.repo = Some(format!("{}/{}", parts[0], parts[1]));
        } else {
            return Err(
                "invalid format for --this-repo-only. Expected [platform/]username/repo_name"
                    .into(),
            );
        }
    }

    if cli.full_auto {
        cli.auto = true;
        if cli.this_repo_only.is_none() && cli.repo.is_none() {
            if let Ok(detected) = detect_repo_from_cwd() {
                eprintln!("full-auto: detected repository {detected}");
                cli.repo = Some(detected);
                cli.scope = Scope::Repo;
            } else {
                eprintln!("full-auto: not in a git checkout. Defaulting to personal user-level batch scope.");
                cli.scope = Scope::User;
            }
        }
    }

    if cli.auto && cli.scope == Scope::Repo && cli.repo.is_none() {
        let detected = detect_repo_from_cwd()?;
        eprintln!("auto: detected repository {detected}");
        cli.repo = Some(detected);
    }

    if cli.scope == Scope::User && cli.user.is_none() {
        let u = if let Ok(login) = gh_login() {
            login
        } else if let Ok(tok) = github_token(cli) {
            get_user_login_from_token(&tok, &cli.http())?
        } else {
            return Err("Could not resolve authenticated user login. Please log in using 'gh auth login' or provide a token.".into());
        };
        eprintln!("user: authenticated login {u}");
        cli.user = Some(u);
    }

    // Convenience: GHA_BATCH=1 implies user scope for current gh user
    if std::env::var("GHA_BATCH").ok().as_deref() == Some("1") && cli.scope == Scope::Repo {
        cli.scope = Scope::User;
        if cli.user.is_none() {
            cli.user = Some(gh_login()?);
        }
        eprintln!(
            "batch: scope=user owner={}",
            cli.user.as_deref().unwrap_or("?")
        );
    }
    Ok(())
}

/// Detect owner/repo from cwd: prefer `gh repo view`, else `git remote get-url origin`.
pub fn detect_repo_from_cwd() -> Result<String, String> {
    if let Ok(out) = Command::new("gh")
        .args([
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "-q",
            ".nameWithOwner",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if is_safe_repo(&s) {
                return Ok(s);
            }
        }
    }
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("git remote failed: {e}"))?;
    if !out.status.success() {
        return Err(
            "could not detect repo (run inside a github checkout, or pass --repo / GHA_REPO)"
                .into(),
        );
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    parse_github_remote(&url).ok_or_else(|| format!("origin is not a github remote: {url}"))
}

pub fn parse_github_remote(url: &str) -> Option<String> {
    // git@github.com:owner/repo.git  or  https://github.com/owner/repo.git
    let s = url.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some(rest) = s.strip_prefix("git@github.com:") {
        return Some(rest.to_string()).filter(|r| is_safe_repo(r));
    }
    if let Some(rest) = s.strip_prefix("https://github.com/") {
        return Some(rest.to_string()).filter(|r| is_safe_repo(r));
    }
    if let Some(rest) = s.strip_prefix("ssh://git@github.com/") {
        return Some(rest.to_string()).filter(|r| is_safe_repo(r));
    }
    None
}

fn gh_login() -> Result<String, String> {
    let out = Command::new("gh")
        .args(["api", "user", "-q", ".login"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("gh api user failed: {e}"))?;
    if !out.status.success() {
        return Err("could not resolve authenticated user (gh auth login)".into());
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !is_safe_ident(&s) {
        return Err("invalid login from gh api".into());
    }
    Ok(s)
}

fn print_detect(cli: &Cli) {
    println!("scope: {:?}", cli.scope);
    match cli.scope {
        Scope::Repo => {
            println!("repo: {}", cli.repo.as_deref().unwrap_or("(unset)"));
            if cli.repo.is_some() {
                println!("register_url: {}", github_url(cli));
            }
        }
        Scope::Org => {
            println!("org: {}", cli.owner.as_deref().unwrap_or("(unset)"));
            println!("register_url: {}", github_url(cli));
        }
        Scope::User => {
            println!("user: {}", cli.user.as_deref().unwrap_or("(unset)"));
            println!("mode: batch personal repos (ephemeral re-register per demand)");
            println!("register_url: (selected per demand at listen time)");
        }
    }
    println!("labels: {}", cli.labels);
    println!("container: {}", cli.container);
}

// --- Validation / redaction --------------------------------------------------

pub fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn is_safe_repo(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() == 2 && parts.iter().all(|p| is_safe_ident(p))
}

/// OCI image reference safety: registry/path:tag or @sha256:hex, optional host:port.
/// Rejects shell metacharacters and path traversal; allows common registry punctuation.
pub fn is_safe_image(s: &str) -> bool {
    if s.is_empty() || s.len() > 384 || s.contains("..") {
        return false;
    }
    // No whitespace or shell metacharacters.
    if s.chars().any(|c| {
        c.is_ascii_whitespace()
            || matches!(
                c,
                ';' | '|'
                    | '&'
                    | '$'
                    | '`'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '\''
                    | '"'
                    | '\\'
                    | '\n'
                    | '\r'
            )
    }) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'))
}

/// Stock packaging default tag (only used by `ImageMode::Auto` convenience).
pub fn is_default_stock_image(image: &str) -> bool {
    let img = image.trim();
    img == DEFAULT_IMAGE
        || img == "localhost/gha-runner-ctl"
        || img.starts_with("localhost/gha-runner-ctl:")
}

pub fn is_safe_runner_user(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    // name, uid, name:group, uid:gid
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

pub fn is_safe_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn is_safe_runner_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub fn is_safe_url(s: &str) -> bool {
    (s.starts_with("https://") || s.starts_with("http://"))
        && s.len() <= 512
        && !s.contains("..")
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '_' | '.' | '/' | ':' | '?' | '=' | '&' | '%' | '+' | '~'
                )
        })
}

/// Resolve auto → build|external without locking users to a single image name.
pub fn effective_image_mode(mode: &ImageMode, image: &str) -> ImageMode {
    match mode {
        ImageMode::Auto => {
            if is_default_stock_image(image) {
                ImageMode::Build
            } else {
                ImageMode::External
            }
        }
        other => other.clone(),
    }
}

pub fn effective_pull_policy(cli_policy: Option<&PullPolicy>, mode: &ImageMode) -> PullPolicy {
    if let Some(p) = cli_policy {
        return p.clone();
    }
    match mode {
        ImageMode::Build => PullPolicy::Never,
        ImageMode::External | ImageMode::Auto => PullPolicy::Missing,
    }
}

pub fn pull_policy_arg(p: &PullPolicy) -> &'static str {
    match p {
        PullPolicy::Never => "never",
        PullPolicy::Missing => "missing",
        PullPolicy::Always => "always",
    }
}

pub fn is_safe_labels(s: &str) -> bool {
    let parts: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    !parts.is_empty()
        && parts.len() <= 16
        && parts.iter().all(|p| is_safe_ident(p) && p.len() <= 64)
}

pub fn is_safe_cpus(s: &str) -> bool {
    if s.is_empty() || s.len() > 8 {
        return false;
    }
    s.parse::<f64>().is_ok_and(|n| n > 0.0 && n <= 64.0)
}

pub fn is_safe_memory(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 16 {
        return false;
    }
    let (num, unit) = s
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map_or((s, ""), |(i, _)| (&s[..i], &s[i..]));
    if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    matches!(
        unit.to_ascii_lowercase().as_str(),
        "" | "b" | "k" | "m" | "g" | "t" | "ki" | "mi" | "gi" | "ti" | "kb" | "mb" | "gb" | "tb"
    )
}

/// Truncate `s` to at most `max_bytes` (on a char boundary), appending `…` when it
/// was cut. Pulled out of the old `redact()` blocklist scrubber so the length cap
/// survives independently of which redaction strategy sits in front of it.
fn truncate_for_dump(mut s: String, max_bytes: usize) -> String {
    if s.len() > max_bytes {
        let mut truncate_at = max_bytes;
        while truncate_at > 0 && !s.is_char_boundary(truncate_at) {
            truncate_at -= 1;
        }
        s.truncate(truncate_at);
        s.push('…');
    }
    s
}

/// Free-text credential scrubber for arbitrary error/subprocess-output strings.
///
/// This used to be an independent 8-entry prefix blocklist (`ghp_`/`gho_`/.../`Bearer
/// `/`RUNNER_TOKEN=`) with no length or shape check on what followed each prefix —
/// materially weaker than [`dump_redact::redact_free_text`], which the fail-closed
/// and plaintext-dump paths use. Having two redactors of different strength in one
/// codebase is exactly how the issue #132 third follow-up audit's finding happened:
/// `debug_dump_on_error`'s `err` field printed via a bare `eprintln!` with **no**
/// redaction of its own, relying entirely on its one caller (`main.rs`) having
/// pre-scrubbed it with this function — and this function's blocklist had no entry
/// for the AWS-shaped secret the auditor used, so it sailed through unredacted.
///
/// `redact()` is now a thin shim over [`dump_redact::redact_free_text`] (retiring the
/// old blocklist entirely — issue #132 third follow-up audit, requirement 3) plus the
/// same 400-byte length cap the old implementation applied, so every one of this
/// function's ~30 existing call sites across `lib.rs`/`appauth.rs` is upgraded to the
/// stronger scanner automatically, with no per-call-site changes needed. The
/// project-specific `RUNNER_TOKEN=` marker the old blocklist knew about that
/// `redact_free_text` did not is now folded into `redact_free_text` itself (see
/// `UnsafeShape::RunnerTokenEnv`), so nothing the old blocklist caught is lost by
/// retiring it.
pub fn redact(s: &str) -> String {
    truncate_for_dump(redact_free_text(s), 400)
}

/// Host `/dev/null` must be a world-writable char device (1,3). A regular file
/// (seen when UID 1001 accidentally creates `/dev/null`) breaks fuse-overlayfs
/// and leaves runners stuck in `Created` with all Actions jobs queued forever.
fn assert_host_dev_null_ok() -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let meta = fs::metadata("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
        if !meta.file_type().is_char_device() {
            return Err(
                "/dev/null is not a character device (host corruption). \
                 Repair as root: rm -f /dev/null && mknod -m 666 /dev/null c 1 3 && chown root:root /dev/null \
                 — rootless Podman cannot start runners until this is fixed."
                    .into(),
            );
        }
        // mode should allow all read/write (0666)
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o222 == 0 {
            return Err(format!(
                "/dev/null mode {mode:o} is not writable — chmod 666 /dev/null"
            ));
        }
    }
    Ok(())
}

fn validate_cli(cli: &Cli) -> Result<(), String> {
    assert_host_dev_null_ok()?;
    match cli.scope {
        Scope::Repo => {
            if cli.repo.is_none() {
                // `warm` uses the allowlist only (no single --repo).
                if cli
                    .effective_allowlist_repos_quiet()
                    .is_some_and(|p| !p.trim().is_empty())
                {
                    // ok
                } else {
                    return Err(
                        "repo scope requires --repo owner/name, GHA_REPO, --auto, or --prefer-repos for warm"
                            .into(),
                    );
                }
            } else if let Some(repo) = &cli.repo {
                if !is_safe_repo(repo) {
                    return Err("invalid --repo".into());
                }
            }
        }
        Scope::Org => {
            let Some(owner) = cli.owner.as_ref() else {
                return Err("org scope requires --owner ORG (or GHA_OWNER)".into());
            };
            if !is_safe_ident(owner) {
                return Err("invalid --owner".into());
            }
        }
        Scope::User => {
            let Some(user) = cli.user.as_ref() else {
                return Err("user scope requires --user LOGIN or authenticated gh".into());
            };
            if !is_safe_ident(user) {
                return Err("invalid --user".into());
            }
            // retain + user is OK only for a sticky single-repo unit (prefer one entry
            // or explicit --repo). Multi-repo user-batch still needs ephemeral re-target.
            if matches!(cli.mode, Mode::Retain) {
                let multi = cli
                    .effective_allowlist_repos_quiet()
                    .map(|p| p.split(',').filter(|x| !x.trim().is_empty()).count() > 1)
                    .unwrap_or(true);
                if multi && cli.repo.is_none() {
                    return Err(
                        "scope=user + retain needs a single sticky --repo (or one-entry GHA_PREFER_REPOS). \
                         For multi-repo: use `warm` (one retain runner per allowlist repo) or ephemeral user-batch."
                            .into(),
                    );
                }
            }
        }
    }
    if !is_safe_image(&cli.image) {
        return Err("invalid --image".into());
    }
    if !is_safe_image(&cli.seed_helper_image) {
        return Err("invalid --seed-helper-image".into());
    }
    if !is_safe_runner_user(&cli.runner_user) {
        return Err("invalid --runner-user (expected uid:gid or name)".into());
    }
    if !is_safe_runner_version(&cli.runner_version) {
        return Err("invalid --runner-version".into());
    }
    if !is_safe_sha256_hex(&cli.runner_sha256) {
        return Err("invalid --runner-sha256 (64 hex chars)".into());
    }
    if !is_safe_ident(&cli.runner_arch) {
        return Err("invalid --runner-arch".into());
    }
    if let Some(url) = cli.runner_seed_url.as_ref() {
        if !is_safe_url(url) {
            return Err("invalid --runner-seed-url (http/https only)".into());
        }
    }
    if let Some(p) = cli.entrypoint.as_ref() {
        if !p.is_file() {
            return Err(format!(
                "entrypoint not found: {} (GHA_ENTRYPOINT / --entrypoint)",
                p.display()
            ));
        }
    }
    if !is_safe_ident(&cli.container) {
        return Err("invalid --container".into());
    }
    if !is_safe_ident(&cli.volume) {
        return Err("invalid --volume".into());
    }
    if !is_safe_ident(&cli.runner_name) {
        return Err("invalid --runner-name".into());
    }
    if !is_safe_labels(&cli.labels) {
        return Err("invalid --labels".into());
    }
    if !is_safe_cpus(&cli.cpus) {
        return Err("invalid --cpus".into());
    }
    if !is_safe_memory(&cli.memory) {
        return Err("invalid --memory".into());
    }
    if let Some(s) = cli.gpu_slice.as_ref() {
        let s = s.trim().to_ascii_lowercase();
        if s != "a" && s != "b" {
            return Err("invalid --gpu-slice (use a or b)".into());
        }
        if !cli.gpu {
            return Err("--gpu-slice requires --gpu".into());
        }
    }
    if let Some(tok) = &cli.wake_token {
        if tok.len() < 16 {
            return Err("GHA_WAKE_TOKEN must be at least 16 characters when set".into());
        }
    }
    if let Some(p) = cli.platform.as_ref() {
        let p = p.trim();
        if p.is_empty()
            || p.len() > 64
            || !p
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        {
            return Err("invalid --platform (expected e.g. linux/arm64)".into());
        }
    }
    if let Some(path) = cli.image_map.as_ref() {
        // Validate map is readable/parseable early (listen/up/prepare).
        load_image_map(Some(path.as_path()))?;
    }
    Ok(())
}

/// Registration URL for config.sh (repo or org). User-batch uses active_repo.
fn github_url(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!(
            "https://github.com/{}",
            cli.repo.as_ref().expect("validated")
        ),
        Scope::Org => format!(
            "https://github.com/{}",
            cli.owner.as_ref().expect("validated")
        ),
        Scope::User => format!(
            "https://github.com/{}",
            cli.repo
                .as_ref()
                .expect("user batch sets active repo before up")
        ),
    }
}

fn registration_api_for_repo(repo: &str, http: &HttpConfig) -> String {
    http.api_url(&format!("repos/{repo}/actions/runners/registration-token"))
}

fn registration_api(cli: &Cli, http: &HttpConfig) -> String {
    match cli.scope {
        Scope::Repo | Scope::User => {
            registration_api_for_repo(cli.repo.as_ref().expect("validated"), http)
        }
        Scope::Org => http.api_url(&format!(
            "orgs/{}/actions/runners/registration-token",
            cli.owner.as_ref().expect("validated")
        )),
    }
}

pub fn current_username() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "default".to_string());
    let sanitized: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

// --- Per-container instance lock (allows multi-runner horizontal scale) ------

struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    /// `kind` is `up` / `listen`; `container` namespaces the lock so multiple
    /// controller processes can run (cpu vs gpu instances).
    fn acquire(kind: &str, container: &str) -> Result<Self, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let safe: String = container
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let user_suffix = current_username();
        let path = dir.join(format!("gha-runner-ctl-{kind}-{safe}-{user_suffix}.lock"));
        for attempt in 0..2 {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            match opts.open(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    return Err(format!(
                        "another gha-runner-ctl {kind} for container '{container}' is already running (lock {})",
                        path.display()
                    ));
                }
                Err(e) => return Err(format!("lock open {}: {e}", path.display())),
            }
        }
        Err(format!("could not acquire lock {}", path.display()))
    }
}

/// A lock file is written in two steps — `create_new` then `writeln!(pid)` — so an
/// empty or partially-written file may belong to a *live* holder that is mid-creation,
/// not a crashed remnant. Reclaiming it in that window would steal a live lock (TOCTOU).
/// Only treat an unreadable/unparseable lock as stale once it has aged past this grace,
/// which is far longer than the microsecond create→write gap but short enough to clear a
/// genuinely crashed-mid-write lock promptly.
const LOCK_WRITE_GRACE_SECS: u64 = 5;

/// True iff `path` is older than [`LOCK_WRITE_GRACE_SECS`] (or its mtime can't be read /
/// it's already gone). Used only for the incomplete-content branches of [`lock_is_stale`].
fn lock_incomplete_and_aged(path: &Path) -> bool {
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime
            .elapsed()
            .map(|age| age.as_secs() >= LOCK_WRITE_GRACE_SECS)
            // Clock went backwards → be conservative, keep it (not yet stale).
            .unwrap_or(false),
        // No metadata: nothing to protect (already removed / unreadable) → stale.
        Err(_) => true,
    }
}

pub(crate) fn lock_is_stale(path: &Path) -> bool {
    let Ok(s) = fs::read_to_string(path) else {
        // Unreadable: only stale if it is not a lock being created right now.
        return lock_incomplete_and_aged(path);
    };
    let Ok(pid) = s.trim().parse::<u32>() else {
        // Empty/partial content: a holder may be between create_new and writeln!(pid).
        return lock_incomplete_and_aged(path);
    };
    // Parseable PID: stale iff the process is gone. (`kill -0` EPERM would mean the
    // process exists under another uid; on the single-user fleet host that does not
    // arise, and ESRCH is the stale signal we want.)
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|st| !st.success())
        .unwrap_or(true)
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// --- Auth / HTTP -------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default)]
struct Config {
    github_token: Option<String>,
}

#[cfg(unix)]
fn chmod_0600(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|e| format!("Failed to read metadata for {}: {e}", path.display()))?
        .permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
        .map_err(|e| format!("Failed to set permissions on {}: {e}", path.display()))
}

fn load_config() -> Option<Config> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path = home
        .join(".config")
        .join("gha-runner-ctl")
        .join("config.json");
    if path.is_file() {
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    } else {
        None
    }
}

fn save_config(config: &Config) -> Result<(), String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("No HOME directory found")?;
    let dir = home.join(".config").join("gha-runner-ctl");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create config dir: {e}"))?;
    let path = dir.join("config.json");
    let content = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize config: {e}"))?;

    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .map_err(|e| format!("Failed to open config file for writing: {e}"))?;
    f.write_all(content.as_bytes())
        .map_err(|e| format!("Failed to write config file: {e}"))?;
    #[cfg(unix)]
    chmod_0600(&path)?;
    Ok(())
}

fn get_token_from_git_credential() -> Option<String> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    {
        let stdin = child.stdin.as_mut()?;
        writeln!(stdin, "protocol=https\nhost=github.com\n").ok()?;
    }

    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }

    let stdout_str = String::from_utf8_lossy(&out.stdout);
    for line in stdout_str.lines() {
        if let Some(token) = line.trim().strip_prefix("password=") {
            let t = token.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn is_gcm_installed() -> bool {
    if Command::new("git-credential-manager")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return true;
    }
    if Command::new("git-credential-manager-core")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        return true;
    }
    if let Ok(out) = Command::new("git")
        .args(["config", "--get", "credential.helper"])
        .output()
    {
        let helper = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if helper.contains("manager") {
            return true;
        }
    }
    false
}

fn install_gcm() -> Result<(), String> {
    eprintln!(
        "prepare: Git Credential Manager (GCM) is missing. Attempting automatic installation..."
    );
    if !Path::new("/usr/bin/dpkg").exists() {
        return Err("Automatic GCM installation is currently only supported on Debian/Ubuntu-based systems.\nTo install GCM on your system, please refer to: https://github.com/git-ecosystem/git-credential-manager/blob/main/docs/install.md".into());
    }

    let ver = "2.5.1";
    let url = format!("https://github.com/git-ecosystem/git-credential-manager/releases/download/v{ver}/gcm-linux_amd64.{ver}.deb");
    eprintln!("Downloading GCM deb from: {url}");

    let dest_path = std::env::temp_dir().join(format!("gcm-{ver}.deb"));

    // Deliberately NOT parameterised on the forge: this is a fixed vendor artifact on
    // github.com/releases (the GCM project's own tarball), not a call against whichever
    // forge we are registering runners with. It goes through the seam only to inherit
    // the shared UA and timeouts.
    let resp = http_agent(&HttpConfig::github())
        .get(&url)
        .call()
        .map_err(|e| format!("Failed to download GCM deb package: {e}"))?;

    if resp.status() != 200 {
        return Err(format!(
            "Failed to download GCM: HTTP status {}",
            resp.status()
        ));
    }

    let mut file =
        File::create(&dest_path).map_err(|e| format!("Failed to create temp GCM deb file: {e}"))?;
    let mut reader = resp.into_reader();
    std::io::copy(&mut reader, &mut file).map_err(|e| format!("Failed to save GCM deb: {e}"))?;

    eprintln!("Installing GCM deb package (requires sudo privileges)...");
    let status = Command::new("sudo")
        .args(["dpkg", "-i", dest_path.to_str().unwrap_or("")])
        .status()
        .map_err(|e| format!("dpkg execution failed: {e}"))?;

    if !status.success() {
        return Err("dpkg failed to install GCM package".into());
    }

    eprintln!("Configuring GCM helper globally...");
    let configure_status = Command::new("git-credential-manager")
        .arg("configure")
        .status()
        .map_err(|e| format!("Failed to configure GCM: {e}"))?;

    if !configure_status.success() {
        eprintln!(
            "Warning: git-credential-manager configure didn't run cleanly. Trying git config..."
        );
        let _ = Command::new("git")
            .args(["config", "--global", "credential.helper", "manager"])
            .status();
    }

    eprintln!("Git Credential Manager successfully installed and configured!");
    Ok(())
}

fn store_token_in_git_credential(token: &str) -> Result<(), String> {
    let mut child = Command::new("git")
        .args(["credential", "approve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("git credential approve failed to start: {e}"))?;

    {
        let stdin = child.stdin.as_mut().ok_or("No stdin for git credential")?;
        writeln!(
            stdin,
            "protocol=https\nhost=github.com\nusername=git\npassword={token}\n"
        )
        .map_err(|e| format!("Failed to write to git credential: {e}"))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed to wait for git credential: {e}"))?;
    if !status.success() {
        return Err("git credential approve failed".into());
    }
    Ok(())
}

fn prompt_token_interactively() -> Option<String> {
    eprint!("Enter your GitHub PAT (input is hidden): ");
    std::io::stderr().flush().ok()?;
    let pass = rpassword::read_password().ok()?;
    let trimmed = pass.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Which credential path produced a token — reported by `doctor`, otherwise
/// discarded. Never carries the token itself.
enum TokenSource {
    GithubApp,
    EnvVar(&'static str),
    GitCredentialHelper,
    GhCli,
    ConfigFile,
    Interactive,
}

impl fmt::Display for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenSource::GithubApp => write!(f, "GitHub App installation token"),
            TokenSource::EnvVar(name) => write!(f, "{name} environment variable"),
            TokenSource::GitCredentialHelper => write!(f, "git credential helper (GCM)"),
            TokenSource::GhCli => write!(f, "gh CLI (`gh auth token`)"),
            TokenSource::ConfigFile => write!(f, "config file"),
            TokenSource::Interactive => write!(f, "interactive prompt"),
        }
    }
}

fn github_token(cli: &Cli) -> Result<String, String> {
    github_token_with_source(cli).map(|(t, _)| t)
}

fn github_token_with_source(cli: &Cli) -> Result<(String, TokenSource), String> {
    // GitHub App auth (opt-in, additive): only engages when --app-id/GHA_APP_ID and
    // --app-private-key/GHA_APP_PRIVATE_KEY are both set (installation id is optional
    // — see src/appauth.rs auto-discovery). `Ok(None)` means nothing App-auth-shaped
    // is configured at all — fall through to the PAT chain below unchanged. Any other
    // outcome (`Ok(Some(cfg))` or `Err`) is authoritative: a mint failure or a partial/
    // invalid config is a hard error, never a silent fall-through to the PAT path,
    // since that could mask a real misconfiguration or mint against the wrong identity.
    if let Some(cfg) = cli.app_auth_config()? {
        let token =
            appauth::installation_token(&cfg, cli.app_auth_owner_hint().as_deref(), &cli.http())?;
        return Ok((token, TokenSource::GithubApp));
    }

    // 1. Try env variables
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(key) {
            if !t.is_empty() {
                return Ok((t, TokenSource::EnvVar(key)));
            }
        }
    }

    // 2. Try GCM or git credential helper
    if let Some(t) = get_token_from_git_credential() {
        return Ok((t, TokenSource::GitCredentialHelper));
    }

    // 3. Try GH CLI
    if let Ok(out) = Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if out.status.success() {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !t.is_empty() {
                return Ok((t, TokenSource::GhCli));
            }
        }
    }

    // 4. Try Config file
    if let Some(cfg) = load_config() {
        if let Some(t) = cfg.github_token {
            if !t.is_empty() {
                return Ok((t, TokenSource::ConfigFile));
            }
        }
    }

    // Check GCM installation status and offer installation if interactive
    let is_atty = std::io::stdin().is_terminal();
    if is_atty && !is_gcm_installed() {
        eprint!("Git Credential Manager (GCM) is missing. Would you like to install it? [y/N]: ");
        std::io::stderr().flush().ok();
        let mut response = String::new();
        if std::io::stdin().read_line(&mut response).is_ok() {
            let resp_trimmed = response.trim().to_lowercase();
            if resp_trimmed == "y" || resp_trimmed == "yes" {
                if let Err(e) = install_gcm() {
                    eprintln!("Failed to install GCM: {e}");
                }
            }
        }
    }

    // 5. Interactive fallback
    if is_atty {
        if let Some(t) = prompt_token_interactively() {
            eprint!("Would you like to securely save this token to config and GCM? [y/N]: ");
            std::io::stderr().flush().ok();
            let mut response = String::new();
            if std::io::stdin().read_line(&mut response).is_ok() {
                let resp_trimmed = response.trim().to_lowercase();
                if resp_trimmed == "y" || resp_trimmed == "yes" {
                    // Save to config
                    let cfg = Config {
                        github_token: Some(t.clone()),
                    };
                    if let Err(e) = save_config(&cfg) {
                        eprintln!("Warning: failed to save config: {e}");
                    }
                    // Save to GCM
                    if is_gcm_installed() {
                        if let Err(e) = store_token_in_git_credential(&t) {
                            eprintln!("Warning: failed to store token in GCM: {e}");
                        }
                    }
                }
            }
            return Ok((t, TokenSource::Interactive));
        }
    }

    Err("No GitHub token or PAT found. Please authenticate via 'gh auth login', set GH_TOKEN environment variable, install Git Credential Manager, or enter it interactively.".into())
}

#[derive(Deserialize)]
struct RegistrationTokenResponse {
    token: String,
}

/// The injectable HTTP seam: **every** outbound request in this crate is built from
/// one of these.
///
/// Before this existed, `http_agent()` took no arguments and each call site pasted
/// `https://api.github.com` into a `format!`, so there was no way to point the client
/// at anything else — which is why the HTTP paths had zero test coverage.
///
/// Production always constructs this via [`HttpConfig::github`] (equivalently
/// `Cli::http`), which reproduces the previously-hardcoded values byte for byte:
/// `GITHUB_API_BASE`, `UA`, `HTTP_TIMEOUT`. Tests construct one with
/// [`HttpConfig::with_api_base`] pointing at a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    /// REST base with **no** trailing slash (`https://api.github.com`).
    api_base: String,
    user_agent: String,
    timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self::github()
    }
}

impl HttpConfig {
    /// Production defaults — the exact values that were hardcoded before the seam.
    pub fn github() -> Self {
        Self {
            api_base: GITHUB_API_BASE.to_string(),
            user_agent: UA.to_string(),
            timeout: HTTP_TIMEOUT,
        }
    }

    /// Point the REST base somewhere else (a test server today; a self-hosted forge
    /// later). Trailing slashes are stripped so [`HttpConfig::api_url`] always joins
    /// with exactly one separator.
    pub fn with_api_base(base: impl Into<String>) -> Self {
        Self {
            api_base: base.into().trim_end_matches('/').to_string(),
            ..Self::github()
        }
    }

    /// Override connect/read/write timeouts. Used by tests so a wedged local server
    /// fails in milliseconds instead of `HTTP_TIMEOUT`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Join a **relative** API path onto the configured base.
    ///
    /// With the default base this is string-identical to the old inline
    /// `format!("https://api.github.com/{path}")`, so no production request changes.
    pub fn api_url(&self, path: &str) -> String {
        format!("{}/{}", self.api_base, path.trim_start_matches('/'))
    }
}

/// Build the ureq agent for a given seam configuration.
///
/// Takes `&HttpConfig` rather than reading the constants directly — that parameter is
/// the whole point of this function; bypassing it is what the HTTP tests are written
/// to catch.
fn http_agent(http: &HttpConfig) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(http.timeout)
        .timeout_read(http.timeout)
        .timeout_write(http.timeout)
        .user_agent(&http.user_agent)
        .build()
}

/// Paces GitHub API calls: min gap, per-poll budget, honor rate-limit headers / backoff.
struct ApiPacer {
    min_gap: Duration,
    max_per_poll: u32,
    calls_this_poll: u32,
    last_call: Option<Instant>,
    backoff: Duration,
    max_backoff: Duration,
    /// When set, skip further API until this instant (rate-limit cool-down).
    cool_until: Option<Instant>,
    /// The HTTP seam every paced request is issued through. Owned by the pacer because
    /// the pacer is already threaded down the whole demand-poll call chain, so nothing
    /// on that chain needs a new parameter to reach the seam.
    http: HttpConfig,
}

impl ApiPacer {
    fn from_cli(cli: &Cli, http: HttpConfig) -> Self {
        let gap_ms = cli.api_min_gap_ms.clamp(50, 60_000);
        let max_per = cli.api_max_per_poll.clamp(2, 500);
        let backoff = Duration::from_secs(cli.api_backoff_secs.clamp(5, MAX_API_BACKOFF_SECS));
        Self {
            min_gap: Duration::from_millis(gap_ms),
            max_per_poll: max_per,
            calls_this_poll: 0,
            last_call: None,
            http,
            backoff,
            max_backoff: Duration::from_secs(MAX_API_BACKOFF_SECS),
            cool_until: None,
        }
    }

    fn begin_poll(&mut self) {
        self.calls_this_poll = 0;
    }

    /// Absolute URL for a relative API path, via this pacer's seam.
    ///
    /// Demand-poll callers build their URL with this instead of pasting
    /// `https://api.github.com/...` into a `format!`, which is what makes the poll
    /// path reachable from a test. [`ApiPacer::get`] still takes an absolute URL so
    /// that `Link: rel="next"` pagination (whose URLs come from the server) works
    /// unchanged.
    fn api_url(&self, path: &str) -> String {
        self.http.api_url(path)
    }

    fn cooling(&self) -> Option<Duration> {
        self.cool_until.and_then(|u| {
            let now = Instant::now();
            if u > now {
                Some(u.saturating_duration_since(now))
            } else {
                None
            }
        })
    }

    fn wait_turn(&mut self) -> Result<(), String> {
        if let Some(wait) = self.cooling() {
            eprintln!(
                "api: cooling {}s (rate-limit / secondary limit)",
                wait.as_secs().max(1)
            );
            thread::sleep(wait);
            self.cool_until = None;
        }
        if self.calls_this_poll >= self.max_per_poll {
            return Err(format!(
                "api: per-poll budget exhausted ({}/{}) — wait for next listen interval",
                self.calls_this_poll, self.max_per_poll
            ));
        }
        if let Some(last) = self.last_call {
            let elapsed = last.elapsed();
            if elapsed < self.min_gap {
                thread::sleep(self.min_gap - elapsed);
            }
        }
        self.last_call = Some(Instant::now());
        self.calls_this_poll += 1;
        Ok(())
    }

    fn note_success(&mut self, remaining: Option<u32>, reset_unix: Option<u64>) {
        // Soft throttle when primary quota is low (still leave headroom).
        if let Some(rem) = remaining {
            if rem < 30 {
                if let Some(reset) = reset_unix {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let wait = reset.saturating_sub(now).clamp(5, MAX_API_BACKOFF_SECS);
                    eprintln!("api: X-RateLimit-Remaining={rem} — cool {wait}s until reset");
                    self.cool_until = Some(Instant::now() + Duration::from_secs(wait));
                    self.backoff = (self.backoff * 2).min(self.max_backoff);
                } else {
                    self.cool_until = Some(Instant::now() + self.backoff);
                    self.backoff = (self.backoff * 2).min(self.max_backoff);
                }
            } else if rem > 200 {
                // Recover toward configured minimum after healthy period.
                // (keep at least min_gap-driven pacing)
            }
        }
    }

    fn note_rate_limited(&mut self, retry_after: Option<u64>, reset_unix: Option<u64>) {
        let mut secs = self.backoff.as_secs();
        if let Some(ra) = retry_after {
            secs = secs.max(ra).min(MAX_API_BACKOFF_SECS);
        } else if let Some(reset) = reset_unix {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            secs = secs
                .max(reset.saturating_sub(now))
                .min(MAX_API_BACKOFF_SECS);
        }
        secs = secs.max(5);
        eprintln!("api: rate-limited — backing off {secs}s (then resume paced calls)");
        self.cool_until = Some(Instant::now() + Duration::from_secs(secs));
        self.backoff = (self.backoff * 2).min(self.max_backoff);
    }

    fn get(&mut self, url: &str, api: &str) -> Result<ureq::Response, String> {
        self.wait_turn()?;
        let result = http_agent(&self.http)
            .get(url)
            .set("Authorization", &format!("Bearer {api}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .call();
        match result {
            Ok(resp) => {
                let remaining: Option<u32> = resp
                    .header("x-ratelimit-remaining")
                    .and_then(|s| s.parse().ok());
                let reset: Option<u64> = resp
                    .header("x-ratelimit-reset")
                    .and_then(|s| s.parse().ok());
                let retry_after: Option<u64> =
                    resp.header("retry-after").and_then(|s| s.parse().ok());
                let status = resp.status();
                if status == 429 {
                    self.note_rate_limited(retry_after, reset);
                    return Err(format!("status code {status} (rate limited)"));
                }
                if status == 403 {
                    let body_snip = resp.into_string().unwrap_or_default();
                    let body_ref = if body_snip.is_empty() {
                        None
                    } else {
                        Some(body_snip.as_str())
                    };
                    if api_status_is_hard_rate_limit(status, remaining, body_ref) {
                        self.note_rate_limited(retry_after, reset);
                        return Err(format!("status code {status} (rate limited)"));
                    }
                    return Err(format!(
                        "status code {status}{}",
                        FORBIDDEN_NOT_RATE_LIMIT_HINT
                    ));
                }
                if status == 401 {
                    return Err(format!("status code {status}{}", UNAUTHORIZED_HINT));
                }
                if status == 404 {
                    return Err(format!("status code {status}"));
                }
                if !(200..300).contains(&status) {
                    return Err(format!("status code {status}"));
                }
                self.note_success(remaining, reset);
                Ok(resp)
            }
            Err(ureq::Error::Status(code, resp)) => {
                let remaining: Option<u32> = resp
                    .header("x-ratelimit-remaining")
                    .and_then(|s| s.parse().ok());
                let reset: Option<u64> = resp
                    .header("x-ratelimit-reset")
                    .and_then(|s| s.parse().ok());
                let retry_after: Option<u64> =
                    resp.header("retry-after").and_then(|s| s.parse().ok());
                let body_snip = resp.into_string().unwrap_or_default();
                let body_ref = if body_snip.is_empty() {
                    None
                } else {
                    Some(body_snip.as_str())
                };
                if code == 429
                    || (code == 403 && api_status_is_hard_rate_limit(code, remaining, body_ref))
                {
                    self.note_rate_limited(retry_after, reset);
                    return Err(format!("status code {code} (rate limited)"));
                }
                if code == 403 {
                    return Err(format!(
                        "status code {code}{}",
                        FORBIDDEN_NOT_RATE_LIMIT_HINT
                    ));
                }
                if code == 401 {
                    return Err(format!("status code {code}{}", UNAUTHORIZED_HINT));
                }
                Err(format!("status code {code}"))
            }
            Err(e) => Err(redact(&e.to_string())),
        }
    }
}

/// Appended to a 403 that `api_status_is_hard_rate_limit` ruled out — this is
/// deliberately a *different* failure from a rate limit and must not be conflated with
/// one: a scope 403 means the credential doesn't cover this repo (GitHub App
/// `repository_selection`, or a PAT's granted scopes), and no amount of backing off
/// will fix it, unlike a rate limit which resolves on its own.
const FORBIDDEN_NOT_RATE_LIMIT_HINT: &str = " (not a rate limit — the credential's \
     installation/token scope likely doesn't cover this repo; run `doctor` to check \
     the GitHub App's repository_selection or the PAT's scopes)";

/// Appended to a bare 401 from a demand-poll GET.
const UNAUTHORIZED_HINT: &str = " (credential rejected — expired/revoked token, rotated \
     App key, or wrong App id; run `doctor` to check)";

/// True when GitHub indicates a hard API rate limit (not a soft permission 403).
fn api_status_is_hard_rate_limit(status: u16, remaining: Option<u32>, body: Option<&str>) -> bool {
    if status == 429 {
        return true;
    }
    if status != 403 {
        return false;
    }
    if remaining == Some(0) {
        return true;
    }
    if let Some(b) = body {
        let lower = b.to_ascii_lowercase();
        if lower.contains("secondary rate limit") || lower.contains("secondary_rate_limit") {
            return true;
        }
    }
    false
}

/// Host-wide registration pacing (shared by all gha-runner-ctl processes).
fn reg_pace_paths() -> (PathBuf, PathBuf) {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user_suffix = current_username();
    (
        dir.join(format!("gha-runner-ctl-reg-pace-{user_suffix}.lock")),
        dir.join(format!("gha-runner-ctl-reg-pace-{user_suffix}.json")),
    )
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegPaceState {
    /// Unix seconds of last successful registration-token POST.
    last_unix: u64,
    /// Successful POST timestamps in the last hour (unix secs).
    recent: Vec<u64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct ExclusiveLockGuard {
    path: PathBuf,
}

impl Drop for ExclusiveLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Wait for host-wide registration budget (min gap + max per hour).
fn pace_registration(cli: &Cli) -> Result<(), String> {
    let (lock_path, state_path) = reg_pace_paths();
    let min_gap = cli.reg_min_gap_secs.clamp(1, 600);
    let max_hour = cli.reg_max_per_hour.clamp(1, 500);
    // Spin gently: registration is rare if retain; ephemeral must not stampede.
    for attempt in 0..120 {
        let _ = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path);
        // Best-effort exclusive via create_new retry on companion lock.
        let exclusive = lock_path.with_extension("exclusive");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&exclusive)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                let _guard = ExclusiveLockGuard {
                    path: exclusive.clone(),
                };
                let mut state: RegPaceState = fs::read_to_string(&state_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let now = now_unix();
                state.recent.retain(|t| now.saturating_sub(*t) < 3600);
                if state.recent.len() as u32 >= max_hour {
                    // Do NOT spin-sleep here — that freezes the listen loop (no reap, no other
                    // repos). Surface budget pressure and let the outer loop continue.
                    let oldest = state.recent.iter().copied().min().unwrap_or(now);
                    let wait = 3600u64
                        .saturating_sub(now.saturating_sub(oldest))
                        .clamp(15, 600);
                    return Err(format!(
                        "register: host budget {max_hour}/hour reached — retry in ~{wait}s"
                    ));
                }
                if state.last_unix > 0 {
                    let elapsed = now.saturating_sub(state.last_unix);
                    if elapsed < min_gap {
                        let wait = min_gap - elapsed;
                        eprintln!("register: pacing {wait}s before next registration-token POST");
                        drop(_guard);
                        thread::sleep(Duration::from_secs(wait));
                        continue;
                    }
                }
                // Budget enforced here; slot committed only after successful token mint.
                return Ok(());
            }
            Err(_) => {
                if attempt == 0 && lock_is_stale(&exclusive) {
                    let _ = fs::remove_file(&exclusive);
                    continue;
                }
                thread::sleep(Duration::from_millis(200 + (attempt as u64 % 5) * 100));
            }
        }
    }
    Err("register: could not acquire registration pace lock".into())
}

/// Best-effort acquire of a short-lived `create_new` exclusive lock, returning an RAII
/// guard that unlinks it on drop, or `None` if the lock is held by a live process (the
/// caller then skips its best-effort update — the same "skip if locked" semantics the
/// old code had). Reclaims a lock only when a *failed* `create_new` (`AlreadyExists`) is
/// followed by a positive [`lock_is_stale`] check; it never removes the file
/// preemptively, so it cannot delete a live holder's lock that is merely mid-creation.
fn try_acquire_exclusive(path: &Path) -> Option<ExclusiveLockGuard> {
    for attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{}", std::process::id());
                return Some(ExclusiveLockGuard {
                    path: path.to_path_buf(),
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if attempt == 0 && lock_is_stale(path) {
                    let _ = fs::remove_file(path);
                    continue;
                }
                return None;
            }
            Err(_) => return None,
        }
    }
    None
}

/// Record a successful registration-token mint in the host-wide hourly budget.
fn commit_registration_slot() {
    let (lock_path, state_path) = reg_pace_paths();
    let exclusive = lock_path.with_extension("exclusive");
    let Some(_guard) = try_acquire_exclusive(&exclusive) else {
        return;
    };
    let mut state: RegPaceState = fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let now = now_unix();
    state.recent.retain(|t| now.saturating_sub(*t) < 3600);
    state.last_unix = now;
    state.recent.push(now);
    if let Ok(s) = serde_json::to_string(&state) {
        let _ = fs::write(&state_path, s);
    }
}

fn note_registration_failure_backoff(secs: u64) {
    let (lock_path, state_path) = reg_pace_paths();
    let exclusive = lock_path.with_extension("exclusive");
    if let Some(_guard) = try_acquire_exclusive(&exclusive) {
        let mut state: RegPaceState = fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Push last_unix forward to force min gap = backoff.
        state.last_unix = now_unix().saturating_add(secs.saturating_sub(1));
        if let Ok(s) = serde_json::to_string(&state) {
            let _ = fs::write(&state_path, s);
        }
    }
    eprintln!("register: backing off {secs}s after failed registration-token POST");
    thread::sleep(Duration::from_secs(secs));
}

fn registration_token(cli: &Cli, api_token: &str, http: &HttpConfig) -> Result<String, String> {
    pace_registration(cli)?;
    let url = registration_api(cli, http);
    let resp = http_agent(http)
        .post(&url)
        .set("Authorization", &format!("Bearer {api_token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();
    let resp = match resp {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            if code == 403 || code == 429 {
                let retry: u64 = r
                    .header("retry-after")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cli.api_backoff_secs.max(60));
                note_registration_failure_backoff(retry.min(MAX_API_BACKOFF_SECS));
            }
            return Err(format!("registration-token request failed: HTTP {code}"));
        }
        Err(e) => {
            return Err(format!(
                "registration-token request failed: {}",
                redact(&e.to_string())
            ));
        }
    };
    let status = resp.status();
    if status == 403 || status == 429 {
        let retry: u64 = resp
            .header("retry-after")
            .and_then(|s| s.parse().ok())
            .unwrap_or(cli.api_backoff_secs.max(60));
        note_registration_failure_backoff(retry.min(MAX_API_BACKOFF_SECS));
        return Err(format!("registration-token HTTP {status} (rate limited)"));
    }
    if !(200..300).contains(&status) {
        return Err(format!(
            "registration-token HTTP {status} (admin rights on target?)"
        ));
    }
    let body: RegistrationTokenResponse = resp
        .into_json()
        .map_err(|e| format!("registration-token parse failed: {e}"))?;
    if body.token.is_empty() || body.token.len() > 512 {
        return Err("registration token empty or implausible length".into());
    }
    commit_registration_slot();
    eprintln!(
        "register: minted registration-token for {}",
        github_url(cli)
    );
    Ok(body.token)
}

/// Ephemeral only when we must re-bind to a different repo (user multi-target).
/// Retain keeps the runner online so GitHub pushes jobs without new tokens.
///
/// Token/credential model: the registration token minted by [`registration_token`]
/// is single-use, consumed once by `config.sh` during registration, and expires in
/// ~1 hour if unused. Once `config.sh` succeeds, the runner has written its own
/// durable credentials (`.runner` / `.credentials*` on the volume) and does not
/// need another token to keep listening or to pick up further jobs — GitHub's
/// Actions service pushes jobs to the already-registered runner. So a retained
/// runner is NOT limited to the 1-hour token lifetime; it can serve jobs
/// indefinitely on that registration. Bounded retirement (`GHA_RETAIN_MAX_AGE_SECS`,
/// `GHA_RETAIN_MAX_JOBS`, see [`volume_has_runner_config`]) exists for workspace
/// hygiene and drift control, not because the credential is about to expire.
fn effective_ephemeral(cli: &Cli) -> bool {
    if matches!(cli.mode, Mode::Retain) {
        return false;
    }
    if matches!(cli.scope, Scope::User) {
        // Forced re-target path: ephemeral so config.sh rebinds cleanly.
        return true;
    }
    matches!(cli.mode, Mode::Ephemeral)
}

// --- Podman ------------------------------------------------------------------

/// Refuse rootful system sockets and remote daemons unless explicitly allowed.
/// Rootless `unix:///run/user/…/podman.sock` (or path containing both) is permitted.
fn refuse_container_host_misconfig() -> Option<String> {
    let host = std::env::var("CONTAINER_HOST").ok()?;
    let allow = std::env::var("GHA_ALLOW_ROOTFUL_SOCKET")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "YES"))
        .unwrap_or(false);
    if allow {
        return None;
    }
    let rootless_podman = host.contains("/run/user/") && host.contains("podman.sock");
    if rootless_podman {
        return None;
    }
    let risky = host.contains("docker.sock")
        || host.contains("podman.sock")
        || host.starts_with("tcp://")
        || host.starts_with("unix://");
    if risky {
        return Some(
            "refusing CONTAINER_HOST (system/remote podman or docker socket). \
             Use rootless socket under /run/user/…/podman.sock, or set GHA_ALLOW_ROOTFUL_SOCKET=1 only if intentional."
                .into(),
        );
    }
    None
}

fn podman(args: &[&str]) -> Result<String, String> {
    // Never point work-plane ops at a rootful / remote socket from an agent process
    // that was expected to be rootless (misconfiguration guard).
    if let Some(msg) = refuse_container_host_misconfig() {
        return Err(msg);
    }
    let out = Command::new("podman")
        .args(args)
        .output()
        .map_err(|e| format!("podman not runnable: {e}"))?;
    if !out.status.success() {
        let err = redact(&String::from_utf8_lossy(&out.stderr));
        // Real exit status included (not just "failed") so a fail-closed debug dump
        // built from this message can tell a genuine command failure apart from e.g. a
        // signal kill — see debug_dump_fail_closed / issue #132.
        let status = out
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string());
        return Err(format!(
            "podman {} failed (exit={status}): {}",
            args.first().unwrap_or(&"?"),
            err.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn podman_ok(args: &[&str]) -> bool {
    podman(args).is_ok()
}

fn container_running(name: &str) -> bool {
    podman(&["inspect", "-f", "{{.State.Running}}", name]).is_ok_and(|s| s == "true")
}

/// Local per-worker busy signal for scale-in safety.
///
/// Does **not** consult the demand scan (which is only a partial prefer-repo RR
/// sample). Uses the actions/runner process tree inside the container:
/// `Runner.Worker` is present only while a job is executing; idle online
/// runners only have `Runner.Listener`.
///
/// Fail-closed: if the container is running but process inspection fails, treat
/// as busy so we never tear down a mid-job worker under uncertainty.
///
/// That fail-closed decision is exactly the silent-but-correct case issue #132 is
/// about: on its own it's harmless (the worker just isn't scaled in this tick), but a
/// `podman top` that starts erroring on every tick means scale-in silently stops
/// happening at all — workers pile up until the host's CPU/memory budget is exhausted,
/// with no signal pointing at `podman top` as the cause. `fail_closed` turns that into
/// an alertable streak; `debug_dump_fail_closed` gives a developer investigating it the
/// real command, exit status and stderr in one pass.
fn container_worker_busy(name: &str) -> bool {
    if !container_running(name) {
        return false;
    }
    match podman(&["top", name]) {
        Ok(out) => {
            check_succeeded("worker_busy_probe");
            let lower = out.to_ascii_lowercase();
            // actions/runner spawns Runner.Worker (any path / casing) for the job.
            lower.contains("runner.worker")
        }
        // Cannot prove idle → not eligible for scale-in.
        Err(reason) => {
            let ev = fail_closed("worker_busy_probe", name, "busy", &reason);
            let inputs: Vec<(&str, String)> = DEBUG_DUMP_ENV_KEYS
                .iter()
                .filter_map(|&k| std::env::var(k).ok().map(|v| (k, v)))
                .collect();
            debug_dump_fail_closed(&ev, "podman top <container>", &inputs);
            true
        }
    }
}

fn container_exists(name: &str) -> bool {
    podman_ok(&["container", "exists", name])
}

fn volume_exists(name: &str) -> bool {
    podman_ok(&["volume", "exists", name])
}

/// A worker volume that *exists* is not necessarily *seeded*.
///
/// An empty volume left behind by a failed spawn (or created by an older build)
/// satisfies [`volume_exists`] forever, so a bare existence check makes
/// [`ensure_worker_volume`] return early and never populate it. The worker then
/// starts against an empty `/opt/actions-runner`, and `entrypoint.sh` exits 1 with
/// "runner binaries missing" on every single spawn — permanently, with no self-heal.
/// Observed on the WSL GPU host: `gha-runner-gpu-{a,b}-w0-data` both existed with
/// zero files while the base volumes held the full 16-entry runner payload.
///
/// Probe for the actual payload instead. `run.sh` is the exact marker
/// `entrypoint.sh` gates on, so this asks the same question the consumer asks.
fn volume_is_seeded(name: &str) -> bool {
    let Ok(out) = podman(&["volume", "inspect", name, "--format", "{{.Mountpoint}}"]) else {
        return false;
    };
    let mount = out.trim();
    if mount.is_empty() {
        return false;
    }
    std::path::Path::new(mount).join("run.sh").exists()
}

fn resolve_build_dir(cli: &Cli) -> Result<PathBuf, String> {
    if let Some(p) = &cli.build_dir {
        let p = p.canonicalize().map_err(|e| format!("build-dir: {e}"))?;
        if !p.join("Containerfile").is_file() {
            return Err("build-dir missing Containerfile".into());
        }
        return Ok(p);
    }
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidate = here
        .join("packaging")
        .canonicalize()
        .map_err(|e| format!("resolve packaging/: {e}"))?;
    if !candidate.join("Containerfile").is_file() {
        return Err(format!(
            "Containerfile not found under {} — pass --build-dir",
            candidate.display()
        ));
    }
    Ok(candidate)
}

// --- Prepare / up / down -----------------------------------------------------

/// Refresh host packages so the build machine (and nested tools) are patched
/// before we bake a long-lived snapshot. Fail soft if no package manager /
/// insufficient privileges — image build still proceeds.
fn update_host_packages() -> Result<(), String> {
    eprintln!("prepare: updating host packages before snapshot…");
    if Path::new("/usr/bin/apt-get").exists() {
        let update = Command::new("apt-get")
            .args(["update", "-qq"])
            .status()
            .map_err(|e| format!("apt-get update: {e}"))?;
        if !update.success() {
            eprintln!("prepare: warning: apt-get update failed (continuing)");
            return Ok(());
        }
        // Security + bugfix upgrades only where unattended-upgrade is available;
        // otherwise full upgrade of installed packages (noninteractive).
        let upgrade = Command::new("apt-get")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .args([
                "upgrade",
                "-y",
                "-qq",
                "-o",
                "Dpkg::Options::=--force-confdef",
                "-o",
                "Dpkg::Options::=--force-confold",
            ])
            .status()
            .map_err(|e| format!("apt-get upgrade: {e}"))?;
        if !upgrade.success() {
            eprintln!("prepare: warning: apt-get upgrade failed (continuing)");
        } else {
            eprintln!("prepare: host apt packages updated");
        }
        let _ = Command::new("apt-get")
            .args(["autoremove", "-y", "-qq"])
            .status();
        return Ok(());
    }
    if Path::new("/usr/bin/dnf").exists() {
        let st = Command::new("dnf")
            .args(["upgrade", "-y", "-q"])
            .status()
            .map_err(|e| format!("dnf upgrade: {e}"))?;
        if st.success() {
            eprintln!("prepare: host dnf packages updated");
        } else {
            eprintln!("prepare: warning: dnf upgrade failed (continuing)");
        }
        return Ok(());
    }
    eprintln!("prepare: no apt-get/dnf — skip host package update");
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
/// Paced batch warm: one retain runner per allowlisted repo (or single --repo).
/// After this, GitHub pushes jobs to online runners — no demand registration storm.
fn warm(cli: &Cli, gap_secs: u64, start: bool) -> Result<(), String> {
    let repos: Vec<String> = if let Some(pref) = cli.effective_allowlist_repos() {
        pref.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    } else if let Some(r) = cli.repo.as_ref() {
        vec![r.clone()]
    } else {
        return Err(
            "warm requires --prefer-repos a/b,c/d or --scope repo --repo owner/name".into(),
        );
    };
    if repos.is_empty() {
        return Err("warm: empty repo list".into());
    }
    let gap = gap_secs.max(cli.reg_min_gap_secs).max(3);
    eprintln!(
        "warm: {} repo(s), gap={gap}s, start={start}, mode=retain (GitHub will push jobs once online)",
        repos.len()
    );
    for (i, repo) in repos.iter().enumerate() {
        if !is_safe_repo(repo) {
            eprintln!("warm: skip invalid repo {repo}");
            continue;
        }
        let slug = repo.replace('/', "-");
        let mut unit = cli.clone_for_listen();
        unit.scope = Scope::Repo;
        unit.repo = Some(repo.clone());
        unit.mode = Mode::Retain;
        unit.container = format!("{}-{}", cli.container, slug);
        unit.volume = format!("{}-{}", cli.volume, slug);
        unit.runner_name = format!("{}-{}", cli.runner_name, slug);
        // Safe truncate names
        if unit.container.len() > 60 {
            unit.container = unit.container.chars().take(60).collect();
        }
        if unit.runner_name.len() > 60 {
            unit.runner_name = unit.runner_name.chars().take(60).collect();
        }
        eprintln!(
            "warm: [{}/{}] {} → container={} runner={}",
            i + 1,
            repos.len(),
            repo,
            unit.container,
            unit.runner_name
        );
        if !volume_exists(&unit.volume) {
            eprintln!("warm: preparing volume {}", unit.volume);
            prepare(&unit, true, true)?;
        }
        if start {
            if let Err(e) = up(&unit) {
                eprintln!("warm: up failed for {repo}: {}", redact(&e));
            }
        } else {
            // Mint token only to prove registration rights (still paced); do not start.
            let api = github_token(&unit)?;
            match registration_token(&unit, &api, &unit.http()) {
                Ok(_) => eprintln!("warm: token mint OK for {repo} (not starting)"),
                Err(e) => eprintln!("warm: token mint failed for {repo}: {}", redact(&e)),
            }
        }
        if i + 1 < repos.len() {
            eprintln!("warm: waiting {gap}s before next registration…");
            thread::sleep(Duration::from_secs(gap));
        }
    }
    eprintln!(
        "warm: done — online retain runners receive jobs via GitHub push (no poll for assign)"
    );
    Ok(())
}

fn prepare(cli: &Cli, with_container: bool, skip_host_update: bool) -> Result<(), String> {
    // Host refresh first so build tools / podman stack are current before we snapshot.
    if !skip_host_update {
        let _ = update_host_packages();
    } else {
        eprintln!("prepare: skipping host update (--skip-host-update / GHA_SKIP_HOST_UPDATE)");
    }

    let mode = effective_image_mode(&cli.image_mode, &cli.image);
    let pull = effective_pull_policy(cli.pull_policy.as_ref(), &mode);
    eprintln!(
        "prepare: image_mode={:?} (resolved={:?}) pull={} image={}",
        cli.image_mode,
        mode,
        pull_policy_arg(&pull),
        cli.image
    );

    match mode {
        ImageMode::Build => prepare_build_image(cli)?,
        ImageMode::External => ensure_image_present(&cli.image, &pull)?,
        ImageMode::Auto => unreachable!("effective_image_mode never returns Auto"),
    }

    if !volume_exists(&cli.volume) {
        eprintln!("prepare: creating volume {}", cli.volume);
        podman(&["volume", "create", &cli.volume])?;
    }

    match mode {
        ImageMode::Build => seed_volume_from_stock_image(cli)?,
        ImageMode::External => seed_volume_runner_kit(cli)?,
        ImageMode::Auto => unreachable!("effective_image_mode never returns Auto"),
    }

    if with_container {
        eprintln!(
            "prepare: snapshot ready (cpus={} memory={} user={})",
            cli.cpus, cli.memory, cli.runner_user
        );
    }
    eprintln!("prepare: done");
    Ok(())
}

fn prepare_build_image(cli: &Cli) -> Result<(), String> {
    let dir = resolve_build_dir(cli)?;
    eprintln!("prepare: building {} from {}", cli.image, dir.display());
    // --pull=always for base OS so snapshot is not stuck on an old ubuntu digest
    podman(&[
        "build",
        "--pull=always",
        "-t",
        &cli.image,
        "-f",
        "Containerfile",
        dir.to_str().unwrap_or("."),
    ])?;
    Ok(())
}

fn ensure_image_present(image: &str, pull: &PullPolicy) -> Result<(), String> {
    ensure_image_present_platform(image, pull, None)
}

/// Ensure image is local; when `platform` is set, pull with `--platform` so the
/// correct arch manifest is fetched for cross-arch emulation (#28).
fn ensure_image_present_platform(
    image: &str,
    pull: &PullPolicy,
    platform: Option<&str>,
) -> Result<(), String> {
    let exists = podman(&["image", "exists", image]).is_ok();
    let pull_with_platform = |img: &str| -> Result<(), String> {
        if let Some(plat) = platform {
            if !plat.is_empty() {
                eprintln!("prepare: pulling {img} --platform {plat}");
                return podman(&["pull", "--platform", plat, img]).map(|_| ());
            }
        }
        eprintln!("prepare: pulling {img}");
        podman(&["pull", img]).map(|_| ())
    };
    match pull {
        PullPolicy::Never => {
            if !exists {
                return Err(format!(
                    "image {image} missing and pull policy is never — pull it, set GHA_PULL_POLICY=missing|always, or use image-mode=build"
                ));
            }
            eprintln!("prepare: using local image {image} (pull=never)");
            Ok(())
        }
        PullPolicy::Missing => {
            if exists {
                // Local tag may be host-arch only; `podman run --platform` + --pull=missing
                // fetches the right manifest at spawn when needed.
                eprintln!("prepare: image {image} already present (pull=missing)");
                Ok(())
            } else {
                pull_with_platform(image)?;
                Ok(())
            }
        }
        PullPolicy::Always => {
            pull_with_platform(image)?;
            Ok(())
        }
    }
}

fn chown_spec(cli: &Cli) -> String {
    // Prefer numeric uid:gid for chown inside helper containers.
    let u = cli.runner_user.trim();
    if u.contains(':')
        && u.split(':')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    {
        u.to_string()
    } else if u.chars().all(|c| c.is_ascii_digit()) {
        format!("{u}:{u}")
    } else {
        // Name-only: best-effort leave ownership to entrypoint / image defaults.
        DEFAULT_RUNNER_USER.to_string()
    }
}

fn seed_volume_from_stock_image(cli: &Cli) -> Result<(), String> {
    let chown = chown_spec(cli);
    let script = format!(
        r#"
set -euo pipefail
if [[ ! -x /opt/actions-runner/run.sh ]]; then
  if [[ -x /opt/actions-runner-seed/run.sh ]]; then
    cp -a /opt/actions-runner-seed/. /opt/actions-runner/
  else
    echo "stock image missing /opt/actions-runner-seed — rebuild packaging image" >&2
    exit 1
  fi
fi
chown -R {chown} /opt/actions-runner 2>/dev/null || true
chmod -R go-w /opt/actions-runner 2>/dev/null || true
date -u +%Y-%m-%dT%H:%M:%SZ > /opt/actions-runner/.snapshot-baseline
chown {chown} /opt/actions-runner/.snapshot-baseline 2>/dev/null || true
echo ok
"#
    );
    eprintln!("prepare: seeding volume from stock image snapshot…");
    podman(&[
        "run",
        "--rm",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "/bin/bash",
        "-v",
        &format!("{}:/opt/actions-runner:Z", cli.volume),
        &cli.image,
        "-c",
        &script,
    ])?;
    Ok(())
}

/// Inject official (or custom-URL) actions/runner into the work volume for any rootfs image.
fn seed_volume_runner_kit(cli: &Cli) -> Result<(), String> {
    let chown = chown_spec(cli);
    let url = cli.runner_seed_url.clone().unwrap_or_else(|| {
        format!(
            "https://github.com/actions/runner/releases/download/v{ver}/actions-runner-linux-{arch}-{ver}.tar.gz",
            ver = cli.runner_version,
            arch = cli.runner_arch
        )
    });
    if !is_safe_url(&url) {
        return Err("runner seed URL failed safety check".into());
    }
    let sha = cli.runner_sha256.clone();
    // Idempotent: skip download when run.sh already present (user pre-seeded or re-prepare).
    let script = format!(
        r#"
set -euo pipefail
HOME_DIR=/opt/actions-runner
if [[ -x "$HOME_DIR/run.sh" ]]; then
  echo "runner kit already present on volume — refreshing ownership only"
  chown -R {chown} "$HOME_DIR" 2>/dev/null || true
  chmod -R go-w "$HOME_DIR" 2>/dev/null || true
  date -u +%Y-%m-%dT%H:%M:%SZ > "$HOME_DIR/.snapshot-baseline"
  chown {chown} "$HOME_DIR/.snapshot-baseline" 2>/dev/null || true
  exit 0
fi
export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update -qq
  apt-get install -y -qq --no-install-recommends ca-certificates curl tar gzip coreutils >/dev/null
elif command -v microdnf >/dev/null 2>&1; then
  microdnf install -y ca-certificates curl tar gzip coreutils >/dev/null
elif command -v dnf >/dev/null 2>&1; then
  dnf install -y ca-certificates curl tar gzip coreutils >/dev/null
elif command -v apk >/dev/null 2>&1; then
  apk add --no-cache ca-certificates curl tar gzip coreutils >/dev/null
fi
command -v curl >/dev/null
command -v tar >/dev/null
mkdir -p "$HOME_DIR"
cd "$HOME_DIR"
curl -fsSL -o actions-runner.tar.gz "{url}"
echo "{sha}  actions-runner.tar.gz" | sha256sum -c -
tar xzf actions-runner.tar.gz
rm -f actions-runner.tar.gz
# Best-effort OS deps for the runner (official script; may no-op on non-Debian).
if [[ -x ./bin/installdependencies.sh ]]; then
  ./bin/installdependencies.sh || true
fi
chown -R {chown} "$HOME_DIR" 2>/dev/null || true
chmod -R go-w "$HOME_DIR" 2>/dev/null || true
date -u +%Y-%m-%dT%H:%M:%SZ > "$HOME_DIR/.snapshot-baseline"
chown {chown} "$HOME_DIR/.snapshot-baseline" 2>/dev/null || true
echo ok
"#
    );
    // Always seed via configurable helper (bash+curl+pkg manager). Work image stays the job rootfs.
    let helper = cli.seed_helper_image.as_str();
    eprintln!(
        "prepare: seeding runner kit into volume via helper {} (work rootfs image remains {})",
        helper, cli.image
    );
    ensure_image_present(helper, &PullPolicy::Missing)?;
    let vol = format!("{}:/opt/actions-runner:Z", cli.volume);
    podman(&[
        "run",
        "--rm",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "/bin/bash",
        "-v",
        &vol,
        helper,
        "-c",
        &script,
    ])?;
    Ok(())
}

fn resolve_entrypoint_path(cli: &Cli) -> Result<PathBuf, String> {
    if let Some(p) = &cli.entrypoint {
        let p = p.canonicalize().map_err(|e| format!("entrypoint: {e}"))?;
        if !p.is_file() {
            return Err(format!("entrypoint not a file: {}", p.display()));
        }
        return Ok(p);
    }
    let dir = resolve_build_dir(cli)?;
    let p = dir.join("entrypoint.sh");
    if p.is_file() {
        return Ok(p);
    }
    Err(format!(
        "entrypoint.sh not found under {} — set GHA_ENTRYPOINT to your runner entrypoint script",
        dir.display()
    ))
}

fn work_image_pull_arg(cli: &Cli) -> &'static str {
    let mode = effective_image_mode(&cli.image_mode, &cli.image);
    let pull = effective_pull_policy(cli.pull_policy.as_ref(), &mode);
    pull_policy_arg(&pull)
}

fn needs_host_entrypoint(cli: &Cli) -> bool {
    matches!(
        effective_image_mode(&cli.image_mode, &cli.image),
        ImageMode::External
    )
}

fn private_env_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user_suffix = current_username();
    dir.join(format!(
        "gha-runner-ctl-{}-{}.env",
        std::process::id(),
        user_suffix
    ))
}

fn retain_marker_path(cli: &Cli) -> PathBuf {
    let user_suffix = current_username();
    reg_pace_paths()
        .0
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join(format!(
            "gha-runner-ctl-retain-{}-{}.ok",
            cli.container, user_suffix
        ))
}

fn retain_max_age_secs() -> u64 {
    std::env::var("GHA_RETAIN_MAX_AGE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RETAIN_MAX_AGE_SECS)
}

fn retain_max_jobs() -> u32 {
    std::env::var("GHA_RETAIN_MAX_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_RETAIN_MAX_JOBS)
}

/// On-disk retain marker: last successful retain target plus the bookkeeping
/// needed to bound reuse (age + reuse count — see [`DEFAULT_RETAIN_MAX_AGE_SECS`]).
/// `created_unix` is set once, at fresh registration, and never touched again;
/// `reuse_count` increments each time [`up`] reuses this registration.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetainMarker {
    url: String,
    created_unix: u64,
    reuse_count: u32,
}

fn read_retain_marker(cli: &Cli) -> Option<RetainMarker> {
    let marker = retain_marker_path(cli);
    let s = fs::read_to_string(&marker).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Old format (pre-bounded-retain): the marker held nothing but the bare repo
    // URL. `serde_json::from_str` on a bare `https://…` string fails to parse as
    // `RetainMarker`, so this branch naturally returns `None` for it — the caller
    // treats a parse failure as "unknown age, do not reuse" (safe direction).
    serde_json::from_str::<RetainMarker>(trimmed).ok()
}

/// Volume already holds a durable runner registration we can reuse without a
/// fresh registration-token POST — *iff* it is for the same repo/org target and
/// within the bounded retain lifetime (age and reuse-count caps).
///
/// This is NOT a credential-expiry check: the registration token is long since
/// consumed (single-use, at `config.sh`) and the runner's own credentials on the
/// volume do not expire on this schedule. The bound is workspace hygiene — evict
/// a long-lived `_work` dir / job history periodically — so an old URL-only
/// marker (no recorded age) is treated as unknown age and refused, the safe
/// direction, rather than assumed fresh.
fn volume_has_runner_config(cli: &Cli) -> bool {
    let Some(marker) = read_retain_marker(cli) else {
        return false;
    };
    if marker.url != github_url(cli) {
        return false;
    }
    let age = now_unix().saturating_sub(marker.created_unix);
    if age > retain_max_age_secs() {
        return false;
    }
    if marker.reuse_count >= retain_max_jobs() {
        return false;
    }
    true
}

/// Record a successful retain target after [`up`]. `reused` must match the
/// `can_reuse` decision that drove this `up()` call: `true` preserves the
/// original `created_unix` and bumps `reuse_count` (this container is riding an
/// existing registration); `false` resets both (a fresh registration-token POST
/// just happened, so the retain clock restarts).
fn mark_retain_ok(cli: &Cli, reused: bool) {
    let marker_path = retain_marker_path(cli);
    let now = now_unix();
    let url = github_url(cli);
    let record = if reused {
        match read_retain_marker(cli).filter(|m| m.url == url) {
            Some(prior) => RetainMarker {
                url,
                created_unix: prior.created_unix,
                reuse_count: prior.reuse_count.saturating_add(1),
            },
            // Prior marker vanished or targeted a different repo — cannot have
            // been the source of the reuse decision; treat as fresh, safe side.
            None => RetainMarker {
                url,
                created_unix: now,
                reuse_count: 0,
            },
        }
    } else {
        RetainMarker {
            url,
            created_unix: now,
            reuse_count: 0,
        }
    };
    if let Ok(s) = serde_json::to_string(&record) {
        let _ = fs::write(&marker_path, s);
    }
}

fn write_env_file(path: &Path, reg_token: &str, cli: &Cli) -> Result<(), String> {
    let ephemeral = effective_ephemeral(cli);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| format!("env file: {e}"))?;
    writeln!(
        f,
        "REPO_URL={}\nRUNNER_NAME={}\nRUNNER_LABELS={}\nRUNNER_EPHEMERAL={}\nRUNNER_RETAIN={}\nRUNNER_TOKEN={}",
        github_url(cli),
        cli.runner_name,
        cli.labels,
        if ephemeral { "true" } else { "false" },
        if ephemeral { "false" } else { "true" },
        reg_token
    )
    .map_err(|e| format!("env write: {e}"))?;
    #[cfg(unix)]
    chmod_0600(path)?;
    Ok(())
}

fn shred_env_file(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let len = meta.len() as usize;
        if let Ok(mut f) = OpenOptions::new().write(true).open(path) {
            let _ = f.write_all(&vec![0_u8; len.max(64)]);
            let _ = f.flush();
        }
    }
    let _ = fs::remove_file(path);
}

/// Active registration target repo for status file (user batch).
fn active_target_path(cli: &Cli) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let user_suffix = current_username();
    dir.join(format!(
        "gha-runner-ctl-active-{}-{}.txt",
        cli.container, user_suffix
    ))
}

fn set_active_target(cli: &Cli, repo: &str) {
    let p = active_target_path(cli);
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&p) {
        if f.write_all(repo.as_bytes()).is_ok() {
            #[cfg(unix)]
            let _ = chmod_0600(&p);
        }
    }
}

fn get_active_target(cli: &Cli) -> Option<String> {
    fs::read_to_string(active_target_path(cli))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| is_safe_repo(s))
}

fn clear_active_target(cli: &Cli) {
    let _ = fs::remove_file(active_target_path(cli));
}

fn up(cli: &Cli) -> Result<(), String> {
    if container_running(&cli.container) {
        eprintln!(
            "up: already running ({}) — GitHub pushes jobs to this session (no re-register)",
            cli.container
        );
        return Ok(());
    }
    if !volume_exists(&cli.volume) {
        return Err(format!(
            "volume {} missing — run `gha-runner-ctl prepare` first",
            cli.volume
        ));
    }
    if matches!(cli.scope, Scope::User) && cli.repo.is_none() {
        return Err("user batch: no active repo with demand (listen selects it)".into());
    }
    // Explicit GHA_PLATFORM / --platform: refuse when binfmt cannot run that arch.
    if let Some(plat) = cli.platform.as_deref() {
        let host = TargetArch::host();
        let target = match plat {
            "linux/amd64" | "linux/x86_64" => Some(TargetArch::Amd64),
            "linux/arm64" | "linux/aarch64" => Some(TargetArch::Arm64),
            "linux/arm/v7" | "linux/arm" => Some(TargetArch::Arm),
            "linux/riscv64" => Some(TargetArch::Riscv64),
            "linux/386" => Some(TargetArch::X86),
            "linux/s390x" => Some(TargetArch::S390x),
            "linux/ppc64le" => Some(TargetArch::Ppc64le),
            _ => None,
        };
        if let Some(arch) = target {
            if arch != host {
                ensure_binfmt_for_arch(arch, true, None)?;
            }
        }
    }

    let ephemeral = effective_ephemeral(cli);
    // Retain reuse: if we already have runner config on the volume for this repo,
    // skip minting a registration-token (biggest API saver).
    let can_reuse = !ephemeral && volume_has_runner_config(cli);
    let env_path = private_env_path();
    if can_reuse {
        eprintln!(
            "up: reusing retained registration on volume for {} (no registration-token POST)",
            github_url(cli)
        );
        write_env_file(&env_path, "REUSE", cli)?;
    } else {
        let api = github_token(cli)?;
        let reg = registration_token(cli, &api, &cli.http())?;
        write_env_file(&env_path, &reg, cli)?;
        drop(reg);
        drop(api);
    }

    if container_exists(&cli.container) {
        let _ = podman(&["rm", "-f", &cli.container]);
    }

    let img_mode = effective_image_mode(&cli.image_mode, &cli.image);
    let pull_arg = work_image_pull_arg(cli);
    eprintln!(
        "up: scope={:?} mode={:?} image_mode={img_mode:?} pull={pull_arg} platform={:?} ephemeral={ephemeral} user={} image={} url={}",
        cli.scope,
        cli.mode,
        cli.platform,
        cli.runner_user,
        cli.image,
        github_url(cli)
    );
    let env_path_str = env_path.to_str().ok_or("env path not utf-8")?.to_string();
    let vol = format!("{}:/opt/actions-runner:Z", cli.volume);
    let eph = if ephemeral { "true" } else { "false" };
    let ret = if ephemeral { "false" } else { "true" };
    let eph_kv = format!("RUNNER_EPHEMERAL={eph}");
    let ret_kv = format!("RUNNER_RETAIN={ret}");

    // Host entrypoint for external images (stock image already has ENTRYPOINT).
    let entrypoint_path = if needs_host_entrypoint(cli) {
        Some(resolve_entrypoint_path(cli)?)
    } else {
        None
    };
    let entrypoint_mount = entrypoint_path.as_ref().map(|p| {
        format!(
            "{}:/entrypoint.sh:ro,Z",
            p.to_str().expect("entrypoint path utf-8")
        )
    });

    // Cross-arch: --platform before image (issue #28). Empty when native / unset.
    let platform_owned: Vec<String> = podman_platform_args(cli.platform.as_deref());
    let platform_refs: Vec<&str> = platform_owned.iter().map(String::as_str).collect();

    let mut args: Vec<&str> = Vec::with_capacity(48);
    args.push("run");
    args.push("-d");
    // Platform must apply to the create/pull of this container.
    args.extend_from_slice(&platform_refs);
    args.extend_from_slice(&[
        "--name",
        cli.container.as_str(),
        "--cpus",
        cli.cpus.as_str(),
        "--memory",
        cli.memory.as_str(),
        "--memory-swap",
        cli.memory.as_str(),
        "--pids-limit",
        "4096",
        "--security-opt",
        "no-new-privileges",
        "--cap-drop",
        "ALL",
        "--pull",
        pull_arg,
        "--user",
        cli.runner_user.as_str(),
        // Work endpoints never receive a container runtime socket (no nested spawn).
        "--env-file",
        env_path_str.as_str(),
        "-e",
        eph_kv.as_str(),
        "-e",
        ret_kv.as_str(),
        "-v",
        vol.as_str(),
    ]);
    if let Some(m) = entrypoint_mount.as_ref() {
        args.push("-v");
        args.push(m.as_str());
        args.push("--entrypoint");
        args.push("/entrypoint.sh");
    }
    // WSL2 GPU: nvidia toolkit + /dev/dxg + host WSL lib mount (verified on this host).
    // Soft dual-slice: both workers may see the full device (GeForce has no MIG); jobs
    // cooperate via labels gpu-slice-a|b. Tear-down on idle frees device processes.
    let mut gpu_env_owned: Vec<String> = Vec::new();
    if cli.gpu {
        args.extend_from_slice(&[
            "--gpus",
            "all",
            "--device",
            "/dev/dxg",
            "-e",
            "LD_LIBRARY_PATH=/usr/lib/wsl/lib",
            "-e",
            "NVIDIA_VISIBLE_DEVICES=all",
            "-e",
            "CUDA_VISIBLE_DEVICES=0",
            "-v",
            "/usr/lib/wsl:/usr/lib/wsl:ro",
            "-e",
            "CUDA_MPS_ACTIVE_THREAD_PERCENTAGE=50",
        ]);
        if let Some(s) = cli.gpu_slice.as_deref() {
            let s = s.trim().to_ascii_lowercase();
            if s == "a" || s == "b" {
                gpu_env_owned.push(format!("GHA_GPU_SLICE={s}"));
            }
        }
    }
    for e in &gpu_env_owned {
        args.push("-e");
        args.push(e.as_str());
    }
    args.push(cli.image.as_str());
    let result = podman(&args);
    shred_env_file(&env_path);
    result?;

    if let Some(repo) = cli.repo.as_ref() {
        set_active_target(cli, repo);
    }
    if !ephemeral {
        mark_retain_ok(cli, can_reuse);
    }
    eprintln!(
        "up: container {} gpu={} slice={:?}",
        cli.container, cli.gpu, cli.gpu_slice
    );
    Ok(())
}

fn down(cli: &Cli, rm: bool) -> Result<(), String> {
    if container_exists(&cli.container) {
        eprintln!("down: stopping {}", cli.container);
        let _ = podman(&["stop", "-t", "30", &cli.container]);
        if rm {
            let _ = podman(&["rm", "-f", &cli.container]);
        }
    } else {
        eprintln!("down: no container {}", cli.container);
    }
    // When this was a GPU worker and no other GPU runner containers remain, note free.
    if cli.gpu {
        let siblings = ["gha-runner-gpu", "gha-runner-gpu-a", "gha-runner-gpu-b"];
        let any_gpu_up = siblings.iter().any(|n| container_running(n));
        if !any_gpu_up {
            eprintln!("down: no GPU runner containers running — GPU returned to host (idle)");
        }
    }
    let ephemeral = effective_ephemeral(cli);
    if ephemeral {
        let vol = format!("{}:/opt/actions-runner:Z", cli.volume);
        let pull = work_image_pull_arg(cli);
        // Prefer seed helper (guaranteed shell) so external rootfs without bash still cleans.
        let cleaner = if needs_host_entrypoint(cli) {
            cli.seed_helper_image.as_str()
        } else {
            cli.image.as_str()
        };
        let _ = podman(&[
            "run",
            "--rm",
            "--security-opt",
            "no-new-privileges",
            "--pull",
            pull,
            "--entrypoint",
            "/bin/sh",
            "-v",
            vol.as_str(),
            cleaner,
            "-c",
            "rm -f /opt/actions-runner/.runner /opt/actions-runner/.credentials /opt/actions-runner/.credentials_rsaparams 2>/dev/null; true",
        ]);
    }
    clear_active_target(cli);
    Ok(())
}

fn status(cli: &Cli) -> Result<(), String> {
    println!("scope: {:?}", cli.scope);
    match cli.scope {
        Scope::Repo => println!("repo: {}", cli.repo.as_deref().unwrap_or("?")),
        Scope::Org => println!("org: {}", cli.owner.as_deref().unwrap_or("?")),
        Scope::User => {
            println!("user: {}", cli.user.as_deref().unwrap_or("?"));
            println!(
                "active_registration: {}",
                get_active_target(cli).unwrap_or_else(|| "(none)".into())
            );
        }
    }
    if matches!(cli.scope, Scope::User) && cli.repo.is_none() {
        println!("register_url: (none until demand selects a repo)");
    } else {
        println!("register_url: {}", github_url(cli));
    }
    println!("container: {}", cli.container);
    if container_exists(&cli.container) {
        println!("  exists: true");
        println!("  running: {}", container_running(&cli.container));
    } else {
        println!("  exists: false");
    }
    println!(
        "volume: {} (exists={})",
        cli.volume,
        volume_exists(&cli.volume)
    );
    println!("mode: {:?}", cli.mode);
    println!("labels: {}", cli.labels);
    Ok(())
}

// --- doctor / auth-check -------------------------------------------------------

#[derive(Deserialize)]
struct RateLimitResponse {
    resources: RateLimitResources,
}

#[derive(Deserialize)]
struct RateLimitResources {
    core: RateLimitDetail,
}

#[derive(Deserialize)]
struct RateLimitDetail {
    limit: u32,
    remaining: u32,
}

/// `GET /rate_limit` for whatever token is active. This is the live figure `doctor`
/// prints — never a compiled-in constant (installation limits scale with installation
/// size; a classic PAT is a flat 5,000/hour; neither is safe to hardcode here).
fn rate_limit_http(token: &str, http: &HttpConfig) -> Result<RateLimitDetail, String> {
    let resp = http_agent(http)
        .get("https://api.github.com/rate_limit")
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("GET /rate_limit failed: {}", redact(&e.to_string())))?;
    let status = resp.status();
    if !(200..300).contains(&status) {
        return Err(format!("GET /rate_limit failed: HTTP {status}"));
    }
    resp.into_json::<RateLimitResponse>()
        .map(|b| b.resources.core)
        .map_err(|e| format!("GET /rate_limit response parse failed: {e}"))
}

/// `gha-runner-ctl doctor`: report which auth path is active and its live health,
/// without ever printing a token, JWT, or key. See `Cmd::Doctor` for why this is a
/// separate command from `status`. Each check prints `[PASS]`/`[FAIL]`/`[INFO]` with
/// actionable text on failure; returns `Err` (non-zero exit) if anything failed.
fn doctor(cli: &Cli) -> Result<(), String> {
    println!("gha-runner-ctl doctor");
    println!("======================");
    let mut any_fail = false;

    match cli.app_auth_config() {
        Ok(Some(cfg)) => {
            println!("[PASS] auth path: GitHub App (app_id={})", cfg.app_id);
            match appauth::doctor_report(&cfg, cli.app_auth_owner_hint().as_deref(), &cli.http()) {
                Ok(report) => {
                    println!(
                        "[PASS] app: {} (id={}, slug={})",
                        report.app_name.as_deref().unwrap_or("(name unavailable)"),
                        report.app_id,
                        report.app_slug.as_deref().unwrap_or("?")
                    );
                    println!(
                        "[PASS] installation: id={} account={} repository_selection={}",
                        report.installation_id,
                        report.account_login,
                        report.repository_selection.as_deref().unwrap_or("?")
                    );
                    let perms_str = if report.permissions.is_empty() {
                        "(none reported)".to_string()
                    } else {
                        report
                            .permissions
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    println!("[PASS] permissions granted: {perms_str}");
                    let missing = appauth::missing_permissions(&report.permissions, cli.scope);
                    let expected =
                        appauth::describe_permissions(appauth::expected_permissions(cli.scope));
                    if missing.is_empty() {
                        println!("[PASS] permissions: cover the documented set ({expected})");
                        // Only personal (user/repo-scoped) installs get this.
                        // An org install is already on the narrow permission and
                        // must not be nagged toward a migration it has done.
                        if !matches!(cli.scope, Scope::Org) {
                            for line in appauth::personal_scope_advisory() {
                                println!("{line}");
                            }
                        }
                    } else {
                        let slug = report.app_slug.as_deref().unwrap_or("<app-slug>");
                        println!(
                            "[FAIL] permissions: missing/under-scoped: {} — fix at \
                             https://github.com/settings/apps/{slug}/permissions, then \
                             approve the update on the account it's installed on",
                            missing.join(", ")
                        );
                        any_fail = true;
                    }
                }
                Err(e) => {
                    println!("[FAIL] app auth: {}", redact(&e));
                    any_fail = true;
                }
            }
        }
        Ok(None) => {
            println!(
                "[INFO] auth path: not using GitHub App auth (--app-id/GHA_APP_ID and/or \
                 --app-private-key/GHA_APP_PRIVATE_KEY not set)"
            );
        }
        Err(e) => {
            println!("[FAIL] app auth configuration: {}", redact(&e));
            any_fail = true;
        }
    }

    match github_token_with_source(cli) {
        Ok((token, source)) => {
            println!("[PASS] token acquired via {source}");
            match rate_limit_http(&token, &cli.http()) {
                Ok(rl) => {
                    println!(
                        "[PASS] rate limit (core, live): {}/{} remaining",
                        rl.remaining, rl.limit
                    );
                }
                Err(e) => {
                    println!("[FAIL] rate limit check: {}", redact(&e));
                    any_fail = true;
                }
            }
        }
        Err(e) => {
            println!("[FAIL] token acquisition: {}", redact(&e));
            any_fail = true;
        }
    }

    println!("======================");
    if any_fail {
        Err("doctor: one or more checks failed (see [FAIL] lines above)".to_string())
    } else {
        println!("all checks passed");
        Ok(())
    }
}

// --- Demand ------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WorkflowRuns {
    workflow_runs: Vec<WorkflowRun>,
}

#[derive(Debug, Deserialize)]
struct WorkflowRun {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct JobsResp {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct Job {
    status: String,
    labels: Vec<String>,
    /// GitHub job display name (used for automatic size heuristics).
    #[serde(default)]
    name: Option<String>,
}

/// Queued/in-progress self-hosted job that matches this listener.
#[derive(Debug, Clone)]
struct DemandJob {
    repo: String,
    job_name: String,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct NamedRepo {
    full_name: String,
    fork: Option<bool>,
    archived: Option<bool>,
    private: Option<bool>,
}

fn parse_repo_csv(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in s.split([',', '\n', '\r']) {
        let p = part.split('#').next().unwrap_or("").trim();
        if p.is_empty() {
            continue;
        }
        if is_safe_repo(p) && !out.iter().any(|x: &String| x == p) {
            out.push(p.to_string());
        }
    }
    out
}

/// Print a deprecation warning at most once per process.
///
/// The previous behaviour printed on EVERY resolve, and since the allowlist is resolved once
/// per poll tick that meant the identical line every interval for as long as the manager ran
/// — observed in a live fleet journal repeating every 3 minutes for hours. A warning that
/// appears thousands of times trains operators to filter it out, which is strictly worse than
/// warning once and being read.
fn warn_deprecated_once(slot: &'static AtomicBool, msg: &str) {
    if !slot.swap(true, AtomicOrdering::Relaxed) {
        eprintln!("{msg}");
    }
}

/// Allowlist: file (if set) then env CSV. Deduped, order preserved.
fn allowlist_repos_list(cli: &Cli) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(path) = cli.effective_allowlist_repos_file() {
        match fs::read_to_string(path) {
            Ok(s) => {
                for p in parse_repo_csv(&s) {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
            Err(e) => eprintln!("listen: allowlist-repos-file {path}: {e}"),
        }
    }
    if let Some(allow) = cli.effective_allowlist_repos() {
        for p in parse_repo_csv(&allow) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

fn priority_repos_list(cli: &Cli) -> Vec<String> {
    cli.priority_repos
        .as_ref()
        .map(|s| parse_repo_csv(s))
        .unwrap_or_default()
}

fn tick_log_path(cli: &Cli) -> Option<PathBuf> {
    let raw = cli.tick_log.trim();
    if raw.is_empty()
        || raw.eq_ignore_ascii_case("off")
        || raw.eq_ignore_ascii_case("false")
        || raw == "0"
    {
        return None;
    }
    if raw.eq_ignore_ascii_case("auto") {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        return Some(base.join("gha-runner-ctl/logs/listen-ticks.jsonl"));
    }
    Some(PathBuf::from(raw))
}

fn append_tick_log(path: &Path, obj: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let line = obj.to_string();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// Normalize Podman/Go time for GNU `date -d`.
/// Podman prints e.g. `2026-07-21 15:52:33.909118621 -0400 EDT` which GNU date rejects.
fn normalize_podman_started_at(raw: &str) -> String {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    // Common forms:
    //   2026-07-21 15:52:33.909118621 -0400 EDT
    //   2026-07-21T15:52:33.909118621-04:00
    if parts.len() >= 3 && parts[0].contains('-') && !parts[0].contains('T') {
        let date = parts[0];
        let mut time = parts[1].to_string();
        if let Some(dot) = time.find('.') {
            time.truncate(dot);
        }
        let offset = parts[2]; // -0400 / +0000
        return format!("{date} {time} {offset}");
    }
    // ISO-8601 single token: strip fractional seconds before offset
    let s = raw.trim();
    if let Some(tpos) = s.find('T') {
        let (date, rest) = s.split_at(tpos);
        // rest starts with T...
        let body = &rest[1..];
        // split time from offset (+/- or Z)
        let mut time = body;
        let mut offset = "";
        if let Some(z) = body.find('Z') {
            time = &body[..z];
            offset = "Z";
        } else if let Some(i) = body.rfind('+').or_else(|| {
            // last '-' after time (skip date-style)
            body.char_indices()
                .skip(8)
                .find(|(_, c)| *c == '-')
                .map(|(i, _)| i)
        }) {
            time = &body[..i];
            offset = &body[i..];
        }
        if let Some(dot) = time.find('.') {
            time = &time[..dot];
        }
        if offset.is_empty() || offset == "Z" {
            return format!("{date} {time} UTC");
        }
        // normalize +04:00 → +0400
        let off = offset.replace(':', "");
        return format!("{date} {time} {off}");
    }
    s.to_string()
}

fn container_started_age_secs(name: &str) -> Option<u64> {
    let out = std::process::Command::new("podman")
        .args(["inspect", "-f", "{{.State.StartedAt}}", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let started_raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if started_raw.is_empty() {
        return None;
    }
    let started = normalize_podman_started_at(&started_raw);
    // GNU date
    let age = std::process::Command::new("date")
        .args(["-d", &started, "+%s"])
        .output()
        .ok()?;
    if !age.status.success() {
        eprintln!(
            "listen: cannot parse container start time for {name}: raw={started_raw:?} norm={started:?}"
        );
        return None;
    }
    let started_unix: u64 = String::from_utf8_lossy(&age.stdout).trim().parse().ok()?;
    let now = now_unix();
    Some(now.saturating_sub(started_unix))
}

/// Stop+rm fleet worker containers that are running, not in pool claims, older than reap_stale_secs.
fn reap_stale_containers(cli: &Cli, pool: &ResourcePool) {
    if cli.reap_stale_secs == 0 {
        return;
    }
    let claimed: std::collections::HashSet<String> = pool
        .claims()
        .map(|c| c.into_iter().map(|x| x.container).collect())
        .unwrap_or_default();
    let out = match std::process::Command::new("podman")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => {
            eprintln!("listen: reap_stale: podman ps failed");
            return;
        }
    };
    let prefix = cli.container.as_str();
    let mut considered = 0u32;
    let mut reaped = 0u32;
    for name in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
        // Only touch our fleet naming: base prefix, or historical retain leftovers.
        let ours = name.starts_with(prefix)
            || name.starts_with("gha-runner-cpu")
            || name.starts_with("gha-runner-ctl");
        if !ours {
            continue;
        }
        if claimed.contains(name) {
            continue;
        }
        considered += 1;
        let Some(age) = container_started_age_secs(name) else {
            continue;
        };
        if age < cli.reap_stale_secs {
            continue;
        }
        eprintln!(
            "listen: reap stale container {name} age={age}s (threshold={}s, not in pool claims)",
            cli.reap_stale_secs
        );
        // Force-rm (no 30s graceful stop) — retain/warm leftovers must not block listen.
        let _ = podman(&["rm", "-f", name]);
        reaped += 1;
    }
    eprintln!(
        "listen: reap_stale done considered={considered} reaped={reaped} threshold={}s claims={}",
        cli.reap_stale_secs,
        claimed.len()
    );
}

fn repos_round_robin_state_path(container: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let safe: String = container
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let user_suffix = current_username();
    dir.join(format!("gha-runner-ctl-rr-{safe}-{user_suffix}.txt"))
}

/// Cursor for the PRIORITY scan, separate from the non-priority round-robin.
fn priority_round_robin_state_path(container: &str) -> PathBuf {
    let p = repos_round_robin_state_path(container);
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().replace("-rr-", "-prio-rr-"))
        .unwrap_or_else(|| "gha-runner-ctl-prio-rr.txt".to_string());
    p.with_file_name(name)
}

/// Rotate `items` so the scan starts at a persisted offset, then advance that
/// offset by however many entries were actually consumed last tick.
///
/// Why this exists: the priority set is scanned in FIXED order every tick, and
/// the scan stops when the per-poll API budget is exhausted. Truncation
/// therefore always cuts the same tail, so once the priority list outgrows the
/// budget the repos at the end are never polled at all — not "polled late",
/// never. Repos appended to the list land exactly there, which is how 47
/// freshly-added mycelium repos sat unscanned while their jobs queued for hours.
///
/// Rotating trades strict head-first ordering for guaranteed coverage: over
/// enough ticks every priority repo reaches the front. If strict ordering
/// matters more than coverage, keep the priority list small enough to fit
/// inside `--api-max-per-poll`, where this rotation is a no-op anyway.
fn rotate_by_cursor(items: &[String], path: &PathBuf) -> (Vec<String>, usize) {
    let len = items.len();
    if len == 0 {
        return (Vec::new(), 0);
    }
    let offset: usize = fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(0)
        % len;
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(items[(offset + i) % len].clone());
    }
    (out, offset)
}

/// Persist where the next tick should start. `consumed` is how many entries the
/// scan actually got through before the budget ran out; advancing by exactly
/// that much makes coverage complete rather than merely shuffled.
fn advance_cursor(path: &PathBuf, offset: usize, consumed: usize, len: usize) {
    if len == 0 {
        return;
    }
    let next = (offset + consumed.max(1)) % len;
    let _ = fs::write(path, next.to_string());
}

/// Subset of allowlisted repos for this demand tick (`repos_per_tick`; 0 = all).
fn select_repos_for_tick(cli: &Cli, repos: &[String]) -> Vec<String> {
    if repos.is_empty() {
        return Vec::new();
    }
    if cli.repos_per_tick == 0 {
        return repos.to_vec();
    }
    let n = cli.repos_per_tick as usize;
    let len = repos.len();
    let path = repos_round_robin_state_path(&cli.container);
    let mut offset: usize = fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
        % len;
    let take = n.min(len);
    let mut out = Vec::with_capacity(take);
    for i in 0..take {
        out.push(repos[(offset + i) % len].clone());
    }
    offset = (offset + take) % len;
    let _ = fs::write(&path, offset.to_string());
    out
}

fn poll_allowlist_repos(
    cli: &Cli,
    api: &str,
    pacer: &mut ApiPacer,
    repos: &[String],
) -> Result<(bool, Option<String>), String> {
    // Priority repos every tick, then RR subset of the rest.
    let priority = priority_repos_list(cli);
    let mut order: Vec<String> = priority
        .iter()
        .filter(|p| repos.iter().any(|r| r == *p))
        .cloned()
        .collect();
    let rest: Vec<String> = repos
        .iter()
        .filter(|r| !priority.iter().any(|p| p == *r))
        .cloned()
        .collect();
    for name in select_repos_for_tick(cli, &rest) {
        if !order.contains(&name) {
            order.push(name);
        }
    }
    if order.is_empty() {
        order = select_repos_for_tick(cli, repos);
    }
    for name in order {
        match repo_needs_runner(cli, &name, api, pacer) {
            Ok(true) => return Ok((true, Some(name))),
            Ok(false) => {}
            Err(e) if is_soft_api_err(&e) => {
                eprintln!("listen: allowlist skip {name}: {}", redact(&e));
                if e.contains("rate limited") || e.contains("budget exhausted") {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok((false, None))
}

/// Returns (need_runner, optional active_repo_for_registration).
fn demand(cli: &Cli, api: &str, pacer: &mut ApiPacer) -> Result<(bool, Option<String>), String> {
    pacer.begin_poll();
    let mut filter_private = false;
    let mut filter_public = false;

    if cli.private_only {
        filter_private = true;
    } else if cli.all_repos {
        // Allow both
    } else {
        // Default to public only (includes when public_only is explicitly set)
        filter_public = true;
    }

    match cli.scope {
        Scope::Repo => {
            if let Some(repo) = cli.repo.as_ref() {
                let repo = repo.clone();
                return Ok((repo_needs_runner(cli, &repo, api, pacer)?, Some(repo)));
            }
            let repos = allowlist_repos_list(cli);
            if !repos.is_empty() {
                return poll_allowlist_repos(cli, api, pacer, &repos);
            }
            Err("repo scope: missing --repo, --prefer-repos, or --prefer-repos-file".into())
        }
        Scope::Org => {
            let owner = cli.owner.as_ref().expect("validated");
            let url = pacer.api_url(&format!("orgs/{owner}/repos?per_page=100&type=all"));
            let repos = list_repos_paginated(&url, api, pacer)?;
            for r in repos {
                if r.archived.unwrap_or(false) || !is_safe_repo(&r.full_name) {
                    continue;
                }
                let is_private = r.private.unwrap_or(false);
                if filter_private && !is_private {
                    continue;
                }
                if filter_public && is_private {
                    continue;
                }
                match repo_needs_runner(cli, &r.full_name, api, pacer) {
                    Ok(true) => return Ok((true, Some(r.full_name))),
                    Ok(false) => {}
                    Err(e) if is_soft_api_err(&e) => {
                        eprintln!("listen: skip {}: {}", r.full_name, redact(&e));
                        if e.contains("rate limited") {
                            return Err(e);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok((false, None))
        }
        Scope::User => {
            let user = cli.user.as_ref().expect("validated");
            // Allowlist mode: when prefer list is set, ONLY poll those repos.
            let pref = allowlist_repos_list(cli);
            if !pref.is_empty() {
                let prefix = format!("{user}/");
                let repos: Vec<String> = pref
                    .into_iter()
                    .filter(|name| name.starts_with(&prefix))
                    .collect();
                return poll_allowlist_repos(cli, api, pacer, &repos);
            }
            // Full owner list — paced + budget-capped; prefer GHA_PREFER_REPOS / _FILE.
            eprintln!(
                "listen: user-batch without prefer list scans owned repos (budget {} GETs/poll, gap {}ms)",
                pacer.max_per_poll,
                pacer.min_gap.as_millis()
            );
            let url = pacer.api_url(&format!(
                "users/{user}/repos?type=owner&per_page=100&sort=updated"
            ));
            let repos = list_repos_paginated(&url, api, pacer)?;
            for r in repos {
                if r.archived.unwrap_or(false) || r.fork.unwrap_or(false) {
                    continue;
                }
                if !is_safe_repo(&r.full_name) {
                    continue;
                }
                if !r.full_name.starts_with(&format!("{user}/")) {
                    continue;
                }
                let is_private = r.private.unwrap_or(false);
                if filter_private && !is_private {
                    continue;
                }
                if filter_public && is_private {
                    continue;
                }
                match repo_needs_runner(cli, &r.full_name, api, pacer) {
                    Ok(true) => return Ok((true, Some(r.full_name))),
                    Ok(false) => {}
                    Err(e) if is_soft_api_err(&e) => {
                        eprintln!("listen: skip {}: {}", r.full_name, redact(&e));
                        if e.contains("rate limited") || e.contains("budget exhausted") {
                            return Err(e);
                        }
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok((false, None))
        }
    }
}

fn is_soft_api_err(e: &str) -> bool {
    e.contains("403")
        || e.contains("404")
        || e.contains("401")
        || e.contains("429")
        || e.contains("rate limit")
        || e.contains("rate limited")
        || e.contains("budget exhausted")
}

fn list_repos_paginated(
    first_url: &str,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<Vec<NamedRepo>, String> {
    let mut out = Vec::new();
    let mut url = Some(first_url.to_string());
    let mut pages = 0;
    while let Some(u) = url {
        pages += 1;
        if pages > 5 {
            // Hard cap: prefer allowlist; never walk 100+ pages mid-poll.
            eprintln!("listen: repo list capped at {pages} pages this poll");
            break;
        }
        let resp = pacer
            .get(&u, api)
            .map_err(|e| format!("list repos: {}", redact(&e)))?;
        let link = resp.header("link").map(|s| s.to_string());
        let batch: Vec<NamedRepo> = resp.into_json().map_err(|e| format!("parse repos: {e}"))?;
        out.extend(batch);
        url = link.and_then(|l| parse_next_link(&l));
    }
    Ok(out)
}

fn parse_next_link(link: &str) -> Option<String> {
    // <url>; rel="next"
    for part in link.split(',') {
        if part.contains("rel=\"next\"") {
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

fn repo_needs_runner(
    cli: &Cli,
    repo: &str,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<bool, String> {
    // Only probe "queued" first (cheaper); check in_progress only if needed for sticky.
    for status in ["queued", "in_progress"] {
        let url = pacer.api_url(&format!(
            "repos/{repo}/actions/runs?status={status}&per_page=5"
        ));
        let runs = match fetch_runs(&url, api, pacer) {
            Ok(r) => r,
            Err(e) if is_soft_api_err(&e) => {
                eprintln!("listen: skip {repo} runs ({status}): {}", redact(&e));
                if e.contains("rate limited") || e.contains("budget exhausted") {
                    return Err(e);
                }
                continue;
            }
            Err(e) => return Err(e),
        };
        // Cap job lookups per repo (stop after first match or few runs).
        for run in runs.into_iter().take(3) {
            match job_matches_listener(cli, repo, run.id, api, pacer) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) if is_soft_api_err(&e) => {
                    eprintln!("listen: skip {repo} jobs: {}", redact(&e));
                    if e.contains("rate limited") || e.contains("budget exhausted") {
                        return Err(e);
                    }
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }
    Ok(false)
}

fn parse_label_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_ascii_lowercase())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Whether an incomplete job's labels should wake this listener.
fn labels_match_demand(cli: &Cli, job_labels: &[String]) -> bool {
    let job: Vec<String> = job_labels
        .iter()
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty())
        .collect();
    if job.is_empty() {
        return false;
    }
    // Baseline: self-hosted or podman (or gpu) so we never wake for pure ubuntu-latest.
    let baseline = job
        .iter()
        .any(|l| l == "self-hosted" || l == "podman" || l == "gpu" || l.starts_with("gpu-slice"));
    if !baseline {
        return false;
    }
    if let Some(req) = cli.demand_require_labels.as_ref() {
        for r in parse_label_csv(req) {
            if !job.iter().any(|l| l == &r) {
                return false;
            }
        }
    }
    if let Some(ex) = cli.demand_exclude_labels.as_ref() {
        for e in parse_label_csv(ex) {
            if job.iter().any(|l| l == &e) {
                return false;
            }
        }
    }
    true
}

fn fetch_runs(url: &str, api: &str, pacer: &mut ApiPacer) -> Result<Vec<WorkflowRun>, String> {
    let resp = pacer
        .get(url, api)
        .map_err(|e| format!("list runs: {url}: {}", redact(&e)))?;
    let body: WorkflowRuns = resp.into_json().map_err(|e| format!("parse runs: {e}"))?;
    Ok(body.workflow_runs)
}

fn job_matches_listener(
    cli: &Cli,
    repo: &str,
    run_id: u64,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<bool, String> {
    Ok(!collect_jobs_for_run(cli, repo, run_id, api, pacer)?.is_empty())
}

fn collect_jobs_for_run(
    cli: &Cli,
    repo: &str,
    run_id: u64,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<Vec<DemandJob>, String> {
    let url = pacer.api_url(&format!("repos/{repo}/actions/runs/{run_id}/jobs"));
    let resp = pacer
        .get(&url, api)
        .map_err(|e| format!("list jobs: {}", redact(&e)))?;
    let body: JobsResp = resp.into_json().map_err(|e| format!("parse jobs: {e}"))?;
    let mut out = Vec::new();
    for j in body.jobs {
        if j.status == "completed" {
            continue;
        }
        if labels_match_demand(cli, &j.labels) {
            out.push(DemandJob {
                repo: repo.to_string(),
                job_name: j.name.unwrap_or_else(|| format!("job-{run_id}")),
                labels: j.labels,
            });
        }
    }
    Ok(out)
}

/// Collect matching queued jobs (for multi-worker + sizing). Cap for API budget.
///
/// On per-poll budget exhaustion: return **partial** results (never fail the whole
/// listen tick empty-handed). That keeps ephemeral workers spawning under backlog
/// instead of spinning on "budget exhausted" with zero ups.
fn list_demand_jobs(
    cli: &Cli,
    api: &str,
    pacer: &mut ApiPacer,
    max_jobs: usize,
) -> Result<Vec<DemandJob>, String> {
    let mut out = Vec::new();
    let mut repos = allowlist_repos_list(cli);
    if repos.is_empty() {
        if let Some(r) = cli.repo.as_ref() {
            repos = vec![r.clone()];
        } else {
            return Ok(out);
        }
    }
    let priority = priority_repos_list(cli);
    // Priority repos every tick (full set, capped), then RR the rest once.
    // The priority set is rotated by a persisted cursor so that budget
    // truncation does not repeatedly starve the same tail — see rotate_by_cursor.
    let mut prio_scan: Vec<String> = Vec::new();
    for p in &priority {
        if (repos.iter().any(|r| r == p) || is_safe_repo(p)) && !prio_scan.contains(p) {
            prio_scan.push(p.clone());
        }
    }
    let prio_cursor_path = priority_round_robin_state_path(&cli.container);
    let (prio_scan, prio_offset) = rotate_by_cursor(&prio_scan, &prio_cursor_path);
    let prio_len = prio_scan.len();
    let mut scan: Vec<String> = prio_scan;
    let rest: Vec<String> = repos
        .iter()
        .filter(|r| !priority.iter().any(|p| p == *r))
        .cloned()
        .collect();
    let tick = if rest.is_empty() {
        Vec::new()
    } else if pool_mode_on(cli) {
        let mut cli_scan = cli.clone_for_listen();
        let cap = cli.pool_scan_per_tick.max(1);
        if cli.repos_per_tick == 0 {
            cli_scan.repos_per_tick = cap;
        } else {
            cli_scan.repos_per_tick = cli.repos_per_tick.min(cap).max(1);
        }
        select_repos_for_tick(&cli_scan, &rest)
    } else {
        select_repos_for_tick(cli, &rest)
    };
    for r in tick {
        if !scan.contains(&r) {
            scan.push(r);
        }
    }

    // Prefer queued runs; also sample in_progress (multi-job matrices can still have
    // queued jobs while the run is overall in_progress). Cap hard for API budget.
    // How many PRIORITY entries the scan got through before stopping. Drives the
    // cursor so the next tick resumes where this one ran out of budget.
    let mut prio_consumed: usize = 0;
    let mut prio_complete = prio_len == 0;
    'budget_hit: {
        for (idx, name) in scan.iter().enumerate() {
            if idx < prio_len {
                prio_consumed = idx;
            }
            if out.len() >= max_jobs {
                break;
            }
            for (status, run_take) in [("queued", 2usize), ("in_progress", 1usize)] {
                if out.len() >= max_jobs {
                    break;
                }
                let url = pacer.api_url(&format!(
                    "repos/{name}/actions/runs?status={status}&per_page=5"
                ));
                let runs = match fetch_runs(&url, api, pacer) {
                    Ok(r) => r,
                    Err(e) if is_soft_api_err(&e) => {
                        if e.contains("budget exhausted") {
                            eprintln!(
                                "listen: list_demand_jobs: budget exhausted mid-scan ({} jobs kept)",
                                out.len()
                            );
                            break 'budget_hit;
                        }
                        if e.contains("rate limited") {
                            return Err(e);
                        }
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                for run in runs.into_iter().take(run_take) {
                    if out.len() >= max_jobs {
                        break;
                    }
                    match collect_jobs_for_run(cli, name, run.id, api, pacer) {
                        Ok(mut jobs) => out.append(&mut jobs),
                        Err(e) if is_soft_api_err(&e) => {
                            if e.contains("budget exhausted") {
                                eprintln!(
                                    "listen: list_demand_jobs: budget exhausted on jobs ({} kept)",
                                    out.len()
                                );
                                break 'budget_hit;
                            }
                            if e.contains("rate limited") {
                                return Err(e);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            if idx + 1 >= prio_len {
                // Whole priority set polled this tick: nothing starved, so keep
                // strict head-first order next tick rather than churning it.
                prio_complete = true;
            }
        }
    }

    if prio_complete {
        let _ = fs::write(&prio_cursor_path, "0");
    } else {
        advance_cursor(&prio_cursor_path, prio_offset, prio_consumed, prio_len);
        eprintln!(
            "listen: priority scan truncated after {prio_consumed}/{prio_len}; next tick resumes there"
        );
    }

    // Dedupe by repo+job_name
    let mut seen = std::collections::HashSet::new();
    out.retain(|j| seen.insert(format!("{}::{}", j.repo, j.job_name)));
    Ok(out)
}

/// True if active registration still has incomplete matching jobs (sticky; do not recycle).
fn active_repo_still_busy(
    cli: &Cli,
    repo: &str,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<bool, String> {
    repo_needs_runner(cli, repo, api, pacer)
}

fn pool_mode_on(cli: &Cli) -> bool {
    matches!(
        cli.pool_mode.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "dynamic"
    )
}

fn ensure_worker_volume(cli: &Cli, worker_volume: &str) -> Result<(), String> {
    let already_present = volume_exists(worker_volume);
    if already_present && volume_is_seeded(worker_volume) {
        return Ok(());
    }
    let base_volume = cli.volume.as_str();
    if !volume_exists(base_volume) {
        return Err(format!(
            "base volume {base_volume} missing — run prepare first"
        ));
    }
    let chown = chown_spec(cli);
    // Use seed helper for external images so rootfs without bash still works.
    let copy_image = if needs_host_entrypoint(cli) {
        cli.seed_helper_image.as_str()
    } else {
        cli.image.as_str()
    };
    let script = format!(
        r#"set -euo pipefail; cp -a /from/. /to/; chown -R {chown} /to 2>/dev/null || true; rm -f /to/.runner /to/.credentials /to/.credentials_rsaparams 2>/dev/null; true"#
    );
    eprintln!("pool: seeding worker volume {worker_volume} from {base_volume} via {copy_image}");
    if !already_present {
        podman(&["volume", "create", worker_volume])?;
    }
    podman(&[
        "run",
        "--rm",
        "--security-opt",
        "no-new-privileges",
        "--entrypoint",
        "/bin/bash",
        "-v",
        &format!("{base_volume}:/from:ro,Z"),
        "-v",
        &format!("{worker_volume}:/to:Z"),
        copy_image,
        "-c",
        &script,
    ])?;
    Ok(())
}

/// Returns number of claims released.
fn reap_pool_workers(cli: &Cli, pool: &ResourcePool) -> u32 {
    let Ok(claims) = pool.claims() else {
        return 0;
    };
    let now = now_unix();
    let mut n = 0u32;
    for c in claims {
        // Only reap workers owned by this listen base name prefix
        if !c.container.starts_with(&cli.container) {
            continue;
        }
        let running = container_running(&c.container);
        // Stale claim: container exited/missing, or claim older than 2h (orphan).
        let stale_age = now.saturating_sub(c.claimed_at_unix) > 7200;
        if !running || stale_age {
            eprintln!(
                "pool: reap {} running={running} stale={stale_age} tier={} repo={:?}",
                c.container, c.tier, c.repo
            );
            let mut dead = cli.clone_for_listen();
            dead.container = c.container.clone();
            dead.volume = format!("{}-data", c.container);
            dead.runner_name = c.worker_id.clone();
            // Always release claim even if down fails — otherwise memory budget leaks to 0 MiB.
            let _ = down(&dead, true);
            if let Err(e) = pool.release(&c.worker_id) {
                eprintln!("pool: release {} failed: {}", c.worker_id, redact(&e));
                let _ = pool.release_container(&c.container);
            }
            n += 1;
        }
    }
    n
}

/// Prune exited fleet worker containers that are not in active claims.
/// Does **not** touch running workers or cancel any GitHub Actions runs.
fn prune_exited_fleet_workers(cli: &Cli, pool: &ResourcePool) -> u32 {
    let claimed: std::collections::HashSet<String> = pool
        .claims()
        .map(|c| c.into_iter().map(|x| x.container).collect())
        .unwrap_or_default();
    let out = match std::process::Command::new("podman")
        .args(["ps", "-a", "--format", "{{.Names}}\t{{.Status}}"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return 0,
    };
    let prefix = cli.container.as_str();
    let mut n = 0u32;
    for line in out.lines() {
        let Some((name, status)) = line.split_once('\t') else {
            continue;
        };
        let name = name.trim();
        let status = status.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let ours = name.starts_with(prefix)
            || name.starts_with("gha-runner-cpu")
            || name.starts_with("gha-runner-ctl");
        if !ours || claimed.contains(name) {
            continue;
        }
        // Only remove clearly stopped/exited/created leftovers — never "up".
        let removable = status.starts_with("exited")
            || status.starts_with("created")
            || status.starts_with("dead")
            || status.contains("exited");
        if !removable {
            continue;
        }
        eprintln!("recover: prune exited leftover {name} ({status})");
        let _ = podman(&["rm", "-f", name]);
        n += 1;
    }
    n
}

/// Safe recovery: free local capacity so listen can pick up **queued** GitHub jobs.
/// Explicitly does **not** cancel or delete workflow runs (queue is preserved on GitHub).
fn recover(cli: &Cli, prune_exited: bool, json: bool) -> Result<(), String> {
    std::env::set_var("GHA_POOL_CPUS", &cli.pool_cpus);
    std::env::set_var("GHA_POOL_MEMORY", &cli.pool_memory);
    std::env::set_var("GHA_POOL_MAX_WORKERS", cli.pool_max_workers.to_string());
    std::env::set_var("GHA_POOL_MODE", &cli.pool_mode);
    let pool = ResourcePool::from_env();

    let (c0, m0, n0) = pool.usage().unwrap_or((0.0, 0, 0));
    eprintln!(
        "recover: start usage={c0:.2}/{:.0}c {m0}/{}MiB claims={n0} (will NOT cancel GitHub runs)",
        pool.max_cpus, pool.max_memory_mib
    );

    let reaped = reap_pool_workers(cli, &pool);
    let mut pruned = 0u32;
    if prune_exited {
        pruned = prune_exited_fleet_workers(cli, &pool);
    }
    // Second pass: claims may point at containers we just pruned.
    let reaped2 = reap_pool_workers(cli, &pool);

    if matches!(cli.mode, Mode::Ephemeral) {
        reap_stale_containers(cli, &pool);
    }

    let (c1, m1, n1) = pool.usage().unwrap_or((0.0, 0, 0));
    let free_c = (pool.max_cpus - c1).max(0.0);
    let free_m = pool.max_memory_mib.saturating_sub(m1);
    eprintln!(
        "recover: done reaped_claims={} pruned_containers={pruned} usage={c1:.2}/{:.0}c {m1}/{}MiB claims={n1} free≈{free_c:.2}c/{free_m}MiB",
        reaped + reaped2,
        pool.max_cpus,
        pool.max_memory_mib
    );
    eprintln!(
        "recover: next — leave listen running (or restart gha-runner-ctl@cpu); queued Actions jobs stay on GitHub and will be claimed when demand poll runs"
    );

    if json {
        println!(
            "{}",
            serde_json::json!({
                "reaped_claims": reaped + reaped2,
                "pruned_containers": pruned,
                "pool_cpus_used": c1,
                "pool_mem_mib_used": m1,
                "pool_claims": n1,
                "pool_cpus_free": free_c,
                "pool_mem_mib_free": free_m,
                "cancels_github_runs": false,
            })
        );
    }
    Ok(())
}

/// Merge fleet base labels with job-requested size/capability/image/arch labels + tier tag.
/// GitHub requires the runner to advertise every `runs-on` label.
pub fn runner_labels_for_job(base_labels: &str, job_labels: &[String], tier: SizeTier) -> String {
    runner_labels_for_job_with_map(base_labels, job_labels, tier, None)
}

/// Like [`runner_labels_for_job`], plus image/arch labels resolved from the map (#28).
pub fn runner_labels_for_job_with_map(
    base_labels: &str,
    job_labels: &[String],
    tier: SizeTier,
    resolved: Option<&JobImageArch>,
) -> String {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<String> = base_labels
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    // Always advertise the resolved tier so size-* jobs can match.
    set.insert(tier.as_str().to_string());
    for l in job_labels {
        let l = l.trim().to_ascii_lowercase();
        if l.is_empty() || !is_safe_ident(&l) {
            continue;
        }
        // Size / GPU capability extras.
        if matches!(
            l.as_str(),
            "micro"
                | "small"
                | "medium"
                | "large"
                | "xlarge"
                | "x-large"
                | "huge"
                | "gpu"
                | "cuda"
                | "nvidia"
                | "size-micro"
                | "size-small"
                | "size-medium"
                | "size-large"
                | "size-xlarge"
        ) || l.starts_with("gpu-slice")
            || l.starts_with("size-")
        {
            set.insert(l);
            continue;
        }
        // Arch tokens (arm64, riscv64, …) — advertised so matrix cells match.
        if TargetArch::from_label(&l).is_some() {
            set.insert(l);
            continue;
        }
        // Distro / image-map keys present on the job (e.g. ubuntu-24.04).
        // We re-check against resolved image_label when available; otherwise accept
        // safe idents that look like distro tags (contain a digit or known prefix).
        if let Some(r) = resolved {
            if r.image_label.as_deref() == Some(l.as_str()) {
                set.insert(l);
            }
        }
    }
    if let Some(r) = resolved {
        for extra in extra_image_arch_labels(r) {
            if is_safe_ident(&extra) {
                set.insert(extra);
            }
        }
        // When advertising a non-host arch, drop conflicting host arch from base
        // so the runner is not dual-labelled x64+arm64 unless the job asked for both.
        if let Some(arch) = r.arch {
            if arch != TargetArch::Amd64 {
                set.remove("x64");
                set.remove("amd64");
                set.remove("x86_64");
            }
            if arch != TargetArch::Arm64 {
                set.remove("arm64");
                set.remove("aarch64");
            }
            set.insert(arch.label().to_string());
        }
    }
    set.into_iter().collect::<Vec<_>>().join(",")
}

/// Spawn with live `fit_to_budget`. Returns `Ok(true)` if the worker was brought up,
/// `Ok(false)` if capacity/claim refused (not an error).
fn spawn_sized_worker(
    base: &Cli,
    pool: &ResourcePool,
    slot: u32,
    job: &DemandJob,
) -> Result<bool, String> {
    let tier = size_for_job(&job.job_name, &job.labels, base.gpu);
    let (want_c_s, want_m_s) = resources_for_tier(tier);
    let want_c = parse_cpus_f64(&want_c_s).unwrap_or(1.0);
    let want_m = parse_memory_mib(&want_m_s).unwrap_or(2048);
    let (used_c, used_m, _) = pool.usage()?;
    let free_c = (pool.max_cpus - used_c).max(0.0);
    let free_m = pool.max_memory_mib.saturating_sub(used_m);
    let Some((c, m)) = fit_to_budget(want_c, want_m, free_c, free_m, 0.25, 256) else {
        eprintln!(
            "pool: no budget for {} tier={} (free={free_c:.2}c/{free_m}MiB)",
            job.job_name,
            tier.as_str()
        );
        return Ok(false);
    };
    spawn_worker_with_resources(base, pool, slot, job, tier, c, m)
}

/// Spawn a pool worker using resources already decided by [`plan_scale`] (or
/// re-checked via [`spawn_sized_worker`]). Claim is the hard capacity gate.
/// `Ok(true)` = container up; `Ok(false)` = claim refused / no-op.
fn spawn_worker_with_resources(
    base: &Cli,
    pool: &ResourcePool,
    slot: u32,
    job: &DemandJob,
    tier: SizeTier,
    cpus: f64,
    memory_mib: u64,
) -> Result<bool, String> {
    // Per-job image + arch from runs-on labels (issue #28). No change when absent.
    // Lives in the shared spawn path so both plan_scale spawns and live-fit spawns
    // resolve image/arch identically.
    let map = load_image_map(base.image_map.as_deref())?;
    let resolved = resolve_job_image_arch(&job.labels, &map, &base.image);
    if resolved.needs_emulation {
        if let Some(arch) = resolved.arch {
            ensure_binfmt_for_arch(arch, true, None)?;
        }
    }
    let worker_id = format!("{}-w{slot}", base.runner_name);
    let container = format!("{}-w{slot}", base.container);
    let volume = format!("{container}-data");
    if !pool.try_claim(
        &worker_id,
        &container,
        cpus,
        memory_mib,
        tier,
        Some(job.repo.as_str()),
    )? {
        eprintln!("pool: claim failed for {container}");
        return Ok(false);
    }
    if let Err(e) = ensure_worker_volume(base, &volume) {
        let _ = pool.release(&worker_id);
        return Err(e);
    }
    let mut unit = base.clone_for_listen();
    unit.repo = Some(job.repo.clone());
    unit.container = container.clone();
    unit.volume = volume;
    unit.runner_name = worker_id.clone();
    unit.cpus = format_cpus(cpus);
    unit.memory = format_memory_mib(memory_mib);

    // Apply workflow-selected image (forces external mode for non-stock OCI refs).
    if resolved.image != base.image || resolved.image_label.is_some() {
        unit.image = resolved.image.clone();
        if !is_default_stock_image(&unit.image) {
            unit.image_mode = ImageMode::External;
        }
        let mode = effective_image_mode(&unit.image_mode, &unit.image);
        let pull = effective_pull_policy(unit.pull_policy.as_ref(), &mode);
        if let Err(e) =
            ensure_image_present_platform(&unit.image, &pull, resolved.platform.as_deref())
        {
            let _ = pool.release(&worker_id);
            return Err(e);
        }
    }
    // CLI --platform wins; else job-resolved platform for cross-arch.
    if unit.platform.is_none() {
        unit.platform = resolved.platform.clone();
    }
    // Align actions/runner asset arch hint when we know the mapping (SHA still operator pin).
    if let Some(arch) = resolved.arch {
        if let Some(ra) = arch.runner_arch() {
            unit.runner_arch = ra.to_string();
        }
    }

    // Register base fleet labels + job size/image/arch labels so GitHub routes the cell.
    unit.labels = runner_labels_for_job_with_map(&base.labels, &job.labels, tier, Some(&resolved));
    eprintln!(
        "pool: up {container} tier={} cpus={} mem={} image={} platform={:?} labels={} repo={} job={}",
        tier.as_str(),
        unit.cpus,
        unit.memory,
        unit.image,
        unit.platform,
        unit.labels,
        job.repo,
        job.job_name
    );
    if let Err(e) = up(&unit) {
        let _ = pool.release(&worker_id);
        return Err(e);
    }
    Ok(true)
}

/// Tear down a local pool worker by id and release its pool claim.
fn scale_in_worker(base: &Cli, pool: &ResourcePool, worker_id: &str) -> Result<(), String> {
    let claims = pool.claims()?;
    let Some(c) = claims.iter().find(|x| x.worker_id == worker_id) else {
        eprintln!("pool: scale-in skip unknown worker {worker_id}");
        return Ok(());
    };
    if !c.container.starts_with(&base.container) {
        // Never touch another manager's workers or warm/base retain runners.
        eprintln!(
            "pool: scale-in refuse non-local container {} (base={})",
            c.container, base.container
        );
        return Ok(());
    }
    eprintln!(
        "pool: scale-in {} tier={} repo={:?}",
        c.container, c.tier, c.repo
    );
    let mut unit = base.clone_for_listen();
    unit.container = c.container.clone();
    unit.volume = format!("{}-data", c.container);
    unit.runner_name = c.worker_id.clone();
    let _ = down(&unit, true);
    if let Err(e) = pool.release(worker_id) {
        eprintln!("pool: release {worker_id} failed: {}", redact(&e));
        let _ = pool.release_container(&c.container);
    }
    Ok(())
}

/// Build planner worker snapshots for containers owned by this listen base name.
///
/// `busy` is filled from the local container process tree ([`container_worker_busy`]),
/// never from the demand scan — so a mid-job worker on an un-scanned prefer-repo
/// is still marked busy and protected from idle scale-in.
fn local_worker_snapshots(cli: &Cli, pool: &ResourcePool, max_local: u32) -> Vec<WorkerSnapshot> {
    let mut out = Vec::new();
    let claims = pool.claims().unwrap_or_default();
    for slot in 0..max_local {
        let container = format!("{}-w{slot}", cli.container);
        let worker_id = format!("{}-w{slot}", cli.runner_name);
        let running = container_running(&container);
        let claimed = claims
            .iter()
            .any(|c| c.container == container || c.worker_id == worker_id);
        if running || claimed {
            // Local job signal only; independent of list_demand_jobs RR sample.
            let busy = running && container_worker_busy(&container);
            let repo = claims
                .iter()
                .find(|c| c.container == container || c.worker_id == worker_id)
                .and_then(|c| c.repo.clone());
            // Age since container start — protects freshly-spawned workers from
            // being misread as "post job exit" (issue #127). `None` when the
            // container isn't running (age is moot; `running` alone already
            // excludes it from reclaim) or inspect failed, which fails closed
            // in `post_job_exit_eligible`.
            let age_secs = if running {
                container_started_age_secs(&container)
            } else {
                None
            };
            // No per-tick log-scraping for "listener exited" exists (and could
            // not fire here anyway: the entrypoint execs run.sh as PID 1, so a
            // finished listener process means the container has already exited
            // -> `running == false` -> reaped by `reap_pool_workers` before this
            // snapshot is ever built, never reaching plan_scale as `running &&
            // !busy`). So there is no positive signal to observe for a worker
            // that is still `running`; leave false and rely on `age_secs`.
            let job_completed = false;
            out.push(WorkerSnapshot {
                slot,
                worker_id,
                container,
                running,
                busy,
                repo,
                age_secs,
                job_completed,
            });
        }
    }
    out
}

fn listen(cli: &Cli, interval: u64, idle_secs: u64, wake_port: Option<u16>) -> Result<(), String> {
    let floor = cli.listen_min_interval.max(15);
    let interval = if matches!(cli.scope, Scope::User) {
        interval.max(floor)
    } else {
        interval
    };
    // Apply pool env from CLI for ResourcePool::from_env
    std::env::set_var("GHA_POOL_CPUS", &cli.pool_cpus);
    std::env::set_var("GHA_POOL_MEMORY", &cli.pool_memory);
    std::env::set_var("GHA_POOL_MAX_WORKERS", cli.pool_max_workers.to_string());
    std::env::set_var("GHA_POOL_MODE", &cli.pool_mode);

    let pool = ResourcePool::from_env();
    let dynamic = pool_mode_on(cli);
    let prefer_n = allowlist_repos_list(cli).len();
    let prio_n = priority_repos_list(cli).len();
    let tick_path = tick_log_path(cli);
    eprintln!(
        "listen: scope={:?} poll={interval}s (floor={floor}) idle={idle_secs}s mode={:?} api_gap={}ms max_per_poll={} pool={} ({:.0}c/{}MiB max_workers={}) prefer={prefer_n} priority={prio_n} scan/tick={} reap_stale={}s",
        cli.scope,
        cli.mode,
        cli.api_min_gap_ms,
        cli.api_max_per_poll,
        if dynamic { "dynamic" } else { "single" },
        pool.max_cpus,
        pool.max_memory_mib,
        pool.max_workers.min(cli.pool_max_workers),
        cli.pool_scan_per_tick,
        cli.reap_stale_secs,
    );
    if matches!(cli.scope, Scope::User) && prefer_n == 0 {
        eprintln!(
            "listen: warning: set GHA_PREFER_REPOS or GHA_PREFER_REPOS_FILE (allowlist) to stay within API budgets"
        );
    }
    if let Some(ref path) = tick_path {
        eprintln!("listen: tick log → {}", path.display());
    }
    // Drop stale retain/warm leftovers so they cannot steal confusion or budget.
    if matches!(cli.mode, Mode::Ephemeral) {
        reap_stale_containers(cli, &pool);
    }
    if !volume_exists(&cli.volume) {
        eprintln!("listen: snapshot missing — prepare…");
        prepare(cli, true, false)?;
    }

    if let Some(port) = wake_port {
        if port == 0 {
            return Err("wake-port must be non-zero".into());
        }
        let Some(token) = cli.wake_token.clone() else {
            return Err("wake-port requires GHA_WAKE_TOKEN (≥16 chars)".into());
        };
        let snap = cli_snapshot(cli);
        thread::spawn(move || wake_server(port, snap, token, interval));
        eprintln!("listen: authenticated wake on 127.0.0.1:{port}");
    }

    let mut idle_since: Option<Instant> = None;
    // Consecutive ticks where the (partial) demand sample was empty.
    // Idle timer starts only after a full prefer-list sweep of empty.
    let mut empty_streak: u32 = 0;
    // Tracks quiesce state so ENTER/EXIT are logged once on transition rather
    // than every tick; a deploy script watches for these lines.
    let mut quiesced = false;
    let mut cli = cli.clone_for_listen();
    let mut pacer = ApiPacer::from_cli(&cli, cli.http());
    let max_local = cli.pool_max_workers.min(pool.max_workers).max(1);
    // Effective partial-scan width for the empty-sweep gate (mirrors list_demand_jobs).
    let scan_width: usize = if dynamic {
        let cap = cli.pool_scan_per_tick.max(1);
        if cli.repos_per_tick == 0 {
            cap as usize
        } else {
            cli.repos_per_tick.min(cap).max(1) as usize
        }
    } else if cli.repos_per_tick == 0 {
        prefer_n.max(1)
    } else {
        cli.repos_per_tick as usize
    };
    let sweep_ticks = empty_sweep_ticks(prefer_n, scan_width);
    if dynamic {
        eprintln!(
            "listen: capacity-safe scale-in: empty_sweep_ticks={sweep_ticks} (prefer={prefer_n}/scan={scan_width}); busy workers never scaled in"
        );
    }

    loop {
        if let Some(wait) = pacer.cooling() {
            let secs = wait.as_secs().max(1);
            eprintln!("listen: API cool-down {secs}s before next poll");
            thread::sleep(wait);
            continue;
        }

        // Always reap finished pool workers first (frees budget).
        if dynamic {
            let n = reap_pool_workers(&cli, &pool);
            if n > 0 {
                eprintln!("listen: reaped {n} finished/orphan claim(s) before poll");
            }
        }

        // Quiesce gate: pause ADMISSION only, deliberately after the reap above.
        //
        // In-flight work runs to completion and finished workers are still
        // reaped, so `running=` converges to zero and a deploy can wait on it.
        // Placed before github_token() so a quiesced manager makes no API calls
        // at all — which also means an idle, quiesced host stops waking up to
        // talk to GitHub.
        //
        // Restarting the unit instead would orphan in-flight containers and
        // cancel their jobs; that is the whole reason this exists.
        if crate::pool::quiesce_active() {
            if !quiesced {
                quiesced = true;
                eprintln!(
                    "listen: quiesce ENTER ({}) — admitting no new work; in-flight jobs continue",
                    crate::pool::quiesce_path().display()
                );
            }
            if dynamic {
                let w = local_worker_snapshots(&cli, &pool, max_local);
                let running_n = w.iter().filter(|x| x.running).count();
                let busy_n = w.iter().filter(|x| x.running && is_busy(x)).count();
                eprintln!("listen: quiesced running={running_n} busy={busy_n} — waiting for drain");
            } else {
                eprintln!("listen: quiesced — admission paused");
            }
            thread::sleep(Duration::from_secs(interval));
            continue;
        }
        if quiesced {
            quiesced = false;
            eprintln!("listen: quiesce EXIT — resuming admission");
        }

        let api = match github_token(&cli) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("listen: auth: {}", redact(&e));
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };

        if dynamic {
            // Reset per-poll GET budget every tick (demand() does this; dynamic path must too).
            pacer.begin_poll();
            // Free capacity again if workers finished during API cool-down.
            let _ = reap_pool_workers(&cli, &pool);
            let jobs = match list_demand_jobs(&cli, &api, &mut pacer, max_local as usize * 2) {
                Ok(j) => j,
                Err(e) => {
                    eprintln!("listen: poll: {}", redact(&e));
                    let wait = pacer
                        .cooling()
                        .map(|d| d.max(Duration::from_secs(interval)))
                        .unwrap_or(Duration::from_secs(interval));
                    drop(api);
                    thread::sleep(wait);
                    // Do not scale-in on API failure (avoid flap when GitHub is dark).
                    continue;
                }
            };
            drop(api);

            // Keep raw demand count for tick log (filter may drop GPU/CPU affinity mismatches).
            let jobs_n = jobs.len();
            // Filter by GPU affinity before planning (CPU listener skips gpu tiers).
            let filtered: Vec<DemandJob> = jobs
                .into_iter()
                .filter(|job| {
                    let tier = size_for_job(&job.job_name, &job.labels, cli.gpu);
                    if cli.gpu {
                        tier == SizeTier::Gpu
                    } else {
                        tier != SizeTier::Gpu
                    }
                })
                .collect();

            let workers = local_worker_snapshots(&cli, &pool, max_local);
            let running_n = workers.iter().filter(|w| w.running).count() as u32;
            let busy_n = workers.iter().filter(|w| w.running && is_busy(w)).count() as u32;
            let (used_c, used_m, host_claims) = pool.usage().unwrap_or((0.0, 0, 0));
            let free_c = (pool.max_cpus - used_c).max(0.0);
            let free_m = pool.max_memory_mib.saturating_sub(used_m);

            // Demand-empty gate + idle timer:
            // 1. A single empty *partial* RR sample must NOT start idle_secs
            //    (busy job may live on an un-scanned prefer-repo).
            // 2. Require consecutive empty ticks covering a full prefer-list
            //    sweep ([`demand_empty_confirmed`] / [`empty_sweep_ticks`]).
            // 3. Only then count idle_secs toward scale-in.
            // Per-worker busy is a second, independent layer in plan_scale.
            let idle_expired = if filtered.is_empty() {
                empty_streak = empty_streak.saturating_add(1);
                let confirmed = demand_empty_confirmed(empty_streak, prefer_n, scan_width);
                if confirmed {
                    let since = idle_since.get_or_insert_with(Instant::now);
                    since.elapsed() >= Duration::from_secs(idle_secs)
                } else {
                    // Still covering the prefer-list — do not start idle clock.
                    idle_since = None;
                    false
                }
            } else {
                empty_streak = 0;
                idle_since = None;
                false
            };

            let max_spawn = std::env::var("GHA_POOL_MAX_SPAWN_PER_TICK")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_MAX_SPAWN_PER_TICK);
            // Grace window protecting a freshly-spawned, never-assigned worker
            // from being reclaimed as "post job exit" (issue #127: `busy=0`
            // right after spawn means "never dispatched", not "already
            // finished"). Operator-tunable for unusually slow GitHub dispatch.
            let spawn_grace_secs = std::env::var("GHA_POOL_SPAWN_GRACE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SPAWN_GRACE_SECS);

            let signals: Vec<DemandSignal> = filtered
                .iter()
                .map(|j| DemandSignal {
                    job_name: j.job_name.clone(),
                    labels: j.labels.clone(),
                    repo: j.repo.clone(),
                })
                .collect();

            // CTL-1 primary (default GHA_MODE=ephemeral): dynamic pool re-registers
            // per job → idle workers must exit (not warm-pin a repo).
            //
            // GHA_MODE=retain (opt-in): idle workers hold a durable registration
            // and should NOT be torn down after one job — that was the bug fixed
            // here (this flag used to be hardcoded `true` regardless of mode,
            // which silently defeated retain: idle retained workers never got a
            // chance to take a second job because plan_scale reclaimed them every
            // tick, see [`pool::plan_scale`] "Post-job exit (CTL-1 primary)").
            // Deriving from `effective_ephemeral` makes retain's own idle-preempt
            // fallback (wrong-repo reclaim, still active) the only reclaim path.
            let ephemeral_post_job_exit = effective_ephemeral(&cli);
            let plan = plan_scale(&ScaleInput {
                jobs: signals,
                workers: workers.clone(),
                free_cpus: free_c,
                free_memory_mib: free_m,
                max_cpus: pool.max_cpus,
                max_memory_mib: pool.max_memory_mib,
                max_local_workers: max_local,
                host_claim_count: host_claims as u32,
                max_host_workers: pool.max_workers,
                force_gpu: cli.gpu,
                idle_expired,
                max_spawn_per_tick: max_spawn,
                ephemeral_post_job_exit,
                spawn_grace_secs,
            });

            eprintln!(
                "pool: plan {} (running={running_n} busy={busy_n} empty_streak={empty_streak}/{sweep_ticks} free={free_c:.2}c/{free_m}MiB)",
                plan.notes
            );

            // Scale-IN first (return capacity before scale-out packing).
            // plan_scale only lists provably-idle workers; re-check the local
            // busy signal at execution time in case a job started mid-tick.
            for wid in &plan.scale_in {
                let container = workers
                    .iter()
                    .find(|w| w.worker_id == *wid)
                    .map(|w| w.container.as_str())
                    .unwrap_or("");
                if !container.is_empty() && container_worker_busy(container) {
                    eprintln!("pool: scale-in skip busy worker {wid} (local job signal)");
                    continue;
                }
                if let Err(e) = scale_in_worker(&cli, &pool, wid) {
                    eprintln!("pool: scale-in failed: {}", redact(&e));
                }
            }
            if !plan.scale_in.is_empty() {
                idle_since = None;
            }

            // Scale-OUT: execute planned spawns; try_claim re-checks capacity under lock.
            let mut spawned = 0u32;
            for req in &plan.spawns {
                let job = DemandJob {
                    repo: req.repo.clone(),
                    job_name: req.job_name.clone(),
                    labels: req.labels.clone(),
                };
                // Prefer planner resources; claim is still the hard gate. If the
                // free pool moved, fall back to fit_to_budget inside spawn_sized_worker.
                let result = spawn_worker_with_resources(
                    &cli,
                    &pool,
                    req.slot,
                    &job,
                    req.tier,
                    req.cpus,
                    req.memory_mib,
                );
                match result {
                    Ok(true) => spawned += 1,
                    Ok(false) => {
                        // Claim refused (slot raced); try live fit once.
                        match spawn_sized_worker(&cli, &pool, req.slot, &job) {
                            Ok(true) => spawned += 1,
                            Ok(false) => {}
                            Err(e) => eprintln!("pool: spawn retry failed: {}", redact(&e)),
                        }
                    }
                    Err(e) => {
                        eprintln!("pool: spawn failed: {}", redact(&e));
                        // Retry once via live fit path (budget may have changed).
                        match spawn_sized_worker(&cli, &pool, req.slot, &job) {
                            Ok(true) => spawned += 1,
                            Ok(false) => {}
                            Err(e2) => eprintln!("pool: spawn retry failed: {}", redact(&e2)),
                        }
                    }
                }
                if let Some(ref path) = tick_path {
                    let (uc, um, n) = pool.usage().unwrap_or((0.0, 0, 0));
                    append_tick_log(
                        path,
                        &serde_json::json!({
                            "ts_unix": now_unix(),
                            "jobs": jobs_n,
                            "spawned": spawned,
                            "running": running_n,
                            "pool_cpus_used": uc,
                            "pool_mem_mib_used": um,
                            "pool_claims": n,
                            "prefer": prefer_n,
                            "priority": prio_n,
                            "mode": "dynamic",
                        }),
                    );
                }
            }
            if spawned > 0 || !plan.scale_in.is_empty() {
                let (uc, um, n) = pool.usage().unwrap_or((0.0, 0, 0));
                eprintln!(
                    "pool: spawned={spawned} scale_in={} usage={uc:.2}/{:.0}c {um}/{}MiB claims={n}",
                    plan.scale_in.len(),
                    pool.max_cpus,
                    pool.max_memory_mib
                );
            }
        } else {
            // Legacy single-container listen path
            let (need, target_repo) = match demand(&cli, &api, &mut pacer) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("listen: poll: {}", redact(&e));
                    let wait = pacer
                        .cooling()
                        .map(|d| d.max(Duration::from_secs(interval)))
                        .unwrap_or(Duration::from_secs(interval));
                    drop(api);
                    thread::sleep(wait);
                    continue;
                }
            };

            if matches!(cli.scope, Scope::User) {
                if let Some(ref r) = target_repo {
                    let active = get_active_target(&cli);
                    if active.as_deref() != Some(r.as_str()) {
                        let busy = active
                            .as_ref()
                            .map(|a| {
                                active_repo_still_busy(&cli, a, &api, &mut pacer).unwrap_or(false)
                            })
                            .unwrap_or(false);
                        if container_running(&cli.container) && busy {
                            eprintln!(
                                "listen: sticky on {active:?} (still busy); defer move to {r}"
                            );
                            if let Some(a) = active {
                                cli.repo = Some(a);
                            }
                        } else if container_running(&cli.container) {
                            eprintln!("listen: demand moved {active:?} → {r}; recycling runner");
                            let _ = down(&cli, true);
                            cli.repo = Some(r.clone());
                        } else {
                            cli.repo = Some(r.clone());
                        }
                    } else {
                        cli.repo = Some(r.clone());
                    }
                }
            }
            drop(api);

            // Vertical size for single worker from first matching job name if any
            if need {
                if let Ok(api2) = github_token(&cli) {
                    if let Ok(jobs) = list_demand_jobs(&cli, &api2, &mut pacer, 1) {
                        if let Some(j) = jobs.first() {
                            let tier = size_for_job(&j.job_name, &j.labels, cli.gpu);
                            let (c, m) = resources_for_tier(tier);
                            cli.cpus = c;
                            cli.memory = m;
                            eprintln!(
                                "listen: size tier={} cpus={} mem={} job={}",
                                tier.as_str(),
                                cli.cpus,
                                cli.memory,
                                j.job_name
                            );
                        }
                    }
                }
            }

            let running = container_running(&cli.container);
            if need && !running {
                eprintln!(
                    "listen: demand — up ({})",
                    cli.repo.as_deref().unwrap_or("org")
                );
                if let Err(e) = up(&cli) {
                    eprintln!("listen: up failed: {}", redact(&e));
                }
                idle_since = None;
            } else if !need && running {
                let since = idle_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_secs(idle_secs) {
                    eprintln!("listen: idle {idle_secs}s — down");
                    if let Err(e) = down(&cli, true) {
                        eprintln!("listen: down failed: {}", redact(&e));
                    }
                    idle_since = None;
                }
            } else {
                idle_since = None;
            }
            if let Some(ref path) = tick_path {
                append_tick_log(
                    path,
                    &serde_json::json!({
                        "ts_unix": now_unix(),
                        "need": need,
                        "target": target_repo,
                        "running": container_running(&cli.container),
                        "prefer": prefer_n,
                        "priority": prio_n,
                        "mode": "legacy",
                    }),
                );
            }
        }

        thread::sleep(Duration::from_secs(interval));
    }
}

/// Clone settings for listen mutability of active repo.
impl Cli {
    fn clone_for_listen(&self) -> Self {
        Self {
            cmd: Some(Cmd::Status),
            scope: self.scope,
            repo: self.repo.clone(),
            owner: self.owner.clone(),
            user: self.user.clone(),
            auto: self.auto,
            image: self.image.clone(),
            image_mode: self.image_mode.clone(),
            pull_policy: self.pull_policy.clone(),
            runner_user: self.runner_user.clone(),
            seed_helper_image: self.seed_helper_image.clone(),
            runner_version: self.runner_version.clone(),
            runner_sha256: self.runner_sha256.clone(),
            runner_arch: self.runner_arch.clone(),
            runner_seed_url: self.runner_seed_url.clone(),
            entrypoint: self.entrypoint.clone(),
            container: self.container.clone(),
            volume: self.volume.clone(),
            runner_name: self.runner_name.clone(),
            labels: self.labels.clone(),
            cpus: self.cpus.clone(),
            memory: self.memory.clone(),
            gpu: self.gpu,
            gpu_slice: self.gpu_slice.clone(),
            demand_require_labels: self.demand_require_labels.clone(),
            demand_exclude_labels: self.demand_exclude_labels.clone(),
            prefer_repos: self.prefer_repos.clone(),
            allowlist_repos_file: self.allowlist_repos_file.clone(),
            allowlist_repos: self.allowlist_repos.clone(),
            prefer_repos_file: self.prefer_repos_file.clone(),
            priority_repos: self.priority_repos.clone(),
            listen_min_interval: self.listen_min_interval,
            pool_scan_per_tick: self.pool_scan_per_tick,
            reap_stale_secs: self.reap_stale_secs,
            tick_log: self.tick_log.clone(),
            api_min_gap_ms: self.api_min_gap_ms,
            api_max_per_poll: self.api_max_per_poll,
            api_backoff_secs: self.api_backoff_secs,
            repos_per_tick: self.repos_per_tick,
            reg_min_gap_secs: self.reg_min_gap_secs,
            reg_max_per_hour: self.reg_max_per_hour,
            pool_cpus: self.pool_cpus.clone(),
            pool_memory: self.pool_memory.clone(),
            pool_max_workers: self.pool_max_workers,
            pool_mode: self.pool_mode.clone(),
            image_map: self.image_map.clone(),
            platform: self.platform.clone(),
            build_dir: self.build_dir.clone(),
            mode: self.mode.clone(),
            wake_token: self.wake_token.clone(),
            full_auto: self.full_auto,
            this_repo_only: self.this_repo_only.clone(),
            public_only: self.public_only,
            private_only: self.private_only,
            all_repos: self.all_repos,
            app_id: self.app_id.clone(),
            app_installation_id: self.app_installation_id.clone(),
            app_private_key: self.app_private_key.clone(),
        }
    }
}

struct CliSnap {
    scope: Scope,
    repo: Option<String>,
    owner: Option<String>,
    user: Option<String>,
    auto: bool,
    image: String,
    image_mode: ImageMode,
    pull_policy: Option<PullPolicy>,
    runner_user: String,
    seed_helper_image: String,
    runner_version: String,
    runner_sha256: String,
    runner_arch: String,
    runner_seed_url: Option<String>,
    entrypoint: Option<PathBuf>,
    container: String,
    volume: String,
    runner_name: String,
    labels: String,
    cpus: String,
    memory: String,
    gpu: bool,
    gpu_slice: Option<String>,
    demand_require_labels: Option<String>,
    demand_exclude_labels: Option<String>,
    prefer_repos: Option<String>,
    allowlist_repos: Option<String>,
    prefer_repos_file: Option<String>,
    allowlist_repos_file: Option<String>,
    priority_repos: Option<String>,
    listen_min_interval: u64,
    pool_scan_per_tick: u32,
    reap_stale_secs: u64,
    tick_log: String,
    api_min_gap_ms: u64,
    api_max_per_poll: u32,
    api_backoff_secs: u64,
    repos_per_tick: u32,
    reg_min_gap_secs: u64,
    reg_max_per_hour: u32,
    pool_cpus: String,
    pool_memory: String,
    pool_max_workers: u32,
    pool_mode: String,
    image_map: Option<PathBuf>,
    platform: Option<String>,
    mode: Mode,
    wake_token: Option<String>,
    full_auto: bool,
    this_repo_only: Option<String>,
    public_only: bool,
    private_only: bool,
    all_repos: bool,
    app_id: Option<String>,
    app_installation_id: Option<String>,
    app_private_key: Option<String>,
}

fn cli_snapshot(cli: &Cli) -> CliSnap {
    CliSnap {
        scope: cli.scope,
        repo: cli.repo.clone(),
        owner: cli.owner.clone(),
        user: cli.user.clone(),
        auto: cli.auto,
        image: cli.image.clone(),
        image_mode: cli.image_mode.clone(),
        pull_policy: cli.pull_policy.clone(),
        runner_user: cli.runner_user.clone(),
        seed_helper_image: cli.seed_helper_image.clone(),
        runner_version: cli.runner_version.clone(),
        runner_sha256: cli.runner_sha256.clone(),
        runner_arch: cli.runner_arch.clone(),
        runner_seed_url: cli.runner_seed_url.clone(),
        entrypoint: cli.entrypoint.clone(),
        container: cli.container.clone(),
        volume: cli.volume.clone(),
        runner_name: cli.runner_name.clone(),
        labels: cli.labels.clone(),
        cpus: cli.cpus.clone(),
        memory: cli.memory.clone(),
        gpu: cli.gpu,
        gpu_slice: cli.gpu_slice.clone(),
        demand_require_labels: cli.demand_require_labels.clone(),
        demand_exclude_labels: cli.demand_exclude_labels.clone(),
        prefer_repos: cli.prefer_repos.clone(),
        allowlist_repos_file: cli.allowlist_repos_file.clone(),
        allowlist_repos: cli.allowlist_repos.clone(),
        prefer_repos_file: cli.prefer_repos_file.clone(),
        priority_repos: cli.priority_repos.clone(),
        listen_min_interval: cli.listen_min_interval,
        pool_scan_per_tick: cli.pool_scan_per_tick,
        reap_stale_secs: cli.reap_stale_secs,
        tick_log: cli.tick_log.clone(),
        api_min_gap_ms: cli.api_min_gap_ms,
        api_max_per_poll: cli.api_max_per_poll,
        api_backoff_secs: cli.api_backoff_secs,
        repos_per_tick: cli.repos_per_tick,
        reg_min_gap_secs: cli.reg_min_gap_secs,
        reg_max_per_hour: cli.reg_max_per_hour,
        pool_cpus: cli.pool_cpus.clone(),
        pool_memory: cli.pool_memory.clone(),
        pool_max_workers: cli.pool_max_workers,
        pool_mode: cli.pool_mode.clone(),
        image_map: cli.image_map.clone(),
        platform: cli.platform.clone(),
        mode: cli.mode.clone(),
        wake_token: cli.wake_token.clone(),
        full_auto: cli.full_auto,
        this_repo_only: cli.this_repo_only.clone(),
        public_only: cli.public_only,
        private_only: cli.private_only,
        all_repos: cli.all_repos,
        app_id: cli.app_id.clone(),
        app_installation_id: cli.app_installation_id.clone(),
        app_private_key: cli.app_private_key.clone(),
    }
}

fn snap_to_cli(s: &CliSnap) -> Cli {
    Cli {
        cmd: Some(Cmd::Status),
        scope: s.scope,
        repo: s.repo.clone(),
        owner: s.owner.clone(),
        user: s.user.clone(),
        auto: s.auto,
        image: s.image.clone(),
        image_mode: s.image_mode.clone(),
        pull_policy: s.pull_policy.clone(),
        runner_user: s.runner_user.clone(),
        seed_helper_image: s.seed_helper_image.clone(),
        runner_version: s.runner_version.clone(),
        runner_sha256: s.runner_sha256.clone(),
        runner_arch: s.runner_arch.clone(),
        runner_seed_url: s.runner_seed_url.clone(),
        entrypoint: s.entrypoint.clone(),
        container: s.container.clone(),
        volume: s.volume.clone(),
        runner_name: s.runner_name.clone(),
        labels: s.labels.clone(),
        cpus: s.cpus.clone(),
        memory: s.memory.clone(),
        gpu: s.gpu,
        gpu_slice: s.gpu_slice.clone(),
        demand_require_labels: s.demand_require_labels.clone(),
        demand_exclude_labels: s.demand_exclude_labels.clone(),
        prefer_repos: s.prefer_repos.clone(),
        allowlist_repos_file: s.allowlist_repos_file.clone(),
        allowlist_repos: s.allowlist_repos.clone(),
        prefer_repos_file: s.prefer_repos_file.clone(),
        priority_repos: s.priority_repos.clone(),
        listen_min_interval: s.listen_min_interval,
        pool_scan_per_tick: s.pool_scan_per_tick,
        reap_stale_secs: s.reap_stale_secs,
        tick_log: s.tick_log.clone(),
        api_min_gap_ms: s.api_min_gap_ms,
        api_max_per_poll: s.api_max_per_poll,
        api_backoff_secs: s.api_backoff_secs,
        repos_per_tick: s.repos_per_tick,
        reg_min_gap_secs: s.reg_min_gap_secs,
        reg_max_per_hour: s.reg_max_per_hour,
        pool_cpus: s.pool_cpus.clone(),
        pool_memory: s.pool_memory.clone(),
        pool_max_workers: s.pool_max_workers,
        pool_mode: s.pool_mode.clone(),
        image_map: s.image_map.clone(),
        platform: s.platform.clone(),
        build_dir: None,
        mode: s.mode.clone(),
        wake_token: s.wake_token.clone(),
        full_auto: s.full_auto,
        this_repo_only: s.this_repo_only.clone(),
        public_only: s.public_only,
        private_only: s.private_only,
        all_repos: s.all_repos,
        app_id: s.app_id.clone(),
        app_installation_id: s.app_installation_id.clone(),
        app_private_key: s.app_private_key.clone(),
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0_u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Whether a single HTTP request header line authorizes the wake server.
///
/// Header *names* (and the `Bearer` scheme keyword) are matched case-insensitively.
/// The secret token bytes themselves are **never** lowercased before compare — mixed-case
/// `GHA_WAKE_TOKEN` values must still authenticate.
pub fn wake_request_line_authorized(line: &str, token: &str) -> bool {
    // Both header types (Authorization: Bearer and X-Wake-Token) are checked case-insensitively.
    // However, the secret token values themselves are compared exactly preserving casing.
    let lower = line.to_ascii_lowercase();
    const BEARER_PREFIX: &str = "authorization: bearer ";
    if lower.starts_with(BEARER_PREFIX) && line.len() >= BEARER_PREFIX.len() {
        // Find the boundary in the original line using the lowercase prefix length to preserve token's case.
        let rest = &line[BEARER_PREFIX.len()..];
        return constant_time_eq(rest.trim(), token);
    }

    const WAKE_TOKEN_PREFIX: &str = "x-wake-token:";
    if lower.starts_with(WAKE_TOKEN_PREFIX) && line.len() >= WAKE_TOKEN_PREFIX.len() {
        let rest = &line[WAKE_TOKEN_PREFIX.len()..];
        return constant_time_eq(rest.trim(), token);
    }
    false
}

/// Route an already-authenticated wake-protocol request to its response, given
/// the current quiesce state. Side effects (`up()`/`down()`) are taken as
/// closures rather than called directly so this decision — in particular the
/// quiesce gate on `POST /wake` — is unit-testable without a real container
/// backend or TCP socket.
///
/// `POST /wake` used to call `up()` unconditionally. The tick loop in
/// `listen()` checks `quiesce_active()` before every admission it makes, but
/// `wake_server` runs on its own thread outside that loop and had no such
/// check — the one external caller who could reach a quiesced host and still
/// admit work during the exact drain window an operator asked to protect.
/// Checking it here, before `admit()` runs, is what makes the documented
/// "listen admits no new work" guarantee (docs/interfaces/ctl-cli-env.md)
/// actually hold for wake-port hosts.
///
/// Returns `(status line incl. any extra headers, body)`. On refusal we
/// return 503 + `Retry-After` — the conventional "up but not currently
/// serving" signal, distinguishable from both success and hard failure, and
/// it tells an automated caller when to retry instead of leaving that
/// undocumented. The refusal is also logged so a client that ignores the
/// body can't mistake this for a silent drop.
fn wake_dispatch(
    req: &str,
    quiesced: bool,
    retry_after_secs: u64,
    admit: impl FnOnce() -> Result<(), String>,
    retire: impl FnOnce() -> Result<(), String>,
) -> (String, &'static str) {
    if req.starts_with("POST /wake") {
        if quiesced {
            eprintln!(
                "wake: refused POST /wake — quiesced ({}), no runner started",
                crate::pool::quiesce_path().display()
            );
            return (
                format!("503 Service Unavailable\r\nRetry-After: {retry_after_secs}"),
                "quiesced: admission paused, no runner started\n",
            );
        }
        return match admit() {
            Ok(()) => ("200 OK".to_string(), "up\n"),
            Err(e) => {
                eprintln!("wake: {}", redact(&e));
                ("500".to_string(), "error\n")
            }
        };
    }
    if req.starts_with("POST /sleep") {
        return match retire() {
            Ok(()) => ("200 OK".to_string(), "down\n"),
            Err(e) => {
                eprintln!("sleep: {}", redact(&e));
                ("500".to_string(), "error\n")
            }
        };
    }
    if req.starts_with("GET /health") {
        return ("200 OK".to_string(), "ok\n");
    }
    ("404".to_string(), "use POST /wake or POST /sleep\n")
}

fn wake_server(port: u16, snap: CliSnap, token: String, retry_after_secs: u64) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    let snap = Arc::new(snap);
    let token = Arc::new(token);
    let bind = format!("127.0.0.1:{port}");
    let Ok(listener) = TcpListener::bind(&bind) else {
        eprintln!("wake: bind {bind} failed");
        return;
    };
    for stream in listener.incoming().flatten() {
        let mut s = stream;
        let timeout = Some(Duration::from_secs(5));
        let _ = s.set_read_timeout(timeout);
        let _ = s.set_write_timeout(timeout);
        let mut buf = [0_u8; 2048];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let authed = req
            .lines()
            .any(|line| wake_request_line_authorized(line, token.as_str()));
        if !authed && !req.starts_with("GET /health") {
            let body = "unauthorized\n";
            let _ = write!(
                s,
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            continue;
        }
        let cli = snap_to_cli(&snap);
        let (code, body) = wake_dispatch(
            &req,
            crate::pool::quiesce_active(),
            retry_after_secs,
            || up(&cli),
            || down(&cli, true),
        );
        let _ = write!(
            s,
            "HTTP/1.1 {code}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

#[cfg(test)]
mod robust_queue_tests {
    use super::*;

    /// Guards every test below that reads or writes the process-wide `GHA_APP_ID` /
    /// `GHA_APP_INSTALLATION_ID` / `GHA_APP_PRIVATE_KEY` env vars. `std::env::set_var`
    /// is process-global and Rust runs tests on multiple threads by default, so without
    /// this a test asserting "none of these are set" can observe a sibling test's var
    /// mid-flight. Every test in that group takes this lock for its whole body.
    static APP_ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn lock_is_stale_grace_protects_mid_creation_and_live_pid() {
        use std::io::Write as _;
        let base = std::env::temp_dir().join(format!("ghar-stale-test-{}", std::process::id()));

        // A just-created empty lock (holder is between create_new and writeln!(pid))
        // must NOT be judged stale — reclaiming it would steal a live lock (TOCTOU).
        let empty = base.with_extension("empty");
        let _ = fs::remove_file(&empty);
        fs::File::create(&empty).unwrap();
        assert!(
            !lock_is_stale(&empty),
            "fresh empty lock must be treated as live within the write grace"
        );

        // A lock owned by a live PID (ourselves) must NOT be stale.
        let live = base.with_extension("live");
        let mut f = fs::File::create(&live).unwrap();
        writeln!(f, "{}", std::process::id()).unwrap();
        drop(f);
        assert!(!lock_is_stale(&live), "our own live PID must not be stale");

        // A non-existent lock path is trivially reclaimable (nothing to protect).
        let missing = base.with_extension("missing");
        let _ = fs::remove_file(&missing);
        assert!(
            lock_is_stale(&missing),
            "a non-existent lock is stale (nothing to protect)"
        );

        let _ = fs::remove_file(&empty);
        let _ = fs::remove_file(&live);
    }

    #[test]
    fn allowlist_repos_supersedes_deprecated_prefer_repos() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--allowlist-repos",
            "owner/a,owner/b",
            "--prefer-repos",
            "owner/old",
        ])
        .unwrap();
        assert_eq!(
            cli.effective_allowlist_repos_quiet().as_deref(),
            Some("owner/a,owner/b"),
            "GHA_ALLOWLIST_REPOS must win when both names are set"
        );
    }

    #[test]
    fn allowlist_repos_file_supersedes_deprecated_prefer_repos_file() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--allowlist-repos-file",
            "/etc/new.list",
            "--prefer-repos-file",
            "/etc/old.list",
        ])
        .unwrap();
        assert_eq!(
            cli.effective_allowlist_repos_file().map(String::as_str),
            Some("/etc/new.list"),
            "GHA_ALLOWLIST_REPOS_FILE must win when both file names are set"
        );
    }

    /// The regression that actually matters. A live fleet host pins the OLD name:
    ///   GHA_PREFER_REPOS_FILE=.../allowlists/active-demand.list
    /// If this resolution ever returns None, that host silently loses its allowlist and
    /// starts polling every owned repo, exhausting its API budget mid-scan and reporting
    /// "no demand" — which is exactly the outage this rename must not cause.
    #[test]
    fn deprecated_prefer_repos_file_alone_is_still_honored() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--prefer-repos-file",
            "/home/gha-agent/.local/share/gha-runner-ctl/allowlists/active-demand.list",
        ])
        .unwrap();
        assert_eq!(
            cli.effective_allowlist_repos_file().map(String::as_str),
            Some("/home/gha-agent/.local/share/gha-runner-ctl/allowlists/active-demand.list"),
            "the deprecated file name must keep working for pinned fleet configs"
        );
    }

    #[test]
    fn no_allowlist_file_set_resolves_to_none() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl"]).unwrap();
        assert_eq!(cli.effective_allowlist_repos_file(), None);
    }

    /// The allowlist rename must not have touched the PRIORITY feature, which is a genuinely
    /// different thing: priority repos are scanned every tick in fixed order BEFORE the
    /// round-robin over the allowlist. Conflating them is the bug this work exists to fix,
    /// so pin that they stay independent.
    #[test]
    fn priority_repos_are_independent_of_the_allowlist() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--allowlist-repos",
            "owner/a,owner/b",
            "--priority-repos",
            "owner/hot",
        ])
        .unwrap();
        assert_eq!(
            priority_repos_list(&cli),
            vec!["owner/hot".to_string()],
            "priority list must come only from --priority-repos"
        );
        assert_eq!(
            cli.effective_allowlist_repos_quiet().as_deref(),
            Some("owner/a,owner/b"),
            "the allowlist must not absorb priority entries"
        );
    }

    /// A deprecation warning that fires every poll tick trains operators to ignore it. This
    /// pins the once-per-process contract at the helper level (the statics themselves are
    /// per-call-site, so this asserts the mechanism, not each caller).
    #[test]
    fn warn_deprecated_once_fires_a_single_time() {
        static SLOT: AtomicBool = AtomicBool::new(false);
        assert!(!SLOT.load(AtomicOrdering::Relaxed));
        warn_deprecated_once(&SLOT, "first");
        assert!(
            SLOT.load(AtomicOrdering::Relaxed),
            "the slot must latch after the first warning"
        );
        // Second call must be a no-op; if the latch did not hold, the slot would still be
        // observable as false here on a fresh swap.
        warn_deprecated_once(&SLOT, "second");
        assert!(SLOT.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn deprecated_prefer_repos_alone_still_resolves() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl", "--prefer-repos", "owner/old"]).unwrap();
        assert_eq!(
            cli.effective_allowlist_repos_quiet().as_deref(),
            Some("owner/old"),
            "the deprecated name must still work during the deprecation window"
        );
    }

    #[test]
    fn neither_allowlist_name_set_resolves_to_none() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl"]).unwrap();
        assert_eq!(cli.effective_allowlist_repos_quiet(), None);
    }

    #[test]
    fn parse_repo_csv_dedupes_and_strips_comments() {
        let s = "tzervas/mycelium-lang, tzervas/cabal-devmelopner\n# comment\ntzervas/mycelium-lang\nbad;repo\n";
        let v = parse_repo_csv(s);
        assert_eq!(
            v,
            vec![
                "tzervas/mycelium-lang".to_string(),
                "tzervas/cabal-devmelopner".to_string()
            ]
        );
    }

    #[test]
    fn parse_repo_csv_newlines_and_hash_inline() {
        let s = "owner/a # note\nowner/b\r\nowner/c";
        let v = parse_repo_csv(s);
        assert_eq!(v, vec!["owner/a", "owner/b", "owner/c"]);
    }

    #[test]
    fn normalize_podman_started_at_go_format() {
        let raw = "2026-07-21 15:52:33.909118621 -0400 EDT";
        assert_eq!(
            normalize_podman_started_at(raw),
            "2026-07-21 15:52:33 -0400"
        );
    }

    #[test]
    fn normalize_podman_started_at_iso() {
        let raw = "2026-07-21T19:52:33.909118621Z";
        assert_eq!(normalize_podman_started_at(raw), "2026-07-21 19:52:33 UTC");
    }

    // --- App-auth CLI surface: flag-vs-env precedence, matches every other option ---
    //
    // These mutate the process-wide GHA_APP_* env vars, which is inherently racy
    // against any *other* test reading the same names — but nothing else in this
    // binary does (they're unique to App auth), so this follows existing precedent
    // (e.g. `dynamic_pool`'s GHA_POOL_* env writes) rather than needing extra machinery.
    // Each test clears what it set so it doesn't leak into a neighbor.

    #[test]
    fn cli_app_id_flag_beats_env_and_env_is_used_when_flag_absent() {
        // Both assertions mutate the same process-wide GHA_APP_ID var, so they live in
        // one test (guaranteed sequential within a single thread) rather than two —
        // splitting them would race any other test/thread reading GHA_APP_ID mid-run.
        let _guard = APP_ENV_TEST_LOCK.lock().unwrap();
        use clap::Parser as _;

        std::env::set_var("GHA_APP_ID", "111111");
        let flag_wins =
            Cli::try_parse_from(["gha-runner-ctl", "--app-id", "222222", "status"]).unwrap();
        assert_eq!(
            flag_wins.app_id.as_deref(),
            Some("222222"),
            "an explicit --app-id must win over GHA_APP_ID, exactly like every other flag"
        );

        let env_used = Cli::try_parse_from(["gha-runner-ctl", "status"]).unwrap();
        assert_eq!(env_used.app_id.as_deref(), Some("111111"));

        std::env::remove_var("GHA_APP_ID");
    }

    #[test]
    fn cli_app_installation_id_and_private_key_flags_and_env_both_populate_cli() {
        let _guard = APP_ENV_TEST_LOCK.lock().unwrap();
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--app-id",
            "4451176",
            "--app-installation-id",
            "150429495",
            "--app-private-key",
            "secret:runner/gha-app-key",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.app_id.as_deref(), Some("4451176"));
        assert_eq!(cli.app_installation_id.as_deref(), Some("150429495"));
        assert_eq!(
            cli.app_private_key.as_deref(),
            Some("secret:runner/gha-app-key")
        );

        std::env::set_var("GHA_APP_PRIVATE_KEY", "file:/etc/gha/key.pem");
        let cli_env = Cli::try_parse_from(["gha-runner-ctl", "status"]).unwrap();
        std::env::remove_var("GHA_APP_PRIVATE_KEY");
        assert_eq!(
            cli_env.app_private_key.as_deref(),
            Some("file:/etc/gha/key.pem")
        );
    }

    #[test]
    fn cli_app_installation_id_is_optional() {
        let _guard = APP_ENV_TEST_LOCK.lock().unwrap();
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "gha-runner-ctl",
            "--app-id",
            "1",
            "--app-private-key",
            "file:/k.pem",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.app_installation_id, None);
    }

    #[test]
    fn cli_app_auth_config_partial_is_a_hard_error_naming_the_missing_flag() {
        let _guard = APP_ENV_TEST_LOCK.lock().unwrap();
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl", "--app-id", "123456", "status"]).unwrap();
        let err = cli.app_auth_config().unwrap_err();
        assert!(err.contains("app-private-key"), "{err}");
    }

    #[test]
    fn cli_app_auth_config_none_set_falls_back_silently() {
        let _guard = APP_ENV_TEST_LOCK.lock().unwrap();
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl", "status"]).unwrap();
        assert!(cli.app_auth_config().unwrap().is_none());
    }

    #[test]
    fn cli_app_auth_owner_hint_prefers_owner_then_user_then_repo() {
        use clap::Parser as _;
        let by_owner = Cli::try_parse_from([
            "gha-runner-ctl",
            "--owner",
            "org-owner",
            "--user",
            "user-login",
            "--repo",
            "repo-owner/name",
            "status",
        ])
        .unwrap();
        assert_eq!(by_owner.app_auth_owner_hint().as_deref(), Some("org-owner"));

        let by_user =
            Cli::try_parse_from(["gha-runner-ctl", "--user", "user-login", "status"]).unwrap();
        assert_eq!(by_user.app_auth_owner_hint().as_deref(), Some("user-login"));

        let by_repo =
            Cli::try_parse_from(["gha-runner-ctl", "--repo", "repo-owner/name", "status"]).unwrap();
        assert_eq!(by_repo.app_auth_owner_hint().as_deref(), Some("repo-owner"));

        let none = Cli::try_parse_from(["gha-runner-ctl", "status"]).unwrap();
        assert_eq!(none.app_auth_owner_hint(), None);
    }

    #[test]
    fn doctor_subcommand_parses() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["gha-runner-ctl", "doctor"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Doctor)));
    }

    /// The defect this guards: `POST /wake` used to call `up()` unconditionally,
    /// bypassing the quiesce flag entirely — the tick loop in `listen()` is the
    /// *only* place that ever checked `quiesce_active()`, and `wake_server` runs
    /// on its own thread outside that loop. A drain window is exactly when an
    /// external wake call is most likely: something outside the fleet still
    /// thinks the host is up and pokes it.
    ///
    /// `admit` increments a counter instead of touching a real container
    /// backend — the point of `wake_dispatch` taking closures is that this needs
    /// no Docker/Podman, no GitHub token, and no filesystem quiesce flag; the
    /// quiesce state is passed straight in as `quiesced: true`.
    #[test]
    fn wake_dispatch_refuses_admission_while_quiesced() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let admit_calls = AtomicUsize::new(0);
        let (status, body) = wake_dispatch(
            "POST /wake HTTP/1.1\r\n\r\n",
            /* quiesced = */ true,
            /* retry_after_secs = */ 45,
            || {
                admit_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || panic!("retire (down) must not run for a /wake request"),
        );
        assert_eq!(
            admit_calls.load(Ordering::SeqCst),
            0,
            "a quiesced /wake must never invoke admit() — no runner may start"
        );
        assert!(
            status.starts_with("503"),
            "quiesced /wake must answer 503 Service Unavailable, got {status:?}"
        );
        assert!(
            status.contains("Retry-After: 45"),
            "503 must carry the Retry-After hint so callers know when to retry, got {status:?}"
        );
        assert!(
            body.contains("quiesced"),
            "body must explicitly say no work was admitted, got {body:?}"
        );
    }

    /// Symmetric check: with quiesce OFF, `POST /wake` must still admit exactly
    /// once. Without this, a fix to the bug above could overcorrect into always
    /// refusing (which would also "pass" a test that only checked the quiesced
    /// case) and silently break wake-on-demand for every non-quiesced host.
    #[test]
    fn wake_dispatch_admits_once_when_not_quiesced() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let admit_calls = AtomicUsize::new(0);
        let (status, body) = wake_dispatch(
            "POST /wake HTTP/1.1\r\n\r\n",
            /* quiesced = */ false,
            30,
            || {
                admit_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || panic!("retire (down) must not run for a /wake request"),
        );
        assert_eq!(admit_calls.load(Ordering::SeqCst), 1);
        assert_eq!(status, "200 OK");
        assert_eq!(body, "up\n");
    }
}

/// Tests that drive **real HTTP** through [`HttpConfig`] — the seam added so that the
/// registration and demand-poll paths are reachable without talking to github.com.
///
/// Every test here asserts on what a local server actually received. That is the point:
/// if a call site stops going through the seam and pastes `https://api.github.com` back
/// into a `format!`, the local server records nothing and these tests fail. They cannot
/// pass by accident and they cannot pass while bypassed.
#[cfg(test)]
mod http_seam_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A request exactly as the server saw it on the wire.
    #[derive(Clone, Debug)]
    struct SeenRequest {
        method: String,
        /// Path + query, verbatim.
        url: String,
        authorization: Option<String>,
        accept: Option<String>,
        api_version: Option<String>,
        user_agent: Option<String>,
    }

    /// One canned reply, matched by method + exact path-with-query.
    struct Route {
        method: &'static str,
        url: String,
        status: u16,
        body: &'static str,
        /// Extra response headers (name, value), e.g. `Retry-After`.
        headers: Vec<(&'static str, &'static str)>,
    }

    impl Route {
        fn get(url: &str, status: u16, body: &'static str) -> Self {
            Self {
                method: "GET",
                url: url.to_string(),
                status,
                body,
                headers: Vec::new(),
            }
        }

        fn post(url: &str, status: u16, body: &'static str) -> Self {
            Self {
                method: "POST",
                url: url.to_string(),
                status,
                body,
                headers: Vec::new(),
            }
        }

        fn header(mut self, name: &'static str, value: &'static str) -> Self {
            self.headers.push((name, value));
            self
        }
    }

    /// Status returned for a request no route matched. Deliberately *not* one of the
    /// codes [`is_soft_api_err`] swallows, so an unrouted request surfaces loudly
    /// instead of being mistaken for "no work queued".
    const UNROUTED: u16 = 599;

    /// A synchronous loopback HTTP server (tiny_http) that serves a fixed route table
    /// and records every request.
    struct TestServer {
        base: String,
        seen: Arc<Mutex<Vec<SeenRequest>>>,
        server: Arc<tiny_http::Server>,
        joiner: Option<std::thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start(routes: Vec<Route>) -> Self {
            let server =
                Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind loopback port"));
            let addr = server
                .server_addr()
                .to_ip()
                .expect("loopback listener has an IP address");
            let base = format!("http://{addr}");
            let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::new(Mutex::new(Vec::new()));

            let srv = Arc::clone(&server);
            let sink = Arc::clone(&seen);
            let joiner = std::thread::spawn(move || {
                for req in srv.incoming_requests() {
                    let record = {
                        // `HeaderField::equiv` compares against a `&'static str`.
                        let header = |name: &'static str| -> Option<String> {
                            req.headers()
                                .iter()
                                .find(|h| h.field.equiv(name))
                                .map(|h| h.value.as_str().to_string())
                        };
                        SeenRequest {
                            method: req.method().as_str().to_string(),
                            url: req.url().to_string(),
                            authorization: header("Authorization"),
                            accept: header("Accept"),
                            api_version: header("X-GitHub-Api-Version"),
                            user_agent: header("User-Agent"),
                        }
                    };

                    let hit = routes
                        .iter()
                        .find(|r| r.method == record.method && r.url == record.url);
                    let (status, body, extra) = match hit {
                        Some(r) => (r.status, r.body, r.headers.as_slice()),
                        None => (UNROUTED, "{\"unrouted\":true}", &[][..]),
                    };
                    sink.lock().expect("seen sink").push(record);

                    let mut resp = tiny_http::Response::from_string(body)
                        .with_status_code(status)
                        .with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                &b"application/json"[..],
                            )
                            .expect("content-type header"),
                        );
                    for (name, value) in extra {
                        resp = resp.with_header(
                            tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
                                .expect("extra header"),
                        );
                    }
                    let _ = req.respond(resp);
                }
            });

            Self {
                base,
                seen,
                server,
                joiner: Some(joiner),
            }
        }

        /// Base URL to hand to [`HttpConfig::with_api_base`].
        fn base(&self) -> &str {
            &self.base
        }

        fn seen(&self) -> Vec<SeenRequest> {
            self.seen.lock().expect("seen sink").clone()
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.server.unblock();
            if let Some(j) = self.joiner.take() {
                let _ = j.join();
            }
        }
    }

    /// Config pointed at `server`, with timeouts short enough that a wedged server
    /// fails the test in seconds rather than `HTTP_TIMEOUT`.
    fn test_http(server: &TestServer) -> HttpConfig {
        HttpConfig::with_api_base(server.base()).with_timeout(Duration::from_secs(5))
    }

    /// Redirect host-wide registration-pace state (see [`reg_pace_paths`]) into a
    /// private directory. Without this the tests would read and mutate the real
    /// hourly registration budget of any listener running on the same machine.
    fn isolate_runtime_dir() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("ghar-http-seam-{}", std::process::id()));
            let _ = fs::create_dir_all(&dir);
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
        });
    }

    fn cli_for(args: &[&str]) -> Cli {
        let mut argv = vec!["gha-runner-ctl"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("test CLI parses")
    }

    fn assert_github_headers(req: &SeenRequest, token: &str) {
        assert_eq!(
            req.authorization.as_deref(),
            Some(format!("Bearer {token}").as_str()),
            "Authorization must reach the wire unchanged"
        );
        assert_eq!(req.accept.as_deref(), Some("application/vnd.github+json"));
        assert_eq!(req.api_version.as_deref(), Some("2022-11-28"));
        assert_eq!(
            req.user_agent.as_deref(),
            Some(UA),
            "the seam must still stamp the crate UA"
        );
    }

    /// The default config must rebuild the *exact* strings that were hardcoded before
    /// the seam existed. This is the no-behaviour-change proof: if `api_url` ever
    /// composes differently (double slash, missing slash, changed host), production
    /// requests change and this fails.
    #[test]
    fn default_config_reproduces_the_previously_hardcoded_urls() {
        let http = HttpConfig::github();
        assert_eq!(http.api_base(), "https://api.github.com");
        assert_eq!(http.api_url("user"), "https://api.github.com/user");
        assert_eq!(
            registration_api_for_repo("owner/repo", &http),
            "https://api.github.com/repos/owner/repo/actions/runners/registration-token"
        );
        assert_eq!(
            http.api_url("orgs/acme/actions/runners/registration-token"),
            "https://api.github.com/orgs/acme/actions/runners/registration-token"
        );
        assert_eq!(
            http.api_url("repos/owner/repo/actions/runs?status=queued&per_page=5"),
            "https://api.github.com/repos/owner/repo/actions/runs?status=queued&per_page=5"
        );
        assert_eq!(
            http.api_url("repos/owner/repo/actions/runs/42/jobs"),
            "https://api.github.com/repos/owner/repo/actions/runs/42/jobs"
        );
        assert_eq!(
            http.api_url("orgs/acme/repos?per_page=100&type=all"),
            "https://api.github.com/orgs/acme/repos?per_page=100&type=all"
        );
        assert_eq!(
            http.api_url("users/tzervas/repos?type=owner&per_page=100&sort=updated"),
            "https://api.github.com/users/tzervas/repos?type=owner&per_page=100&sort=updated"
        );
        // Production always resolves to the GitHub defaults today.
        assert_eq!(cli_for(&["--repo", "owner/repo"]).http(), http);
    }

    /// A base with a trailing slash must not produce `//` — the join has exactly one
    /// separator regardless of how the base was written.
    #[test]
    fn api_base_join_is_slash_normalised() {
        let http = HttpConfig::with_api_base("https://example.test/api/v4/");
        assert_eq!(http.api_base(), "https://example.test/api/v4");
        assert_eq!(
            http.api_url("/user/runners"),
            "https://example.test/api/v4/user/runners"
        );
        assert_eq!(
            http.api_url("user/runners"),
            "https://example.test/api/v4/user/runners"
        );
    }

    /// Drives `registration_token()` — a real POST over a real socket.
    ///
    /// Covers the success mint and the rate-limited path, in that order and in one
    /// test, because both share the host-wide pace lock and must not race.
    #[test]
    fn registration_token_posts_through_the_seam() {
        isolate_runtime_dir();

        let server = TestServer::start(vec![
            Route::post(
                "/repos/owner/repo/actions/runners/registration-token",
                201,
                "{\"token\":\"AAAA1111BBBB2222\"}",
            ),
            Route::post(
                "/orgs/acme/actions/runners/registration-token",
                403,
                "{\"message\":\"rate limited\"}",
            )
            .header("Retry-After", "1"),
        ]);
        let http = test_http(&server);

        // --- repo scope, success -------------------------------------------------
        let cli = cli_for(&[
            "--scope",
            "repo",
            "--repo",
            "owner/repo",
            "--reg-min-gap-secs",
            "1",
            "--reg-max-per-hour",
            "500",
        ]);
        let token =
            registration_token(&cli, "pat-repo", &http).expect("registration token is minted");
        assert_eq!(token, "AAAA1111BBBB2222");

        // --- org scope, 403 + Retry-After ---------------------------------------
        let org_cli = cli_for(&[
            "--scope",
            "org",
            "--owner",
            "acme",
            "--reg-min-gap-secs",
            "1",
            "--reg-max-per-hour",
            "500",
        ]);
        let err = registration_token(&org_cli, "pat-org", &http)
            .expect_err("403 must surface as an error");
        assert!(err.contains("403"), "unexpected error text: {err}");

        // --- what actually went over the wire ------------------------------------
        let seen = server.seen();
        assert_eq!(
            seen.len(),
            2,
            "expected exactly the two registration POSTs, got {seen:?}"
        );

        assert_eq!(seen[0].method, "POST");
        assert_eq!(
            seen[0].url, "/repos/owner/repo/actions/runners/registration-token",
            "repo scope must POST the repo-scoped registration path"
        );
        assert_github_headers(&seen[0], "pat-repo");

        assert_eq!(seen[1].method, "POST");
        assert_eq!(
            seen[1].url, "/orgs/acme/actions/runners/registration-token",
            "org scope must POST the org-scoped registration path"
        );
        assert_github_headers(&seen[1], "pat-org");
    }

    /// Drives the demand poll (`repo_needs_runner`) end to end: the runs listing and
    /// the per-run jobs listing, both issued through `ApiPacer`'s seam.
    #[test]
    fn demand_poll_gets_runs_and_jobs_through_the_seam() {
        let server = TestServer::start(vec![
            Route::get(
                "/repos/owner/repo/actions/runs?status=queued&per_page=5",
                200,
                "{\"workflow_runs\":[{\"id\":42}]}",
            ),
            Route::get(
                "/repos/owner/repo/actions/runs/42/jobs",
                200,
                "{\"jobs\":[{\"name\":\"build\",\"status\":\"queued\",\
                  \"labels\":[\"self-hosted\",\"linux\",\"podman\"]}]}",
            ),
        ]);

        let cli = cli_for(&[
            "--scope",
            "repo",
            "--repo",
            "owner/repo",
            "--api-min-gap-ms",
            "50",
            "--api-max-per-poll",
            "8",
        ]);
        let mut pacer = ApiPacer::from_cli(&cli, test_http(&server));
        pacer.begin_poll();

        let needs = repo_needs_runner(&cli, "owner/repo", "pat-demand", &mut pacer)
            .expect("demand poll succeeds");
        assert!(needs, "a queued self-hosted job must be reported as demand");

        let seen = server.seen();
        assert_eq!(
            seen.len(),
            2,
            "expected the runs GET then the jobs GET, got {seen:?}"
        );
        assert_eq!(seen[0].method, "GET");
        assert_eq!(
            seen[0].url,
            "/repos/owner/repo/actions/runs?status=queued&per_page=5"
        );
        assert_github_headers(&seen[0], "pat-demand");

        assert_eq!(seen[1].method, "GET");
        assert_eq!(seen[1].url, "/repos/owner/repo/actions/runs/42/jobs");
        assert_github_headers(&seen[1], "pat-demand");

        // The pacer counted both calls against the per-poll budget.
        assert_eq!(pacer.calls_this_poll, 2);
    }

    /// A queued job whose labels do not match must NOT wake the listener — and the
    /// listener must then also probe `in_progress`. Exercises a second, distinct
    /// demand request shape through the same seam.
    #[test]
    fn demand_poll_probes_in_progress_when_queued_does_not_match() {
        let server = TestServer::start(vec![
            Route::get(
                "/repos/owner/repo/actions/runs?status=queued&per_page=5",
                200,
                "{\"workflow_runs\":[{\"id\":7}]}",
            ),
            // ubuntu-latest only: no self-hosted/podman baseline, so no demand.
            Route::get(
                "/repos/owner/repo/actions/runs/7/jobs",
                200,
                "{\"jobs\":[{\"name\":\"lint\",\"status\":\"queued\",\
                  \"labels\":[\"ubuntu-latest\"]}]}",
            ),
            Route::get(
                "/repos/owner/repo/actions/runs?status=in_progress&per_page=5",
                200,
                "{\"workflow_runs\":[]}",
            ),
        ]);

        let cli = cli_for(&[
            "--scope",
            "repo",
            "--repo",
            "owner/repo",
            "--api-min-gap-ms",
            "50",
            "--api-max-per-poll",
            "8",
        ]);
        let mut pacer = ApiPacer::from_cli(&cli, test_http(&server));
        pacer.begin_poll();

        let needs = repo_needs_runner(&cli, "owner/repo", "pat-demand", &mut pacer)
            .expect("demand poll succeeds");
        assert!(
            !needs,
            "ubuntu-latest jobs must not wake a self-hosted runner"
        );

        let urls: Vec<String> = server.seen().into_iter().map(|r| r.url).collect();
        assert_eq!(
            urls,
            vec![
                "/repos/owner/repo/actions/runs?status=queued&per_page=5",
                "/repos/owner/repo/actions/runs/7/jobs",
                "/repos/owner/repo/actions/runs?status=in_progress&per_page=5",
            ]
        );
    }

    /// `get_user_login_from_token` is the third seam call site; prove it too resolves
    /// its URL from the config rather than a literal.
    #[test]
    fn user_login_lookup_goes_through_the_seam() {
        let server = TestServer::start(vec![Route::get("/user", 200, "{\"login\":\"tzervas\"}")]);

        let login = get_user_login_from_token("pat-user", &test_http(&server))
            .expect("login resolves from the test server");
        assert_eq!(login, "tzervas");

        let seen = server.seen();
        assert_eq!(
            seen.len(),
            1,
            "expected exactly one GET /user, got {seen:?}"
        );
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].url, "/user");
        assert_github_headers(&seen[0], "pat-user");
    }

    /// Guard for the guard: if the seam were bypassed the local server would simply
    /// see nothing, so confirm that "server saw nothing" is in fact a failing state
    /// for the assertions above — an unrouted request is not silently tolerated.
    #[test]
    fn unrouted_requests_are_not_soft_errors() {
        assert!(
            !is_soft_api_err(&format!("status code {UNROUTED}")),
            "the unrouted status must not be swallowed by the demand poll's soft-error filter, \
             otherwise a bypassed seam could look like 'no demand' instead of a test failure"
        );
    }
}

#[cfg(test)]
mod priority_cursor_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gha-prio-cursor-{}-{}-{}",
            tag,
            std::process::id(),
            tag.len()
        ));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn no_cursor_file_starts_at_head() {
        let p = tmp("head");
        let items: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let (rot, off) = rotate_by_cursor(&items, &p);
        assert_eq!(off, 0);
        assert_eq!(
            rot, items,
            "a fresh host must scan in strict priority order"
        );
    }

    #[test]
    fn rotation_preserves_every_entry() {
        let items: Vec<String> = (0..7).map(|i| format!("r{i}")).collect();
        let p = tmp("perm");
        for off in 0..7 {
            fs::write(&p, off.to_string()).unwrap();
            let (rot, got) = rotate_by_cursor(&items, &p);
            assert_eq!(got, off);
            assert_eq!(rot.len(), items.len(), "rotation must not drop entries");
            let mut s = rot.clone();
            s.sort();
            let mut orig = items.clone();
            orig.sort();
            assert_eq!(s, orig, "rotation must be a permutation, never a filter");
            assert_eq!(rot[0], items[off], "rotation must start at the cursor");
        }
        let _ = fs::remove_file(&p);
    }

    /// The regression this exists for: with a fixed scan order, truncation cuts
    /// the same tail forever and those repos are NEVER polled. Advancing by what
    /// was actually consumed must eventually bring every entry to the front.
    #[test]
    fn truncated_scans_eventually_cover_every_repo() {
        let items: Vec<String> = (0..10).map(|i| format!("r{i}")).collect();
        let p = tmp("cover");
        let budget = 3usize; // only 3 repos fit per tick
        let mut ever_first: std::collections::HashSet<String> = Default::default();
        for _ in 0..10 {
            let (rot, off) = rotate_by_cursor(&items, &p);
            for r in rot.iter().take(budget) {
                ever_first.insert(r.clone());
            }
            advance_cursor(&p, off, budget, items.len());
        }
        assert_eq!(
            ever_first.len(),
            items.len(),
            "every priority repo must get scanned within a bounded number of ticks; \
             starved entries are exactly the mycelium-never-polled bug"
        );
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn advance_never_stalls_even_when_nothing_consumed() {
        let items: Vec<String> = (0..4).map(|i| format!("r{i}")).collect();
        let p = tmp("stall");
        fs::write(&p, "0").unwrap();
        // consumed = 0 (budget died immediately). Cursor must still move, or the
        // same head repo is retried forever and the tail never advances.
        advance_cursor(&p, 0, 0, items.len());
        let (_, off) = rotate_by_cursor(&items, &p);
        assert_ne!(off, 0, "a zero-progress tick must still advance the cursor");
        let _ = fs::remove_file(&p);
    }
}

/// Proves the plaintext fail-closed debug dump (`debug_dump_fail_closed`, tested here
/// via its testable core, `write_debug_dump_fail_closed`, which writes to any `Write`
/// instead of stderr directly) never prints a raw credential from `check`/`object`/
/// `assumed`/`reason` — the second of the two output paths the issue #132 follow-up
/// audit's HIGH-1/HIGH-2 findings named (the first, the WARN JSON event, is proven in
/// `fail_closed::tests`). Both paths read from the same already-redacted
/// `FailClosedEvent` getters, so these tests are really exercising that
/// `write_debug_dump_fail_closed` does not reintroduce a leak by, say, printing a raw
/// field it was handed directly instead of going through the getters.
#[cfg(test)]
mod fail_closed_debug_dump_tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn dump(ev: &FailClosedEvent) -> String {
        dump_with_command(ev, "podman top <container>")
    }

    fn dump_with_command(ev: &FailClosedEvent, command: &str) -> String {
        let mut buf: Vec<u8> = Vec::new();
        let command = RedactedCommand::new(command);
        write_debug_dump_fail_closed(&mut buf, ev, &command, &[]).unwrap();
        String::from_utf8(buf).expect("dump is valid UTF-8")
    }

    #[test]
    fn synthetic_credential_in_check_is_redacted_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let ev = t.record(&synthetic, "obj", "referenced", "boom", UNIX_EPOCH);
        let out = dump(&ev);
        assert!(
            !out.contains(&synthetic),
            "credential leaked in dump: {out}"
        );
    }

    #[test]
    fn synthetic_credential_in_object_is_redacted_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let synthetic = "AKIAIOSFODNN7EXAMPLE";
        let ev = t.record(
            "image_refcount",
            synthetic,
            "referenced",
            "boom",
            UNIX_EPOCH,
        );
        let out = dump(&ev);
        assert!(!out.contains(synthetic), "credential leaked in dump: {out}");
    }

    #[test]
    fn synthetic_credential_in_assumed_is_redacted_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let ev = t.record("image_refcount", "obj", &synthetic, "boom", UNIX_EPOCH);
        let out = dump(&ev);
        assert!(
            !out.contains(&synthetic),
            "credential leaked in dump: {out}"
        );
    }

    /// The exact scenario the auditor confirmed live: a synthetic credential placed
    /// in `object`, unredacted by the caller, must not reach the dump.
    #[test]
    fn synthetic_credential_placed_in_object_never_reaches_dump_live_repro() {
        let t = FailClosedTracker::new();
        let synthetic = format!("github_pat_{}", "a1B2c3D4e5".repeat(9));
        let ev = t.record(
            "worker_busy_probe",
            &synthetic,
            "busy",
            "podman top: exit 1",
            UNIX_EPOCH,
        );
        let out = dump(&ev);
        assert!(
            !out.contains(&synthetic),
            "credential leaked in dump: {out}"
        );
    }

    /// HIGH-2, mid-sentence in raw stderr, plaintext-dump side — and the diagnostic
    /// text around it must survive (requirement 3).
    #[test]
    fn synthetic_credential_embedded_mid_sentence_in_reason_is_redacted_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let synthetic = "AKIAIOSFODNN7EXAMPLE";
        let reason = format!(
            "podman top failed: exit 125, cannot chdir to /home/kang, aws_key={synthetic} rejected"
        );
        let ev = t.record("worker_busy_probe", "worker-3", "busy", &reason, UNIX_EPOCH);
        let out = dump(&ev);
        assert!(!out.contains(synthetic), "credential leaked in dump: {out}");
        assert!(
            out.contains("podman top failed: exit 125, cannot chdir to /home/kang"),
            "diagnostic detail must survive intact: {out}"
        );
    }

    /// Requirement 3's literal example, end to end through the real dump writer (not
    /// just `redact_free_text` in isolation): must appear byte-for-byte in the dump.
    #[test]
    fn ordinary_diagnostic_reason_survives_verbatim_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let reason = "podman top failed: exit 125, cannot chdir to /home/kang";
        let ev = t.record("worker_busy_probe", "worker-3", "busy", reason, UNIX_EPOCH);
        let out = dump(&ev);
        assert!(
            out.contains(&format!("reason:      {reason}")),
            "reason line must survive intact: {out}"
        );
    }

    // --- MEDIUM-B fix (second follow-up audit): `command` is now redacted ----------

    /// Confirmed live by the auditor: a dynamically-built command string embedding a
    /// synthetic credential used to print verbatim in the `command:` line, since
    /// `write_debug_dump_fail_closed` printed its `command: &str` parameter with zero
    /// redaction. `command` now goes through [`RedactedCommand::new`] (which calls
    /// [`redact_free_text`]) before it ever reaches the writer.
    #[test]
    fn synthetic_credential_in_dynamically_built_command_is_redacted() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let ev = t.record("worker_busy_probe", "worker-3", "busy", "boom", UNIX_EPOCH);
        let command = format!("podman exec --env TOKEN={synthetic} worker-3 true");
        let out = dump_with_command(&ev, &command);
        assert!(
            !out.contains(&synthetic),
            "credential leaked via command field: {out}"
        );
        // Diagnostic value must survive: the rest of the command line is intact.
        assert!(out.contains("podman exec --env TOKEN="));
        assert!(out.contains("worker-3 true"));
    }

    /// A digit-free credential embedded in the command — proves the fix routes
    /// through the same MEDIUM-A-fixed `redact_free_text`, not a separate weaker path.
    #[test]
    fn digit_free_synthetic_credential_in_command_is_redacted() {
        let t = FailClosedTracker::new();
        let secret = "QzXpLwVnTbYhSfJdMcUaEzXkNbPqRsTvWyAeGh";
        let ev = t.record("worker_busy_probe", "worker-3", "busy", "boom", UNIX_EPOCH);
        let command = format!("curl -H 'Authorization: Bearer {secret}' https://internal");
        let out = dump_with_command(&ev, &command);
        assert!(
            !out.contains(secret),
            "credential leaked via command field: {out}"
        );
    }

    /// Ordinary static command text (today's single real call site) must survive
    /// byte-for-byte — the fix must not damage diagnostic usability.
    #[test]
    fn ordinary_command_survives_verbatim_in_plaintext_dump() {
        let t = FailClosedTracker::new();
        let ev = t.record("worker_busy_probe", "worker-3", "busy", "boom", UNIX_EPOCH);
        let out = dump_with_command(&ev, "podman top <container>");
        assert!(
            out.contains("command:     podman top <container>"),
            "command line must survive intact: {out}"
        );
    }
}

/// Issue #132 THIRD follow-up audit: `debug_dump_on_error`'s `err` parameter (and,
/// while sweeping the rest of the function, `user`/`pwd`/podman stdout/stderr/ps
/// lines) printed via bare `eprintln!` with zero redaction of its own — the round-3
/// finding, reproduced live by the auditor with an AWS-shaped secret that the old
/// `redact()` blocklist's 8 fixed prefixes had no entry for. `write_debug_dump_on_error`
/// is the concrete test surface for that fix, mirroring
/// `fail_closed_debug_dump_tests` exactly: every value written here goes through
/// `RedactedText::new` at the point of printing (see that type's doc comment), so
/// feeding a synthetic credential into any parameter and asserting it never reaches
/// the buffer is a direct test of the structural guarantee, not just of `redact()` in
/// isolation.
#[cfg(test)]
mod debug_dump_on_error_tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn dump(
        err: &str,
        user: &str,
        cwd: Option<&str>,
        podman_stdout: Option<&str>,
        podman_stderr: Option<&str>,
        podman_unrunnable_err: Option<&str>,
        ps_lines: &[String],
    ) -> String {
        let podman = PodmanDumpSnapshot {
            stdout: podman_stdout.map(str::to_string),
            stderr: podman_stderr.map(str::to_string),
            unrunnable_err: podman_unrunnable_err.map(str::to_string),
            ps_lines: ps_lines.to_vec(),
        };
        let mut buf: Vec<u8> = Vec::new();
        write_debug_dump_on_error(&mut buf, err, user, false, cwd, &podman).unwrap();
        String::from_utf8(buf).expect("dump is valid UTF-8")
    }

    fn dump_err_only(err: &str) -> String {
        dump(err, "someone", None, None, None, None, &[])
    }

    /// The auditor's exact live repro: an AWS-shaped secret (not covered by the old
    /// `redact()` blocklist's 8 fixed prefixes) embedded in `err` must not reach the
    /// dump, since `err` is now routed through `RedactedText::new` unconditionally —
    /// no longer dependent on the caller (`main.rs`) having pre-redacted it.
    #[test]
    fn aws_shaped_secret_in_err_is_redacted_live_repro() {
        let synthetic = "AKIAIOSFODNN7EXAMPLE";
        let err = format!("registry auth failed: x-amz-access-key={synthetic} rejected");
        let out = dump_err_only(&err);
        assert!(!out.contains(synthetic), "credential leaked in dump: {out}");
        assert!(
            out.contains("registry auth failed:") && out.contains("rejected"),
            "diagnostic detail must survive: {out}"
        );
    }

    /// Same finding, but proven even when the caller does NOT pre-redact at all
    /// (main.rs still does, as defense in depth, but this function must not depend
    /// on that) — a GitHub token this time, to also cover the shape the old blocklist
    /// nominally did know about, just to confirm no regression there either.
    #[test]
    fn github_token_in_err_is_redacted_even_without_caller_pre_redaction() {
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let err = format!("clone failed: authentication error token={synthetic}");
        let out = dump_err_only(&err);
        assert!(
            !out.contains(&synthetic),
            "credential leaked in dump: {out}"
        );
    }

    /// A digit-free secret (MEDIUM-A shape) in `err` — proves this routes through the
    /// same fully-fixed `redact_free_text`, not some earlier, weaker snapshot of it.
    #[test]
    fn digit_free_secret_in_err_is_redacted() {
        let secret = "QzXpLwVnTbYhSfJdMcUaEzXkNbPqRsTvWyAeGh";
        let err = format!("auth error: token {secret} rejected by upstream");
        let out = dump_err_only(&err);
        assert!(!out.contains(secret), "credential leaked in dump: {out}");
        assert!(out.contains("auth error: token"));
        assert!(out.contains("rejected by upstream"));
    }

    /// requirement 3's exact sentence must survive byte-for-byte as the `error:` line.
    #[test]
    fn ordinary_diagnostic_err_survives_verbatim() {
        let err = "podman top failed: exit 125, cannot chdir to /home/kang";
        let out = dump_err_only(err);
        assert!(
            out.contains(&format!("error:      {err}")),
            "error line must survive intact: {out}"
        );
    }

    /// `user` ($USER) is now also wrapped, closing the same class of gap for the one
    /// other raw environment-derived field this function prints outside the
    /// allowlisted `dump_resolved_env` path.
    #[test]
    fn synthetic_credential_in_user_is_redacted() {
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let out = dump("boom", &synthetic, None, None, None, None, &[]);
        assert!(
            !out.contains(&synthetic),
            "credential leaked via user field: {out}"
        );
    }

    /// `pwd` ($PWD via `current_dir()`) likewise.
    #[test]
    fn synthetic_credential_in_pwd_is_redacted() {
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let cwd = format!("/home/{synthetic}");
        let out = dump("boom", "someone", Some(&cwd), None, None, None, &[]);
        assert!(
            !out.contains(&synthetic),
            "credential leaked via pwd field: {out}"
        );
    }

    /// Ordinary paths must still survive verbatim — the point is redacting
    /// credentials, not eating every `pwd` line.
    #[test]
    fn ordinary_pwd_survives_verbatim() {
        let cwd = "/home/gha-agent/gha-runner-ctl";
        let out = dump("boom", "gha-agent", Some(cwd), None, None, None, &[]);
        assert!(
            out.contains(&format!("pwd:        {cwd}")),
            "pwd line must survive intact: {out}"
        );
    }

    /// `podman info`'s stdout/stderr streams — same shape as `debug_dump_fail_closed`'s
    /// resolved-inputs guarantee, proven here directly for this dump's own podman
    /// snapshot fields.
    #[test]
    fn synthetic_credential_in_podman_stdout_and_stderr_is_redacted() {
        let synthetic_out = "AKIAIOSFODNN7EXAMPLE";
        let synthetic_err = format!("gho_{}", "e5F6g7H8".repeat(5));
        let out = dump(
            "boom",
            "someone",
            None,
            Some(&format!("rootless=true key={synthetic_out}")),
            Some(&format!("warning token={synthetic_err}")),
            None,
            &[],
        );
        assert!(!out.contains(synthetic_out), "leaked via stdout: {out}");
        assert!(!out.contains(&synthetic_err), "leaked via stderr: {out}");
    }

    /// The `podman: not runnable (<io error>)` branch — an `io::Error`'s `Display`
    /// text is normally just "No such file or directory", but nothing stops a
    /// platform/locale from putting arbitrary text there, so it goes through the same
    /// wrapper as everything else rather than being assumed safe as a special case.
    #[test]
    fn synthetic_credential_in_podman_unrunnable_error_is_redacted() {
        let synthetic = "AKIAIOSFODNN7EXAMPLE";
        let out = dump(
            "boom",
            "someone",
            None,
            None,
            None,
            Some(&format!("exec failed: {synthetic}")),
            &[],
        );
        assert!(!out.contains(synthetic), "credential leaked: {out}");
    }

    /// `podman ps -a` lines (container names / status / image) — a container name is
    /// caller/operator controlled and could in principle be set to anything.
    #[test]
    fn synthetic_credential_in_podman_ps_line_is_redacted() {
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let lines = vec![format!(
            "worker-{synthetic}\tUp 2 minutes\tlocalhost/img:latest"
        )];
        let out = dump("boom", "someone", None, None, None, None, &lines);
        assert!(
            !out.contains(&synthetic),
            "credential leaked via ps line: {out}"
        );
    }

    /// Ordinary `podman ps -a` output must survive verbatim.
    #[test]
    fn ordinary_podman_ps_line_survives_verbatim() {
        let lines = vec!["gha-runner-ctl\tUp 3 hours\tlocalhost/gha-runner-ctl:latest".to_string()];
        let out = dump("boom", "someone", None, None, None, None, &lines);
        assert!(
            out.contains("gha-runner-ctl\tUp 3 hours\tlocalhost/gha-runner-ctl:latest"),
            "ps line must survive intact: {out}"
        );
    }
}

/// Issue #132 third follow-up audit, requirement 4: make the "every dump emission
/// point is redacted" invariant testable, not just provable by inspection.
///
/// A full compiler-enforced guarantee isn't expressible in safe Rust here — nothing
/// stops a future contributor from adding an entirely new sibling dump function with
/// its own fresh `eprintln!`, exactly as `debug_dump_on_error` itself did relative to
/// `debug_dump_fail_closed` (that is precisely how the round-3 finding happened: the
/// type-level fix already applied to one sibling did nothing for the other, because
/// the other's `eprintln!`s were never routed through it in the first place). What
/// *is* expressible, and is checked here, is a source-level regression guard over the
/// two known dump-writer functions: every `{}`/`{:}`-style interpolation in their
/// `write!`/`writeln!` calls must reference either a string literal, a
/// known-safe/typed identifier (`RedactedText`/`RedactedCommand`-derived, or a
/// `redact_for_dump`/`redact_env_dump`-derived `field.key`/`field.value`), or a
/// non-textual value (`bool`, an integer, a getter that returns one). This fails loud
/// if a future edit adds a new interpolated argument to either function that isn't in
/// the allowlist below — which is exactly the shape the round-3 bug would have had if
/// it had been introduced as an edit to an already-covered function instead of via a
/// brand new sibling.
///
/// **What this test does NOT catch** (and what a reviewer must check by hand for any
/// change touching the dump/fail-closed subsystem):
/// - A brand-new dump function added elsewhere in the file with its own `eprintln!`s,
///   never routed through `write_debug_dump_on_error` / `write_debug_dump_fail_closed`
///   at all (the actual round-3 shape). Grep for `eprintln!`/`println!` additions in
///   `src/lib.rs`/`src/fail_closed.rs` and confirm any new one either goes through one
///   of these two writer functions or is manifestly static/numeric text.
/// - A parameter that's typed `RedactedText`/`RedactedCommand` but constructed from a
///   value that was already lossily pre-processed in a way that defeats
///   `redact_free_text` (there is no way to statically prove "this string arrived here
///   without ever being concatenated from untrusted parts before redaction" — that's a
///   review judgment call, not a type-level one).
#[cfg(test)]
mod dump_writer_source_invariant_tests {
    use std::fs;

    /// Expressions allowed to appear as a captured/interpolated value in a
    /// `write!`/`writeln!` call in the two audited functions — as either an inline
    /// `{ident}` capture inside the format string, or a positional argument listed
    /// after it. Deliberately conservative: anything not obviously safe fails the
    /// test rather than being guessed at. `RedactedText::new(...)` is matched by
    /// prefix (any argument) rather than listed by exact call, since the constructor
    /// itself — not its argument — is the guarantee.
    const ALLOWLISTED_EXACT: &[&str] = &[
        "command", // already-typed &RedactedCommand parameter (Display)
        // FailClosedEvent getters — redacted at construction (FailClosedEvent::redacted).
        "ev.check()",
        "ev.object()",
        "ev.assumed()",
        "ev.reason()",
        "ev.consecutive",
        "ev.since",
        // redact_for_dump / redact_env_dump output — allowlist + shape checked.
        "field.key",
        "field.value",
        // Plain booleans — never free text.
        "euid_root",
    ];

    fn is_allowlisted(expr: &str) -> bool {
        let expr = expr.trim();
        ALLOWLISTED_EXACT.contains(&expr)
            || (expr.starts_with("RedactedText::new(") && expr.ends_with(')'))
    }

    /// Pull the body of a top-level `fn <name>` out of `src/lib.rs` by brace
    /// counting. Good enough for this file's style (no other `fn <name>` substring
    /// collision for either audited name, checked by the sanity-count assertion in
    /// [`assert_all_args_allowlisted`]).
    fn extract_fn_body(source: &str, fn_name: &str) -> String {
        let sig = format!("fn {fn_name}");
        let start = source
            .find(&sig)
            .unwrap_or_else(|| panic!("function {fn_name} not found in source"));
        let brace_start = source[start..]
            .find('{')
            .map(|i| start + i)
            .unwrap_or_else(|| panic!("no opening brace found for {fn_name}"));
        let mut depth = 0i32;
        for (offset, ch) in source[brace_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return source[brace_start..=brace_start + offset].to_string();
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced braces extracting {fn_name}");
    }

    /// Split `s` on top-level commas — i.e. commas not nested inside `()`/`[]`/`{}`
    /// and not inside a `"..."` string literal (with `\`-escape awareness). Used to
    /// pull a macro call's comma-separated argument list apart without a real Rust
    /// parser.
    fn split_top_level_commas(s: &str) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut parts = Vec::new();
        let mut depth = 0i32;
        let mut in_string = false;
        let mut start = 0usize;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            if in_string {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    in_string = false;
                }
            } else {
                match c {
                    '"' => in_string = true,
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    ',' if depth == 0 => {
                        parts.push(chars[start..i].iter().collect::<String>());
                        start = i + 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        let tail: String = chars[start..].iter().collect();
        if !tail.trim().is_empty() {
            parts.push(tail);
        }
        parts.into_iter().map(|p| p.trim().to_string()).collect()
    }

    /// Inline `{ident}` captures inside a format-string literal's *contents*
    /// (quotes already stripped). Rust's inline-capture syntax only accepts bare
    /// identifiers/simple field access, never a call expression like
    /// `RedactedText::new(x)` — those must be (and in this codebase are) passed as
    /// positional trailing arguments instead, which [`macro_call_captures`] handles
    /// separately. `{{`/`}}` escapes and `{ident:spec}` format specs are handled.
    fn inline_captures(fmt_contents: &str) -> Vec<String> {
        let chars: Vec<char> = fmt_contents.chars().collect();
        let mut caps = Vec::new();
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    i += 2;
                    continue;
                }
                let start = i + 1;
                if let Some(rel_end) = chars[start..].iter().position(|&c| c == '}') {
                    let end = start + rel_end;
                    let raw: String = chars[start..end].iter().collect();
                    let ident = raw.split(':').next().unwrap_or(&raw).trim();
                    if !ident.is_empty() {
                        caps.push(ident.to_string());
                    }
                    i = end + 1;
                    continue;
                }
            }
            i += 1;
        }
        caps
    }

    /// Every `write!(`/`writeln!(` call in `body`, as (writer-arg, format-string
    /// literal-with-quotes, positional-args) — used by
    /// [`macro_call_captures`] to pull out everything that gets interpolated.
    fn find_macro_calls(body: &str) -> Vec<Vec<String>> {
        let mut calls = Vec::new();
        let chars: Vec<char> = body.chars().collect();
        let mut idx = 0usize;
        while idx < chars.len() {
            let rest: String = chars[idx..].iter().collect();
            let next_writeln = rest.find("writeln!(");
            let next_write = rest.find("write!(");
            let rel = match (next_writeln, next_write) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), None) => a,
                (None, Some(b)) => b,
                (None, None) => break,
            };
            let call_start = idx + rel;
            let paren_open = chars[call_start..]
                .iter()
                .position(|&c| c == '(')
                .map(|i| call_start + i)
                .expect("macro call must have an opening paren");
            let mut depth = 0i32;
            let mut in_string = false;
            let mut i = paren_open;
            let mut close = None;
            while i < chars.len() {
                let c = chars[i];
                if in_string {
                    if c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        '"' => in_string = true,
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                close = Some(i);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            let close = close.expect("unbalanced parens in macro call");
            let args_str: String = chars[paren_open + 1..close].iter().collect();
            calls.push(split_top_level_commas(&args_str));
            idx = close + 1;
        }
        calls
    }

    /// Every value captured/interpolated by any `write!`/`writeln!` call in `body`:
    /// inline `{ident}` captures from the format-string literal, plus every
    /// positional argument listed after it. (The macro's own first argument — the
    /// writer, e.g. `w` — is never itself interpolated, so it's skipped.)
    fn macro_call_captures(body: &str) -> Vec<String> {
        let mut all = Vec::new();
        for parts in find_macro_calls(body) {
            // parts[0] = writer, parts[1] = format string literal, parts[2..] = positional args.
            if parts.len() < 2 {
                continue;
            }
            let fmt_lit = parts[1].trim();
            let contents = fmt_lit
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(fmt_lit);
            all.extend(inline_captures(contents));
            all.extend(parts[2..].iter().cloned());
        }
        all
    }

    fn assert_all_args_allowlisted(fn_name: &str, source: &str) {
        let body = extract_fn_body(source, fn_name);
        let captures = macro_call_captures(&body);
        assert!(
            captures.len() >= 4,
            "sanity check failed: found suspiciously few captured values ({}) in {fn_name} — \
             the extraction logic itself may be broken, not the function",
            captures.len()
        );
        for expr in &captures {
            assert!(
                is_allowlisted(expr),
                "{fn_name} interpolates `{expr}`, which is not in ALLOWLISTED_EXACT and is not \
                 a RedactedText::new(...) call. If this is a genuinely pre-redacted/typed/\
                 non-textual value, add it to the allowlist explicitly (with a comment saying \
                 why it's safe). If it's a raw caller-supplied string, route it through \
                 RedactedText::new(...) first — this is exactly the class of bug issue #132's \
                 third follow-up audit closed."
            );
        }
    }

    #[test]
    fn write_debug_dump_fail_closed_only_interpolates_allowlisted_values() {
        let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("read src/lib.rs");
        assert_all_args_allowlisted("write_debug_dump_fail_closed", &source);
    }

    #[test]
    fn write_debug_dump_on_error_only_interpolates_allowlisted_values() {
        let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("read src/lib.rs");
        assert_all_args_allowlisted("write_debug_dump_on_error", &source);
    }

    /// Positive control: confirm the scanner actually flags a raw, unwrapped
    /// interpolation when one is present — otherwise a scanner that (via a bug)
    /// always passes would make the two tests above worthless. Uses a synthetic
    /// snippet shaped exactly like the round-3 bug (`eprintln!("error: {err}")`
    /// wrapped in a `write!` call for this synthetic case), not real source.
    #[test]
    fn scanner_rejects_a_raw_unwrapped_interpolation() {
        let synthetic_body = r#"{
    writeln!(w, "error:      {}", err)?;
    Ok(())
}"#;
        let captures = macro_call_captures(synthetic_body);
        assert_eq!(captures, vec!["err".to_string()]);
        assert!(
            !is_allowlisted("err"),
            "scanner must reject a bare raw identifier — if this now passes, the \
             allowlist/positive-control logic itself is broken"
        );
    }
}

/// Bounded-retain marker tests (`GHA_RETAIN_MAX_AGE_SECS` / `GHA_RETAIN_MAX_JOBS`).
/// Each test uses a distinct `--container` name so the on-disk marker path
/// (derived from container + username, see [`retain_marker_path`]) cannot
/// collide with another test running in the same process.
#[cfg(test)]
mod retain_marker_tests {
    use super::*;

    fn retain_test_cli(container: &str) -> Cli {
        Cli::try_parse_from([
            "gha-runner-ctl",
            "--repo",
            "tzervas/retain-marker-test",
            "--container",
            container,
        ])
        .unwrap()
    }

    #[test]
    fn retain_marker_fresh_within_bounds_is_reusable() {
        let cli = retain_test_cli("retain-fresh-test");
        let marker_path = retain_marker_path(&cli);
        let _ = fs::remove_file(&marker_path);
        mark_retain_ok(&cli, false);
        assert!(
            volume_has_runner_config(&cli),
            "freshly marked retain target should be reusable"
        );
        let _ = fs::remove_file(&marker_path);
    }

    #[test]
    fn retain_marker_expires_on_age() {
        let cli = retain_test_cli("retain-age-test");
        let marker_path = retain_marker_path(&cli);
        let stale = RetainMarker {
            url: github_url(&cli),
            created_unix: now_unix().saturating_sub(DEFAULT_RETAIN_MAX_AGE_SECS + 60),
            reuse_count: 0,
        };
        fs::write(&marker_path, serde_json::to_string(&stale).unwrap()).unwrap();
        assert!(
            !volume_has_runner_config(&cli),
            "marker older than GHA_RETAIN_MAX_AGE_SECS must not be reusable"
        );
        let _ = fs::remove_file(&marker_path);
    }

    #[test]
    fn retain_marker_expires_on_job_count() {
        let cli = retain_test_cli("retain-jobs-test");
        let marker_path = retain_marker_path(&cli);
        let maxed = RetainMarker {
            url: github_url(&cli),
            created_unix: now_unix(),
            reuse_count: DEFAULT_RETAIN_MAX_JOBS,
        };
        fs::write(&marker_path, serde_json::to_string(&maxed).unwrap()).unwrap();
        assert!(
            !volume_has_runner_config(&cli),
            "marker at/above GHA_RETAIN_MAX_JOBS must not be reusable"
        );
        let _ = fs::remove_file(&marker_path);
    }

    /// Pre-bounded-retain markers were a bare repo URL with no recorded age.
    /// Unknown age must resolve to "not reusable" — the safe direction — rather
    /// than being assumed fresh.
    #[test]
    fn retain_marker_old_url_only_format_is_not_reusable() {
        let cli = retain_test_cli("retain-legacy-test");
        let marker_path = retain_marker_path(&cli);
        fs::write(&marker_path, github_url(&cli)).unwrap();
        assert!(
            !volume_has_runner_config(&cli),
            "URL-only legacy marker (unknown age) must not be reusable"
        );
        let _ = fs::remove_file(&marker_path);
    }

    #[test]
    fn retain_marker_wrong_repo_is_not_reusable() {
        let cli = retain_test_cli("retain-wrong-repo-test");
        let marker_path = retain_marker_path(&cli);
        let other = RetainMarker {
            url: "https://github.com/tzervas/some-other-repo".into(),
            created_unix: now_unix(),
            reuse_count: 0,
        };
        fs::write(&marker_path, serde_json::to_string(&other).unwrap()).unwrap();
        assert!(
            !volume_has_runner_config(&cli),
            "marker for a different repo must not be reusable"
        );
        let _ = fs::remove_file(&marker_path);
    }

    #[test]
    fn mark_retain_ok_reuse_preserves_created_and_bumps_count() {
        let cli = retain_test_cli("retain-reuse-test");
        let marker_path = retain_marker_path(&cli);
        let _ = fs::remove_file(&marker_path);

        mark_retain_ok(&cli, false);
        let first = read_retain_marker(&cli).expect("marker written on fresh registration");
        assert_eq!(first.reuse_count, 0);

        mark_retain_ok(&cli, true);
        let second = read_retain_marker(&cli).expect("marker written on reuse");
        assert_eq!(
            second.created_unix, first.created_unix,
            "reuse must preserve the original creation time"
        );
        assert_eq!(second.reuse_count, 1, "reuse must bump the reuse counter");

        let _ = fs::remove_file(&marker_path);
    }
}

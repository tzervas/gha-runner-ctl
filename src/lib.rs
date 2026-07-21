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

mod pool;

pub use pool::{
    fit_to_budget, format_cpus, format_memory_mib, parse_cpus_f64, parse_memory_mib,
    resources_for_tier, size_for_job, ResourcePool, SizeTier,
};

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_IMAGE: &str = "localhost/gha-runner-ctl:latest";
const DEFAULT_CONTAINER: &str = "gha-runner-ctl";
const DEFAULT_VOLUME: &str = "gha-runner-ctl-data";
const DEFAULT_LABELS: &str = "self-hosted,linux,x64,podman";
const DEFAULT_NAME: &str = "shared-podman-1";
const UA: &str = "gha-runner-ctl/0.2.8";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
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
/// Floor for user-batch demand interval (seconds).
const USER_BATCH_MIN_INTERVAL_SECS: u64 = 120;
/// Default: check this many allowlisted repos per tick (round-robin stagger).
/// 0 = all allowlisted repos each tick (still paced by min-gap).
const DEFAULT_REPOS_PER_TICK: u32 = 1;
/// Min seconds between registration-token POSTs (shared across processes on host).
const DEFAULT_REG_MIN_GAP_SECS: u64 = 5;
/// Max registration-token POSTs per rolling hour (shared host budget).
const DEFAULT_REG_MAX_PER_HOUR: u32 = 90;

#[derive(Debug, Clone, ValueEnum)]
pub enum Mode {
    Ephemeral,
    Retain,
}

#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum Scope {
    /// One repository. Use with --repo or --auto.
    Repo,
    /// Organization registration (repos must live in that org).
    Org,
    /// Batch all personal (owner) repos under a user login; re-register per demand.
    User,
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

    #[arg(long, env = "GHA_IMAGE", default_value = DEFAULT_IMAGE, global = true)]
    image: String,

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
    #[arg(long, env = "GHA_PREFER_REPOS", global = true)]
    prefer_repos: Option<String>,

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
}

#[derive(Debug, Subcommand, Clone)]
pub enum Cmd {
    /// Build image + seed volume snapshot (updates host packages first unless skipped)
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
}

/// Dump troubleshooting context after a failure (no secrets).
///
/// Enabled when `GHA_DEBUG=1` or `GHA_DEBUG_ON_ERR` is unset/`1` (default on).
/// Disable with `GHA_DEBUG_ON_ERR=0` once the stack is stable.
pub fn debug_dump_on_error(err: &str) {
    let always = env_truthy("GHA_DEBUG");
    let on_err = match std::env::var("GHA_DEBUG_ON_ERR") {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "YES" | ""),
        // Default ON while stabilizing the fleet agent / rootless path.
        Err(_) => true,
    };
    if !always && !on_err {
        return;
    }
    eprintln!("========== gha-runner-ctl DEBUG ON ERROR ==========");
    eprintln!("error:      {err}");
    eprintln!(
        "user:       {} euid_root={}",
        std::env::var("USER").unwrap_or_else(|_| "?".into()),
        effective_uid_is_root()
    );
    if let Ok(cwd) = std::env::current_dir() {
        eprintln!("pwd:        {}", cwd.display());
    }
    for key in [
        "HOME",
        "XDG_RUNTIME_DIR",
        "CONTAINER_HOST",
        "GHA_ALLOW_ROOT",
        "GHA_SCOPE",
        "GHA_USER",
        "GHA_REPO",
        "GHA_PREFER_REPOS",
        "GHA_MODE",
        "GHA_CONTAINER",
        "GHA_VOLUME",
        "GHA_IMAGE",
        "GHA_GPU",
    ] {
        if let Ok(v) = std::env::var(key) {
            // Never dump tokens.
            if key.contains("TOKEN") || key.contains("SECRET") {
                continue;
            }
            eprintln!("{key}={v}");
        }
    }
    // Best-effort podman snapshot (stdout/stderr redacted).
    match Command::new("podman")
        .args([
            "info",
            "--format",
            "rootless={{.Host.Security.Rootless}} runtime={{.Host.OCIRuntime.Name}}",
        ])
        .output()
    {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            let e = String::from_utf8_lossy(&o.stderr);
            if !s.trim().is_empty() {
                eprintln!("podman:     {}", redact(s.trim()));
            }
            if !o.status.success() && !e.trim().is_empty() {
                eprintln!("podman_err: {}", redact(e.trim()));
            }
        }
        Err(e) => eprintln!("podman:     not runnable ({e})"),
    }
    if let Ok(o) = Command::new("podman")
        .args([
            "ps",
            "-a",
            "--format",
            "{{.Names}}\t{{.Status}}\t{{.Image}}",
        ])
        .output()
    {
        let s = String::from_utf8_lossy(&o.stdout);
        for (i, line) in s.lines().take(15).enumerate() {
            if i == 0 {
                eprintln!("--- podman ps -a (max 15) ---");
            }
            eprintln!("{}", redact(line));
        }
    }
    eprintln!("hint:       GHA_DEBUG=1 for more; GHA_DEBUG_ON_ERR=0 to silence");
    eprintln!("===================================================");
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
    }
}

pub fn run() -> Result<(), String> {
    let mut cli = Cli::parse();
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
    }
}

// --- Resolve auto / batch context --------------------------------------------

fn get_user_login_from_token(token: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct UserResponse {
        login: String,
    }

    let resp = http_agent()
        .get("https://api.github.com/user")
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
        } else if let Ok(tok) = github_token() {
            get_user_login_from_token(&tok)?
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

pub fn is_safe_image(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
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

pub fn redact(s: &str) -> String {
    let mut out = s.to_string();
    for key in [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "Bearer ",
        "RUNNER_TOKEN=",
    ] {
        let mut start_search_idx = 0;
        while start_search_idx < out.len() {
            if let Some(offset) = out[start_search_idx..].find(key) {
                let i = start_search_idx + offset;
                let rest_idx = i + key.len();
                let rest = &out[rest_idx..];

                let mut chars_taken = 0;
                let mut secret_len_bytes = 0;
                for (idx, c) in rest.char_indices() {
                    if chars_taken >= 200 {
                        break;
                    }
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                        chars_taken += 1;
                        secret_len_bytes = idx + c.len_utf8();
                    } else {
                        break;
                    }
                }

                let replacement = format!("{key}***REDACTED***");
                out.replace_range(i..(rest_idx + secret_len_bytes), &replacement);
                start_search_idx = i + replacement.len();
            } else {
                break;
            }
        }
    }
    if out.len() > 400 {
        let mut truncate_at = 400;
        while truncate_at > 0 && !out.is_char_boundary(truncate_at) {
            truncate_at -= 1;
        }
        out = format!("{}…", &out[..truncate_at]);
    }
    out
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
                // `warm` uses prefer_repos only (no single --repo).
                if cli
                    .prefer_repos
                    .as_ref()
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
                    .prefer_repos
                    .as_ref()
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

fn registration_api_for_repo(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/actions/runners/registration-token")
}

fn registration_api(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo | Scope::User => {
            registration_api_for_repo(cli.repo.as_ref().expect("validated"))
        }
        Scope::Org => format!(
            "https://api.github.com/orgs/{}/actions/runners/registration-token",
            cli.owner.as_ref().expect("validated")
        ),
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

fn lock_is_stale(path: &Path) -> bool {
    let Ok(s) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(pid) = s.trim().parse::<u32>() else {
        return true;
    };
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

    let resp = http_agent()
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

fn github_token() -> Result<String, String> {
    // 1. Try env variables
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(key) {
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }

    // 2. Try GCM or git credential helper
    if let Some(t) = get_token_from_git_credential() {
        return Ok(t);
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
                return Ok(t);
            }
        }
    }

    // 4. Try Config file
    if let Some(cfg) = load_config() {
        if let Some(t) = cfg.github_token {
            if !t.is_empty() {
                return Ok(t);
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
            return Ok(t);
        }
    }

    Err("No GitHub token or PAT found. Please authenticate via 'gh auth login', set GH_TOKEN environment variable, install Git Credential Manager, or enter it interactively.".into())
}

#[derive(Deserialize)]
struct RegistrationTokenResponse {
    token: String,
}

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(HTTP_TIMEOUT)
        .timeout_read(HTTP_TIMEOUT)
        .timeout_write(HTTP_TIMEOUT)
        .user_agent(UA)
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
}

impl ApiPacer {
    fn from_cli(cli: &Cli) -> Self {
        let gap_ms = cli.api_min_gap_ms.clamp(50, 60_000);
        let max_per = cli.api_max_per_poll.clamp(2, 500);
        let backoff = Duration::from_secs(cli.api_backoff_secs.clamp(5, MAX_API_BACKOFF_SECS));
        Self {
            min_gap: Duration::from_millis(gap_ms),
            max_per_poll: max_per,
            calls_this_poll: 0,
            last_call: None,
            backoff,
            max_backoff: Duration::from_secs(MAX_API_BACKOFF_SECS),
            cool_until: None,
        }
    }

    fn begin_poll(&mut self) {
        self.calls_this_poll = 0;
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
        let result = http_agent()
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
                    return Err(format!("status code {status}"));
                }
                if status == 401 || status == 404 {
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
                Err(format!("status code {code}"))
            }
            Err(e) => Err(redact(&e.to_string())),
        }
    }
}

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
                let mut state: RegPaceState = fs::read_to_string(&state_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let now = now_unix();
                state.recent.retain(|t| now.saturating_sub(*t) < 3600);
                if state.recent.len() as u32 >= max_hour {
                    let _ = fs::remove_file(&exclusive);
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
                        let _ = fs::remove_file(&exclusive);
                        eprintln!("register: pacing {wait}s before next registration-token POST");
                        thread::sleep(Duration::from_secs(wait));
                        continue;
                    }
                }
                // Budget enforced here; slot committed only after successful token mint.
                let _ = fs::remove_file(&exclusive);
                return Ok(());
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(200 + (attempt as u64 % 5) * 100));
            }
        }
    }
    Err("register: could not acquire registration pace lock".into())
}

/// Record a successful registration-token mint in the host-wide hourly budget.
fn commit_registration_slot() {
    let (lock_path, state_path) = reg_pace_paths();
    let exclusive = lock_path.with_extension("exclusive");
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&exclusive)
        .is_ok()
    {
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
        let _ = fs::remove_file(&exclusive);
    }
}

fn note_registration_failure_backoff(secs: u64) {
    let (lock_path, state_path) = reg_pace_paths();
    let exclusive = lock_path.with_extension("exclusive");
    if OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&exclusive)
        .is_ok()
    {
        let mut state: RegPaceState = fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Push last_unix forward to force min gap = backoff.
        state.last_unix = now_unix().saturating_add(secs.saturating_sub(1));
        if let Ok(s) = serde_json::to_string(&state) {
            let _ = fs::write(&state_path, s);
        }
        let _ = fs::remove_file(&exclusive);
    }
    eprintln!("register: backing off {secs}s after failed registration-token POST");
    thread::sleep(Duration::from_secs(secs));
}

fn registration_token(cli: &Cli, api_token: &str) -> Result<String, String> {
    pace_registration(cli)?;
    let url = registration_api(cli);
    let resp = http_agent()
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
        return Err(format!(
            "podman {} failed: {}",
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

fn container_exists(name: &str) -> bool {
    podman_ok(&["container", "exists", name])
}

fn volume_exists(name: &str) -> bool {
    podman_ok(&["volume", "exists", name])
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
    let repos: Vec<String> = if let Some(pref) = cli.prefer_repos.as_ref() {
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
            let api = github_token()?;
            match registration_token(&unit, &api) {
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

    // Drop stale image so `podman build` cannot silently reuse an old rootfs layer
    // when only host-side packages changed (still uses cache for unchanged layers).
    let _ = podman(&["image", "exists", &cli.image]);

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

    if !volume_exists(&cli.volume) {
        eprintln!("prepare: creating volume {}", cli.volume);
        podman(&["volume", "create", &cli.volume])?;
    }

    eprintln!("prepare: seeding volume snapshot…");
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
        r"
set -euo pipefail
if [[ ! -x /opt/actions-runner/run.sh ]]; then
  cp -a /opt/actions-runner-seed/. /opt/actions-runner/
fi
# Match image non-root user (UID 1001)
chown -R 1001:1001 /opt/actions-runner 2>/dev/null || true
chmod -R go-w /opt/actions-runner 2>/dev/null || true
date -u +%Y-%m-%dT%H:%M:%SZ > /opt/actions-runner/.snapshot-baseline
chown 1001:1001 /opt/actions-runner/.snapshot-baseline 2>/dev/null || true
echo ok
",
    ])?;

    if with_container {
        eprintln!(
            "prepare: snapshot ready (cpus={} memory={})",
            cli.cpus, cli.memory
        );
    }
    eprintln!("prepare: done");
    Ok(())
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

fn volume_has_runner_config(cli: &Cli) -> bool {
    // Heuristic: volume was used before; entrypoint will detect .runner.
    // We cannot inspect volume contents without a container; prefer retain path
    // and let entrypoint skip config when .runner exists.
    // Marker file on host tracks last successful retain target.
    let user_suffix = current_username();
    let marker = reg_pace_paths()
        .0
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join(format!(
            "gha-runner-ctl-retain-{}-{}.ok",
            cli.container, user_suffix
        ));
    if !marker.is_file() {
        return false;
    }
    let Ok(s) = fs::read_to_string(&marker) else {
        return false;
    };
    s.trim() == github_url(cli)
}

fn mark_retain_ok(cli: &Cli) {
    let user_suffix = current_username();
    let marker = reg_pace_paths()
        .0
        .parent()
        .unwrap_or(Path::new("/tmp"))
        .join(format!(
            "gha-runner-ctl-retain-{}-{}.ok",
            cli.container, user_suffix
        ));
    let _ = fs::write(&marker, github_url(cli));
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
        let api = github_token()?;
        let reg = registration_token(cli, &api)?;
        write_env_file(&env_path, &reg, cli)?;
        drop(reg);
        drop(api);
    }

    if container_exists(&cli.container) {
        let _ = podman(&["rm", "-f", &cli.container]);
    }

    eprintln!(
        "up: scope={:?} mode={:?} ephemeral={ephemeral} url={}",
        cli.scope,
        cli.mode,
        github_url(cli)
    );
    let env_path_str = env_path.to_str().ok_or("env path not utf-8")?.to_string();
    let vol = format!("{}:/opt/actions-runner:Z", cli.volume);
    let eph = if ephemeral { "true" } else { "false" };
    let ret = if ephemeral { "false" } else { "true" };
    let eph_kv = format!("RUNNER_EPHEMERAL={eph}");
    let ret_kv = format!("RUNNER_RETAIN={ret}");

    let mut args: Vec<&str> = vec![
        "run",
        "-d",
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
        "never",
        "--user",
        "1001:1001",
        // Work endpoints never receive a container runtime socket (no nested spawn).
        "--env-file",
        env_path_str.as_str(),
        "-e",
        eph_kv.as_str(),
        "-e",
        ret_kv.as_str(),
        "-v",
        vol.as_str(),
    ];
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
        mark_retain_ok(cli);
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
        let _ = podman(&[
            "run",
            "--rm",
            "--security-opt",
            "no-new-privileges",
            "--pull",
            "never",
            "--entrypoint",
            "/bin/bash",
            "-v",
            vol.as_str(),
            cli.image.as_str(),
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
    for name in select_repos_for_tick(cli, repos) {
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
            if let Some(pref) = cli.prefer_repos.as_ref() {
                let repos: Vec<String> = pref
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty() && is_safe_repo(x))
                    .map(|s| s.to_string())
                    .collect();
                return poll_allowlist_repos(cli, api, pacer, &repos);
            }
            Err("repo scope: missing --repo or --prefer-repos".into())
        }
        Scope::Org => {
            let owner = cli.owner.as_ref().expect("validated");
            let url = format!("https://api.github.com/orgs/{owner}/repos?per_page=100&type=all");
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
            // Allowlist mode: when prefer_repos is set, ONLY poll those repos.
            if let Some(pref) = cli.prefer_repos.as_ref() {
                let repos: Vec<String> = pref
                    .split(',')
                    .map(|x| x.trim())
                    .filter(|x| !x.is_empty() && is_safe_repo(x))
                    .filter(|name| name.starts_with(&format!("{user}/")))
                    .map(|s| s.to_string())
                    .collect();
                return poll_allowlist_repos(cli, api, pacer, &repos);
            }
            // Full owner list — paced + budget-capped; prefer setting GHA_PREFER_REPOS.
            eprintln!(
                "listen: user-batch without GHA_PREFER_REPOS scans owned repos (budget {} GETs/poll, gap {}ms)",
                pacer.max_per_poll,
                pacer.min_gap.as_millis()
            );
            let url = format!(
                "https://api.github.com/users/{user}/repos?type=owner&per_page=100&sort=updated"
            );
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
        let url =
            format!("https://api.github.com/repos/{repo}/actions/runs?status={status}&per_page=5");
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
    let url = format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs");
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
    let repos: Vec<String> = if let Some(pref) = cli.prefer_repos.as_ref() {
        pref.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| is_safe_repo(s))
            .collect()
    } else if let Some(r) = cli.repo.as_ref() {
        vec![r.clone()]
    } else {
        return Ok(out);
    };
    let tick = select_repos_for_tick(cli, &repos);
    // When pool-scaling, scan a modest number of repos per tick (staggered).
    // Keep this small so budget remains for job detail GETs + registration POSTs.
    let scan = if pool_mode_on(cli) {
        // Always stagger via round-robin; never pin to allowlist head (starves tail repos).
        let mut cli_scan = cli.clone_for_listen();
        if cli.repos_per_tick == 0 {
            cli_scan.repos_per_tick = 6;
        } else {
            cli_scan.repos_per_tick = cli.repos_per_tick.min(6);
        }
        select_repos_for_tick(&cli_scan, &repos)
    } else {
        tick
    };

    // Prefer queued runs; also sample in_progress (multi-job matrices can still have
    // queued jobs while the run is overall in_progress). Cap hard for API budget.
    'budget_hit: {
        for name in &scan {
            if out.len() >= max_jobs {
                break;
            }
            for (status, run_take) in [("queued", 2usize), ("in_progress", 1usize)] {
                if out.len() >= max_jobs {
                    break;
                }
                let url = format!(
                    "https://api.github.com/repos/{name}/actions/runs?status={status}&per_page=5"
                );
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
        }
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

fn ensure_worker_volume(base_volume: &str, worker_volume: &str, image: &str) -> Result<(), String> {
    if volume_exists(worker_volume) {
        return Ok(());
    }
    if !volume_exists(base_volume) {
        return Err(format!(
            "base volume {base_volume} missing — run prepare first"
        ));
    }
    eprintln!("pool: seeding worker volume {worker_volume} from {base_volume}");
    podman(&["volume", "create", worker_volume])?;
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
        image,
        "-c",
        r"set -euo pipefail; cp -a /from/. /to/; chown -R 1001:1001 /to 2>/dev/null || true; rm -f /to/.runner /to/.credentials /to/.credentials_rsaparams 2>/dev/null; true",
    ])?;
    Ok(())
}

fn reap_pool_workers(cli: &Cli, pool: &ResourcePool) {
    let Ok(claims) = pool.claims() else {
        return;
    };
    for c in claims {
        // Only reap workers owned by this listen base name prefix
        if !c.container.starts_with(&cli.container) {
            continue;
        }
        if !container_running(&c.container) {
            eprintln!(
                "pool: reap {} (tier={} repo={:?})",
                c.container, c.tier, c.repo
            );
            let mut dead = cli.clone_for_listen();
            dead.container = c.container.clone();
            dead.volume = format!("{}-data", c.container);
            dead.runner_name = c.worker_id.clone();
            let _ = down(&dead, true);
            let _ = pool.release(&c.worker_id);
        }
    }
}

fn spawn_sized_worker(
    base: &Cli,
    pool: &ResourcePool,
    slot: u32,
    job: &DemandJob,
) -> Result<(), String> {
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
        return Ok(());
    };
    let worker_id = format!("{}-w{slot}", base.runner_name);
    let container = format!("{}-w{slot}", base.container);
    let volume = format!("{container}-data");
    if !pool.try_claim(&worker_id, &container, c, m, tier, Some(job.repo.as_str()))? {
        eprintln!("pool: claim failed for {container}");
        return Ok(());
    }
    ensure_worker_volume(&base.volume, &volume, &base.image)?;
    let mut unit = base.clone_for_listen();
    unit.repo = Some(job.repo.clone());
    unit.container = container.clone();
    unit.volume = volume;
    unit.runner_name = worker_id.clone();
    unit.cpus = format_cpus(c);
    unit.memory = format_memory_mib(m);
    eprintln!(
        "pool: up {container} tier={} cpus={} mem={} repo={} job={}",
        tier.as_str(),
        unit.cpus,
        unit.memory,
        job.repo,
        job.job_name
    );
    if let Err(e) = up(&unit) {
        let _ = pool.release(&worker_id);
        return Err(e);
    }
    Ok(())
}

fn listen(cli: &Cli, interval: u64, idle_secs: u64, wake_port: Option<u16>) -> Result<(), String> {
    let interval = if matches!(cli.scope, Scope::User) {
        interval.max(USER_BATCH_MIN_INTERVAL_SECS)
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
    eprintln!(
        "listen: scope={:?} poll={interval}s idle={idle_secs}s mode={:?} api_gap={}ms max_per_poll={} pool={} ({:.0}c/{}MiB max_workers={})",
        cli.scope,
        cli.mode,
        cli.api_min_gap_ms,
        cli.api_max_per_poll,
        if dynamic { "dynamic" } else { "single" },
        pool.max_cpus,
        pool.max_memory_mib,
        pool.max_workers.min(cli.pool_max_workers),
    );
    if matches!(cli.scope, Scope::User) && cli.prefer_repos.is_none() {
        eprintln!(
            "listen: warning: set GHA_PREFER_REPOS=owner/r1,owner/r2 (allowlist) to stay within API budgets"
        );
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
        thread::spawn(move || wake_server(port, snap, token));
        eprintln!("listen: authenticated wake on 127.0.0.1:{port}");
    }

    let mut idle_since: Option<Instant> = None;
    let mut cli = cli.clone_for_listen();
    let mut pacer = ApiPacer::from_cli(&cli);
    let max_local = cli.pool_max_workers.min(pool.max_workers).max(1);

    loop {
        if let Some(wait) = pacer.cooling() {
            let secs = wait.as_secs().max(1);
            eprintln!("listen: API cool-down {secs}s before next poll");
            thread::sleep(wait);
            continue;
        }

        // Always reap finished pool workers first (frees budget).
        if dynamic {
            reap_pool_workers(&cli, &pool);
        }

        let api = match github_token() {
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
                    continue;
                }
            };
            drop(api);

            let running_n = pool
                .claims()
                .map(|c| {
                    c.iter()
                        .filter(|x| {
                            x.container.starts_with(&cli.container)
                                && container_running(&x.container)
                        })
                        .count()
                })
                .unwrap_or(0);

            if jobs.is_empty() {
                if running_n == 0 {
                    let since = idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(idle_secs) {
                        // nothing to down at base container in pure multi-worker mode
                        idle_since = None;
                    }
                } else {
                    idle_since = None;
                }
            } else {
                idle_since = None;
                let mut slot: u32 = 0;
                let mut spawned = 0u32;
                for job in &jobs {
                    // GPU listener only takes gpu-tier jobs; CPU skips gpu.
                    let tier = size_for_job(&job.job_name, &job.labels, cli.gpu);
                    if cli.gpu && tier != SizeTier::Gpu {
                        continue;
                    }
                    if !cli.gpu && tier == SizeTier::Gpu {
                        continue;
                    }
                    // find free slot id
                    while slot < max_local {
                        let cname = format!("{}-w{slot}", cli.container);
                        if !container_running(&cname) {
                            break;
                        }
                        slot += 1;
                    }
                    if slot >= max_local {
                        eprintln!("pool: local max workers {max_local} reached");
                        break;
                    }
                    if let Err(e) = spawn_sized_worker(&cli, &pool, slot, job) {
                        eprintln!("pool: spawn failed: {}", redact(&e));
                    } else {
                        spawned += 1;
                    }
                    slot += 1;
                }
                if spawned > 0 {
                    let (uc, um, n) = pool.usage().unwrap_or((0.0, 0, 0));
                    eprintln!(
                        "pool: spawned={spawned} usage={uc:.2}/{:.0}c {um}/{}MiB claims={n}",
                        pool.max_cpus, pool.max_memory_mib
                    );
                }
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
                if let Ok(api2) = github_token() {
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
        }

        thread::sleep(Duration::from_secs(interval));
    }
}

/// Clone settings for listen mutability of active repo.
impl Cli {
    fn clone_for_listen(&self) -> Self {
        Self {
            cmd: Some(Cmd::Status),
            scope: self.scope.clone(),
            repo: self.repo.clone(),
            owner: self.owner.clone(),
            user: self.user.clone(),
            auto: self.auto,
            image: self.image.clone(),
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
            build_dir: self.build_dir.clone(),
            mode: self.mode.clone(),
            wake_token: self.wake_token.clone(),
            full_auto: self.full_auto,
            this_repo_only: self.this_repo_only.clone(),
            public_only: self.public_only,
            private_only: self.private_only,
            all_repos: self.all_repos,
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
    mode: Mode,
    wake_token: Option<String>,
    full_auto: bool,
    this_repo_only: Option<String>,
    public_only: bool,
    private_only: bool,
    all_repos: bool,
}

fn cli_snapshot(cli: &Cli) -> CliSnap {
    CliSnap {
        scope: cli.scope.clone(),
        repo: cli.repo.clone(),
        owner: cli.owner.clone(),
        user: cli.user.clone(),
        auto: cli.auto,
        image: cli.image.clone(),
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
        mode: cli.mode.clone(),
        wake_token: cli.wake_token.clone(),
        full_auto: cli.full_auto,
        this_repo_only: cli.this_repo_only.clone(),
        public_only: cli.public_only,
        private_only: cli.private_only,
        all_repos: cli.all_repos,
    }
}

fn snap_to_cli(s: &CliSnap) -> Cli {
    Cli {
        cmd: Some(Cmd::Status),
        scope: s.scope.clone(),
        repo: s.repo.clone(),
        owner: s.owner.clone(),
        user: s.user.clone(),
        auto: s.auto,
        image: s.image.clone(),
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
        build_dir: None,
        mode: s.mode.clone(),
        wake_token: s.wake_token.clone(),
        full_auto: s.full_auto,
        this_repo_only: s.this_repo_only.clone(),
        public_only: s.public_only,
        private_only: s.private_only,
        all_repos: s.all_repos,
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

fn wake_server(port: u16, snap: CliSnap, token: String) {
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
        let (code, body) = if req.starts_with("POST /wake") {
            match up(&cli) {
                Ok(()) => ("200 OK", "up\n"),
                Err(e) => {
                    eprintln!("wake: {}", redact(&e));
                    ("500", "error\n")
                }
            }
        } else if req.starts_with("POST /sleep") {
            match down(&cli, true) {
                Ok(()) => ("200 OK", "down\n"),
                Err(e) => {
                    eprintln!("sleep: {}", redact(&e));
                    ("500", "error\n")
                }
            }
        } else if req.starts_with("GET /health") {
            ("200 OK", "ok\n")
        } else {
            ("404", "use POST /wake or POST /sleep\n")
        };
        let _ = write!(
            s,
            "HTTP/1.1 {code}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

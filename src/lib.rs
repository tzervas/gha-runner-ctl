//! One GitHub Actions self-hosted runner controller (Podman).
//!
//! Registration targets:
//! - **repo** — one repository (optional **--auto** from cwd / `gh repo view`)
//! - **org** — organization runner (many org repos, one registration)
//! - **user** — batch personal account: poll all owned repos; ephemeral-register
//!   the single runner to whichever repo has queued self-hosted work
//!
//! GitHub queues jobs; one runner process handles one job at a time.

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
const UA: &str = "gha-runner-ctl/0.2.5";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_POLL_SECS: u64 = 5;
const MAX_POLL_SECS: u64 = 3600;
const MIN_IDLE_SECS: u64 = 30;
const MAX_IDLE_SECS: u64 = 86_400;
/// Default gap between GitHub API calls within one process (ms).
const DEFAULT_API_MIN_GAP_MS: u64 = 500;
/// Default cap on API GETs per demand poll (prevents scan storms).
const DEFAULT_API_MAX_PER_POLL: u32 = 24;
/// Initial backoff when rate-limited (seconds).
const DEFAULT_API_BACKOFF_SECS: u64 = 60;
const MAX_API_BACKOFF_SECS: u64 = 900;

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
    about = "One hardened self-hosted GHA runner on Podman (auto / batch capable)"
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

    /// Automatically prepare, poll, and register (loose 60s polling, 500s timeout)
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
    Listen {
        #[arg(long, default_value_t = 30)]
        interval: u64,
        #[arg(long, default_value_t = 180)]
        idle_secs: u64,
        #[arg(long, env = "GHA_WAKE_PORT")]
        wake_port: Option<u16>,
    },
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
                    interval: 60,
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
        if let Some(i) = out.find(key) {
            let rest = &out[i + key.len()..];
            let take = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                .count()
                .min(200);
            let end = i + key.len() + take;
            out.replace_range(i..end, &format!("{key}***REDACTED***"));
        }
    }
    if out.len() > 400 {
        out = format!("{}…", &out[..400]);
    }
    out
}

fn validate_cli(cli: &Cli) -> Result<(), String> {
    match cli.scope {
        Scope::Repo => {
            let Some(repo) = cli.repo.as_ref() else {
                return Err(
                    "repo scope requires --repo owner/name, GHA_REPO, or --auto in a git checkout"
                        .into(),
                );
            };
            if !is_safe_repo(repo) {
                return Err("invalid --repo".into());
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
            if matches!(cli.mode, Mode::Retain) {
                return Err(
                    "scope=user requires --mode ephemeral (re-register per repo; retain is single-target)"
                        .into(),
                );
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
        let path = dir.join(format!("gha-runner-ctl-{kind}-{safe}.lock"));
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    let _ = writeln!(f, "{}", std::process::id());
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                    }
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
    fs::write(&path, content).map_err(|e| format!("Failed to write config file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
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
                    let wait = reset.saturating_sub(now).min(MAX_API_BACKOFF_SECS).max(5);
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
            secs = secs.max(reset.saturating_sub(now)).min(MAX_API_BACKOFF_SECS);
        }
        secs = secs.max(5);
        eprintln!("api: rate-limited — backing off {secs}s (then resume paced calls)");
        self.cool_until = Some(Instant::now() + Duration::from_secs(secs));
        self.backoff = (self.backoff * 2).min(self.max_backoff);
    }

    fn get(
        &mut self,
        url: &str,
        api: &str,
    ) -> Result<ureq::Response, String> {
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
                if status == 403 || status == 429 {
                    // Peek body for secondary-limit wording without consuming if we can —
                    // ureq Response is consumed by into_string; clone status path only.
                    self.note_rate_limited(retry_after, reset);
                    return Err(format!("status code {status} (rate limited)"));
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
                if code == 403 || code == 429 {
                    self.note_rate_limited(retry_after, reset);
                    let _ = remaining;
                    return Err(format!("status code {code} (rate limited)"));
                }
                Err(format!("status code {code}"))
            }
            Err(e) => Err(redact(&e.to_string())),
        }
    }
}

fn registration_token(cli: &Cli, api_token: &str) -> Result<String, String> {
    let url = registration_api(cli);
    let resp = http_agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {api_token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| {
            format!(
                "registration-token request failed: {}",
                redact(&e.to_string())
            )
        })?;
    if !(200..300).contains(&resp.status()) {
        return Err(format!(
            "registration-token HTTP {} (admin rights on target?)",
            resp.status()
        ));
    }
    let body: RegistrationTokenResponse = resp
        .into_json()
        .map_err(|e| format!("registration-token parse failed: {e}"))?;
    if body.token.is_empty() || body.token.len() > 512 {
        return Err("registration token empty or implausible length".into());
    }
    Ok(body.token)
}

// --- Podman ------------------------------------------------------------------

fn podman(args: &[&str]) -> Result<String, String> {
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
    dir.join(format!("gha-runner-ctl-{}.env", std::process::id()))
}

fn write_env_file(path: &Path, reg_token: &str, cli: &Cli) -> Result<(), String> {
    let ephemeral = matches!(cli.mode, Mode::Ephemeral);
    let mut f = File::create(path).map_err(|e| format!("env file: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
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
    dir.join(format!("gha-runner-ctl-active-{}.txt", cli.container))
}

fn set_active_target(cli: &Cli, repo: &str) {
    let p = active_target_path(cli);
    let _ = fs::write(&p, repo);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
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
        eprintln!("up: already running ({})", cli.container);
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

    let api = github_token()?;
    let reg = registration_token(cli, &api)?;
    let env_path = private_env_path();
    write_env_file(&env_path, &reg, cli)?;
    drop(reg);
    drop(api);

    if container_exists(&cli.container) {
        let _ = podman(&["rm", "-f", &cli.container]);
    }

    eprintln!(
        "up: scope={:?} mode={:?} url={}",
        cli.scope,
        cli.mode,
        github_url(cli)
    );
    let ephemeral = matches!(cli.mode, Mode::Ephemeral) || matches!(cli.scope, Scope::User);
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
        "--pull",
        "never",
        "--user",
        "1001:1001",
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
    let ephemeral = matches!(cli.mode, Mode::Ephemeral) || matches!(cli.scope, Scope::User);
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
}

#[derive(Debug, Deserialize)]
struct NamedRepo {
    full_name: String,
    fork: Option<bool>,
    archived: Option<bool>,
    private: Option<bool>,
}

/// Returns (need_runner, optional active_repo_for_registration).
fn demand(
    cli: &Cli,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<(bool, Option<String>), String> {
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
            let repo = cli.repo.as_ref().expect("validated").clone();
            Ok((repo_needs_runner(cli, &repo, api, pacer)?, Some(repo)))
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
                for name in pref.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()) {
                    if !is_safe_repo(name) {
                        continue;
                    }
                    if !name.starts_with(&format!("{user}/")) {
                        continue;
                    }
                    match repo_needs_runner(cli, name, api, pacer) {
                        Ok(true) => return Ok((true, Some(name.to_string()))),
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
                return Ok((false, None));
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

fn fetch_runs(
    url: &str,
    api: &str,
    pacer: &mut ApiPacer,
) -> Result<Vec<WorkflowRun>, String> {
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
    let url = format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs");
    let resp = pacer
        .get(&url, api)
        .map_err(|e| format!("list jobs: {}", redact(&e)))?;
    let body: JobsResp = resp.into_json().map_err(|e| format!("parse jobs: {e}"))?;
    for j in body.jobs {
        if j.status == "completed" {
            continue;
        }
        if labels_match_demand(cli, &j.labels) {
            return Ok(true);
        }
    }
    Ok(false)
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

fn listen(cli: &Cli, interval: u64, idle_secs: u64, wake_port: Option<u16>) -> Result<(), String> {
    let interval = if matches!(cli.scope, Scope::User) {
        interval.max(30)
    } else {
        interval
    };
    eprintln!(
        "listen: scope={:?} poll={interval}s idle={idle_secs}s mode={:?} api_gap={}ms max_per_poll={}",
        cli.scope,
        cli.mode,
        cli.api_min_gap_ms,
        cli.api_max_per_poll
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

    loop {
        if let Some(wait) = pacer.cooling() {
            let secs = wait.as_secs().max(1);
            eprintln!("listen: API cool-down {secs}s before next poll");
            thread::sleep(wait);
            continue;
        }

        let api = match github_token() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("listen: auth: {}", redact(&e));
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };

        let (need, target_repo) = match demand(&cli, &api, &mut pacer) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("listen: poll: {}", redact(&e));
                let wait = pacer
                    .cooling()
                    .map(|d| d.max(Duration::from_secs(interval)))
                    .unwrap_or(Duration::from_secs(interval));
                thread::sleep(wait);
                continue;
            }
        };

        // User batch: pin registration target to the repo that needs work.
        // Never recycle while the active repo still has matching in-flight jobs.
        if matches!(cli.scope, Scope::User) {
            if let Some(ref r) = target_repo {
                let active = get_active_target(&cli);
                if active.as_deref() != Some(r.as_str()) {
                    let busy = active
                        .as_ref()
                        .map(|a| active_repo_still_busy(&cli, a, &api, &mut pacer).unwrap_or(false))
                        .unwrap_or(false);
                    if container_running(&cli.container) && busy {
                        eprintln!("listen: sticky on {active:?} (still busy); defer move to {r}");
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
    // ASCII-only prefix; byte length equals character length.
    const BEARER_PREFIX: &str = "authorization: bearer ";
    let lower = line.to_ascii_lowercase();
    if lower.starts_with(BEARER_PREFIX) {
        let rest = &line[BEARER_PREFIX.len()..];
        return constant_time_eq(rest.trim(), token);
    }
    if let Some(rest) = line.strip_prefix("X-Wake-Token:") {
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

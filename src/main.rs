//! One GitHub Actions self-hosted runner controller (Podman).
//!
//! Hardening goals (fail closed on identity inputs; never log secrets):
//! - Validate owner/repo/labels/names before they reach the shell or Podman.
//! - Short-lived registration tokens in a private env file, deleted after start.
//! - HTTP timeouts; API errors scrubbed of bearer material.
//! - Single-instance lock for `listen` / `up`.
//! - Loopback wake endpoint requires a shared secret when enabled.
//! - Container: no-new-privileges, no host docker.sock, resource caps, --pull=never.
//!
//! GitHub owns job queueing; this tool only ensures one runner process exists
//! when labeled work is waiting.

use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_IMAGE: &str = "localhost/gha-runner-ctl:latest";
const DEFAULT_CONTAINER: &str = "gha-runner-ctl";
const DEFAULT_VOLUME: &str = "gha-runner-ctl-data";
const DEFAULT_LABELS: &str = "self-hosted,linux,x64,podman";
const DEFAULT_NAME: &str = "shared-podman-1";
const UA: &str = "gha-runner-ctl/0.1.1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const MIN_POLL_SECS: u64 = 5;
const MAX_POLL_SECS: u64 = 3600;
const MIN_IDLE_SECS: u64 = 30;
const MAX_IDLE_SECS: u64 = 86_400;

#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    /// Re-register each spin-up (`config.sh --ephemeral`).
    Ephemeral,
    /// Keep `.runner` on the snapshot volume.
    Retain,
}

#[derive(Debug, Clone, ValueEnum)]
enum Scope {
    /// Single repository (personal account or one-repo use).
    Repo,
    /// GitHub Organization (one runner for many org repos).
    Org,
}

#[derive(Debug, Parser)]
#[command(
    name = "gha-runner-ctl",
    about = "One hardened self-hosted GHA runner on Podman"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    #[arg(long, env = "GHA_SCOPE", value_enum, default_value_t = Scope::Repo, global = true)]
    scope: Scope,

    #[arg(long, env = "GHA_REPO", global = true)]
    repo: Option<String>,

    #[arg(long, env = "GHA_OWNER", global = true)]
    owner: Option<String>,

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

    #[arg(long, env = "GHA_BUILD_DIR", global = true)]
    build_dir: Option<PathBuf>,

    #[arg(long, env = "GHA_MODE", value_enum, default_value_t = Mode::Ephemeral, global = true)]
    mode: Mode,

    /// Shared secret for loopback wake server (required if --wake-port is set).
    #[arg(long, env = "GHA_WAKE_TOKEN", global = true)]
    wake_token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    Prepare {
        #[arg(long, default_value_t = true)]
        with_container: bool,
    },
    Up,
    Down {
        #[arg(long, default_value_t = true)]
        rm: bool,
    },
    Status,
    Listen {
        #[arg(long, default_value_t = 30)]
        interval: u64,
        #[arg(long, default_value_t = 180)]
        idle_secs: u64,
        #[arg(long, env = "GHA_WAKE_PORT")]
        wake_port: Option<u16>,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("gha-runner-ctl: {}", redact(&e));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    validate_cli(&cli)?;
    match &cli.cmd {
        Cmd::Prepare { with_container } => prepare(&cli, *with_container),
        Cmd::Up => {
            let _lock = InstanceLock::acquire("up")?;
            up(&cli)
        }
        Cmd::Down { rm } => down(&cli, *rm),
        Cmd::Status => status(&cli),
        Cmd::Listen {
            interval,
            idle_secs,
            wake_port,
        } => {
            let interval = (*interval).clamp(MIN_POLL_SECS, MAX_POLL_SECS);
            let idle_secs = (*idle_secs).clamp(MIN_IDLE_SECS, MAX_IDLE_SECS);
            let _lock = InstanceLock::acquire("listen")?;
            listen(&cli, interval, idle_secs, *wake_port)
        }
    }
}

// --- Validation / redaction --------------------------------------------------

/// Identifiers safe for Podman names and GitHub logins (no shell metacharacters).
fn is_safe_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn is_safe_repo(s: &str) -> bool {
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() == 2 && parts.iter().all(|p| is_safe_ident(p))
}

/// Image refs: registry/name:tag — still no shell metacharacters or path tricks.
fn is_safe_image(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':'))
}

fn is_safe_labels(s: &str) -> bool {
    let parts: Vec<&str> = s
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    !parts.is_empty()
        && parts.len() <= 16
        && parts.iter().all(|p| is_safe_ident(p) && p.len() <= 64)
}

fn is_safe_cpus(s: &str) -> bool {
    // "5" or "2.5"
    if s.is_empty() || s.len() > 8 {
        return false;
    }
    s.parse::<f64>().is_ok_and(|n| n > 0.0 && n <= 64.0)
}

fn is_safe_memory(s: &str) -> bool {
    // 512m, 8g, 8192Mi — digits + optional unit
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

/// Strip credential-shaped substrings from error text before logging.
fn redact(s: &str) -> String {
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
    // Long hex-ish tokens
    let re_approx = out.clone();
    if re_approx.len() > 400 {
        out = format!("{}…", &re_approx[..400]);
    }
    out
}

fn validate_cli(cli: &Cli) -> Result<(), String> {
    match cli.scope {
        Scope::Repo => {
            let Some(repo) = cli.repo.as_ref() else {
                return Err("repo scope requires --repo owner/name (or GHA_REPO)".into());
            };
            if !is_safe_repo(repo) {
                return Err("invalid --repo (expected owner/name, safe charset only)".into());
            }
        }
        Scope::Org => {
            let Some(owner) = cli.owner.as_ref() else {
                return Err("org scope requires --owner ORG (or GHA_OWNER)".into());
            };
            if !is_safe_ident(owner) {
                return Err("invalid --owner (safe charset only)".into());
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
        return Err("invalid --labels (comma-separated safe idents, max 16)".into());
    }
    if !is_safe_cpus(&cli.cpus) {
        return Err("invalid --cpus (positive number ≤ 64)".into());
    }
    if !is_safe_memory(&cli.memory) {
        return Err("invalid --memory (e.g. 8g, 512m)".into());
    }
    if let Some(tok) = &cli.wake_token {
        if tok.len() < 16 {
            return Err("GHA_WAKE_TOKEN must be at least 16 characters when set".into());
        }
    }
    Ok(())
}

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
    }
}

fn registration_api(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!(
            "https://api.github.com/repos/{}/actions/runners/registration-token",
            cli.repo.as_ref().expect("validated")
        ),
        Scope::Org => format!(
            "https://api.github.com/orgs/{}/actions/runners/registration-token",
            cli.owner.as_ref().expect("validated")
        ),
    }
}

fn runners_api(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!(
            "https://api.github.com/repos/{}/actions/runners",
            cli.repo.as_ref().expect("validated")
        ),
        Scope::Org => format!(
            "https://api.github.com/orgs/{}/actions/runners",
            cli.owner.as_ref().expect("validated")
        ),
    }
}

// --- Single-instance lock (pid file; no unsafe) ------------------------------

struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    fn acquire(kind: &str) -> Result<Self, String> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("gha-runner-ctl-{kind}.lock"));
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
                        "another gha-runner-ctl {kind} is already running (lock {})",
                        path.display()
                    ));
                }
                Err(e) => {
                    return Err(format!("lock open {}: {e}", path.display()));
                }
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
    // kill -0: process exists?
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

// --- Auth --------------------------------------------------------------------

fn github_token() -> Result<String, String> {
    for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(t) = std::env::var(key) {
            if !t.is_empty() {
                return Ok(t);
            }
        }
    }
    let out = Command::new("gh")
        .args(["auth", "token"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("gh auth token failed: {e}"))?;
    if !out.status.success() {
        return Err(
            "authenticate with `gh auth login` or set GH_TOKEN (runner registration rights)".into(),
        );
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() {
        return Err("empty token from gh auth token".into());
    }
    Ok(t)
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
            "registration-token HTTP {} (check org/repo admin rights)",
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

#[allow(clippy::needless_pass_by_value)]
fn prepare(cli: &Cli, with_container: bool) -> Result<(), String> {
    let dir = resolve_build_dir(cli)?;
    eprintln!("prepare: building {} from {}", cli.image, dir.display());
    // prepare is the only path that may pull base images
    podman(&[
        "build",
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
# Drop world-writable bits on the snapshot home
chmod -R go-w /opt/actions-runner 2>/dev/null || true
date -u +%Y-%m-%dT%H:%M:%SZ > /opt/actions-runner/.snapshot-baseline
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
    // Fixed keys only — values already validated except the token itself.
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

    let api = github_token()?;
    let reg = registration_token(cli, &api)?;
    let env_path = private_env_path();
    write_env_file(&env_path, &reg, cli)?;
    // Drop owned copy of registration token from this process ASAP after write.
    drop(reg);
    drop(api);

    if container_exists(&cli.container) {
        let _ = podman(&["rm", "-f", &cli.container]);
    }

    eprintln!(
        "up: one runner scope={:?} mode={:?} url={}",
        cli.scope,
        cli.mode,
        github_url(cli)
    );
    let ephemeral = matches!(cli.mode, Mode::Ephemeral);
    let env_path_str = env_path.to_str().ok_or("env path not utf-8")?.to_string();
    let vol = format!("{}:/opt/actions-runner:Z", cli.volume);
    let eph = if ephemeral { "true" } else { "false" };
    let ret = if ephemeral { "false" } else { "true" };

    let mut args = vec![
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
        "--env-file",
        env_path_str.as_str(),
        "-e",
    ];
    // -e pairs need owned strings for format! — use static then extend carefully
    let eph_kv = format!("RUNNER_EPHEMERAL={eph}");
    let ret_kv = format!("RUNNER_RETAIN={ret}");
    args.push(eph_kv.as_str());
    args.push("-e");
    args.push(ret_kv.as_str());
    args.push("-v");
    args.push(vol.as_str());
    args.push(cli.image.as_str());

    let result = podman(&args);
    shred_env_file(&env_path);
    result?;
    eprintln!("up: container {}", cli.container);
    eprintln!(
        "up: note — prefer private repos on self-hosted runners (public forks can run untrusted workflows)"
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
    if matches!(cli.mode, Mode::Ephemeral) {
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
    Ok(())
}

fn status(cli: &Cli) -> Result<(), String> {
    println!("scope: {:?}", cli.scope);
    println!("url: {}", github_url(cli));
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
    println!("image: {}", cli.image);
    println!("mode: {:?}", cli.mode);
    println!("labels: {}", cli.labels);

    if let Ok(api) = github_token() {
        let url = runners_api(cli);
        if let Ok(resp) = http_agent()
            .get(&url)
            .set("Authorization", &format!("Bearer {api}"))
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .call()
        {
            #[derive(Deserialize)]
            struct Runners {
                runners: Vec<Runner>,
            }
            #[derive(Deserialize)]
            struct Runner {
                name: String,
                status: String,
                busy: bool,
            }
            if let Ok(body) = resp.into_json::<Runners>() {
                println!("github runners:");
                for r in body.runners {
                    println!("  - {} status={} busy={}", r.name, r.status, r.busy);
                }
            }
        }
    }
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
struct OrgRepos {
    full_name: String,
}

fn demand(cli: &Cli, api: &str) -> Result<bool, String> {
    match cli.scope {
        Scope::Repo => {
            let repo = cli.repo.as_ref().expect("validated");
            repo_needs_runner(repo, api)
        }
        Scope::Org => {
            let owner = cli.owner.as_ref().expect("validated");
            let url = format!("https://api.github.com/orgs/{owner}/repos?per_page=50&type=all");
            let resp = http_agent()
                .get(&url)
                .set("Authorization", &format!("Bearer {api}"))
                .set("Accept", "application/vnd.github+json")
                .set("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .map_err(|e| format!("list org repos: {}", redact(&e.to_string())))?;
            let repos: Vec<OrgRepos> = resp
                .into_json()
                .map_err(|e| format!("parse org repos: {e}"))?;
            for r in repos {
                if !is_safe_repo(&r.full_name) {
                    continue;
                }
                if repo_needs_runner(&r.full_name, api)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

fn repo_needs_runner(repo: &str, api: &str) -> Result<bool, String> {
    for status in ["queued", "in_progress"] {
        let url =
            format!("https://api.github.com/repos/{repo}/actions/runs?status={status}&per_page=10");
        let runs = fetch_runs(&url, api)?;
        for run in runs {
            if job_wants_self_hosted(repo, run.id, api)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn fetch_runs(url: &str, api: &str) -> Result<Vec<WorkflowRun>, String> {
    let resp = http_agent()
        .get(url)
        .set("Authorization", &format!("Bearer {api}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("list runs: {}", redact(&e.to_string())))?;
    let body: WorkflowRuns = resp.into_json().map_err(|e| format!("parse runs: {e}"))?;
    Ok(body.workflow_runs)
}

fn job_wants_self_hosted(repo: &str, run_id: u64, api: &str) -> Result<bool, String> {
    let url = format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs");
    let resp = http_agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {api}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("list jobs: {}", redact(&e.to_string())))?;
    let body: JobsResp = resp.into_json().map_err(|e| format!("parse jobs: {e}"))?;
    for j in body.jobs {
        if j.status == "completed" {
            continue;
        }
        let labels = j.labels.join(",").to_ascii_lowercase();
        if labels.contains("self-hosted") || labels.contains("podman") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn listen(cli: &Cli, interval: u64, idle_secs: u64, wake_port: Option<u16>) -> Result<(), String> {
    eprintln!(
        "listen: scope={:?} poll={interval}s idle={idle_secs}s mode={:?}",
        cli.scope, cli.mode
    );
    if !volume_exists(&cli.volume) {
        eprintln!("listen: snapshot missing — prepare…");
        prepare(cli, true)?;
    }

    if let Some(port) = wake_port {
        if port == 0 {
            return Err("wake-port must be non-zero".into());
        }
        let Some(token) = cli.wake_token.clone() else {
            return Err(
                "wake-port requires GHA_WAKE_TOKEN (≥16 chars) — refuse unauthenticated wake"
                    .into(),
            );
        };
        let snap = cli_snapshot(cli);
        thread::spawn(move || wake_server(port, snap, token));
        eprintln!("listen: authenticated wake on 127.0.0.1:{port}");
    }

    let mut idle_since: Option<Instant> = None;
    loop {
        let api = match github_token() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("listen: auth: {}", redact(&e));
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };

        let need = match demand(cli, &api) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("listen: poll: {}", redact(&e));
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };
        drop(api);

        let running = container_running(&cli.container);

        if need && !running {
            eprintln!("listen: demand — up");
            if let Err(e) = up(cli) {
                eprintln!("listen: up failed: {}", redact(&e));
            }
            idle_since = None;
        } else if !need && running {
            let since = idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(idle_secs) {
                eprintln!("listen: idle {idle_secs}s — down");
                if let Err(e) = down(cli, true) {
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

struct CliSnap {
    scope: Scope,
    repo: Option<String>,
    owner: Option<String>,
    image: String,
    container: String,
    volume: String,
    runner_name: String,
    labels: String,
    cpus: String,
    memory: String,
    mode: Mode,
    wake_token: Option<String>,
}

fn cli_snapshot(cli: &Cli) -> CliSnap {
    CliSnap {
        scope: cli.scope.clone(),
        repo: cli.repo.clone(),
        owner: cli.owner.clone(),
        image: cli.image.clone(),
        container: cli.container.clone(),
        volume: cli.volume.clone(),
        runner_name: cli.runner_name.clone(),
        labels: cli.labels.clone(),
        cpus: cli.cpus.clone(),
        memory: cli.memory.clone(),
        mode: cli.mode.clone(),
        wake_token: cli.wake_token.clone(),
    }
}

fn snap_to_cli(s: &CliSnap) -> Cli {
    Cli {
        cmd: Cmd::Status,
        scope: s.scope.clone(),
        repo: s.repo.clone(),
        owner: s.owner.clone(),
        image: s.image.clone(),
        container: s.container.clone(),
        volume: s.volume.clone(),
        runner_name: s.runner_name.clone(),
        labels: s.labels.clone(),
        cpus: s.cpus.clone(),
        memory: s.memory.clone(),
        build_dir: None,
        mode: s.mode.clone(),
        wake_token: s.wake_token.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shell_metacharacters_in_repo() {
        assert!(!is_safe_repo("foo/bar;rm"));
        assert!(!is_safe_repo("../etc/passwd"));
        assert!(is_safe_repo("tzervas/tg-agent-relay"));
    }

    #[test]
    fn redacts_bearer_and_ghp() {
        let s = redact("error Bearer ghp_ABCDEFGHIJKLMNOPQRSTUV secret");
        assert!(!s.contains("ABCDEF"));
        assert!(s.contains("REDACTED") || s.contains("***"));
    }

    #[test]
    fn labels_bounded() {
        assert!(is_safe_labels("self-hosted,linux,x64,podman"));
        assert!(!is_safe_labels("ok,bad label"));
        assert!(!is_safe_labels(""));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq("abcdef0123456789", "abcdef0123456789"));
        assert!(!constant_time_eq("abcdef0123456789", "abcdef0123456780"));
    }
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
        // Require Authorization: Bearer <token> or X-Wake-Token
        let authed = req.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("authorization: bearer ") {
                return constant_time_eq(rest.trim(), token.as_str());
            }
            if let Some(rest) = line.strip_prefix("X-Wake-Token:") {
                return constant_time_eq(rest.trim(), token.as_str());
            }
            false
        });
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

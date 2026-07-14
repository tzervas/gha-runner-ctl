//! One GitHub Actions self-hosted runner controller (Podman).
//!
//! Single instance: snapshot baseline, short-lived registration, on-demand
//! up/down. Shared across repos via **org-level** registration + common labels.
//! GitHub owns the job queue; this tool only ensures a runner exists when work waits.

use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use std::fs;
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
const UA: &str = "gha-runner-ctl/0.1";

#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    /// Re-register each spin-up (`config.sh --ephemeral`).
    Ephemeral,
    /// Keep `.runner` on the snapshot volume.
    Retain,
}

#[derive(Debug, Clone, ValueEnum)]
enum Scope {
    /// Register against one repository (personal accounts, or single-repo use).
    Repo,
    /// Register against a GitHub Organization (one runner, many org repos).
    Org,
}

#[derive(Debug, Parser)]
#[command(
    name = "gha-runner-ctl",
    about = "One self-hosted GHA runner on Podman: prepare, listen, up/down"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,

    /// repo | org
    #[arg(long, env = "GHA_SCOPE", value_enum, default_value_t = Scope::Repo, global = true)]
    scope: Scope,

    /// owner/repo when --scope repo
    #[arg(long, env = "GHA_REPO", global = true)]
    repo: Option<String>,

    /// Organization login when --scope org
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
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Build image + seed volume snapshot (once / after runner version bumps)
    Prepare {
        #[arg(long, default_value_t = true)]
        with_container: bool,
    },
    /// Register (if needed) and start the one runner
    Up,
    /// Stop the runner
    Down {
        #[arg(long, default_value_t = true)]
        rm: bool,
    },
    Status,
    /// Poll for demand; up on queue; down after idle
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
        eprintln!("gha-runner-ctl: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    validate_scope(&cli)?;
    match &cli.cmd {
        Cmd::Prepare { with_container } => prepare(&cli, *with_container),
        Cmd::Up => up(&cli),
        Cmd::Down { rm } => down(&cli, *rm),
        Cmd::Status => status(&cli),
        Cmd::Listen {
            interval,
            idle_secs,
            wake_port,
        } => listen(&cli, *interval, *idle_secs, *wake_port),
    }
}

fn validate_scope(cli: &Cli) -> Result<(), String> {
    match cli.scope {
        Scope::Repo => {
            if cli.repo.as_ref().is_none_or(|s| !s.contains('/')) {
                return Err(
                    "repo scope requires --repo owner/name (or GHA_REPO=owner/name)".into(),
                );
            }
        }
        Scope::Org => {
            if cli.owner.as_ref().is_none_or(String::is_empty) {
                return Err("org scope requires --owner ORG (or GHA_OWNER=ORG)".into());
            }
        }
    }
    Ok(())
}

fn github_url(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!("https://github.com/{}", cli.repo.as_ref().unwrap()),
        Scope::Org => format!("https://github.com/{}", cli.owner.as_ref().unwrap()),
    }
}

fn registration_api(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!(
            "https://api.github.com/repos/{}/actions/runners/registration-token",
            cli.repo.as_ref().unwrap()
        ),
        Scope::Org => format!(
            "https://api.github.com/orgs/{}/actions/runners/registration-token",
            cli.owner.as_ref().unwrap()
        ),
    }
}

fn runners_api(cli: &Cli) -> String {
    match cli.scope {
        Scope::Repo => format!(
            "https://api.github.com/repos/{}/actions/runners",
            cli.repo.as_ref().unwrap()
        ),
        Scope::Org => format!(
            "https://api.github.com/orgs/{}/actions/runners",
            cli.owner.as_ref().unwrap()
        ),
    }
}

// --- Auth (never log token material) -----------------------------------------

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
            "authenticate with `gh auth login` or set GH_TOKEN (needs runner registration rights)"
                .into(),
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

fn registration_token(cli: &Cli, api_token: &str) -> Result<String, String> {
    let url = registration_api(cli);
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {api_token}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", UA)
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("registration-token request failed: {e}"))?;
    let body: RegistrationTokenResponse = resp
        .into_json()
        .map_err(|e| format!("registration-token parse failed: {e}"))?;
    if body.token.is_empty() {
        return Err("empty registration token".into());
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
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("podman {} failed: {}", args.join(" "), err.trim()));
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
        return Ok(p.clone());
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

#[allow(clippy::unnecessary_wraps, clippy::needless_pass_by_value)]
fn prepare(cli: &Cli, with_container: bool) -> Result<(), String> {
    let dir = resolve_build_dir(cli)?;
    eprintln!("prepare: building {} from {}", cli.image, dir.display());
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
date -u +%Y-%m-%dT%H:%M:%SZ > /opt/actions-runner/.snapshot-baseline
echo ok
",
    ])?;

    if with_container {
        eprintln!(
            "prepare: snapshot ready (cpus={} memory={}); container starts on `up`",
            cli.cpus, cli.memory
        );
    }
    eprintln!("prepare: done");
    Ok(())
}

fn write_env_file(path: &Path, reg_token: &str, cli: &Cli) -> Result<(), String> {
    let ephemeral = matches!(cli.mode, Mode::Ephemeral);
    let mut f = fs::File::create(path).map_err(|e| format!("env file: {e}"))?;
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
    let env_path = std::env::temp_dir().join(format!("gha-runner-{}-env", std::process::id()));
    write_env_file(&env_path, &reg, cli)?;

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
    podman(&[
        "run",
        "-d",
        "--name",
        &cli.container,
        "--cpus",
        &cli.cpus,
        "--memory",
        &cli.memory,
        "--memory-swap",
        &cli.memory,
        "--pids-limit",
        "4096",
        "--env-file",
        env_path.to_str().unwrap_or("/dev/null"),
        "-e",
        &format!(
            "RUNNER_EPHEMERAL={}",
            if ephemeral { "true" } else { "false" }
        ),
        "-e",
        &format!("RUNNER_RETAIN={}", if ephemeral { "false" } else { "true" }),
        "-v",
        &format!("{}:/opt/actions-runner:Z", cli.volume),
        &cli.image,
    ])?;

    let _ = fs::remove_file(&env_path);
    eprintln!("up: container {}", cli.container);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
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
        let _ = podman(&[
            "run",
            "--rm",
            "--entrypoint",
            "/bin/bash",
            "-v",
            &format!("{}:/opt/actions-runner:Z", cli.volume),
            &cli.image,
            "-c",
            "rm -f /opt/actions-runner/.runner /opt/actions-runner/.credentials /opt/actions-runner/.credentials_rsaparams 2>/dev/null; true",
        ]);
    }
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
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
        if let Ok(resp) = ureq::get(&url)
            .set("Authorization", &format!("Bearer {api}"))
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", UA)
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

// --- Demand (GitHub queues; we only detect need) -----------------------------

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
            let repo = cli.repo.as_ref().unwrap();
            Ok(repo_needs_runner(repo, api)?)
        }
        Scope::Org => {
            let owner = cli.owner.as_ref().unwrap();
            // First page of org repos is enough for typical personal/org scale.
            let url = format!("https://api.github.com/orgs/{owner}/repos?per_page=50&type=all");
            let resp = ureq::get(&url)
                .set("Authorization", &format!("Bearer {api}"))
                .set("Accept", "application/vnd.github+json")
                .set("User-Agent", UA)
                .set("X-GitHub-Api-Version", "2022-11-28")
                .call()
                .map_err(|e| format!("list org repos: {e}"))?;
            let repos: Vec<OrgRepos> = resp
                .into_json()
                .map_err(|e| format!("parse org repos: {e}"))?;
            for r in repos {
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
    let resp = ureq::get(url)
        .set("Authorization", &format!("Bearer {api}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", UA)
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("list runs: {e}"))?;
    let body: WorkflowRuns = resp.into_json().map_err(|e| format!("parse runs: {e}"))?;
    Ok(body.workflow_runs)
}

fn job_wants_self_hosted(repo: &str, run_id: u64, api: &str) -> Result<bool, String> {
    let url = format!("https://api.github.com/repos/{repo}/actions/runs/{run_id}/jobs");
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {api}"))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", UA)
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| format!("list jobs: {e}"))?;
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
        let snap = cli_snapshot(cli);
        thread::spawn(move || wake_server(port, snap));
        eprintln!("listen: wake on 127.0.0.1:{port} (POST /wake | /sleep)");
    }

    let mut idle_since: Option<Instant> = None;
    loop {
        let api = match github_token() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("listen: auth: {e}");
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };

        let need = match demand(cli, &api) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("listen: poll: {e}");
                thread::sleep(Duration::from_secs(interval));
                continue;
            }
        };

        let running = container_running(&cli.container);

        if need && !running {
            eprintln!("listen: demand — up");
            if let Err(e) = up(cli) {
                eprintln!("listen: up failed: {e}");
            }
            idle_since = None;
        } else if !need && running {
            let since = idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(idle_secs) {
                eprintln!("listen: idle {idle_secs}s — down");
                if let Err(e) = down(cli, true) {
                    eprintln!("listen: down failed: {e}");
                }
                idle_since = None;
            }
        } else {
            // need&&running (busy) or !need&&!running (parked)
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
    }
}

fn wake_server(port: u16, snap: CliSnap) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    let snap = Arc::new(snap);
    let bind = format!("127.0.0.1:{port}");
    let Ok(listener) = TcpListener::bind(&bind) else {
        eprintln!("wake: bind {bind} failed");
        return;
    };
    for stream in listener.incoming().flatten() {
        let mut s = stream;
        let mut buf = [0_u8; 1024];
        let n = s.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let cli = snap_to_cli(&snap);
        let (code, body) = if req.starts_with("POST /wake") {
            match up(&cli) {
                Ok(()) => ("200 OK", "up\n"),
                Err(e) => {
                    eprintln!("wake: {e}");
                    ("500", "error\n")
                }
            }
        } else if req.starts_with("POST /sleep") {
            match down(&cli, true) {
                Ok(()) => ("200 OK", "down\n"),
                Err(e) => {
                    eprintln!("sleep: {e}");
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

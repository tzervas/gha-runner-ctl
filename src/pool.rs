//! Host-wide ephemeral runner resource pool.
//!
//! Budget is shared across all `gha-runner-ctl` processes (CPU + GPU listeners).
//! Workers claim millicores + MiB before `podman run`, release on container exit.
//!
//! Job sizing is **automatic** from job name + labels — workflows need not set
//! allocation. See [`size_for_job`].

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default host pool: 16 cores / 16 GiB for all ephemeral work containers.
/// Single-worker ceiling matches xlarge/gpu tiers (16c / 16 GiB max claim).
pub const DEFAULT_POOL_CPUS: f64 = 16.0;
pub const DEFAULT_POOL_MEMORY_MIB: u64 = 16 * 1024;
pub const DEFAULT_MAX_WORKERS: u32 = 24;
/// Smallest worker: 250m CPU / 256 MiB (planner floor for fit_to_budget).
pub const DEFAULT_MIN_CPUS: f64 = 0.25;
pub const DEFAULT_MIN_MEMORY_MIB: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeTier {
    /// fleet-security / lint / gitleaks-class
    Micro,
    /// light unit tests, ruff, detect; also the catch-all default for job names
    /// `size_for_job` does not recognise (see the tail of that function)
    Small,
    /// default cargo test / full CI (enough RAM to avoid OOM on medium crates)
    Medium,
    /// multi-crate, release, e2e, image build
    Large,
    /// workspace-wide / chromium-class / justified max CPU+RAM (≤16c/16g)
    Xlarge,
    /// GPU jobs (CPU+RAM claim + device attach on GPU listeners)
    Gpu,
}

impl SizeTier {
    pub fn as_str(self) -> &'static str {
        match self {
            SizeTier::Micro => "micro",
            SizeTier::Small => "small",
            SizeTier::Medium => "medium",
            SizeTier::Large => "large",
            SizeTier::Xlarge => "xlarge",
            SizeTier::Gpu => "gpu",
        }
    }
}

/// Explicit size labels workflows may put on `runs-on` (must be registered on the worker).
/// Example: `runs-on: [self-hosted, linux, x64, podman, large]`
fn tier_from_labels(labs: &[String]) -> Option<SizeTier> {
    // Prefer most specific / largest explicit label.
    let has = |s: &str| labs.iter().any(|l| l == s || l == &format!("size-{s}"));
    if has("gpu")
        || labs
            .iter()
            .any(|l| l.starts_with("gpu-slice") || l == "cuda" || l.contains("nvidia"))
    {
        return Some(SizeTier::Gpu);
    }
    if has("xlarge") || has("x-large") || has("huge") {
        return Some(SizeTier::Xlarge);
    }
    if has("large") {
        return Some(SizeTier::Large);
    }
    if has("medium") {
        return Some(SizeTier::Medium);
    }
    if has("small") {
        return Some(SizeTier::Small);
    }
    if has("micro") {
        return Some(SizeTier::Micro);
    }
    None
}

/// Automatic size from job name + labels.
///
/// **Label override** (preferred for justified heavy jobs): put a size token in
/// `runs-on` alongside the fleet labels, e.g.
/// `[self-hosted, linux, x64, podman, large]`. Workers register that label so
/// GitHub routes correctly and the pool claims the matching tier.
pub fn size_for_job(job_name: &str, labels: &[String], force_gpu: bool) -> SizeTier {
    let name = job_name.to_ascii_lowercase();
    let labs: Vec<String> = labels
        .iter()
        .map(|l| l.trim().to_ascii_lowercase())
        .collect();
    if force_gpu {
        return SizeTier::Gpu;
    }
    if let Some(t) = tier_from_labels(&labs) {
        return t;
    }
    // Xlarge signals (justified heavy compiles / full workspaces)
    if name_contains_any(
        &name,
        &[
            "xlarge",
            "workspace-build",
            "full-workspace",
            "chromium",
            "compile-all",
            "all-features",
            "heavy-build",
        ],
    ) {
        return SizeTier::Xlarge;
    }
    // Large signals
    if name_contains_any(
        &name,
        &[
            "train",
            "finetune",
            "fine-tune",
            "release",
            "build-image",
            "docker",
            "podman-build",
            "benchmark",
            "perf",
            "full-suite",
            "integration",
            "e2e",
            "matrix",
            "local parity",
            "local-parity",
            "build and test",
        ],
    ) {
        return SizeTier::Large;
    }
    // Light / security / lint (docs/clippy alone stay micro)
    if name_contains_any(
        &name,
        &[
            "gitleaks",
            "trivy",
            "license",
            "lint",
            "ruff",
            "fmt",
            "format",
            "typos",
            "markdown",
            "spell",
            "security",
            "reuse",
            "sbom",
            "commitizen",
            "conventional",
            // Observed light jobs (fleet-ops pr-ci.yml) that were falling through to the
            // Medium-default block below (or the bare-Medium catch-all) and holding 8g
            // for work that is a generator/scanner/notifier, not a compile. Traced
            // concretely: `quadlet-generate` matched none of the branches above and
            // landed on Medium via the catch-all, holding 8 GiB for a trivial generator.
            "quadlet-generate",
            "capture-diff",
            "registry-check",
            "secret-keymap",
            "policy-check",
            "adversarial",
            "yamllint",
            "shellcheck",
            "notify",
            "render",
        ],
    ) {
        return SizeTier::Micro;
    }
    // Clippy-only jobs are light; "cargo clippy" with build stays medium via cargo below
    if name.contains("clippy") && !name.contains("build") && !name.contains("test") {
        return SizeTier::Micro;
    }
    // Single "build" jobs (product ci.yml job name) need RAM for rustup + LTO-ish
    // builds. Undersizing caused OOM kill 137 on self-hosted. Prefer large.
    if name == "build" || name.starts_with("build ") || name.ends_with(" build") {
        return SizeTier::Large;
    }
    // Rust compilation is the fleet's memory-hungry workload, and Medium (2c/4g) is
    // not enough for it. Observed: `cargo check --workspace --all-targets` on
    // mycelium-l1 was OOM-killed with exit 137 (run 29955035985) on a job named
    // "cargo check/test", which landed on Medium via the catch-all below.
    //
    // A workspace-wide compile gets Xlarge; any other cargo compile/check/test gets
    // Large. Lint-only cargo jobs (clippy/fmt without build or test) are already
    // routed to Micro above, so they are unaffected.
    if name.contains("cargo") && name_contains_any(&name, &["check", "test", "build", "doc"]) {
        return if name_contains_any(&name, &["workspace", "all-targets", "all targets"]) {
            SizeTier::Xlarge
        } else {
            SizeTier::Large
        };
    }
    // Bare `test` / `mutants` are Rust COMPILE jobs on this fleet, not light checks.
    // aphelion-scribe-{daemon,core,cli,runner} name their cargo build+test job exactly
    // "test", and cargo-mutants names its job "mutants". Both were reaching Medium (via
    // the "test" needle below) or the catch-all, which after the memory rebalance puts a
    // full `cargo test` + `cargo build --release` — two private git deps plus rusqlite
    // `bundled`, i.e. the entire SQLite C amalgamation — back onto 4g. That is the exact
    // configuration that produced the historic exit-137 OOM kills, so it must not be
    // reachable by a job whose only sin is a short name.
    //
    // Matched on boundaries, not substrings: a bare `"test"` needle would also swallow
    // "pytest"/"latest". Caught in review on #107 — the catch-all was checked, this list
    // was not.
    if matches!(name.as_str(), "test" | "mutants")
        || name.starts_with("test ")
        || name_contains_any(&name, &["build + test", "build+test", "build-and-test"])
    {
        return SizeTier::Large;
    }
    // Medium-default non-Rust test/build (pytest, generic ci)
    //
    // NOTE: the bare "ci" needle here is a known-loose substring match — it fires on
    // any job name containing "ci" as a fragment (e.g. "precision", "decision",
    // "special-case"), not just CI jobs. Left as-is: tightening it (e.g. requiring a
    // "-ci"/"ci-"/exact-"ci" boundary) risks silently reclassifying real job names this
    // fleet already depends on, and doing that safely needs an inventory of every job
    // name across every consuming repo, which is out of scope for this change. Flagged
    // as a follow-up in the PR body rather than changed here.
    if name_contains_any(
        &name,
        &[
            "test", "check", "build", "cargo", "pytest", "ci", "unit", "docs",
        ],
    ) {
        return SizeTier::Medium;
    }
    // fleet-ci / fleet-security workflow job names
    if name.contains("fleet-security") || name.contains("noop") || name.contains("gate") {
        return SizeTier::Micro;
    }
    if name.contains("fleet-ci") || name.contains("detect") {
        return SizeTier::Small;
    }
    // Catch-all for anything unrecognised: default to Small, not Medium. An
    // unrecognised job name is far more likely to be something light (a generator,
    // a notifier, a doc step) than an undetected heavy compile — real compile/test/
    // build work is already caught explicitly by the cargo/build/xlarge/large
    // branches above, so lowering this fallback does not put those jobs at risk.
    // This was the actual root cause of the pool's memory exhaustion under load:
    // `quadlet-generate` (now caught above) matched nothing and fell all the way
    // through to this line, landing on Medium (8g in the old table) purely because
    // it was unrecognised, not because it needed 8g.
    SizeTier::Small
}

fn name_contains_any(name: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| name.contains(n))
}

/// Map tier → (cpus string, memory string) for podman.
///
/// Sized for a 56-core/125 GiB-class homelab host, not a laptop. The pool is
/// **memory-bound, not CPU-bound**: measured controller output under load showed
/// `free_left=2.00c/0MiB` — cores still free, memory exhausted — while `skip_cap`
/// climbed because nothing could fit. Root cause: light jobs held far more RAM
/// than they use (e.g. `quadlet-generate`, a trivial generator, held 8 GiB on
/// `medium`), so at 8 GiB/worker a 114 GiB pool caps out around ~14 concurrent
/// jobs regardless of idle cores. Memory is cut hard here (roughly half or better
/// per tier); CPU is left generous since cores were measured idle, not the
/// bottleneck — keeping CPU high keeps each job fast while memory now governs
/// concurrency. At the 114 GiB pool cap this roughly *doubles* concurrency:
/// `medium` 14 → 28 workers, `large` 4 → 7 workers.
///
/// The OOM history is respected, not reopened: the original exit-137 OOM kills
/// (see `size_for_job`) were `cargo test`/`cargo build --release` running in the
/// OLD `large` tier at **4 GiB**. The new `large` is **16 GiB — still 4x
/// that** — a rebalance toward the measured bottleneck, not a return to the
/// configuration that actually OOM-killed. `micro` was briefly 0.25c/512 MiB
/// earlier and lint/scan jobs (gitleaks, trivy, shellcheck, yamllint) completed
/// fine on it; `1c/1g` is double that, so 1 GiB is not tight for that class.
///
/// These are *preferred* sizes — `fit_to_budget` still shrinks toward the free
/// remainder (floor 0.25c/256 MiB) when the pool is tight, so small hosts
/// degrade gracefully. Caps: xlarge ≤ 20 CPU / 28 GiB; gpu ≤ 8 CPU / 16 GiB
/// host-side (device is separate).
///
/// Per-tier override: `GHA_TIER_<TIER>` as `<cpus>:<memory>`, e.g.
/// `GHA_TIER_LARGE=6:12g`. Unset tiers keep the defaults below.
///
/// Why tunable rather than fixed: the right shape depends on the pool's
/// cpu:memory ratio, which is per-host. The defaults were chosen when the pool
/// was MEMORY-bound (`free_left=2.00c/0MiB` — cores free, memory exhausted).
/// A CPU-bound host sees the exact inverse and wants smaller per-job CPU:
/// homelab at 48c/86g runs `large` (12c) only 4-wide while 26 GiB sits idle, so
/// a 111-job queue drains at a quarter of the rate its memory budget could
/// support. Halving `large` CPU roughly doubles concurrency, and cargo does not
/// scale linearly past ~8 cores, so per-job cost is well below the gain.
fn tier_override(tier: SizeTier) -> Option<(String, String)> {
    let key = format!("GHA_TIER_{}", tier.as_str().to_ascii_uppercase());
    let raw = std::env::var(&key).ok()?;
    match parse_tier_override(&raw) {
        Some(v) => Some(v),
        None => {
            eprintln!("pool: ignoring malformed {key}={raw} (want <cpus>:<memory>, e.g. 6:12g)");
            None
        }
    }
}

/// Pure parser, so the validation is testable without mutating process env
/// (which races under parallel test execution).
///
/// The memory side is validated with the *same* [`parse_memory_mib`] that the
/// spawn path uses, not just an emptiness check. Without this, a value like
/// `6:0g` or `6:abc` passed the old (weaker) check, then silently failed
/// `parse_memory_mib` two call-layers downstream where the caller does
/// `.unwrap_or(2048)` with no log line — an override that looks accepted here
/// (no "ignoring malformed" message) but actually degrades every worker of
/// that tier to a flat 2 GiB. That is the "config typo presented as capacity
/// loss" failure this function exists to prevent; it must be caught here,
/// where we can still log which key and value were rejected.
fn parse_tier_override(raw: &str) -> Option<(String, String)> {
    let (c, m) = raw.split_once(':')?;
    let (c, m) = (c.trim(), m.trim());
    if c.parse::<f64>().map(|v| v <= 0.0).unwrap_or(true) {
        return None;
    }
    parse_memory_mib(m)?;
    Some((c.to_string(), m.to_string()))
}

pub fn resources_for_tier(tier: SizeTier) -> (String, String) {
    if let Some(v) = tier_override(tier) {
        return v;
    }
    match tier {
        // Lint/secrets scans (gitleaks, ruff, fmt…) — cheap, but no longer starved at 0.25c.
        SizeTier::Micro => ("1".into(), "1g".into()),
        SizeTier::Small => ("2".into(), "2g".into()),
        // Medium crates / cargo check — memory-bound pool: cut RAM, keep CPU generous.
        SizeTier::Medium => ("4".into(), "4g".into()),
        // cargo test + release --locked: 16g is still 4x the 4g that actually OOM-killed.
        SizeTier::Large => ("12".into(), "16g".into()),
        SizeTier::Xlarge => ("20".into(), "28g".into()),
        // GPU jobs: solid host CPU/RAM for data loaders + full device on GPU slice
        SizeTier::Gpu => ("8".into(), "16g".into()),
    }
}

pub fn parse_cpus_f64(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|n| *n > 0.0 && *n <= 64.0)
}

/// Parse memory like `512m`, `2g`, `8192` (MiB if bare number) → MiB.
pub fn parse_memory_mib(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s
        .char_indices()
        .find(|(_, c)| c.is_ascii_alphabetic())
        .map_or((s.as_str(), ""), |(i, _)| (&s[..i], &s[i..]));
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(match unit {
        "" | "m" | "mb" | "mi" => n,
        "g" | "gb" | "gi" => n.saturating_mul(1024),
        "k" | "kb" | "ki" => n.saturating_div(1024).max(1),
        "t" | "tb" | "ti" => n.saturating_mul(1024 * 1024),
        "b" => 1,
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolClaim {
    pub worker_id: String,
    pub container: String,
    pub cpus: f64,
    pub memory_mib: u64,
    pub tier: String,
    pub repo: Option<String>,
    pub claimed_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PoolState {
    claims: Vec<PoolClaim>,
}

pub struct ResourcePool {
    path: PathBuf,
    pub max_cpus: f64,
    pub max_memory_mib: u64,
    pub max_workers: u32,
}

impl ResourcePool {
    pub fn from_env() -> Self {
        let max_cpus = std::env::var("GHA_POOL_CPUS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POOL_CPUS);
        let max_memory_mib = std::env::var("GHA_POOL_MEMORY")
            .ok()
            .and_then(|s| parse_memory_mib(&s))
            .or_else(|| {
                std::env::var("GHA_POOL_MEMORY_MIB")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(DEFAULT_POOL_MEMORY_MIB);
        let max_workers = std::env::var("GHA_POOL_MAX_WORKERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_WORKERS);
        let path = pool_state_path();
        Self {
            path,
            max_cpus,
            max_memory_mib,
            max_workers,
        }
    }

    pub fn enabled() -> bool {
        match std::env::var("GHA_POOL_MODE") {
            Ok(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on" | "dynamic"),
            // Default on when pool caps are set, else default **on** for new policy.
            Err(_) => true,
        }
    }

    fn with_lock<F, R>(&self, f: F) -> Result<R, String>
    where
        F: FnOnce(&mut PoolState) -> Result<R, String>,
    {
        struct PoolLockGuard {
            path: PathBuf,
        }

        impl Drop for PoolLockGuard {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.path);
            }
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("pool dir: {e}"))?;
        }
        let lock_path = self.path.with_extension("lock");
        // Exclusive create lock (no unsafe flock; matches InstanceLock style).
        let _guard = {
            let mut acquired = None;
            for attempt in 0..200 {
                let mut opts = OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                match opts.open(&lock_path) {
                    Ok(mut f) => {
                        let _ = writeln!(f, "{}", std::process::id());
                        acquired = Some(PoolLockGuard {
                            path: lock_path.clone(),
                        });
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        if attempt == 0 && super::lock_is_stale(&lock_path) {
                            let _ = fs::remove_file(&lock_path);
                            continue;
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(e) => return Err(format!("pool lock: {e}")),
                }
            }
            acquired.ok_or_else(|| "pool lock timeout".to_string())?
        };
        let buf = fs::read_to_string(&self.path).unwrap_or_default();
        let mut state: PoolState = if buf.trim().is_empty() {
            PoolState::default()
        } else {
            serde_json::from_str(&buf).unwrap_or_default()
        };
        let out = f(&mut state);
        if out.is_ok() {
            let json = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
            fs::write(&self.path, json).map_err(|e| format!("pool write: {e}"))?;
        }
        out
    }

    pub fn usage(&self) -> Result<(f64, u64, usize), String> {
        self.with_lock(|st| {
            let c: f64 = st.claims.iter().map(|c| c.cpus).sum();
            let m: u64 = st.claims.iter().map(|c| c.memory_mib).sum();
            Ok((c, m, st.claims.len()))
        })
    }

    pub fn try_claim(
        &self,
        worker_id: &str,
        container: &str,
        cpus: f64,
        memory_mib: u64,
        tier: SizeTier,
        repo: Option<&str>,
    ) -> Result<bool, String> {
        self.with_lock(|st| {
            // replace existing claim for same worker
            st.claims.retain(|c| c.worker_id != worker_id);
            if st.claims.len() as u32 >= self.max_workers {
                return Ok(false);
            }
            let used_c: f64 = st.claims.iter().map(|c| c.cpus).sum();
            let used_m: u64 = st.claims.iter().map(|c| c.memory_mib).sum();
            if used_c + cpus > self.max_cpus + 1e-9 {
                return Ok(false);
            }
            if used_m.saturating_add(memory_mib) > self.max_memory_mib {
                return Ok(false);
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            st.claims.push(PoolClaim {
                worker_id: worker_id.to_string(),
                container: container.to_string(),
                cpus,
                memory_mib,
                tier: tier.as_str().to_string(),
                repo: repo.map(|s| s.to_string()),
                claimed_at_unix: now,
            });
            Ok(true)
        })
    }

    pub fn release(&self, worker_id: &str) -> Result<(), String> {
        self.with_lock(|st| {
            st.claims.retain(|c| c.worker_id != worker_id);
            Ok(())
        })
    }

    pub fn release_container(&self, container: &str) -> Result<(), String> {
        self.with_lock(|st| {
            st.claims.retain(|c| c.container != container);
            Ok(())
        })
    }

    pub fn claims(&self) -> Result<Vec<PoolClaim>, String> {
        self.with_lock(|st| Ok(st.claims.clone()))
    }
}

fn pool_state_path() -> PathBuf {
    if let Ok(p) = std::env::var("GHA_POOL_STATE") {
        return PathBuf::from(p);
    }
    pool_dir().join("state.json")
}

fn pool_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("gha-runner-ctl/pool")
}

/// Path to the quiesce flag file. Its presence means "admit no new work".
///
/// A file rather than a signal: it survives a supervisor restart, is trivially
/// inspectable by an operator or a deploy script, and needs no PID discovery.
/// Override with `GHA_QUIESCE_FILE`.
pub fn quiesce_path() -> PathBuf {
    if let Ok(p) = std::env::var("GHA_QUIESCE_FILE") {
        return PathBuf::from(p);
    }
    pool_dir().join("quiesce")
}

/// True while admission is paused. Cheap enough to call every tick.
pub fn quiesce_active() -> bool {
    quiesce_active_at(&quiesce_path())
}

/// Pure form, so the predicate is testable without mutating process env.
///
/// A directory does NOT count: a stray `mkdir` should not silently wedge the
/// fleet into a paused state that looks identical to a deliberate one.
///
/// Fail-safe on stat error: `Path::is_file()` collapses every `stat` outcome
/// down to a bare bool via `.unwrap_or(false)` — "genuinely absent" (`ENOENT`)
/// and "could not tell" (permission denied, missing/unmounted parent dir,
/// stale NFS handle, any other I/O error) are indistinguishable and both read
/// as "not quiesced". For a flag whose entire job is to stop admission before
/// a restart, that is the wrong default: it means the one time the probe is
/// unreliable is exactly the time this silently behaves as if nothing were
/// wrong and keeps admitting work — the orphaned-container failure this
/// feature exists to prevent. Only a confirmed-absent file should mean
/// "run normally"; every other stat error fails toward pausing, and is logged
/// so it doesn't masquerade as a quiet, healthy tick.
pub fn quiesce_active_at(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(m) => m.is_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            eprintln!(
                "pool: quiesce probe error on {} ({e}) — failing safe, treating as quiesced",
                path.display()
            );
            true
        }
    }
}

/// Shrink request to fit remaining budget (never below min). Returns None if cannot fit min.
pub fn fit_to_budget(
    want_cpus: f64,
    want_mib: u64,
    free_cpus: f64,
    free_mib: u64,
    min_cpus: f64,
    min_mib: u64,
) -> Option<(f64, u64)> {
    if free_cpus + 1e-9 < min_cpus || free_mib < min_mib {
        return None;
    }
    let c = want_cpus.min(free_cpus).max(min_cpus);
    let m = want_mib.min(free_mib).max(min_mib);
    // if want was larger than free, still ok if we shrank
    if c > free_cpus + 1e-9 || m > free_mib {
        return None;
    }
    Some((c, m))
}

pub fn format_cpus(c: f64) -> String {
    if (c - c.round()).abs() < 1e-9 {
        format!("{}", c.round() as u64)
    } else {
        format!("{c:.2}")
    }
}

pub fn format_memory_mib(m: u64) -> String {
    if m >= 1024 && m.is_multiple_of(1024) {
        format!("{}g", m / 1024)
    } else {
        format!("{m}m")
    }
}

// ---------------------------------------------------------------------------
// Demand-driven scale planner (pure — no I/O, no GitHub, no Podman)
// ---------------------------------------------------------------------------

/// Default cap on new worker registrations per listen tick.
/// Bounds registration storms even when the queue is deep; next tick continues.
pub const DEFAULT_MAX_SPAWN_PER_TICK: u32 = 4;

/// One queued/in-progress job the autoscaler may size and assign a worker to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemandSignal {
    pub job_name: String,
    pub labels: Vec<String>,
    pub repo: String,
}

/// Snapshot of a local pool worker (`{base}-w{N}`) for planning.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerSnapshot {
    pub slot: u32,
    pub worker_id: String,
    pub container: String,
    /// Whether the container process is still running.
    pub running: bool,
    /// Whether this worker is **actively executing a job** (local signal —
    /// container process tree / claim bookkeeping — **not** the demand scan).
    /// Scale-in must never target a busy worker even when the partial RR demand
    /// sample looks empty (busy job may live on an un-scanned prefer-repo).
    pub busy: bool,
    /// Repo this worker is registered/claimed for (`owner/name`), when known.
    /// Ephemeral user-batch workers only serve that one repo until retargeted.
    pub repo: Option<String>,
    /// Seconds since this worker's container was (re)started, when known.
    /// `None` means the age could not be determined (inspect failed / racing
    /// creation) — treated as **not** past the grace window, fail-safe: we
    /// never reclaim a worker whose age we cannot prove (issue #127).
    pub age_secs: Option<u64>,
    /// Positive job-completion signal, independent of `busy`. True only when
    /// there is direct evidence the assigned job's run is over — e.g. the
    /// runner's `Runner.Listener` process has exited, or the Actions API
    /// reports the run/attempt complete. `busy == false` alone must NEVER set
    /// this: a freshly-spawned worker that has not yet been dispatched a job
    /// also has `busy == false`, and the two states are not the same thing
    /// (issue #127 — "busy=0" collapsing "never started" into "finished").
    pub job_completed: bool,
}

/// True when the worker is known to be executing a job (not merely online/idle).
///
/// Independent of demand polling: a busy worker on an un-scanned prefer-repo
/// must still report `busy` so scale-in cannot kill it mid-run.
#[inline]
pub fn is_busy(worker: &WorkerSnapshot) -> bool {
    worker.busy
}

/// Default grace window (seconds since container start) during which a
/// `running && !busy` worker may NOT be reclaimed as "post job exit" absent a
/// positive completion signal. Sized against live evidence (issue #127): the
/// pool was observed reclaiming freshly-spawned, never-dispatched workers
/// 40-70s after `up`, well before GitHub could schedule a job onto them. 90s
/// gives clean margin over the worst observed case.
pub const DEFAULT_SPAWN_GRACE_SECS: u64 = 90;

/// Whether a `running && !busy` worker may be treated as "finished its job"
/// for reclaim purposes.
///
/// This is the fix for issue #127: `busy == false` is ambiguous by itself —
/// it is true both for a worker whose job genuinely finished *and* for one
/// that was never dispatched a job at all. The two must never be
/// conflated. A worker is eligible only when:
///   * it carries a positive completion signal (`job_completed`), which is
///     age-independent — a job that finished in 5s must still be reclaimed
///     promptly, not held for the full grace window (that would leak
///     capacity, the inverse bug); or
///   * it has been running at least `grace_secs` since spawn, at which point
///     "never assigned yet" stops being a plausible explanation for
///     `busy == false` and it is safe to recycle the slot.
///
/// Unknown age (`None`) fails closed: never eligible without proof.
#[inline]
pub fn post_job_exit_eligible(worker: &WorkerSnapshot, grace_secs: u64) -> bool {
    if worker.job_completed {
        return true;
    }
    matches!(worker.age_secs, Some(age) if age >= grace_secs)
}

/// How many consecutive **empty** demand ticks are required before the idle
/// timer may start counting, given a prefer-list of `prefer_len` and a partial
/// round-robin scan width of `scan_per_tick`.
///
/// Derivation (correct at any fleet size — not a magic constant):
///
/// ```text
/// empty_sweep_ticks = ceil(prefer_len / max(scan_per_tick, 1))
///                   = max(1, …)   // at least one observation
/// ```
///
/// With prefer=236 and scan=12 this is 20 ticks — one full prefer-list sweep
/// under partial RR. A single empty partial sample is never enough.
pub fn empty_sweep_ticks(prefer_len: usize, scan_per_tick: usize) -> u32 {
    let width = scan_per_tick.max(1);
    if prefer_len == 0 {
        // No allowlist → one observation is a full "sweep".
        return 1;
    }
    prefer_len.div_ceil(width) as u32
}

/// Whether a streak of empty partial-scan ticks constitutes a **confirmed**
/// empty queue (a full prefer-list sweep has been observed empty).
///
/// Only after this returns true may `idle_secs` start counting toward scale-in.
#[inline]
pub fn demand_empty_confirmed(empty_streak: u32, prefer_len: usize, scan_per_tick: usize) -> bool {
    empty_streak >= empty_sweep_ticks(prefer_len, scan_per_tick)
}

/// Inputs for one scale decision. All numbers are **host-pool** free/max
/// after reap; free resources must not be double-counted with planned spawns.
#[derive(Debug, Clone)]
pub struct ScaleInput {
    /// Matching demand jobs (already filtered by listener labels / GPU affinity).
    pub jobs: Vec<DemandSignal>,
    /// Currently known local pool workers (any state).
    pub workers: Vec<WorkerSnapshot>,
    /// Free CPUs / MiB in the shared pool (max − claimed).
    pub free_cpus: f64,
    pub free_memory_mib: u64,
    /// Hard pool ceilings (for notes / clamp; free_* already respects them).
    pub max_cpus: f64,
    pub max_memory_mib: u64,
    /// Max workers this listen process may own (min of local + pool caps).
    pub max_local_workers: u32,
    /// Total claims already held host-wide (all managers).
    pub host_claim_count: u32,
    /// Host-wide max workers (pool).
    pub max_host_workers: u32,
    /// Force GPU tier resolution (GPU listener).
    pub force_gpu: bool,
    /// True when the listen idle timer has expired with no demand.
    pub idle_expired: bool,
    /// Anti-storm: max new spawns this tick.
    pub max_spawn_per_tick: u32,
    /// CTL-1 primary (Claude): ephemeral workers must exit when idle after a job.
    /// When true, every `running && !busy` worker **that is also
    /// [`post_job_exit_eligible`]** is a scale-in candidate — pinning is fixed
    /// by construction (no warm idle retain), without reclaiming a worker that
    /// simply has not been dispatched a job yet. Wrong-repo preempt remains the
    /// fallback when this flag is false.
    pub ephemeral_post_job_exit: bool,
    /// Grace window (seconds since spawn) protecting a freshly-spawned,
    /// never-assigned worker from being misread as "post job exit" merely
    /// because `busy == false` (issue #127). See [`post_job_exit_eligible`].
    pub spawn_grace_secs: u64,
}

/// One planned worker spin-up.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRequest {
    pub slot: u32,
    pub tier: SizeTier,
    pub cpus: f64,
    pub memory_mib: u64,
    pub job_name: String,
    pub labels: Vec<String>,
    pub repo: String,
}

/// Result of [`plan_scale`]: what to create and what to tear down.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalePlan {
    /// Workers to start (capacity already simulated).
    pub spawns: Vec<SpawnRequest>,
    /// Local worker_ids to tear down (idle scale-in).
    pub scale_in: Vec<String>,
    /// Target running count from queue pressure (pre capacity clamp packing).
    pub desired_count: u32,
    /// Human-readable decision summary for logs.
    pub notes: String,
}

impl Default for ScaleInput {
    fn default() -> Self {
        Self {
            jobs: Vec::new(),
            workers: Vec::new(),
            free_cpus: DEFAULT_POOL_CPUS,
            free_memory_mib: DEFAULT_POOL_MEMORY_MIB,
            max_cpus: DEFAULT_POOL_CPUS,
            max_memory_mib: DEFAULT_POOL_MEMORY_MIB,
            max_local_workers: DEFAULT_MAX_WORKERS,
            host_claim_count: 0,
            max_host_workers: DEFAULT_MAX_WORKERS,
            force_gpu: false,
            idle_expired: false,
            max_spawn_per_tick: DEFAULT_MAX_SPAWN_PER_TICK,
            ephemeral_post_job_exit: false,
            spawn_grace_secs: DEFAULT_SPAWN_GRACE_SECS,
        }
    }
}

/// Pure demand-driven scale decision.
///
/// * **Horizontal:** queue depth → desired **total** worker count (clamped by local +
///   host caps). Spawns only the delta (`desired − already occupied`), not one new
///   worker per job on top of existing capacity.
/// * **Vertical:** each job → tier via [`size_for_job`] + preferred full tier size;
///   if nothing preferred fits, one deferred job may [`fit_to_budget`] shrink into
///   the remainder.
/// * **Capacity:** never plans a spawn that does not fit free CPU **and** memory
///   (and free local/host worker slots). Claimed-or-running workers occupy slots.
/// * **Scale-in:** when there is no demand and `idle_expired`, tear down only
///   **provably-idle** local pool workers (`running && !busy`). Busy workers
///   (local job signal) are never scaled in — demand emptiness alone is not
///   enough when the prefer-list is only partially scanned.
/// * **Post-job exit (CTL-1 primary):** when `ephemeral_post_job_exit`, every idle
///   worker that is [`post_job_exit_eligible`] is reclaimed immediately (with or
///   without demand). Fixes pinning by construction — a worker that exits after
///   one job cannot stick to a repo. An idle worker still inside its
///   `spawn_grace_secs` window (and without a positive completion signal) is
///   protected — `busy == false` right after spawn means "never dispatched a
///   job", not "already finished" (issue #127).
/// * **Preempt (CTL-1 fallback):** when post-job exit is off and demand exists,
///   idle workers claimed for a **different** repo cannot serve it under
///   repo-scoped ephemeral registration. Reclaim them so the next tick can spawn
///   onto the demand repo (avoids pool mem stuck on e.g. scribe-core).
/// * **Storm bound:** at most `max_spawn_per_tick` spawns per call.
///
/// Callers must set `idle_expired` only after a **full prefer-repo sweep** of
/// empty observations ([`demand_empty_confirmed`]) **and** `idle_secs` elapsed.
pub fn plan_scale(input: &ScaleInput) -> ScalePlan {
    let running_local = input.workers.iter().filter(|w| w.running).count() as u32;
    let busy_local = input
        .workers
        .iter()
        .filter(|w| w.running && is_busy(w))
        .count() as u32;
    // Any known local worker (running **or** claimed-but-not-yet-running) holds its
    // slot id — otherwise a mid-spawn claim is double-booked on the next tick.
    let used_slots: std::collections::HashSet<u32> = input.workers.iter().map(|w| w.slot).collect();
    let occupied_local = used_slots.len() as u32;

    // All provably-idle running workers (busy never included) that are also
    // eligible for reclaim — i.e. `busy == false` is not merely "never
    // assigned a job yet" (issue #127: a freshly-spawned worker has
    // `running && !busy` too, indistinguishable from "finished" by that flag
    // alone). See [`post_job_exit_eligible`].
    let idle_workers: Vec<String> = input
        .workers
        .iter()
        .filter(|w| w.running && !is_busy(w) && post_job_exit_eligible(w, input.spawn_grace_secs))
        .map(|w| w.worker_id.clone())
        .collect();
    // Idle workers still inside their spawn grace window — protected from
    // reclaim, but also not yet "available capacity"; they count as covering
    // demand on their own repo below so the planner does not double-spawn
    // while GitHub has not yet had a chance to dispatch to them.
    let grace_protected_idle: std::collections::HashSet<&str> = input
        .workers
        .iter()
        .filter(|w| w.running && !is_busy(w) && !post_job_exit_eligible(w, input.spawn_grace_secs))
        .map(|w| w.worker_id.as_str())
        .collect();

    // --- Idle scale-IN: no demand ---
    if input.jobs.is_empty() {
        // Post-job exit: reclaim idle immediately (do not wait for idle_expired).
        let should_reclaim_idle =
            input.ephemeral_post_job_exit || (input.idle_expired && running_local > 0);
        if should_reclaim_idle && !idle_workers.is_empty() {
            let n = idle_workers.len();
            let why = if input.ephemeral_post_job_exit {
                "post-job-exit"
            } else {
                "idle"
            };
            return ScalePlan {
                spawns: Vec::new(),
                scale_in: idle_workers,
                desired_count: 0,
                notes: format!(
                    "scale-in: {why}, tearing down {n} idle worker(s) (held {busy_local} busy)"
                ),
            };
        }
        if (input.idle_expired || input.ephemeral_post_job_exit) && running_local > 0 {
            return ScalePlan {
                spawns: Vec::new(),
                scale_in: Vec::new(),
                desired_count: 0,
                notes: format!(
                    "hold: no demand, {busy_local} busy worker(s) protected (not scale-in)"
                ),
            };
        }
        return ScalePlan {
            spawns: Vec::new(),
            scale_in: Vec::new(),
            desired_count: 0,
            notes: if running_local > 0 {
                format!(
                    "hold: no demand, {running_local} worker(s) still running (idle not expired; busy={busy_local})"
                )
            } else {
                "idle: no demand, no workers".into()
            },
        };
    }

    // --- Scale-OUT from queue pressure ---
    let demand_repos: std::collections::HashSet<&str> =
        input.jobs.iter().map(|j| j.repo.as_str()).collect();

    // Idle wrong-repo (fallback when post-job exit is off). Same eligibility
    // gate as post-job-exit reclaim: a freshly-spawned worker still inside its
    // grace window has not been dispatched a job yet, so its current repo
    // claim (if any) is not yet provably stale (issue #127).
    let preempt_idle_wrong_repo: Vec<String> = input
        .workers
        .iter()
        .filter(|w| w.running && !is_busy(w) && post_job_exit_eligible(w, input.spawn_grace_secs))
        .filter(|w| match w.repo.as_deref() {
            Some(r) => !demand_repos.contains(r),
            None => false,
        })
        .map(|w| w.worker_id.clone())
        .collect();

    // Capacity that can serve current demand:
    // - post-job exit: **busy** workers cover, and so do idle workers still
    //   inside their spawn grace window (they have not been reclaimed and
    //   may yet take the job) — only *eligible* idle workers are excluded,
    //   since those are the ones about to be torn down this tick.
    // - otherwise: known claim on a demand repo (busy or idle), or unknown claim
    // Busy workers on a *known wrong* repo do not cover but stay protected.
    let covering_local = input
        .workers
        .iter()
        .filter(|w| {
            if input.ephemeral_post_job_exit {
                // Idle exits; only active jobs and grace-protected idlers cover.
                // Claimed-not-running mid-spawn still covers so we do not
                // double-spawn into the same claim.
                if is_busy(w) {
                    return match w.repo.as_deref() {
                        Some(r) => demand_repos.contains(r),
                        None => true,
                    };
                }
                if !w.running {
                    return match w.repo.as_deref() {
                        Some(r) => demand_repos.contains(r),
                        None => true,
                    };
                }
                if grace_protected_idle.contains(w.worker_id.as_str()) {
                    return match w.repo.as_deref() {
                        Some(r) => demand_repos.contains(r),
                        None => true,
                    };
                }
                return false; // idle running, eligible → not covering (about to be reclaimed)
            }
            if !w.running && !is_busy(w) {
                return match w.repo.as_deref() {
                    Some(r) => demand_repos.contains(r),
                    None => true,
                };
            }
            match w.repo.as_deref() {
                Some(r) => demand_repos.contains(r) && (is_busy(w) || w.running),
                None => w.running || is_busy(w),
            }
        })
        .count() as u32;

    let host_slots_left = input
        .max_host_workers
        .saturating_sub(input.host_claim_count);
    let local_slots_left = input.max_local_workers.saturating_sub(occupied_local);
    let slot_cap = host_slots_left.min(local_slots_left);
    let desired_count = (input.jobs.len() as u32).min(input.max_local_workers);
    let need = desired_count.saturating_sub(covering_local);

    let can_spawn_now = slot_cap > 0
        && input.free_cpus + 1e-9 >= DEFAULT_MIN_CPUS
        && input.free_memory_mib >= DEFAULT_MIN_MEMORY_MIB;

    // Reclaim list for this demand tick.
    let reclaim: Vec<String> = if input.ephemeral_post_job_exit {
        idle_workers
    } else if need > 0 {
        preempt_idle_wrong_repo
    } else {
        Vec::new()
    };

    // When we need capacity but free budget is held by idlers, reclaim this tick
    // (listen runs scale_in before spawn; try_claim re-checks after release).
    if need > 0 && !reclaim.is_empty() && !can_spawn_now {
        let n = reclaim.len();
        let tag = if input.ephemeral_post_job_exit {
            "post-job-exit"
        } else {
            "preempt"
        };
        return ScalePlan {
            spawns: Vec::new(),
            scale_in: reclaim,
            desired_count,
            notes: format!(
                "{tag}: reclaim {n} idle worker(s) for demand (covering={covering_local} need={need}); spawn next tick"
            ),
        };
    }

    let spawn_budget = input.max_spawn_per_tick.min(slot_cap).min(need);

    let mut free_c = input.free_cpus.max(0.0);
    let mut free_m = input.free_memory_mib;
    let mut used = used_slots;
    let mut spawns = Vec::new();
    let mut skipped_capacity = 0u32;
    // Jobs that could not take their preferred size (candidate for one shrink fill).
    let mut deferred: Vec<&DemandSignal> = Vec::new();

    for job in &input.jobs {
        if spawns.len() as u32 >= spawn_budget {
            break;
        }
        // Lowest free slot id under max_local_workers.
        let slot = match (0..input.max_local_workers).find(|s| !used.contains(s)) {
            Some(s) => s,
            None => break,
        };

        let tier = size_for_job(&job.job_name, &job.labels, input.force_gpu);
        let (want_c_s, want_m_s) = resources_for_tier(tier);
        let want_c = parse_cpus_f64(&want_c_s).unwrap_or(1.0);
        let want_m = parse_memory_mib(&want_m_s).unwrap_or(2048);

        // Prefer full tier size so a heavy job does not shrink and starve lighter ones.
        if free_c + 1e-9 >= want_c && free_m >= want_m {
            free_c = (free_c - want_c).max(0.0);
            free_m = free_m.saturating_sub(want_m);
            used.insert(slot);
            spawns.push(SpawnRequest {
                slot,
                tier,
                cpus: want_c,
                memory_mib: want_m,
                job_name: job.job_name.clone(),
                labels: job.labels.clone(),
                repo: job.repo.clone(),
            });
        } else {
            skipped_capacity += 1;
            deferred.push(job);
        }
    }

    // Best-effort: if nothing preferred fit but budget ≥ floor, shrink one deferred
    // job into the remainder (keeps a single worker useful under tight headroom).
    if spawns.is_empty() && spawn_budget > 0 {
        if let Some(job) = deferred.first() {
            if let Some(slot) = (0..input.max_local_workers).find(|s| !used.contains(s)) {
                let tier = size_for_job(&job.job_name, &job.labels, input.force_gpu);
                let (want_c_s, want_m_s) = resources_for_tier(tier);
                let want_c = parse_cpus_f64(&want_c_s).unwrap_or(1.0);
                let want_m = parse_memory_mib(&want_m_s).unwrap_or(2048);
                if let Some((c, m)) = fit_to_budget(
                    want_c,
                    want_m,
                    free_c,
                    free_m,
                    DEFAULT_MIN_CPUS,
                    DEFAULT_MIN_MEMORY_MIB,
                ) {
                    free_c = (free_c - c).max(0.0);
                    free_m = free_m.saturating_sub(m);
                    used.insert(slot);
                    spawns.push(SpawnRequest {
                        slot,
                        tier,
                        cpus: c,
                        memory_mib: m,
                        job_name: job.job_name.clone(),
                        labels: job.labels.clone(),
                        repo: job.repo.clone(),
                    });
                    skipped_capacity = skipped_capacity.saturating_sub(1);
                }
            }
        }
    }

    let scale_in = reclaim;
    let exit_tag = if input.ephemeral_post_job_exit {
        " post-job-exit"
    } else {
        ""
    };
    let notes = format!(
        "scale-out: queue={} desired={} spawn={} skip_cap={} covering={} reclaim={}{exit_tag} free_left={:.2}c/{}MiB",
        input.jobs.len(),
        desired_count,
        spawns.len(),
        skipped_capacity,
        covering_local,
        scale_in.len(),
        free_c,
        free_m
    );

    ScalePlan {
        spawns,
        scale_in,
        desired_count,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_override_parses_cpus_and_memory() {
        assert_eq!(
            parse_tier_override("6:12g"),
            Some(("6".into(), "12g".into()))
        );
        assert_eq!(
            parse_tier_override(" 0.5 : 512m "),
            Some(("0.5".into(), "512m".into())),
            "fractional cpus and whitespace must both be accepted"
        );
    }

    /// Malformed input must fall back to the default tier, never reach podman.
    /// A bad --cpus flag fails the container at spawn, which looks like capacity
    /// loss rather than a config typo.
    #[test]
    fn tier_override_rejects_malformed_input() {
        for bad in [
            "12",     // no separator
            "12g",    // memory in the cpu slot, no separator
            ":12g",   // missing cpus
            "6:",     // missing memory
            "0:8g",   // zero cpus
            "-2:8g",  // negative cpus
            "abc:8g", // non-numeric cpus
        ] {
            assert_eq!(parse_tier_override(bad), None, "should reject {bad:?}");
        }
    }

    /// The memory side must be validated as strictly as the cpu side. Before
    /// this test, only `m.is_empty()` was checked here, so `6:0g`/`6:abc`
    /// passed `parse_tier_override` (no "ignoring malformed" log), then
    /// silently failed the *real* `parse_memory_mib` two call-layers
    /// downstream, where the caller does `.unwrap_or(2048)` with no logging
    /// at all. That is a silently-degraded tier (flat 2 GiB regardless of
    /// which tier was overridden) with zero operator-visible signal — the
    /// exact "config typo presented as capacity loss" this feature's own
    /// doc comment says it wants to avoid.
    #[test]
    fn tier_override_rejects_malformed_memory() {
        for bad in [
            "6:0g",  // zero memory parses as a *quantity* but is not usable
            "6:abc", // non-numeric memory
            "6:-5g", // negative memory
            "6:0",   // zero, bare units
        ] {
            assert_eq!(parse_tier_override(bad), None, "should reject {bad:?}");
        }
    }

    /// A missing flag file means "run normally". This is the fail-safe direction:
    /// losing the file must never wedge a host into permanent pause.
    #[test]
    fn quiesce_absent_means_active_admission() {
        let d = std::env::temp_dir().join(format!("gha-quiesce-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(!quiesce_active_at(&d.join("quiesce")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn quiesce_file_pauses_admission() {
        let d = std::env::temp_dir().join(format!("gha-quiesce-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("quiesce");
        std::fs::write(&f, b"").unwrap();
        assert!(quiesce_active_at(&f));
        // Removing it resumes admission — quiesce must be reversible without a restart.
        std::fs::remove_file(&f).unwrap();
        assert!(!quiesce_active_at(&f));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A directory at the flag path must NOT pause the fleet. A stray mkdir
    /// would otherwise be indistinguishable from a deliberate pause, and would
    /// silently stop the host claiming work with no obvious cause.
    #[test]
    fn quiesce_directory_does_not_pause() {
        let d = std::env::temp_dir().join(format!("gha-quiesce-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("quiesce")).unwrap();
        assert!(!quiesce_active_at(&d.join("quiesce")));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A stat error that is NOT "confirmed absent" must fail toward pausing,
    /// not toward normal admission. `Path::is_file()` alone cannot express
    /// this distinction (it folds every error into `false`), which is exactly
    /// how a permission error or an unmounted parent directory would silently
    /// read as "not quiesced" and keep admitting work through the one window
    /// this flag exists to close. A NUL byte in the path is a portable,
    /// privilege-independent way to force `fs::metadata` to fail with
    /// something other than `NotFound` (`InvalidInput`), without relying on
    /// dropping privileges (this suite may run as root, where permission bits
    /// are bypassed and would not otherwise reproduce the error path).
    #[test]
    fn quiesce_probe_error_other_than_not_found_fails_safe() {
        let bogus = PathBuf::from("/tmp/gha-quiesce-\0-invalid");
        assert!(
            quiesce_active_at(&bogus),
            "a stat error that isn't confirmed-absent must be treated as quiesced"
        );
    }

    /// Regression guard for grok's review catch on #107: the scribe repos name their
    /// cargo build+test job exactly "test", and cargo-mutants names its job "mutants".
    /// Both previously reached Medium/the catch-all, which after the memory rebalance
    /// would have put a full cargo compile back on 4g — the historic exit-137 floor.
    #[test]
    fn bare_test_and_mutants_are_large_not_medium() {
        for n in [
            "test",
            "mutants",
            "Build + test",
            "build-and-test",
            "test (ubuntu)",
        ] {
            assert_eq!(
                size_for_job(n, &[], false),
                SizeTier::Large,
                "{n:?} must be Large — it is a Rust compile job on this fleet"
            );
        }
        // Boundary check: these must NOT be swept up by the promotion.
        assert_ne!(size_for_job("pytest", &[], false), SizeTier::Large);
        assert_ne!(size_for_job("latest-docs", &[], false), SizeTier::Large);
    }

    #[test]
    fn tier_gitleaks_micro() {
        assert_eq!(
            size_for_job("gitleaks", &["self-hosted".into()], false),
            SizeTier::Micro
        );
    }

    /// Rust compiles used to default to Medium (2c/4g) and OOM-killed there:
    /// mycelium-l1's "cargo check/test" job exited 137. They now get Large.
    #[test]
    fn tier_cargo_test_large() {
        assert_eq!(
            size_for_job("cargo test", &["self-hosted".into()], false),
            SizeTier::Large
        );
        assert_eq!(
            size_for_job("cargo check/test", &["self-hosted".into()], false),
            SizeTier::Large
        );
    }

    /// A workspace-wide compile is the heaviest shape and gets Xlarge.
    #[test]
    fn tier_cargo_workspace_xlarge() {
        assert_eq!(
            size_for_job("cargo check --workspace", &["self-hosted".into()], false),
            SizeTier::Xlarge
        );
        assert_eq!(
            size_for_job("cargo build (all-targets)", &["self-hosted".into()], false),
            SizeTier::Xlarge
        );
    }

    /// Non-Rust jobs keep the Medium default — this change is scoped to cargo.
    #[test]
    fn tier_non_rust_test_stays_medium() {
        assert_eq!(
            size_for_job("pytest", &["self-hosted".into()], false),
            SizeTier::Medium
        );
        assert_eq!(
            size_for_job("unit test", &["self-hosted".into()], false),
            SizeTier::Medium
        );
    }

    /// Lint-only cargo jobs must not be promoted by the rule above.
    #[test]
    fn tier_cargo_lint_stays_micro() {
        assert_eq!(
            size_for_job("cargo clippy", &["self-hosted".into()], false),
            SizeTier::Micro
        );
        assert_eq!(
            size_for_job("cargo fmt", &["self-hosted".into()], false),
            SizeTier::Micro
        );
    }

    /// An explicit size label still wins over the cargo heuristic.
    #[test]
    fn tier_label_overrides_cargo_rule() {
        assert_eq!(
            size_for_job(
                "cargo check",
                &["self-hosted".into(), "size-small".into()],
                false
            ),
            SizeTier::Small
        );
    }

    #[test]
    fn tier_gpu_label() {
        assert_eq!(
            size_for_job("train", &["self-hosted".into(), "gpu".into()], false),
            SizeTier::Gpu
        );
    }

    #[test]
    fn tier_explicit_large_label() {
        assert_eq!(
            size_for_job("unit", &["self-hosted".into(), "large".into()], false),
            SizeTier::Large
        );
    }

    #[test]
    fn tier_build_and_test_large() {
        assert_eq!(
            size_for_job(
                "Build and Test (local parity)",
                &["self-hosted".into()],
                false
            ),
            SizeTier::Large
        );
    }

    /// Bare product `build` job name (ci.yml) must not land on Medium — OOM 137.
    #[test]
    fn tier_bare_build_large() {
        assert_eq!(
            size_for_job("build", &["self-hosted".into()], false),
            SizeTier::Large
        );
    }

    #[test]
    fn resources_medium_has_headroom() {
        let (c, m) = resources_for_tier(SizeTier::Medium);
        assert_eq!(c, "4");
        assert_eq!(m, "4g");
    }

    #[test]
    fn resources_xlarge_cap() {
        let (c, m) = resources_for_tier(SizeTier::Xlarge);
        assert_eq!(c, "20");
        assert_eq!(m, "28g");
    }

    /// The catch-all for an unrecognised job name is Small, not Medium — an
    /// unrecognised name is more likely light (generator/notifier/doc step) than
    /// an undetected heavy compile, and real compile work is caught explicitly
    /// above this fallback.
    #[test]
    fn tier_unrecognised_name_falls_to_small() {
        assert_eq!(
            size_for_job("some-totally-unknown-job", &["self-hosted".into()], false),
            SizeTier::Small
        );
    }

    /// Observed light job names (fleet-ops pr-ci.yml) must not fall through to the
    /// Medium/Small default paths — they should be recognised as Micro directly.
    #[test]
    fn tier_observed_light_jobs_micro() {
        for name in [
            "quadlet-generate",
            "capture-diff",
            "registry-check",
            "secret-keymap",
            "policy-check",
            "adversarial",
            "yamllint",
            "shellcheck",
            "notify",
        ] {
            assert_eq!(
                size_for_job(name, &["self-hosted".into()], false),
                SizeTier::Micro,
                "job {name} should be Micro"
            );
        }
    }

    #[test]
    fn parse_mem() {
        assert_eq!(parse_memory_mib("512m"), Some(512));
        assert_eq!(parse_memory_mib("2g"), Some(2048));
        assert_eq!(parse_memory_mib("8gb"), Some(8192));
    }

    #[test]
    fn fit_budget() {
        let r = fit_to_budget(2.0, 4096, 1.0, 1024, 0.25, 256).unwrap();
        assert!((r.0 - 1.0).abs() < 1e-9);
        assert_eq!(r.1, 1024);
    }

    #[test]
    fn fit_none_when_empty() {
        assert!(fit_to_budget(1.0, 1024, 0.1, 100, 0.25, 256).is_none());
    }

    fn job(name: &str, labels: &[&str]) -> DemandSignal {
        DemandSignal {
            job_name: name.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            repo: "owner/repo".into(),
        }
    }

    fn base_input(jobs: Vec<DemandSignal>) -> ScaleInput {
        ScaleInput {
            jobs,
            free_cpus: 16.0,
            free_memory_mib: 16 * 1024,
            max_cpus: 16.0,
            max_memory_mib: 16 * 1024,
            max_local_workers: 8,
            host_claim_count: 0,
            max_host_workers: 24,
            max_spawn_per_tick: 8,
            ..ScaleInput::default()
        }
    }

    /// Queue pressure: N matching jobs → up to N planned spawns (horizontal).
    #[test]
    fn scale_queue_pressure_to_count() {
        let jobs = vec![
            job("gitleaks", &["self-hosted"]),
            job("ruff", &["self-hosted"]),
            job("lint", &["self-hosted"]),
        ];
        let plan = plan_scale(&base_input(jobs));
        assert_eq!(plan.spawns.len(), 3, "notes={}", plan.notes);
        assert_eq!(plan.desired_count, 3);
        assert!(plan.scale_in.is_empty());
        // Micro jobs get distinct slots 0,1,2
        let mut slots: Vec<_> = plan.spawns.iter().map(|s| s.slot).collect();
        slots.sort();
        assert_eq!(slots, vec![0, 1, 2]);
    }

    /// Vertical: job size/labels map to tier + preferred resources in the plan.
    #[test]
    fn scale_job_size_vertical() {
        // micro (1c/1g) + large (12c/16g) + medium (4c/4g) = 17c/21g; fits under
        // a 20c/24g free budget (base_input's default 16c/16g is too tight now
        // that Large/Xlarge are sized for a many-core homelab host). Main packing
        // loop only spawns a job at its FULL preferred size (no shrink) when
        // there is enough free budget, in job order — so each of these three
        // gets exactly its resources_for_tier() preferred (cpus, memory_mib).
        let jobs = vec![
            job("gitleaks", &["self-hosted"]),
            job("cargo test", &["self-hosted"]),
            job("pytest", &["self-hosted"]),
        ];
        let mut input = base_input(jobs);
        input.free_cpus = 20.0;
        input.free_memory_mib = 24 * 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 3, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].tier, SizeTier::Micro);
        assert_eq!(plan.spawns[1].tier, SizeTier::Large);
        assert_eq!(plan.spawns[2].tier, SizeTier::Medium);
        assert!((plan.spawns[0].cpus - 1.0).abs() < 1e-9);
        assert_eq!(plan.spawns[0].memory_mib, 1024);
        assert!((plan.spawns[1].cpus - 12.0).abs() < 1e-9);
        assert_eq!(plan.spawns[1].memory_mib, 16 * 1024);
        assert!((plan.spawns[2].cpus - 4.0).abs() < 1e-9);
        assert_eq!(plan.spawns[2].memory_mib, 4 * 1024);
    }

    /// Explicit xlarge label gets full preferred size when budget allows.
    #[test]
    fn scale_xlarge_preferred_when_budget_allows() {
        let mut input = base_input(vec![job("unit", &["self-hosted", "xlarge"])]);
        // Xlarge now prefers 20c/28g; give it headroom above that so the plan
        // grants the full preferred size rather than shrinking to free.
        input.free_cpus = 24.0;
        input.free_memory_mib = 32 * 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1);
        assert_eq!(plan.spawns[0].tier, SizeTier::Xlarge);
        assert!((plan.spawns[0].cpus - 20.0).abs() < 1e-9);
        assert_eq!(plan.spawns[0].memory_mib, 28 * 1024);
    }

    /// Capacity ceiling: never plan more workers than free CPU/memory allow.
    #[test]
    fn scale_capacity_bound_clamp() {
        // 2c free / 4g free → Medium (4c/4g preferred) shrinks to fit; second Medium skipped.
        let mut input = base_input(vec![
            job("pytest", &["self-hosted"]),
            job("unit test", &["self-hosted"]),
            job("ci", &["self-hosted"]),
        ]);
        input.free_cpus = 2.0;
        input.free_memory_mib = 4 * 1024;
        input.max_cpus = 2.0;
        input.max_memory_mib = 4 * 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].tier, SizeTier::Medium);
        // desired_count still reflects queue pressure before packing
        assert_eq!(plan.desired_count, 3);
    }

    /// Full pool: zero free → zero spawns (hard bound).
    #[test]
    fn scale_no_oversubscribe_when_empty_budget() {
        let mut input = base_input(vec![job("cargo test", &["self-hosted"])]);
        input.free_cpus = 0.0;
        input.free_memory_mib = 0;
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty(), "notes={}", plan.notes);
        assert!(plan.scale_in.is_empty());
    }

    /// Max workers clamp (local + host claim count).
    #[test]
    fn scale_max_workers_clamp() {
        let mut input = base_input(vec![
            job("gitleaks", &["self-hosted"]),
            job("ruff", &["self-hosted"]),
            job("fmt", &["self-hosted"]),
        ]);
        input.max_local_workers = 2;
        input.max_spawn_per_tick = 8;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 2, "notes={}", plan.notes);
        assert_eq!(plan.desired_count, 2);
    }

    /// Host claim count reduces available slots.
    #[test]
    fn scale_host_claim_cap() {
        let mut input = base_input(vec![
            job("gitleaks", &["self-hosted"]),
            job("ruff", &["self-hosted"]),
        ]);
        input.max_host_workers = 3;
        input.host_claim_count = 3; // full
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty(), "notes={}", plan.notes);
    }

    /// Anti-storm: max_spawn_per_tick bounds a deep queue.
    #[test]
    fn scale_spawn_per_tick_bound() {
        let jobs: Vec<_> = (0..10)
            .map(|i| job(&format!("gitleaks-{i}"), &["self-hosted"]))
            .collect();
        let mut input = base_input(jobs);
        input.max_spawn_per_tick = 3;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 3, "notes={}", plan.notes);
        assert_eq!(plan.desired_count, 8); // clamped by max_local_workers=8
    }

    // Default test workers are "long since spawned" (age far past the default
    // grace window) — these helpers model the *general* idle/reclaim cases the
    // existing suite exercises, not the issue #127 spawn-race scenario, which
    // gets its own explicit fixtures below (`fresh_worker` / `completed_worker`).
    const LONG_AGO_SECS: u64 = 10 * DEFAULT_SPAWN_GRACE_SECS;

    fn worker(slot: u32, running: bool, busy: bool) -> WorkerSnapshot {
        WorkerSnapshot {
            slot,
            worker_id: format!("runner-w{slot}"),
            container: format!("ctl-w{slot}"),
            running,
            busy,
            repo: None,
            age_secs: Some(LONG_AGO_SECS),
            job_completed: false,
        }
    }

    fn worker_repo(slot: u32, running: bool, busy: bool, repo: &str) -> WorkerSnapshot {
        WorkerSnapshot {
            slot,
            worker_id: format!("runner-w{slot}"),
            container: format!("ctl-w{slot}"),
            running,
            busy,
            repo: Some(repo.into()),
            age_secs: Some(LONG_AGO_SECS),
            job_completed: false,
        }
    }

    /// A worker spawned `secs_ago` seconds ago, never assigned a job
    /// (`busy=false`, no completion signal) — models issue #127's race.
    fn fresh_worker(slot: u32, repo: &str, secs_ago: u64) -> WorkerSnapshot {
        WorkerSnapshot {
            slot,
            worker_id: format!("runner-w{slot}"),
            container: format!("ctl-w{slot}"),
            running: true,
            busy: false,
            repo: Some(repo.into()),
            age_secs: Some(secs_ago),
            job_completed: false,
        }
    }

    /// A worker whose job has genuinely finished (positive completion
    /// signal), regardless of how little time has passed since spawn.
    fn completed_worker(slot: u32, repo: &str, secs_ago: u64) -> WorkerSnapshot {
        WorkerSnapshot {
            slot,
            worker_id: format!("runner-w{slot}"),
            container: format!("ctl-w{slot}"),
            running: true,
            busy: false,
            repo: Some(repo.into()),
            age_secs: Some(secs_ago),
            job_completed: true,
        }
    }

    /// CTL-1 fallback: idle workers on another repo must not block demand for repo B.
    #[test]
    fn preempt_idle_wrong_repo_when_demand() {
        let jobs = vec![DemandSignal {
            job_name: "test".into(),
            labels: vec!["self-hosted".into()],
            repo: "tzervas/aphelion-scribe-daemon".into(),
        }];
        let input = ScaleInput {
            jobs,
            workers: vec![
                worker_repo(0, true, false, "tzervas/aphelion-scribe-core"),
                worker_repo(1, true, false, "tzervas/aphelion-scribe-core"),
                worker_repo(2, true, true, "tzervas/aphelion-scribe-core"), // busy protected
            ],
            // Pool full of medium claims (no free mem) — classic sticky case.
            free_cpus: 0.0,
            free_memory_mib: 0,
            host_claim_count: 3,
            idle_expired: false,
            ephemeral_post_job_exit: false,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(
            plan.spawns.is_empty(),
            "should not spawn without free budget: {}",
            plan.notes
        );
        assert_eq!(plan.scale_in.len(), 2, "notes={}", plan.notes);
        assert!(plan.scale_in.contains(&"runner-w0".into()));
        assert!(plan.scale_in.contains(&"runner-w1".into()));
        assert!(
            !plan.scale_in.contains(&"runner-w2".into()),
            "busy worker must not be preempted"
        );
        assert!(plan.notes.contains("preempt"), "notes={}", plan.notes);
    }

    /// Idle worker already on the demand repo counts as covering — no need to preempt it
    /// when post-job exit is off (warm cover on matching repo).
    #[test]
    fn matching_idle_repo_covers_demand() {
        let jobs = vec![DemandSignal {
            job_name: "test".into(),
            labels: vec!["self-hosted".into()],
            repo: "tzervas/aphelion-scribe-daemon".into(),
        }];
        let input = ScaleInput {
            jobs,
            workers: vec![worker_repo(
                0,
                true,
                false,
                "tzervas/aphelion-scribe-daemon",
            )],
            free_cpus: 14.0,
            free_memory_mib: 12_288,
            host_claim_count: 1,
            idle_expired: false,
            ephemeral_post_job_exit: false,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(
            plan.spawns.is_empty(),
            "covering idle on same repo: {}",
            plan.notes
        );
        assert!(
            plan.scale_in.is_empty(),
            "must not preempt matching repo: {}",
            plan.notes
        );
    }

    /// CTL-1 primary: post-job exit reclaims *all* idle workers, even matching repo.
    #[test]
    fn post_job_exit_reclaims_matching_idle() {
        let jobs = vec![DemandSignal {
            job_name: "test".into(),
            labels: vec!["self-hosted".into()],
            repo: "tzervas/aphelion-scribe-daemon".into(),
        }];
        let input = ScaleInput {
            jobs,
            workers: vec![
                worker_repo(0, true, false, "tzervas/aphelion-scribe-daemon"),
                worker_repo(1, true, true, "tzervas/aphelion-scribe-daemon"),
            ],
            free_cpus: 0.0,
            free_memory_mib: 0,
            host_claim_count: 2,
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty(), "notes={}", plan.notes);
        assert_eq!(plan.scale_in, vec!["runner-w0".to_string()]);
        assert!(
            !plan.scale_in.contains(&"runner-w1".into()),
            "busy protected"
        );
        assert!(plan.notes.contains("post-job-exit"), "notes={}", plan.notes);
    }

    /// CTL-1 primary: no demand → reclaim idle without waiting for idle_expired.
    #[test]
    fn post_job_exit_no_demand_immediate() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![
                worker_repo(0, true, false, "tzervas/aphelion-scribe-core"),
                worker_repo(1, true, true, "tzervas/aphelion-scribe-core"),
            ],
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty());
        assert_eq!(plan.scale_in, vec!["runner-w0".to_string()]);
        assert!(plan.notes.contains("post-job-exit"), "notes={}", plan.notes);
    }

    // -------------------------------------------------------------------
    // Issue #127: a freshly-spawned, never-assigned worker has `busy ==
    // false` for exactly the same reason a genuinely-finished worker does.
    // The planner must not conflate the two. These tests reproduce the live
    // churn (worker reclaimed ~42s after spawn, before GitHub could dispatch
    // a job) against the *pre-fix* code: with the eligibility gate removed
    // (i.e. reverting `post_job_exit_eligible` to `true`), both of the "not
    // reclaimed" assertions below fail — see PR description for the revert
    // check.
    // -------------------------------------------------------------------

    /// A worker spawned 10s ago (well inside the 90s default grace window),
    /// never dispatched a job, with no other demand: must NOT be reclaimed.
    /// Pre-fix, `ephemeral_post_job_exit` alone reclaimed every idle running
    /// worker with no age check at all — this is the exact 42s-after-`up`
    /// churn from issue #127.
    #[test]
    fn issue127_fresh_never_assigned_worker_not_reclaimed_no_demand() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![fresh_worker(0, "tzervas/aphelion-scribe-core", 10)],
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(
            plan.scale_in.is_empty(),
            "freshly-spawned worker reclaimed within grace window: {}",
            plan.notes
        );
    }

    /// Same freshly-spawned worker, but now under demand pressure for its own
    /// repo: it must still not be reclaimed (it may yet take the job), and no
    /// duplicate spawn should be planned for the same repo while it is
    /// covering.
    #[test]
    fn issue127_fresh_never_assigned_worker_not_reclaimed_under_demand() {
        let jobs = vec![DemandSignal {
            job_name: "test".into(),
            labels: vec!["self-hosted".into()],
            repo: "tzervas/aphelion-scribe-core".into(),
        }];
        let input = ScaleInput {
            jobs,
            workers: vec![fresh_worker(0, "tzervas/aphelion-scribe-core", 10)],
            free_cpus: 14.0,
            free_memory_mib: 12_288,
            host_claim_count: 1,
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(
            !plan.scale_in.contains(&"runner-w0".to_string()),
            "freshly-spawned worker reclaimed under demand within grace window: {}",
            plan.notes
        );
        assert!(
            plan.spawns.is_empty(),
            "must not double-spawn while grace-protected worker covers demand: {}",
            plan.notes
        );
    }

    /// A worker whose job genuinely completed (positive completion signal),
    /// only 5s after spawn: must still be reclaimed promptly. Proves the fix
    /// does not introduce the inverse bug (holding finished workers for the
    /// full grace window would leak capacity).
    #[test]
    fn issue127_completed_worker_reclaimed_promptly_despite_being_fresh() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![completed_worker(0, "tzervas/aphelion-scribe-core", 5)],
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert_eq!(
            plan.scale_in,
            vec!["runner-w0".to_string()],
            "genuinely-completed worker not reclaimed: {}",
            plan.notes
        );
    }

    /// Once a never-assigned worker ages past the grace window, it becomes
    /// fair game for reclaim (the mechanism must not leak capacity forever
    /// on a worker GitHub never dispatches to).
    #[test]
    fn issue127_never_assigned_worker_reclaimed_after_grace_expires() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![fresh_worker(
                0,
                "tzervas/aphelion-scribe-core",
                DEFAULT_SPAWN_GRACE_SECS + 1,
            )],
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert_eq!(
            plan.scale_in,
            vec!["runner-w0".to_string()],
            "worker past grace window with no job never reclaimed: {}",
            plan.notes
        );
    }

    /// Unknown age (inspect failed / racing creation) fails closed — never
    /// reclaimed absent proof of age or a completion signal.
    #[test]
    fn issue127_unknown_age_fails_closed_not_reclaimed() {
        let mut w = fresh_worker(0, "tzervas/aphelion-scribe-core", 0);
        w.age_secs = None;
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![w],
            idle_expired: false,
            ephemeral_post_job_exit: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(
            plan.scale_in.is_empty(),
            "unknown-age worker reclaimed without proof: {}",
            plan.notes
        );
    }

    /// Direct unit coverage of the eligibility predicate itself.
    #[test]
    fn post_job_exit_eligible_unit() {
        assert!(!post_job_exit_eligible(
            &fresh_worker(0, "r", 10),
            DEFAULT_SPAWN_GRACE_SECS
        ));
        assert!(post_job_exit_eligible(
            &fresh_worker(0, "r", DEFAULT_SPAWN_GRACE_SECS),
            DEFAULT_SPAWN_GRACE_SECS
        ));
        assert!(post_job_exit_eligible(
            &completed_worker(0, "r", 0),
            DEFAULT_SPAWN_GRACE_SECS
        ));
        let mut w = fresh_worker(0, "r", 0);
        w.age_secs = None;
        assert!(!post_job_exit_eligible(&w, DEFAULT_SPAWN_GRACE_SECS));
    }

    /// Idle scale-in: no jobs + idle_expired → tear down **idle** running pool workers.
    #[test]
    fn scale_idle_scale_in() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![
                worker(0, true, false),
                worker(1, true, false),
                worker(2, false, false), // already dead — not in scale_in
            ],
            idle_expired: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty());
        assert_eq!(plan.desired_count, 0);
        assert_eq!(plan.scale_in.len(), 2);
        assert!(plan.scale_in.contains(&"runner-w0".into()));
        assert!(plan.scale_in.contains(&"runner-w1".into()));
    }

    /// Idle but timer not expired: hold workers (no scale-in yet).
    #[test]
    fn scale_idle_hold_before_timeout() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![worker(0, true, false)],
            idle_expired: false,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty());
        assert!(plan.scale_in.is_empty());
        assert!(plan.notes.contains("hold"));
    }

    /// **Capacity-safety regression (a):** a busy worker on an un-scanned prefer-repo
    /// must NOT be scaled in when the partial demand sample looks empty.
    ///
    /// Old behavior: `jobs.is_empty() && idle_expired` → tear down every running
    /// worker, including ones still executing a job. New: only `!busy` workers.
    #[test]
    fn scale_idle_skips_busy_worker_on_unscanned_repo() {
        // Partial RR demand sample returned empty (busy job lives on a repo not
        // in this tick's scan window), idle timer has fired, but w0 is mid-job.
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![
                worker(0, true, true),  // busy mid-job → PROTECTED
                worker(1, true, false), // idle → eligible
            ],
            idle_expired: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty());
        assert_eq!(plan.scale_in, vec!["runner-w1".to_string()]);
        assert!(!plan.scale_in.iter().any(|id| id == "runner-w0"));
        assert!(plan.notes.contains("held"), "notes={}", plan.notes);
        assert!(is_busy(&input.workers[0]));
        assert!(!is_busy(&input.workers[1]));
    }

    /// All running workers busy → scale-in list empty (never kill mid-run fleet).
    #[test]
    fn scale_idle_all_busy_no_scale_in() {
        let input = ScaleInput {
            jobs: Vec::new(),
            workers: vec![worker(0, true, true), worker(1, true, true)],
            idle_expired: true,
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty());
        assert!(
            plan.scale_in.is_empty(),
            "must not scale-in busy workers: {:?}",
            plan.scale_in
        );
        assert!(plan.notes.contains("busy"), "notes={}", plan.notes);
    }

    /// **Capacity-safety regression (b):** idle scale-in only after a full prefer-list
    /// sweep of empty observations — modeled via the pure sweep-gate helpers.
    ///
    /// Old behavior: one empty partial RR tick started `idle_secs`. New: require
    /// `ceil(prefer_len / scan_width)` consecutive empty ticks first.
    #[test]
    fn demand_empty_gate_requires_full_prefer_sweep() {
        // Fleet-sized prefer-list (236) with partial scan width 12 → 20 ticks.
        assert_eq!(empty_sweep_ticks(236, 12), 20);
        assert_eq!(empty_sweep_ticks(12, 12), 1);
        assert_eq!(empty_sweep_ticks(13, 12), 2);
        assert_eq!(empty_sweep_ticks(1, 12), 1);
        assert_eq!(empty_sweep_ticks(0, 12), 1);
        assert_eq!(empty_sweep_ticks(100, 0), 100); // width floors to 1

        // Single partial-empty tick is NOT confirmed empty.
        assert!(!demand_empty_confirmed(1, 236, 12));
        assert!(!demand_empty_confirmed(19, 236, 12));
        // Full sweep of empty observations → confirmed; idle_secs may start.
        assert!(demand_empty_confirmed(20, 236, 12));
        assert!(demand_empty_confirmed(21, 236, 12));

        // Small allowlist: one empty tick is a full sweep.
        assert!(demand_empty_confirmed(1, 6, 12));
        assert!(!demand_empty_confirmed(0, 6, 12));
    }

    /// Planner still holds when idle_expired is false (caller has not completed
    /// full-sweep empty + idle_secs). Models partial-scan: empty jobs alone ≠ scale-in.
    #[test]
    fn scale_idle_no_scale_before_sweep_gate() {
        let input = ScaleInput {
            jobs: Vec::new(), // partial sample empty
            workers: vec![worker(0, true, false)],
            idle_expired: false, // sweep gate / idle_secs not yet satisfied
            ..ScaleInput::default()
        };
        let plan = plan_scale(&input);
        assert!(plan.scale_in.is_empty());
        assert!(plan.spawns.is_empty());
    }

    /// Occupied workers hold slots; deficit spawns use the next free slot id.
    #[test]
    fn scale_skips_occupied_slots() {
        // 2 jobs, 1 already up → need exactly one more on slot 1.
        let mut input = base_input(vec![
            job("gitleaks", &["self-hosted"]),
            job("ruff", &["self-hosted"]),
        ]);
        input.workers = vec![worker(0, true, false)];
        input.host_claim_count = 1;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].slot, 1);
        assert_eq!(plan.desired_count, 2);
    }

    /// Claimed-but-not-running still occupies the slot (avoids double-book mid-spawn).
    #[test]
    fn scale_claimed_not_running_occupies_slot() {
        let mut input = base_input(vec![
            job("gitleaks", &["self-hosted"]),
            job("ruff", &["self-hosted"]),
        ]);
        input.workers = vec![worker(0, false, false)];
        input.host_claim_count = 1;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].slot, 1);
    }

    /// One job + one occupied worker → no over-spawn (desired already met).
    #[test]
    fn scale_no_overspawn_when_covered() {
        let mut input = base_input(vec![job("gitleaks", &["self-hosted"])]);
        input.workers = vec![worker(0, true, false)];
        input.host_claim_count = 1;
        let plan = plan_scale(&input);
        assert!(plan.spawns.is_empty(), "notes={}", plan.notes);
        assert_eq!(plan.desired_count, 1);
    }

    /// Under tight budget, a micro job can still fit after a large one is skipped.
    #[test]
    fn scale_skips_large_allows_micro() {
        let mut input = base_input(vec![
            job("unit", &["self-hosted", "xlarge"]), // 20c/28g — won't fit
            job("gitleaks", &["self-hosted"]),       // micro (1c/1g) — fits
        ]);
        input.free_cpus = 2.0;
        input.free_memory_mib = 3 * 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].tier, SizeTier::Micro);
        assert_eq!(plan.spawns[0].job_name, "gitleaks");
    }
}

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
use std::path::PathBuf;
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
    /// light unit tests, ruff, detect
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
    // Medium-default non-Rust test/build (pytest, generic ci)
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
    SizeTier::Medium
}

fn name_contains_any(name: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| name.contains(n))
}

/// Map tier → (cpus string, memory string) for podman.
/// Caps: xlarge/gpu ≤ 16 CPU / 16 GiB (host pool default matches).
pub fn resources_for_tier(tier: SizeTier) -> (String, String) {
    match tier {
        SizeTier::Micro => ("0.25".into(), "512m".into()),
        SizeTier::Small => ("0.5".into(), "1g".into()),
        // Medium crates / cargo check — 2c/4g avoids OOM on self-hosted workers
        SizeTier::Medium => ("2".into(), "4g".into()),
        SizeTier::Large => ("4".into(), "8g".into()),
        SizeTier::Xlarge => ("8".into(), "16g".into()),
        // GPU jobs: solid host CPU/RAM for data loaders + full device on GPU slice
        SizeTier::Gpu => ("4".into(), "8g".into()),
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
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("pool dir: {e}"))?;
        }
        let lock_path = self.path.with_extension("lock");
        // Exclusive create lock (no unsafe flock; matches InstanceLock style).
        let _guard = {
            let mut acquired = None;
            for _ in 0..200 {
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
                        acquired = Some(lock_path.clone());
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
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
        let _ = fs::remove_file(&_guard);
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
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("gha-runner-ctl/pool/state.json")
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
}

/// True when the worker is known to be executing a job (not merely online/idle).
///
/// Independent of demand polling: a busy worker on an un-scanned prefer-repo
/// must still report `busy` so scale-in cannot kill it mid-run.
#[inline]
pub fn is_busy(worker: &WorkerSnapshot) -> bool {
    worker.busy
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

    // --- Idle scale-IN: no demand, idle timer fired → tear down *idle* pool workers ---
    if input.jobs.is_empty() {
        if input.idle_expired && running_local > 0 {
            // Never down a busy worker — even when the (partial) demand sample is empty.
            let scale_in: Vec<String> = input
                .workers
                .iter()
                .filter(|w| w.running && !is_busy(w))
                .map(|w| w.worker_id.clone())
                .collect();
            if scale_in.is_empty() {
                return ScalePlan {
                    spawns: Vec::new(),
                    scale_in: Vec::new(),
                    desired_count: 0,
                    notes: format!(
                        "hold: no demand, {busy_local} busy worker(s) protected (not scale-in)"
                    ),
                };
            }
            let n = scale_in.len();
            return ScalePlan {
                spawns: Vec::new(),
                scale_in,
                desired_count: 0,
                notes: format!(
                    "scale-in: idle, tearing down {n} idle worker(s) (held {busy_local} busy)"
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
    let host_slots_left = input
        .max_host_workers
        .saturating_sub(input.host_claim_count);
    let local_slots_left = input.max_local_workers.saturating_sub(occupied_local);
    let slot_cap = host_slots_left.min(local_slots_left);
    // Desired total workers from queue pressure (before packing failures).
    let desired_count = (input.jobs.len() as u32)
        .min(input.max_local_workers)
        .min(occupied_local.saturating_add(host_slots_left));

    // Only plan the deficit toward desired — do not stack N new spawns when N
    // workers already cover N (queued or in_progress) jobs.
    let need = desired_count.saturating_sub(occupied_local);
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

    let notes = format!(
        "scale-out: queue={} desired={} spawn={} skip_cap={} free_left={:.2}c/{}MiB",
        input.jobs.len(),
        desired_count,
        spawns.len(),
        skipped_capacity,
        free_c,
        free_m
    );

    ScalePlan {
        spawns,
        scale_in: Vec::new(),
        desired_count,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(c, "2");
        assert_eq!(m, "4g");
    }

    #[test]
    fn resources_xlarge_cap() {
        let (c, m) = resources_for_tier(SizeTier::Xlarge);
        assert_eq!(c, "8");
        assert_eq!(m, "16g");
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
        // micro (0.25c/512) + large (4c/8g) + medium (2c/4g) fit under 16c/16g.
        let jobs = vec![
            job("gitleaks", &["self-hosted"]),
            job("cargo test", &["self-hosted"]),
            job("pytest", &["self-hosted"]),
        ];
        let plan = plan_scale(&base_input(jobs));
        assert_eq!(plan.spawns.len(), 3, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].tier, SizeTier::Micro);
        assert_eq!(plan.spawns[1].tier, SizeTier::Large);
        assert_eq!(plan.spawns[2].tier, SizeTier::Medium);
        assert!((plan.spawns[0].cpus - 0.25).abs() < 1e-9);
        assert_eq!(plan.spawns[0].memory_mib, 512);
        assert!((plan.spawns[1].cpus - 4.0).abs() < 1e-9);
        assert_eq!(plan.spawns[1].memory_mib, 8 * 1024);
        assert!((plan.spawns[2].cpus - 2.0).abs() < 1e-9);
        assert_eq!(plan.spawns[2].memory_mib, 4 * 1024);
    }

    /// Explicit xlarge label gets full preferred size when budget allows.
    #[test]
    fn scale_xlarge_preferred_when_budget_allows() {
        let mut input = base_input(vec![job("unit", &["self-hosted", "xlarge"])]);
        input.free_cpus = 16.0;
        input.free_memory_mib = 16 * 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1);
        assert_eq!(plan.spawns[0].tier, SizeTier::Xlarge);
        assert!((plan.spawns[0].cpus - 8.0).abs() < 1e-9);
        assert_eq!(plan.spawns[0].memory_mib, 16 * 1024);
    }

    /// Capacity ceiling: never plan more workers than free CPU/memory allow.
    #[test]
    fn scale_capacity_bound_clamp() {
        // 2c free / 4g free → at most one Medium (2c/4g); second Medium skipped.
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

    fn worker(slot: u32, running: bool, busy: bool) -> WorkerSnapshot {
        WorkerSnapshot {
            slot,
            worker_id: format!("runner-w{slot}"),
            container: format!("ctl-w{slot}"),
            running,
            busy,
        }
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
            job("unit", &["self-hosted", "xlarge"]), // 8c/16g — won't fit
            job("gitleaks", &["self-hosted"]),       // micro — fits
        ]);
        input.free_cpus = 1.0;
        input.free_memory_mib = 1024;
        let plan = plan_scale(&input);
        assert_eq!(plan.spawns.len(), 1, "notes={}", plan.notes);
        assert_eq!(plan.spawns[0].tier, SizeTier::Micro);
        assert_eq!(plan.spawns[0].job_name, "gitleaks");
    }
}

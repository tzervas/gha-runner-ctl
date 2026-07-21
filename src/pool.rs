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

/// Default host pool: 8 cores / 8 GiB for all ephemeral work containers.
pub const DEFAULT_POOL_CPUS: f64 = 8.0;
pub const DEFAULT_POOL_MEMORY_MIB: u64 = 8192;
pub const DEFAULT_MAX_WORKERS: u32 = 24;
/// Smallest worker: 250m CPU / 256 MiB.
pub const DEFAULT_MIN_CPUS: f64 = 0.25;
pub const DEFAULT_MIN_MEMORY_MIB: u64 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeTier {
    /// fleet-security / lint / gitleaks-class
    Micro,
    /// light unit tests, ruff
    Small,
    /// default cargo test / full CI
    Medium,
    /// heavy build, multi-crate, release
    Large,
    /// GPU jobs (still claims CPU/RAM from pool)
    Gpu,
}

impl SizeTier {
    pub fn as_str(self) -> &'static str {
        match self {
            SizeTier::Micro => "micro",
            SizeTier::Small => "small",
            SizeTier::Medium => "medium",
            SizeTier::Large => "large",
            SizeTier::Gpu => "gpu",
        }
    }
}

/// Automatic size from job name + labels (no workflow knobs required).
pub fn size_for_job(job_name: &str, labels: &[String], force_gpu: bool) -> SizeTier {
    let name = job_name.to_ascii_lowercase();
    let labs: Vec<String> = labels
        .iter()
        .map(|l| l.trim().to_ascii_lowercase())
        .collect();
    let has_gpu = force_gpu
        || labs.iter().any(|l| {
            l == "gpu" || l.starts_with("gpu-slice") || l == "cuda" || l.contains("nvidia")
        });
    if has_gpu {
        return SizeTier::Gpu;
    }
    // Heavy signals
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
        ],
    ) {
        return SizeTier::Large;
    }
    // Light / security / lint
    if name_contains_any(
        &name,
        &[
            "gitleaks", "trivy", "license", "lint", "ruff", "fmt", "format", "clippy", "typos",
            "markdown", "docs", "spell", "security", "reuse", "sbom",
        ],
    ) {
        return SizeTier::Micro;
    }
    // Medium-default cargo / pytest
    if name_contains_any(
        &name,
        &["test", "check", "build", "cargo", "pytest", "ci", "unit"],
    ) {
        return SizeTier::Medium;
    }
    // fleet-ci / fleet-security workflow job names
    if name.contains("fleet-security") || name.contains("noop") {
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
pub fn resources_for_tier(tier: SizeTier) -> (String, String) {
    match tier {
        SizeTier::Micro => ("0.25".into(), "512m".into()),
        SizeTier::Small => ("0.5".into(), "1g".into()),
        SizeTier::Medium => ("1".into(), "2g".into()),
        SizeTier::Large => ("2".into(), "4g".into()),
        SizeTier::Gpu => ("2".into(), "4g".into()),
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
    if m >= 1024 && m % 1024 == 0 {
        format!("{}g", m / 1024)
    } else {
        format!("{m}m")
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

    #[test]
    fn tier_cargo_test_medium() {
        assert_eq!(
            size_for_job("cargo test", &["self-hosted".into()], false),
            SizeTier::Medium
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
}

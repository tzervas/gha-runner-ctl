//! Workflow-selectable work images and cross-arch emulation (#28).
//!
//! Fleet runners are Podman containers with **no** nested engine. Jobs that need
//! a specific distro/arch must select it at **spawn** time via `runs-on` labels
//! (e.g. `ubuntu-24.04`, `arm64`) rather than starting podman/docker inside the job.
//!
//! Pure resolution helpers live here so unit tests need no Podman/host binfmt.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Labels that never select a work image (fleet / size / capability tokens).
const RESERVED_LABELS: &[&str] = &[
    "self-hosted",
    "linux",
    "windows",
    "macos",
    "macOS",
    "podman",
    "docker",
    "micro",
    "small",
    "medium",
    "large",
    "xlarge",
    "x-large",
    "huge",
    "gpu",
    "cuda",
    "nvidia",
    // arch tokens are handled separately
    "x64",
    "x86_64",
    "amd64",
    "x86",
    "i386",
    "i686",
    "arm64",
    "aarch64",
    "arm",
    "armv7",
    "riscv64",
    "riscv",
    "s390x",
    "ppc64le",
];

/// OCI platform string + actions/runner arch segment for a target architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetArch {
    /// linux/amd64 — actions/runner asset `x64`
    Amd64,
    /// linux/arm64 — `arm64`
    Arm64,
    /// linux/arm/v7 — `arm`
    Arm,
    /// linux/riscv64 (experimental; runner kit may need a custom seed)
    Riscv64,
    /// linux/386
    X86,
    /// linux/s390x
    S390x,
    /// linux/ppc64le
    Ppc64le,
}

impl TargetArch {
    /// Podman/OCI `--platform` value.
    pub fn platform(self) -> &'static str {
        match self {
            TargetArch::Amd64 => "linux/amd64",
            TargetArch::Arm64 => "linux/arm64",
            TargetArch::Arm => "linux/arm/v7",
            TargetArch::Riscv64 => "linux/riscv64",
            TargetArch::X86 => "linux/386",
            TargetArch::S390x => "linux/s390x",
            TargetArch::Ppc64le => "linux/ppc64le",
        }
    }

    /// `actions/runner` release asset arch segment (`x64`, `arm64`, `arm`).
    /// Returns `None` when there is no official asset name we know of.
    pub fn runner_arch(self) -> Option<&'static str> {
        match self {
            TargetArch::Amd64 => Some("x64"),
            TargetArch::Arm64 => Some("arm64"),
            TargetArch::Arm => Some("arm"),
            TargetArch::Riscv64 | TargetArch::X86 | TargetArch::S390x | TargetArch::Ppc64le => None,
        }
    }

    /// Short label commonly used on `runs-on` (prefer fleet-style names).
    pub fn label(self) -> &'static str {
        match self {
            TargetArch::Amd64 => "x64",
            TargetArch::Arm64 => "arm64",
            TargetArch::Arm => "arm",
            TargetArch::Riscv64 => "riscv64",
            TargetArch::X86 => "x86",
            TargetArch::S390x => "s390x",
            TargetArch::Ppc64le => "ppc64le",
        }
    }

    /// QEMU/`binfmt_misc` interpreter name fragments used when scanning registrations.
    pub fn binfmt_markers(self) -> &'static [&'static str] {
        match self {
            TargetArch::Amd64 => &["qemu-x86_64", "qemu-x86_64-static", "x86_64"],
            TargetArch::Arm64 => &["qemu-aarch64", "qemu-aarch64-static", "aarch64"],
            TargetArch::Arm => &["qemu-arm", "qemu-arm-static"],
            TargetArch::Riscv64 => &["qemu-riscv64", "qemu-riscv64-static", "riscv64"],
            TargetArch::X86 => &["qemu-i386", "qemu-i386-static", "i386"],
            TargetArch::S390x => &["qemu-s390x", "qemu-s390x-static", "s390x"],
            TargetArch::Ppc64le => &["qemu-ppc64le", "qemu-ppc64le-static", "ppc64le"],
        }
    }

    /// Parse a single `runs-on` arch token (case-insensitive).
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "x64" | "x86_64" | "amd64" => Some(TargetArch::Amd64),
            "arm64" | "aarch64" => Some(TargetArch::Arm64),
            "arm" | "armv7" | "armhf" => Some(TargetArch::Arm),
            "riscv64" | "riscv" => Some(TargetArch::Riscv64),
            "x86" | "i386" | "i686" => Some(TargetArch::X86),
            "s390x" => Some(TargetArch::S390x),
            "ppc64le" => Some(TargetArch::Ppc64le),
            _ => None,
        }
    }

    /// Host CPU from `std::env::consts::ARCH`.
    pub fn host() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => TargetArch::Amd64,
            "aarch64" => TargetArch::Arm64,
            "arm" => TargetArch::Arm,
            "riscv64" => TargetArch::Riscv64,
            "x86" => TargetArch::X86,
            "s390x" => TargetArch::S390x,
            "powerpc64" => TargetArch::Ppc64le,
            other => {
                // Unknown → treat as amd64 for marker purposes; platform string uses raw.
                let _ = other;
                TargetArch::Amd64
            }
        }
    }
}

/// Label → OCI image map (config file + built-in defaults).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageMap {
    /// `ubuntu-24.04` → `docker.io/library/ubuntu:24.04`
    pub images: BTreeMap<String, String>,
    /// Optional arch label → platform override (normally built-in).
    pub arch_platforms: BTreeMap<String, String>,
}

/// JSON shape for the image map file.
#[derive(Debug, Deserialize)]
struct ImageMapJson {
    #[serde(default)]
    images: BTreeMap<String, String>,
    /// Optional: `"arm64": "linux/arm64"`
    #[serde(default)]
    arches: BTreeMap<String, String>,
}

impl ImageMap {
    /// Built-in defaults for common distro draw-in cells (mycelium-lang use case).
    pub fn builtin() -> Self {
        let mut images = BTreeMap::new();
        for (k, v) in [
            ("ubuntu-24.04", "docker.io/library/ubuntu:24.04"),
            ("ubuntu-22.04", "docker.io/library/ubuntu:22.04"),
            ("ubuntu-20.04", "docker.io/library/ubuntu:20.04"),
            ("debian-bookworm", "docker.io/library/debian:bookworm"),
            ("debian-12", "docker.io/library/debian:bookworm"),
            ("debian-bullseye", "docker.io/library/debian:bullseye"),
            ("debian-11", "docker.io/library/debian:bullseye"),
            ("rocky-9", "docker.io/library/rockylinux:9"),
            ("rockylinux-9", "docker.io/library/rockylinux:9"),
            ("fedora-40", "docker.io/library/fedora:40"),
            ("fedora-41", "docker.io/library/fedora:41"),
            ("alpine-3.20", "docker.io/library/alpine:3.20"),
            ("alpine", "docker.io/library/alpine:3.20"),
        ] {
            images.insert(k.to_string(), v.to_string());
        }
        Self {
            images,
            arch_platforms: BTreeMap::new(),
        }
    }

    /// Merge `other` on top of `self` (other wins on key conflict).
    pub fn merge_over(mut self, other: ImageMap) -> Self {
        self.images.extend(other.images);
        self.arch_platforms.extend(other.arch_platforms);
        self
    }
}

/// Parse image-map file contents (JSON or minimal TOML `[images]` / `[arches]`).
pub fn parse_image_map(content: &str) -> Result<ImageMap, String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(ImageMap::default());
    }
    if trimmed.starts_with('{') {
        return parse_image_map_json(trimmed);
    }
    parse_image_map_toml_lite(trimmed)
}

fn parse_image_map_json(content: &str) -> Result<ImageMap, String> {
    let v: ImageMapJson =
        serde_json::from_str(content).map_err(|e| format!("image-map JSON: {e}"))?;
    validate_image_map(ImageMap {
        images: v.images,
        arch_platforms: v.arches,
    })
}

/// Minimal TOML: only `[images]` / `[arches]` tables of `key = "value"` string pairs.
/// Avoids a new `toml` crate dependency for this draft.
fn parse_image_map_toml_lite(content: &str) -> Result<ImageMap, String> {
    let mut images = BTreeMap::new();
    let mut arch_platforms = BTreeMap::new();
    let mut section: Option<&str> = None;

    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim();
            section = match name {
                "images" => Some("images"),
                "arches" => Some("arches"),
                other => {
                    return Err(format!(
                        "image-map TOML: unsupported section [{other}] at line {}",
                        lineno + 1
                    ));
                }
            };
            continue;
        }
        let Some(sec) = section else {
            return Err(format!(
                "image-map TOML: key outside [images]/[arches] at line {}",
                lineno + 1
            ));
        };
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!(
                "image-map TOML: expected key = \"value\" at line {}",
                lineno + 1
            ));
        };
        let key = k.trim().trim_matches('"').to_string();
        let val = v.trim().trim_matches('"').to_string();
        if key.is_empty() || val.is_empty() {
            return Err(format!(
                "image-map TOML: empty key/value at line {}",
                lineno + 1
            ));
        }
        match sec {
            "images" => {
                images.insert(key, val);
            }
            "arches" => {
                arch_platforms.insert(key, val);
            }
            _ => unreachable!(),
        }
    }
    validate_image_map(ImageMap {
        images,
        arch_platforms,
    })
}

fn validate_image_map(map: ImageMap) -> Result<ImageMap, String> {
    for (k, v) in &map.images {
        if k.is_empty() || k.len() > 64 {
            return Err(format!("image-map: invalid label key `{k}`"));
        }
        // Labels use same charset as runner labels.
        if !k
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!("image-map: unsafe label key `{k}`"));
        }
        if !crate::is_safe_image(v) {
            return Err(format!("image-map: unsafe image ref for `{k}`: {v}"));
        }
    }
    for (k, v) in &map.arch_platforms {
        if k.is_empty()
            || k.len() > 32
            || !k
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!("image-map: invalid arch key `{k}`"));
        }
        if v.is_empty()
            || v.len() > 64
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        {
            return Err(format!("image-map: invalid platform for arch `{k}`: {v}"));
        }
    }
    Ok(map)
}

/// Load map: builtin defaults, then optional file overrides.
pub fn load_image_map(path: Option<&Path>) -> Result<ImageMap, String> {
    let base = ImageMap::builtin();
    let Some(p) = path else {
        return Ok(base);
    };
    let content =
        std::fs::read_to_string(p).map_err(|e| format!("read image-map {}: {e}", p.display()))?;
    let file_map = parse_image_map(&content)?;
    Ok(base.merge_over(file_map))
}

/// Result of scanning job labels for image + arch selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobImageArch {
    /// Selected work image (always set; may equal fleet default).
    pub image: String,
    /// Label that selected the image, if any (e.g. `ubuntu-24.04`).
    pub image_label: Option<String>,
    /// Target arch from labels, if an arch token was present.
    pub arch: Option<TargetArch>,
    /// Podman `--platform` value when cross-arch (or explicit non-host) is requested.
    pub platform: Option<String>,
    /// Whether `--platform` is required (target ≠ host).
    pub needs_emulation: bool,
}

/// Labels that are reserved (not image selectors).
pub fn is_reserved_label(label: &str) -> bool {
    let l = label.trim().to_ascii_lowercase();
    if RESERVED_LABELS.iter().any(|r| r.eq_ignore_ascii_case(&l)) {
        return true;
    }
    if l.starts_with("size-") || l.starts_with("gpu-slice") {
        return true;
    }
    // Arch tokens
    TargetArch::from_label(&l).is_some()
}

/// Resolve work image + arch from job `runs-on` labels.
///
/// * Image: first label (in workflow order) that appears as a key in `map.images`.
/// * Arch: prefer non-amd64 tokens if present; else amd64 if `x64`/`amd64` present;
///   else `None` (keep fleet default / host native, no `--platform`).
/// * Default image used when no image label matches.
pub fn resolve_job_image_arch(
    labels: &[String],
    map: &ImageMap,
    default_image: &str,
) -> JobImageArch {
    let mut image_label: Option<String> = None;
    let mut image = default_image.to_string();

    for raw in labels {
        let lab = raw.trim().to_ascii_lowercase();
        if lab.is_empty() || is_reserved_label(&lab) {
            continue;
        }
        // Exact key in map (keys stored lowercase-friendly; try lower then original)
        if let Some(img) = map.images.get(&lab).or_else(|| map.images.get(raw.trim())) {
            image_label = Some(lab);
            image = img.clone();
            break;
        }
    }

    let arch = resolve_arch_from_labels(labels);
    let host = TargetArch::host();
    let (platform, needs_emulation) = match arch {
        Some(a) if a != host => {
            let plat = map
                .arch_platforms
                .get(a.label())
                .cloned()
                .unwrap_or_else(|| a.platform().to_string());
            (Some(plat), true)
        }
        Some(a) => {
            // Same as host: no --platform needed (native). Still record arch for labels.
            let _ = a;
            (None, false)
        }
        None => (None, false),
    };

    JobImageArch {
        image,
        image_label,
        arch,
        platform,
        needs_emulation,
    }
}

/// Pick target arch from job labels.
///
/// Prefer explicit non-amd64 (arm64, riscv64, …) over `x64`/`amd64` so a matrix
/// cell `[self-hosted, linux, arm64, podman]` wins over ambient host defaults.
pub fn resolve_arch_from_labels(labels: &[String]) -> Option<TargetArch> {
    let mut found: Vec<TargetArch> = Vec::new();
    for raw in labels {
        if let Some(a) = TargetArch::from_label(raw) {
            if !found.contains(&a) {
                found.push(a);
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    // Prefer non-amd64 if any.
    if let Some(a) = found.iter().copied().find(|a| *a != TargetArch::Amd64) {
        return Some(a);
    }
    found.into_iter().next()
}

/// Podman args for platform selection (`--platform <val>`). Empty if none.
pub fn podman_platform_args(platform: Option<&str>) -> Vec<String> {
    match platform {
        Some(p) if !p.is_empty() => vec!["--platform".into(), p.to_string()],
        _ => Vec::new(),
    }
}

/// Pure binfmt probe: `entries` is a list of `binfmt_misc` registration names
/// (e.g. directory listing of `/proc/sys/fs/binfmt_misc`).
///
/// Returns `true` when at least one marker for `arch` appears as a full name or substring.
pub fn binfmt_lists_arch(arch: TargetArch, entries: &[String]) -> bool {
    let lower: Vec<String> = entries.iter().map(|e| e.to_ascii_lowercase()).collect();
    for marker in arch.binfmt_markers() {
        let m = marker.to_ascii_lowercase();
        if lower.iter().any(|e| e == &m || e.contains(&m)) {
            return true;
        }
    }
    false
}

/// Clear error when emulation is required but binfmt is missing.
pub fn binfmt_missing_error(arch: TargetArch) -> String {
    format!(
        "cannot spawn {} runner (platform {}): QEMU/binfmt_misc is not registered on this fleet host. \
         Refusing a silent wrong-arch run. Register handlers, e.g. \
         `podman run --privileged --rm tonistiigi/binfmt --install all` \
         (or install qemu-user-static / systemd-binfmt for {}). \
         Then re-queue the job.",
        arch.label(),
        arch.platform(),
        arch.binfmt_markers().first().copied().unwrap_or("qemu-*")
    )
}

/// Ensure binfmt is available when `needs_emulation`. Pure when `entries` is supplied;
/// host path uses [`read_binfmt_entries`].
pub fn ensure_binfmt_for_arch(
    arch: TargetArch,
    needs_emulation: bool,
    entries: Option<&[String]>,
) -> Result<(), String> {
    if !needs_emulation {
        return Ok(());
    }
    let owned;
    let list: &[String] = match entries {
        Some(e) => e,
        None => {
            owned = read_binfmt_entries();
            &owned
        }
    };
    if binfmt_lists_arch(arch, list) {
        return Ok(());
    }
    Err(binfmt_missing_error(arch))
}

/// Read `/proc/sys/fs/binfmt_misc` entry names (best-effort; empty if unavailable).
pub fn read_binfmt_entries() -> Vec<String> {
    let dir = Path::new("/proc/sys/fs/binfmt_misc");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name == "register" || name == "status" {
            continue;
        }
        out.push(name);
    }
    out
}

/// Extra runner labels to advertise so GitHub routes image/arch-labelled jobs.
pub fn extra_image_arch_labels(resolved: &JobImageArch) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(ref lab) = resolved.image_label {
        out.push(lab.clone());
    }
    if let Some(arch) = resolved.arch {
        out.push(arch.label().to_string());
        // Also advertise aarch64 synonym for arm64 cells.
        if arch == TargetArch::Arm64 {
            out.push("aarch64".into());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_image_map() {
        let raw = r#"{
          "images": {
            "ubuntu-24.04": "docker.io/library/ubuntu:24.04",
            "custom-ci": "ghcr.io/org/ci:1"
          },
          "arches": { "arm64": "linux/arm64" }
        }"#;
        let m = parse_image_map(raw).unwrap();
        assert_eq!(
            m.images.get("ubuntu-24.04").map(String::as_str),
            Some("docker.io/library/ubuntu:24.04")
        );
        assert_eq!(
            m.images.get("custom-ci").map(String::as_str),
            Some("ghcr.io/org/ci:1")
        );
        assert_eq!(
            m.arch_platforms.get("arm64").map(String::as_str),
            Some("linux/arm64")
        );
    }

    #[test]
    fn parse_toml_lite_image_map() {
        let raw = r#"
# fleet image map
[images]
ubuntu-24.04 = "docker.io/library/ubuntu:24.04"
rocky-9 = "docker.io/library/rockylinux:9"

[arches]
arm64 = "linux/arm64"
riscv64 = "linux/riscv64"
"#;
        let m = parse_image_map(raw).unwrap();
        assert!(m.images.contains_key("ubuntu-24.04"));
        assert!(m.images.contains_key("rocky-9"));
        assert_eq!(
            m.arch_platforms.get("riscv64").map(String::as_str),
            Some("linux/riscv64")
        );
    }

    #[test]
    fn reject_unsafe_image_in_map() {
        let raw = r#"{"images":{"evil":"img;rm -rf /"}}"#;
        assert!(parse_image_map(raw).is_err());
    }

    #[test]
    fn resolve_image_from_labels() {
        let map = ImageMap::builtin();
        let labels = vec![
            "self-hosted".into(),
            "linux".into(),
            "x64".into(),
            "podman".into(),
            "ubuntu-24.04".into(),
        ];
        let r = resolve_job_image_arch(&labels, &map, "localhost/gha-runner-ctl:latest");
        assert_eq!(r.image_label.as_deref(), Some("ubuntu-24.04"));
        assert_eq!(r.image, "docker.io/library/ubuntu:24.04");
        assert_eq!(r.arch, Some(TargetArch::Amd64));
        assert!(!r.needs_emulation || TargetArch::host() != TargetArch::Amd64);
    }

    #[test]
    fn resolve_no_image_label_keeps_default() {
        let map = ImageMap::builtin();
        let labels = vec![
            "self-hosted".into(),
            "linux".into(),
            "x64".into(),
            "podman".into(),
            "large".into(),
        ];
        let r = resolve_job_image_arch(&labels, &map, "localhost/gha-runner-ctl:latest");
        assert!(r.image_label.is_none());
        assert_eq!(r.image, "localhost/gha-runner-ctl:latest");
    }

    #[test]
    fn resolve_arch_prefers_arm64_over_x64() {
        let labels = vec![
            "self-hosted".into(),
            "linux".into(),
            "x64".into(),
            "arm64".into(),
            "podman".into(),
        ];
        assert_eq!(resolve_arch_from_labels(&labels), Some(TargetArch::Arm64));
    }

    #[test]
    fn resolve_arm64_needs_platform_when_host_not_arm() {
        let map = ImageMap::builtin();
        let labels = vec![
            "self-hosted".into(),
            "linux".into(),
            "arm64".into(),
            "podman".into(),
            "ubuntu-22.04".into(),
        ];
        let r = resolve_job_image_arch(&labels, &map, "localhost/stock:latest");
        assert_eq!(r.image, "docker.io/library/ubuntu:22.04");
        assert_eq!(r.arch, Some(TargetArch::Arm64));
        if TargetArch::host() != TargetArch::Arm64 {
            assert!(r.needs_emulation);
            assert_eq!(r.platform.as_deref(), Some("linux/arm64"));
        } else {
            assert!(!r.needs_emulation);
            assert!(r.platform.is_none());
        }
    }

    #[test]
    fn podman_platform_args_empty_or_pair() {
        assert!(podman_platform_args(None).is_empty());
        assert!(podman_platform_args(Some("")).is_empty());
        assert_eq!(
            podman_platform_args(Some("linux/arm64")),
            vec!["--platform".to_string(), "linux/arm64".to_string()]
        );
    }

    #[test]
    fn binfmt_guard_ok_when_present() {
        let entries = vec![
            "qemu-aarch64".into(),
            "qemu-riscv64".into(),
            "status".into(),
        ];
        assert!(binfmt_lists_arch(TargetArch::Arm64, &entries));
        assert!(binfmt_lists_arch(TargetArch::Riscv64, &entries));
        assert!(!binfmt_lists_arch(TargetArch::S390x, &entries));
        ensure_binfmt_for_arch(TargetArch::Arm64, true, Some(&entries)).unwrap();
        let err = ensure_binfmt_for_arch(TargetArch::S390x, true, Some(&entries)).unwrap_err();
        assert!(err.contains("binfmt_misc"));
        assert!(err.contains("Refusing"));
        // No emulation needed → always ok even with empty entries.
        ensure_binfmt_for_arch(TargetArch::Arm64, false, Some(&[])).unwrap();
    }

    #[test]
    fn config_overrides_builtin() {
        let file = parse_image_map(r#"{"images":{"ubuntu-24.04":"ghcr.io/org/ubuntu:24.04-ci"}}"#)
            .unwrap();
        let map = ImageMap::builtin().merge_over(file);
        assert_eq!(
            map.images.get("ubuntu-24.04").map(String::as_str),
            Some("ghcr.io/org/ubuntu:24.04-ci")
        );
        // builtin keys still present
        assert!(map.images.contains_key("rocky-9"));
    }

    #[test]
    fn extra_labels_include_image_and_arch() {
        let r = JobImageArch {
            image: "docker.io/library/ubuntu:24.04".into(),
            image_label: Some("ubuntu-24.04".into()),
            arch: Some(TargetArch::Arm64),
            platform: Some("linux/arm64".into()),
            needs_emulation: true,
        };
        let labs = extra_image_arch_labels(&r);
        assert!(labs.contains(&"ubuntu-24.04".into()));
        assert!(labs.contains(&"arm64".into()));
        assert!(labs.contains(&"aarch64".into()));
    }
}

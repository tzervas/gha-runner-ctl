use gha_runner_ctl::*;

#[test]
fn test_wake_auth_preserves_token_case() {
    // Mixed-case secret must match when Authorization/Bearer casing varies.
    let token = "AbCdEfGhIjKlMnOp"; // 16 chars
    assert!(wake_request_line_authorized(
        &format!("Authorization: Bearer {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("authorization: bearer {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("AUTHORIZATION: BEARER {token}"),
        token
    ));
    // Lowercasing the secret must NOT authenticate against the original token.
    assert!(!wake_request_line_authorized(
        &format!("Authorization: Bearer {}", token.to_ascii_lowercase()),
        token
    ));
    // Wrong secret rejected.
    assert!(!wake_request_line_authorized(
        "Authorization: Bearer totally-wrong-tok",
        token
    ));
    // X-Wake-Token path (exact header name) still works.
    assert!(wake_request_line_authorized(
        &format!("X-Wake-Token: {token}"),
        token
    ));
    assert!(!wake_request_line_authorized(
        &format!("X-Wake-Token: {}", token.to_ascii_lowercase()),
        token
    ));
}

/// Parameterized test case structure for is_safe_repo validation
struct RepoTestCase {
    repo: &'static str,
    expected: bool,
}

#[test]
fn test_is_safe_repo_parameterized() {
    let test_cases = vec![
        RepoTestCase {
            repo: "tzervas/tg-agent-relay",
            expected: true,
        },
        RepoTestCase {
            repo: "foo/bar",
            expected: true,
        },
        RepoTestCase {
            repo: "foo/bar;rm",
            expected: false,
        },
        RepoTestCase {
            repo: "foo/../bar",
            expected: false,
        },
        RepoTestCase {
            repo: "foo/bar ",
            expected: false,
        },
        RepoTestCase {
            repo: "owner-name/repo_name.dot",
            expected: true,
        },
        RepoTestCase {
            repo: "owner/repo_name/extra",
            expected: false,
        },
    ];

    for case in test_cases {
        assert_eq!(
            is_safe_repo(case.repo),
            case.expected,
            "Failed for repo: {}",
            case.repo
        );
    }
}

#[test]
fn test_case_insensitive_wake_auth_headers() {
    let token = "SomeTokenCasePreserved123";
    // Check Authorization: Bearer variations
    assert!(wake_request_line_authorized(
        &format!("Authorization: Bearer {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("authorization: bearer {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("AUTHORIZATION: BEARER {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("AuThOrIzAtIoN: bEaReR {token}"),
        token
    ));

    // Check X-Wake-Token variations
    assert!(wake_request_line_authorized(
        &format!("X-Wake-Token: {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("x-wake-token: {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("X-WAKE-TOKEN: {token}"),
        token
    ));
    assert!(wake_request_line_authorized(
        &format!("X-WaKe-ToKeN: {token}"),
        token
    ));
}

#[test]
fn test_multiple_secret_redactions() {
    let raw =
        "Here are two secrets: Bearer ghp_ABC123 and another Bearer ghp_XYZ789 in the same string.";
    let redacted_str = redact(raw);
    assert!(!redacted_str.contains("ABC123"));
    assert!(!redacted_str.contains("XYZ789"));
    assert!(redacted_str.matches("***REDACTED***").count() >= 2);
}

#[test]
fn test_redact_multibyte_truncation_safety() {
    // Large string of multibyte chars (e.g. '🦀' which is 4 bytes).
    let raw = "🦀".repeat(150);
    let redacted_str = redact(&raw);
    // Ensure no panic during truncation (does not slice char boundaries)
    assert!(redacted_str.ends_with('…') || redacted_str.ends_with('🦀'));
}

/// Parameterized test case structure for parse_github_remote
struct RemoteTestCase {
    url: &'static str,
    expected: Option<&'static str>,
}

#[test]
fn test_parse_github_remote_parameterized() {
    let test_cases = vec![
        RemoteTestCase {
            url: "git@github.com:tzervas/foo.git",
            expected: Some("tzervas/foo"),
        },
        RemoteTestCase {
            url: "https://github.com/tzervas/foo.git",
            expected: Some("tzervas/foo"),
        },
        RemoteTestCase {
            url: "ssh://git@github.com/tzervas/bar.git",
            expected: Some("tzervas/bar"),
        },
        RemoteTestCase {
            url: "https://gitlab.com/tzervas/foo.git",
            expected: None,
        },
    ];

    for case in test_cases {
        assert_eq!(
            parse_github_remote(case.url).as_deref(),
            case.expected,
            "Failed for URL: {}",
            case.url
        );
    }
}

/// Parameterized test case structure for redact function
struct RedactTestCase {
    input: &'static str,
    expected_contains: &'static str,
    expected_not_contains: &'static str,
}

#[test]
fn test_redact_parameterized() {
    let test_cases = vec![RedactTestCase {
        input: "Bearer ghp_ABCDEFGHIJKLMNOPQRST",
        expected_contains: "***REDACTED***",
        expected_not_contains: "ABCDEF",
    }];

    for case in test_cases {
        let result = redact(case.input);
        assert!(
            result.contains(case.expected_contains),
            "Expected contains: {}, got: {}",
            case.expected_contains,
            result
        );
        assert!(
            !result.contains(case.expected_not_contains),
            "Expected not contains: {}, got: {}",
            case.expected_not_contains,
            result
        );
    }
}

#[test]
fn test_redact_gho_prefix() {
    // Built at runtime so gitleaks does not flag a static OAuth-shaped secret.
    let input = format!("RUNNER_TOKEN={}1234567890abcdef", concat!("gh", "o_"));
    let result = redact(&input);
    assert!(result.contains("***REDACTED***"), "got: {result}");
    assert!(!result.contains("12345"), "got: {result}");
}

/// Parameterized validation tests for CPU and memory specs
struct SpecTestCase {
    spec: &'static str,
    expected: bool,
}

#[test]
fn test_is_safe_cpus_parameterized() {
    let test_cases = vec![
        SpecTestCase {
            spec: "1",
            expected: true,
        },
        SpecTestCase {
            spec: "0.5",
            expected: true,
        },
        SpecTestCase {
            spec: "64",
            expected: true,
        },
        SpecTestCase {
            spec: "0",
            expected: false,
        },
        SpecTestCase {
            spec: "-1",
            expected: false,
        },
        SpecTestCase {
            spec: "65",
            expected: false,
        },
        SpecTestCase {
            spec: "abc",
            expected: false,
        },
    ];

    for case in test_cases {
        assert_eq!(
            is_safe_cpus(case.spec),
            case.expected,
            "Failed for CPU spec: {}",
            case.spec
        );
    }
}

#[test]
fn test_is_safe_memory_parameterized() {
    let test_cases = vec![
        SpecTestCase {
            spec: "8g",
            expected: true,
        },
        SpecTestCase {
            spec: "512m",
            expected: true,
        },
        SpecTestCase {
            spec: "16gb",
            expected: true,
        },
        SpecTestCase {
            spec: "4gi",
            expected: true,
        },
        SpecTestCase {
            spec: "abc",
            expected: false,
        },
        SpecTestCase {
            spec: "-4g",
            expected: false,
        },
    ];

    for case in test_cases {
        assert_eq!(
            is_safe_memory(case.spec),
            case.expected,
            "Failed for memory spec: {}",
            case.spec
        );
    }
}

#[test]
fn test_is_safe_image_oci_refs() {
    assert!(is_safe_image("localhost/gha-runner-ctl:latest"));
    assert!(is_safe_image("docker.io/library/ubuntu:24.04"));
    assert!(is_safe_image("docker.io/library/fedora:40"));
    assert!(is_safe_image("ghcr.io/org/ci-tools:1.2.3"));
    assert!(is_safe_image("registry.example.com:5000/team/ci:stable"));
    assert!(is_safe_image(
        "ghcr.io/org/img@sha256:4ef2f25285f0ae4477f1fe1e346db76d2f3ebf03824e2ddd1973a2819bf6c8cf"
    ));
    assert!(is_safe_image("quay.io/podman/hello"));
    // FreeBSD/OpenBSD-named Linux userspace images (OCI on Linux hosts)
    assert!(is_safe_image("docker.io/library/alpine:3.20"));
    assert!(!is_safe_image(""));
    assert!(!is_safe_image("img;rm -rf /"));
    assert!(!is_safe_image("img$(reboot)"));
    assert!(!is_safe_image("../evil"));
    assert!(!is_safe_image("img with space"));
}

#[test]
fn test_effective_image_mode_auto() {
    assert_eq!(
        effective_image_mode(&ImageMode::Auto, "localhost/gha-runner-ctl:latest"),
        ImageMode::Build
    );
    assert_eq!(
        effective_image_mode(&ImageMode::Auto, "docker.io/library/ubuntu:24.04"),
        ImageMode::External
    );
    assert_eq!(
        effective_image_mode(&ImageMode::External, "localhost/gha-runner-ctl:latest"),
        ImageMode::External
    );
    assert_eq!(
        effective_image_mode(&ImageMode::Build, "docker.io/library/fedora:40"),
        ImageMode::Build
    );
}

#[test]
fn test_effective_pull_policy_defaults() {
    assert_eq!(
        effective_pull_policy(None, &ImageMode::Build),
        PullPolicy::Never
    );
    assert_eq!(
        effective_pull_policy(None, &ImageMode::External),
        PullPolicy::Missing
    );
    assert_eq!(
        effective_pull_policy(Some(&PullPolicy::Always), &ImageMode::Build),
        PullPolicy::Always
    );
}

#[test]
fn test_runner_user_and_sha_validation() {
    assert!(is_safe_runner_user("1001:1001"));
    assert!(is_safe_runner_user("0:0"));
    assert!(is_safe_runner_user("runner"));
    assert!(!is_safe_runner_user(""));
    assert!(!is_safe_runner_user("1001;root"));
    assert!(is_safe_sha256_hex(
        "4ef2f25285f0ae4477f1fe1e346db76d2f3ebf03824e2ddd1973a2819bf6c8cf"
    ));
    assert!(!is_safe_sha256_hex("abcd"));
    assert!(is_safe_url(
        "https://github.com/actions/runner/releases/download/v2.335.1/actions-runner-linux-x64-2.335.1.tar.gz"
    ));
    assert!(!is_safe_url("file:///etc/passwd"));
}

/// Issue #28: label → image resolution (builtin map + workflow distro labels).
#[test]
fn test_label_parsing_to_image_resolution() {
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

    // mycelium-lang draw-in cells
    for (lab, img) in [
        ("debian-bookworm", "docker.io/library/debian:bookworm"),
        ("rocky-9", "docker.io/library/rockylinux:9"),
        ("ubuntu-22.04", "docker.io/library/ubuntu:22.04"),
    ] {
        let labs = vec![
            "self-hosted".into(),
            "linux".into(),
            "x64".into(),
            "podman".into(),
            lab.into(),
        ];
        let r = resolve_job_image_arch(&labs, &map, "localhost/stock:latest");
        assert_eq!(r.image, img, "label {lab}");
    }

    // No image label → default unchanged
    let bare = vec![
        "self-hosted".into(),
        "linux".into(),
        "x64".into(),
        "podman".into(),
    ];
    let r = resolve_job_image_arch(&bare, &map, "localhost/gha-runner-ctl:latest");
    assert!(r.image_label.is_none());
    assert_eq!(r.image, "localhost/gha-runner-ctl:latest");
}

/// Issue #28: arch label → podman --platform args.
#[test]
fn test_arch_label_to_podman_platform_args() {
    assert_eq!(TargetArch::from_label("arm64"), Some(TargetArch::Arm64));
    assert_eq!(TargetArch::from_label("aarch64"), Some(TargetArch::Arm64));
    assert_eq!(TargetArch::from_label("riscv64"), Some(TargetArch::Riscv64));
    assert_eq!(TargetArch::Arm64.platform(), "linux/arm64");
    assert_eq!(TargetArch::Riscv64.platform(), "linux/riscv64");

    let args = podman_platform_args(Some("linux/arm64"));
    assert_eq!(args, vec!["--platform", "linux/arm64"]);
    assert!(podman_platform_args(None).is_empty());

    let labs = vec![
        "self-hosted".into(),
        "linux".into(),
        "arm64".into(),
        "podman".into(),
    ];
    assert_eq!(resolve_arch_from_labels(&labs), Some(TargetArch::Arm64));
    // Prefer arm64 over ambient x64 when both appear
    let mixed = vec!["x64".into(), "arm64".into()];
    assert_eq!(resolve_arch_from_labels(&mixed), Some(TargetArch::Arm64));
}

/// Issue #28: binfmt-missing guard (clear error, not silent wrong-arch).
#[test]
fn test_binfmt_missing_guard() {
    let empty: Vec<String> = vec![];
    let err = ensure_binfmt_for_arch(TargetArch::Arm64, true, Some(&empty)).unwrap_err();
    assert!(err.contains("binfmt_misc"), "{err}");
    assert!(err.contains("Refusing"), "{err}");
    assert!(
        err.contains("arm64") || err.contains("linux/arm64"),
        "{err}"
    );

    let present = vec!["qemu-aarch64".into(), "qemu-riscv64".into()];
    ensure_binfmt_for_arch(TargetArch::Arm64, true, Some(&present)).unwrap();
    ensure_binfmt_for_arch(TargetArch::Riscv64, true, Some(&present)).unwrap();
    // needs_emulation=false → never errors
    ensure_binfmt_for_arch(TargetArch::Arm64, false, Some(&empty)).unwrap();
    assert!(binfmt_lists_arch(TargetArch::Arm64, &present));
    assert!(!binfmt_lists_arch(TargetArch::S390x, &present));
    assert!(binfmt_missing_error(TargetArch::Arm64).contains("tonistiigi/binfmt"));
}

#[test]
fn test_image_map_json_and_toml_parse() {
    let j = parse_image_map(
        r#"{"images":{"my-ci":"ghcr.io/org/ci:1"},"arches":{"arm64":"linux/arm64"}}"#,
    )
    .unwrap();
    assert_eq!(
        j.images.get("my-ci").map(String::as_str),
        Some("ghcr.io/org/ci:1")
    );

    let t = parse_image_map(
        r#"
[images]
my-ci = "ghcr.io/org/ci:1"
[arches]
arm64 = "linux/arm64"
"#,
    )
    .unwrap();
    assert_eq!(t.images.get("my-ci"), j.images.get("my-ci"));
    assert!(parse_image_map(r#"{"images":{"x":"bad;img"}}"#).is_err());
}

#[test]
fn test_runner_labels_include_image_arch() {
    let resolved = JobImageArch {
        image: "docker.io/library/ubuntu:24.04".into(),
        image_label: Some("ubuntu-24.04".into()),
        arch: Some(TargetArch::Arm64),
        platform: Some("linux/arm64".into()),
        needs_emulation: true,
    };
    let labs = runner_labels_for_job_with_map(
        "self-hosted,linux,x64,podman",
        &[
            "self-hosted".into(),
            "linux".into(),
            "arm64".into(),
            "podman".into(),
            "ubuntu-24.04".into(),
        ],
        SizeTier::Medium,
        Some(&resolved),
    );
    assert!(labs.contains("ubuntu-24.04"), "{labs}");
    assert!(labs.contains("arm64"), "{labs}");
    assert!(labs.contains("medium"), "{labs}");
    // Conflicting host x64 stripped when advertising arm64
    assert!(!labs.split(',').any(|l| l == "x64"), "{labs}");
}

/// Regression: work image must force JS actions onto node24 via the documented
/// runner env (FORCE_JAVASCRIPT_ACTIONS_TO_NODE24). The internal-only knob
/// ACTIONS_RUNNER_FORCED_INTERNAL_NODE_VERSION does not affect JS actions.
#[test]
fn test_work_image_forces_js_actions_node24() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/Containerfile");
    let body =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        body.contains("FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true"),
        "Containerfile must set FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true for JS actions"
    );
    assert!(
        !body.contains("ACTIONS_RUNNER_FORCED_INTERNAL_NODE_VERSION=node24"),
        "do not use ACTIONS_RUNNER_FORCED_INTERNAL_NODE_VERSION for JS actions (internal only)"
    );
}

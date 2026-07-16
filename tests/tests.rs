use gha_runner_ctl::*;

#[test]
fn test_wake_auth_preserves_token_case() {
    // Both Authorization: Bearer and X-Wake-Token are check case-insensitively.
    // However, the secret token itself preserves case.
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
    assert!(wake_request_line_authorized(
        &format!("AuThOrIzAtIoN: bEaReR {token}"),
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

    // X-Wake-Token path works case-insensitively.
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
fn test_overwrite_permission_preservation() {
    let dir = std::env::temp_dir();
    let path = dir.join("gha-runner-ctl-test-perms-overwrite.txt");

    // 1. Create file with 0o644 permissions (or standard umask)
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, "initial content").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    // 2. Perform overwrite with OpenOptions and post-write set_permissions
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).unwrap();
    use std::io::Write;
    f.write_all(b"new content").unwrap();
    drop(f);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let _ = std::fs::remove_file(&path);
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

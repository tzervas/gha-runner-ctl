use gha_runner_ctl::*;

/// Parameterized test case structure for is_safe_repo validation
struct RepoTestCase {
    repo: &'static str,
    expected: bool,
}

#[test]
fn test_is_safe_repo_parameterized() {
    let test_cases = vec![
        RepoTestCase { repo: "tzervas/tg-agent-relay", expected: true },
        RepoTestCase { repo: "foo/bar", expected: true },
        RepoTestCase { repo: "foo/bar;rm", expected: false },
        RepoTestCase { repo: "foo/../bar", expected: false },
        RepoTestCase { repo: "foo/bar ", expected: false },
        RepoTestCase { repo: "owner-name/repo_name.dot", expected: true },
        RepoTestCase { repo: "owner/repo_name/extra", expected: false },
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
    let test_cases = vec![
        RedactTestCase {
            input: "Bearer ghp_ABCDEFGHIJKLMNOPQRST",
            expected_contains: "***REDACTED***",
            expected_not_contains: "ABCDEF",
        },
        RedactTestCase {
            input: "RUNNER_TOKEN=gho_1234567890abcdef",
            expected_contains: "***REDACTED***",
            expected_not_contains: "12345",
        },
    ];

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

/// Parameterized validation tests for CPU and memory specs
struct SpecTestCase {
    spec: &'static str,
    expected: bool,
}

#[test]
fn test_is_safe_cpus_parameterized() {
    let test_cases = vec![
        SpecTestCase { spec: "1", expected: true },
        SpecTestCase { spec: "0.5", expected: true },
        SpecTestCase { spec: "64", expected: true },
        SpecTestCase { spec: "0", expected: false },
        SpecTestCase { spec: "-1", expected: false },
        SpecTestCase { spec: "65", expected: false },
        SpecTestCase { spec: "abc", expected: false },
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
        SpecTestCase { spec: "8g", expected: true },
        SpecTestCase { spec: "512m", expected: true },
        SpecTestCase { spec: "16gb", expected: true },
        SpecTestCase { spec: "4gi", expected: true },
        SpecTestCase { spec: "abc", expected: false },
        SpecTestCase { spec: "-4g", expected: false },
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

use gha_runner_ctl::{is_safe_repo, parse_github_remote, redact};

#[test]
fn rejects_shell_metacharacters_in_repo() {
    assert!(!is_safe_repo("foo/bar;rm"));
    assert!(is_safe_repo("tzervas/tg-agent-relay"));
}

#[test]
fn parse_remotes() {
    assert_eq!(
        parse_github_remote("git@github.com:tzervas/foo.git").as_deref(),
        Some("tzervas/foo")
    );
    assert_eq!(
        parse_github_remote("https://github.com/tzervas/foo.git").as_deref(),
        Some("tzervas/foo")
    );
}

#[test]
fn redacts_bearer() {
    let s = redact("Bearer ghp_ABCDEFGHIJKLMNOPQRST");
    assert!(!s.contains("ABCDEF"));
}

#[test]
fn redacts_multiple_secrets() {
    let s = redact("Here is token1 ghp_ABC and token2 ghp_DEF on the same line.");
    assert!(!s.contains("ABC"));
    assert!(!s.contains("DEF"));
    assert!(s.contains("ghp_***REDACTED***"));
}

#[test]
fn redact_multi_byte_safe() {
    let s = redact("Bearer ghp_ABC¢DEF");
    // '¢' is multi-byte (2 bytes in UTF-8). It is not alphanumeric or [_-.],
    // so redaction should stop right before it, and we must not slice in the middle of '¢'.
    assert!(!s.contains("ABC"));
    assert!(s.contains("¢DEF"));
}

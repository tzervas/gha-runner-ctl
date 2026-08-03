use ap_runner_ctl::{is_safe_repo, parse_github_remote, redact};

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

// --- issue #132 third follow-up audit: redact() now delegates to
// dump_redact::redact_free_text instead of an independent, weaker 8-entry prefix
// blocklist (see redact()'s doc comment in lib.rs for why: two redactors of
// different strength in one codebase is exactly how the round-3 finding happened).
// The tests below were pinned to that old blocklist's specific output format
// ("ghp_***REDACTED***", no shape info, no minimum body length) and are updated here
// to the new, stronger, shape-aware behavior — using realistic-length synthetic
// tokens (36+ chars after the prefix, the real GitHub token shape) rather than the
// old tests' short, unrealistic bodies, which is what the new shape check requires
// to avoid false-positiving on ordinary identifiers that merely start with "ghp_".

#[test]
fn redacts_ghp_secret_with_shape_labeled_placeholder() {
    let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5)); // 40 chars, realistic length
    let s = redact(&format!("Here is token1 {synthetic} and more text."));
    assert!(!s.contains(&synthetic), "credential leaked: {s}");
    assert!(
        s.contains("***REDACTED(github_token)***"),
        "expected shape-labeled placeholder, got: {s}"
    );
    assert!(
        s.contains("and more text."),
        "diagnostic tail must survive: {s}"
    );
}

#[test]
fn redact_multi_byte_safe() {
    // A multi-byte char ('¢', 2 bytes in UTF-8) sits directly adjacent to the
    // credential with no delimiter at all — this must not panic (byte-boundary
    // safety in the underlying scan) and the credential must still be redacted, with
    // surrounding diagnostic text intact.
    let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
    let s = redact(&format!("token={synthetic}¢ trailing"));
    assert!(!s.contains(&synthetic), "credential leaked: {s}");
    assert!(s.contains("trailing"), "diagnostic tail must survive: {s}");
}

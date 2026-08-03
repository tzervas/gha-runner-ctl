//! Allowlist-based redaction for developer debug dumps (issue #132).
//!
//! `lib::redact()` is a *blocklist* scrubber: it walks free-text (subprocess stderr,
//! `podman ps` lines) looking for known bad prefixes (`ghp_`, `Bearer `, …) and cuts
//! them out. That is the right tool for free text, where you cannot know ahead of time
//! which substrings will appear — but it is the wrong tool for a structured dump of
//! *named* values (env vars, resolved config), because a blocklist only knows what it
//! has been taught to reject. It eventually misses one.
//!
//! This module instead answers: "is this named value safe to print?" using an
//! **allowlist of exact key names** (never a prefix — `GHA_*` would let
//! `GHA_SECRET_TOKEN` straight through) *and* a check of the value's own shape,
//! because a key being on the allowlist does not make an unexpected value safe.
//! `GHA_APP_PRIVATE_KEY` is allowlisted because its documented, supported forms
//! (`secret:group/key`, `file:/path`, a bare path) are safe to print — but if it ever
//! holds inline PEM material (a caller/config bug, since the parser is supposed to
//! reject that upstream), the value-shape check still catches and redacts it.
//!
//! Every function here is pure (no I/O, no global state) so it is trivial to fuzz:
//! feed arbitrary `(key, value)` pairs to [`redact_for_dump`] / [`classify_value`] and
//! assert the invariant "no [`ValueVerdict::Safe`] output ever matches a known
//! credential shape".

/// Env / config keys considered safe to print in a debug dump, **provided the value
/// also passes [`classify_value`]**. Exact string match only (see module docs for why
/// prefix matching is rejected). Add to this list deliberately, one key at a time —
/// it is the entire security boundary for what this module will ever print.
pub const DUMP_ALLOWLIST: &[&str] = &[
    "HOME",
    "USER",
    "PWD",
    "XDG_RUNTIME_DIR",
    "CONTAINER_HOST",
    "GHA_ALLOW_ROOT",
    "GHA_SCOPE",
    "GHA_USER",
    "GHA_REPO",
    "GHA_PREFER_REPOS",
    "GHA_ALLOWLIST_REPOS",
    "GHA_MODE",
    "GHA_CONTAINER",
    "GHA_VOLUME",
    "GHA_IMAGE",
    "GHA_GPU",
    "GHA_DEBUG",
    "GHA_DEBUG_ON_ERR",
    "GHA_APP_ID",
    "GHA_APP_INSTALLATION_ID",
    // Safe by construction ONLY in its documented forms (secret:/file:/path) — see
    // module docs. classify_value still checks every value that arrives here.
    "GHA_APP_PRIVATE_KEY",
];

/// Exact-match membership test. Deliberately not `starts_with` — see module docs.
pub fn is_allowlisted_key(key: &str) -> bool {
    DUMP_ALLOWLIST.contains(&key)
}

/// Why a value was judged unsafe to print verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsafeShape {
    GithubToken,
    GithubFineGrainedPat,
    AwsAccessKeyId,
    PemBlock,
    Jwt,
    BearerToken,
    BasicAuthUrl,
    HighEntropy,
}

impl UnsafeShape {
    pub fn as_str(self) -> &'static str {
        match self {
            UnsafeShape::GithubToken => "github_token",
            UnsafeShape::GithubFineGrainedPat => "github_fine_grained_pat",
            UnsafeShape::AwsAccessKeyId => "aws_access_key_id",
            UnsafeShape::PemBlock => "pem_block",
            UnsafeShape::Jwt => "jwt",
            UnsafeShape::BearerToken => "bearer_token",
            UnsafeShape::BasicAuthUrl => "basic_auth_url",
            UnsafeShape::HighEntropy => "high_entropy",
        }
    }
}

/// Result of inspecting a value's *shape*, independent of whether its key is
/// allowlisted at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueVerdict {
    /// Safe by construction: a vault reference, a path, a bool/int, a hostname, an
    /// image ref, or a comma-separated list of such things.
    Safe,
    /// Looks like credential material of the given shape.
    Unsafe(UnsafeShape),
}

/// A single field ready to print (or not) in a debug dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedField {
    pub key: String,
    /// The value to print: the original value if safe, else a `***REDACTED(reason)***`
    /// placeholder. Never contains the original value when `redacted` is true.
    pub value: String,
    pub redacted: bool,
    /// Set whenever `redacted` is true: either an [`UnsafeShape`] name, or
    /// `"key_not_allowlisted"`.
    pub reason: Option<&'static str>,
}

/// Redact (or pass through) one key/value pair for a debug dump.
///
/// Two independent gates, both must pass for the value to be printed verbatim:
/// 1. `key` is exactly one of [`DUMP_ALLOWLIST`].
/// 2. `value`'s shape is [`ValueVerdict::Safe`] per [`classify_value`].
pub fn redact_for_dump(key: &str, value: &str) -> RedactedField {
    if !is_allowlisted_key(key) {
        return RedactedField {
            key: key.to_string(),
            value: "***REDACTED(key_not_allowlisted)***".to_string(),
            redacted: true,
            reason: Some("key_not_allowlisted"),
        };
    }
    match classify_value(value) {
        ValueVerdict::Safe => RedactedField {
            key: key.to_string(),
            value: value.to_string(),
            redacted: false,
            reason: None,
        },
        ValueVerdict::Unsafe(shape) => RedactedField {
            key: key.to_string(),
            value: format!("***REDACTED({})***", shape.as_str()),
            redacted: true,
            reason: Some(shape.as_str()),
        },
    }
}

/// Redact a whole batch of resolved key/value pairs, e.g. everything gathered for a
/// debug dump. Order is preserved.
pub fn redact_env_dump<'a, I>(pairs: I) -> Vec<RedactedField>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    pairs
        .into_iter()
        .map(|(k, v)| redact_for_dump(k, v))
        .collect()
}

/// Classify a raw value by shape alone (no key context). Public because it is the
/// natural fuzz target: it is a pure `&str -> ValueVerdict` function.
///
/// Order matters and is deliberate: unambiguous *safe* prefixes (`secret:`, `file:`)
/// and exact bool/int forms are recognised first; then known *credential* shapes are
/// checked (so e.g. a JWT is never miscategorised as a "hostname" just because it has
/// dot-separated segments); only values that match neither are turned over to
/// the last-resort high-entropy heuristic, itself run only after the remaining
/// safe-by-construction shapes (path / hostname / image ref / comma list) are ruled
/// out.
pub fn classify_value(value: &str) -> ValueVerdict {
    let v = value.trim();
    if v.is_empty() {
        return ValueVerdict::Safe;
    }

    // --- Unambiguous safe prefixes -----------------------------------------
    if let Some(rest) = v.strip_prefix("secret:") {
        if looks_like_vault_group_key(rest) {
            return ValueVerdict::Safe;
        }
    }
    if let Some(rest) = v.strip_prefix("file:") {
        if !rest.is_empty() && !rest.contains("://") && !rest.chars().any(char::is_whitespace) {
            return ValueVerdict::Safe;
        }
    }
    if is_bool_literal(v) || is_int_literal(v) {
        return ValueVerdict::Safe;
    }

    // --- Known credential shapes (checked before the generic safe-shape catch-alls
    //     below, so e.g. a JWT never falls through to "looks like a hostname") ------
    if is_pem_block(v) {
        return ValueVerdict::Unsafe(UnsafeShape::PemBlock);
    }
    if is_github_token(v) {
        return ValueVerdict::Unsafe(UnsafeShape::GithubToken);
    }
    if is_github_fine_grained_pat(v) {
        return ValueVerdict::Unsafe(UnsafeShape::GithubFineGrainedPat);
    }
    if is_aws_access_key_id(v) {
        return ValueVerdict::Unsafe(UnsafeShape::AwsAccessKeyId);
    }
    if is_bearer_token(v) {
        return ValueVerdict::Unsafe(UnsafeShape::BearerToken);
    }
    if is_basic_auth_url(v) {
        return ValueVerdict::Unsafe(UnsafeShape::BasicAuthUrl);
    }
    if is_jwt(v) {
        return ValueVerdict::Unsafe(UnsafeShape::Jwt);
    }

    // --- Remaining safe-by-construction shapes ------------------------------
    if is_hostname(v) {
        return ValueVerdict::Safe;
    }
    if is_fs_path(v) {
        return ValueVerdict::Safe;
    }
    if is_image_ref(v) {
        return ValueVerdict::Safe;
    }
    if let Some(verdict) = classify_comma_list(v) {
        return verdict;
    }

    // --- Last resort: generic high-entropy string ---------------------------
    if is_high_entropy(v) {
        return ValueVerdict::Unsafe(UnsafeShape::HighEntropy);
    }
    ValueVerdict::Safe
}

fn looks_like_vault_group_key(rest: &str) -> bool {
    let Some((group, key)) = rest.split_once('/') else {
        return false;
    };
    !group.is_empty()
        && !key.is_empty()
        && !key.contains('/')
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.'))
}

fn is_bool_literal(v: &str) -> bool {
    matches!(v, "true" | "false" | "True" | "False" | "TRUE" | "FALSE")
}

fn is_int_literal(v: &str) -> bool {
    let v = v.strip_prefix('-').unwrap_or(v);
    !v.is_empty() && v.len() <= 20 && v.chars().all(|c| c.is_ascii_digit())
}

/// `ghp_`/`gho_`/`ghs_`/`ghu_`/`ghr_` + 36 alphanumeric chars (classic GitHub PATs /
/// OAuth / server-to-server / user-to-server / refresh tokens all share this shape).
fn is_github_token(v: &str) -> bool {
    for prefix in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
        if let Some(rest) = v.strip_prefix(prefix) {
            if rest.len() >= 36 && rest.chars().all(|c| c.is_ascii_alphanumeric()) {
                return true;
            }
        }
    }
    false
}

/// `github_pat_` + 82 chars (fine-grained PAT).
fn is_github_fine_grained_pat(v: &str) -> bool {
    v.strip_prefix("github_pat_").is_some_and(|rest| {
        rest.len() >= 82 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// AWS access key ID: `AKIA`/`ASIA` + 16 uppercase-alnum chars (20 total).
fn is_aws_access_key_id(v: &str) -> bool {
    for prefix in ["AKIA", "ASIA"] {
        if let Some(rest) = v.strip_prefix(prefix) {
            if rest.len() == 16
                && rest
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                return true;
            }
        }
    }
    false
}

fn is_pem_block(v: &str) -> bool {
    v.contains("-----BEGIN ") || v.contains("PRIVATE KEY")
}

fn is_bearer_token(v: &str) -> bool {
    v.strip_prefix("Bearer ")
        .is_some_and(|rest| rest.len() >= 8 && !rest.trim().is_empty())
}

/// `scheme://user:pass@host...` — basic auth embedded in a URL.
fn is_basic_auth_url(v: &str) -> bool {
    let Some(after_scheme) = v.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let Some((userinfo, _host_and_rest)) = after_scheme.split_once('@') else {
        return false;
    };
    !userinfo.is_empty() && userinfo.contains(':') && !userinfo.contains('/')
}

/// Three dot-separated base64url segments, each long enough to be a real JWT header
/// (`{"alg":...}` base64url-encoded is at least a dozen chars) — not just any string
/// with two dots in it.
fn is_jwt(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let is_b64url = |s: &str| {
        s.len() >= 10
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    };
    parts.iter().all(|p| is_b64url(p))
}

/// RFC 1123-ish hostname: dot-separated labels of alnum/hyphen, no scheme, no
/// whitespace, no userinfo/path punctuation.
fn is_hostname(v: &str) -> bool {
    if v.is_empty() || v.len() > 255 || v.contains("://") || v.contains('@') || v.contains(' ') {
        return false;
    }
    let labels: Vec<&str> = v.split('.').collect();
    labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Plain filesystem path: absolute (`/…`) or a lone relative segment, no shell
/// metacharacters, no URL scheme/userinfo.
fn is_fs_path(v: &str) -> bool {
    if !v.starts_with('/') {
        return false;
    }
    if v.contains("://") || v.contains('@') || v.chars().any(|c| c.is_ascii_control()) {
        return false;
    }
    v.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '~' | '+' | ':' | ' ')
    })
}

/// Container image reference: `[registry/]repo[:tag][@digest]`. Deliberately checked
/// only after every credential shape above, since its permitted charset overlaps with
/// e.g. a JWT's.
fn is_image_ref(v: &str) -> bool {
    if v.is_empty() || v.len() > 384 || v.contains("://") || v.contains('@') || v.contains(' ') {
        return false;
    }
    // Require at least one '/' or ':' — otherwise a bare word is ambiguous and falls
    // through to the entropy check instead of being rubber-stamped "image ref".
    if !v.contains('/') && !v.contains(':') {
        return false;
    }
    v.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/' | ':')
    })
}

/// Comma-separated list where every element is itself a safe repo-ish token
/// (`org/repo` or a bare identifier). Used for `GHA_PREFER_REPOS` /
/// `GHA_ALLOWLIST_REPOS` shaped values. Returns `None` (not a list / not applicable)
/// rather than `Safe` when there's no comma, so the caller keeps trying other shapes.
fn classify_comma_list(v: &str) -> Option<ValueVerdict> {
    if !v.contains(',') {
        return None;
    }
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    if parts.is_empty() || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let all_safe_tokens = parts.iter().all(|p| is_repo_ish_token(p));
    if all_safe_tokens {
        Some(ValueVerdict::Safe)
    } else {
        None
    }
}

fn is_repo_ish_token(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 || s.contains("://") || s.contains('@') {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

/// Last-resort catch-all: a long string with high character-level (Shannon) entropy
/// is treated as opaque credential material even if it matches none of the named
/// shapes above — this is what catches raw AWS secret access keys (40 base64-ish
/// chars) and anything structurally similar we haven't special-cased.
fn is_high_entropy(v: &str) -> bool {
    const MIN_LEN: usize = 20;
    const MIN_BITS_PER_CHAR: f64 = 3.5;
    if v.len() < MIN_LEN || v.contains(' ') {
        return false;
    }
    shannon_entropy_bits_per_char(v) >= MIN_BITS_PER_CHAR
}

fn shannon_entropy_bits_per_char(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    let mut total = 0u32;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }
    let total_f = f64::from(total);
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / total_f;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- allowlist: exact match, never prefix --------------------------------

    #[test]
    fn allowlist_is_exact_match_not_prefix() {
        assert!(is_allowlisted_key("GHA_MODE"));
        // The precise trap called out in issue #132: a prefix-matching allowlist
        // ("GHA_*") would let this straight through.
        assert!(!is_allowlisted_key("GHA_SECRET_TOKEN"));
        assert!(!is_allowlisted_key("GHA_MODE_EXTRA"));
        assert!(!is_allowlisted_key("MY_GHA_MODE"));
    }

    #[test]
    fn unallowlisted_key_is_redacted_regardless_of_value() {
        let f = redact_for_dump("GH_TOKEN", "totally-benign-value");
        assert!(f.redacted);
        assert_eq!(f.reason, Some("key_not_allowlisted"));
        assert!(!f.value.contains("totally-benign-value"));
    }

    // --- vault references: safe by construction, key example from issue #132 -

    #[test]
    fn vault_reference_is_safe() {
        assert_eq!(
            classify_value("secret:runner/aphelion-app-key"),
            ValueVerdict::Safe
        );
        let f = redact_for_dump("GHA_APP_PRIVATE_KEY", "secret:runner/aphelion-app-key");
        assert!(!f.redacted);
        assert_eq!(f.value, "secret:runner/aphelion-app-key");
    }

    #[test]
    fn file_reference_is_safe() {
        assert_eq!(
            classify_value("file:/etc/gha/app-key.pem"),
            ValueVerdict::Safe
        );
    }

    #[test]
    fn plain_path_is_safe() {
        assert_eq!(
            classify_value("/home/gha-agent/.local/bin"),
            ValueVerdict::Safe
        );
    }

    #[test]
    fn booleans_and_integers_are_safe() {
        for v in ["true", "false", "0", "1", "8", "-1"] {
            assert_eq!(classify_value(v), ValueVerdict::Safe, "value={v}");
        }
    }

    #[test]
    fn hostname_is_safe() {
        assert_eq!(
            classify_value("runner-fleet-03.internal.example.com"),
            ValueVerdict::Safe
        );
        assert_eq!(classify_value("localhost"), ValueVerdict::Safe);
    }

    #[test]
    fn image_ref_is_safe() {
        assert_eq!(
            classify_value("ghcr.io/tzervas/gha-runner-ctl:0.3.3"),
            ValueVerdict::Safe
        );
        assert_eq!(
            classify_value("localhost/gha-runner-ctl:latest"),
            ValueVerdict::Safe
        );
    }

    #[test]
    fn comma_separated_repo_list_is_safe() {
        assert_eq!(
            classify_value("tzervas/gha-runner-ctl,tzervas/other-repo"),
            ValueVerdict::Safe
        );
    }

    // --- credential shapes: every one from issue #132 must be redacted --------

    #[test]
    fn github_classic_tokens_are_redacted() {
        for prefix in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
            // Synthetic — not a real token. 36 chars after the prefix.
            let synthetic = format!("{prefix}{}", "a1B2c3D4".repeat(5));
            assert_eq!(
                classify_value(&synthetic),
                ValueVerdict::Unsafe(UnsafeShape::GithubToken),
                "prefix={prefix}"
            );
        }
    }

    #[test]
    fn github_fine_grained_pat_is_redacted() {
        // Synthetic — not a real token. 82 chars after the prefix.
        let synthetic = format!("github_pat_{}", "a1B2c3D4e5".repeat(9));
        assert_eq!(
            classify_value(&synthetic),
            ValueVerdict::Unsafe(UnsafeShape::GithubFineGrainedPat)
        );
    }

    #[test]
    fn aws_access_key_id_is_redacted() {
        // Synthetic — not a real key. AKIA/ASIA + 16 uppercase-alnum chars (20 total).
        assert_eq!("AKIAIOSFODNN7EXAMPLE".len(), 20);
        assert_eq!(
            classify_value("AKIAIOSFODNN7EXAMPLE"),
            ValueVerdict::Unsafe(UnsafeShape::AwsAccessKeyId)
        );
        assert_eq!(
            classify_value("ASIAIOSFODNN7EXAMPLE"),
            ValueVerdict::Unsafe(UnsafeShape::AwsAccessKeyId)
        );
    }

    #[test]
    fn aws_secret_key_shaped_value_caught_by_entropy_fallback() {
        // Synthetic — not a real secret. 40-char mixed-case/digit/symbol string, the
        // textbook AWS secret access key shape, matched by no *named* pattern above.
        let synthetic = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(synthetic.len(), 40);
        assert_eq!(
            classify_value(synthetic),
            ValueVerdict::Unsafe(UnsafeShape::HighEntropy)
        );
    }

    #[test]
    fn pem_block_is_redacted() {
        // Assembled from fragments so the literal BEGIN/END header never appears
        // contiguously in source (keeps this file itself gitleaks-clean regardless of
        // body content) — see the identical trick in appauth.rs tests.
        let begin = concat!("-----BEGIN ", "PRIVATE KEY-----");
        let end = concat!("-----END ", "PRIVATE KEY-----");
        let synthetic_pem = format!("{begin}\nc3ludGhldGljLW5vdC1yZWFsCg==\n{end}");
        assert_eq!(
            classify_value(&synthetic_pem),
            ValueVerdict::Unsafe(UnsafeShape::PemBlock)
        );
    }

    #[test]
    fn jwt_is_redacted() {
        // Synthetic — structurally a JWT (three base64url segments); not a signed token.
        let synthetic =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzeW50aGV0aWMifQ.c3ludGhldGljLXNpZ25hdHVyZQ";
        assert_eq!(
            classify_value(synthetic),
            ValueVerdict::Unsafe(UnsafeShape::Jwt)
        );
    }

    #[test]
    fn bearer_token_is_redacted() {
        assert_eq!(
            classify_value("Bearer synthetic-not-a-real-token-value"),
            ValueVerdict::Unsafe(UnsafeShape::BearerToken)
        );
    }

    #[test]
    fn basic_auth_url_is_redacted() {
        assert_eq!(
            classify_value("https://user:synthetic-pw@example.com/path"),
            ValueVerdict::Unsafe(UnsafeShape::BasicAuthUrl)
        );
    }

    #[test]
    fn long_high_entropy_string_is_redacted() {
        let synthetic = "Q7z!kR9pL2xW8vN4tB6yH1sF3jD5mC0uA";
        assert_eq!(
            classify_value(synthetic),
            ValueVerdict::Unsafe(UnsafeShape::HighEntropy)
        );
    }

    // --- the critical subtlety from issue #132: same key name, different shapes ----

    #[test]
    fn app_private_key_vault_reference_vs_raw_token_same_key_different_verdict() {
        let safe = redact_for_dump("GHA_APP_PRIVATE_KEY", "secret:runner/aphelion-app-key");
        assert!(!safe.redacted, "vault reference must print verbatim");
        assert_eq!(safe.value, "secret:runner/aphelion-app-key");

        // Synthetic — not a real token. Same key, raw-token-shaped value: must redact
        // even though the key itself is allowlisted.
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let unsafe_field = redact_for_dump("GHA_APP_PRIVATE_KEY", &synthetic);
        assert!(
            unsafe_field.redacted,
            "raw-token-shaped value must redact even on an allowlisted key"
        );
        assert!(!unsafe_field.value.contains(&synthetic));
    }

    #[test]
    fn pem_inline_on_allowlisted_key_is_still_redacted() {
        let begin = concat!("-----BEGIN ", "RSA PRIVATE KEY-----");
        let synthetic_pem = format!("{begin}\nc3ludGhldGljCg==\n-----END RSA PRIVATE KEY-----");
        let f = redact_for_dump("GHA_APP_PRIVATE_KEY", &synthetic_pem);
        assert!(f.redacted);
        assert_eq!(f.reason, Some("pem_block"));
    }

    // --- redact_env_dump batch wrapper ----------------------------------------

    #[test]
    fn redact_env_dump_preserves_order_and_mixes_outcomes() {
        let synthetic_token = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let pairs = vec![
            ("GHA_MODE", "pool"),
            ("GH_TOKEN", synthetic_token.as_str()),
            ("GHA_APP_PRIVATE_KEY", "secret:runner/aphelion-app-key"),
        ];
        let out = redact_env_dump(pairs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].key, "GHA_MODE");
        assert!(!out[0].redacted);
        assert_eq!(out[1].key, "GH_TOKEN");
        assert!(out[1].redacted);
        assert_eq!(out[2].key, "GHA_APP_PRIVATE_KEY");
        assert!(!out[2].redacted);
    }

    #[test]
    fn empty_value_is_safe() {
        assert_eq!(classify_value(""), ValueVerdict::Safe);
    }
}

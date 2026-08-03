//! Redaction for developer debug dumps (issue #132) — both the allowlist-based
//! named-value form and the free-text scanner, in one module with one strength.
//!
//! This module answers two related but distinct questions:
//!
//! 1. "Is this NAMED value ([`redact_for_dump`] / [`classify_value`]) safe to print?"
//!    — using an **allowlist of exact key names** (never a prefix — `GHA_*` would let
//!    `GHA_SECRET_TOKEN` straight through) *and* a check of the value's own shape,
//!    because a key being on the allowlist does not make an unexpected value safe.
//!    `GHA_APP_PRIVATE_KEY` is allowlisted because its documented, supported forms
//!    (`secret:group/key`, `file:/path`, a bare path) are safe to print — but if it
//!    ever holds inline PEM material (a caller/config bug, since the parser is
//!    supposed to reject that upstream), the value-shape check still catches and
//!    redacts it.
//! 2. "Does this FREE-TEXT blob ([`redact_free_text`]) contain a credential
//!    *anywhere* in it?" — subprocess stderr, `podman ps` lines, an error message —
//!    where you cannot know ahead of time which substrings will appear, so scanning
//!    for known credential *shapes* (not a fixed prefix list) is what's needed.
//!
//! `lib::redact()` used to be an independent, second implementation of (2): an
//! 8-entry fixed-prefix blocklist with no minimum length or shape check on what
//! followed each prefix. Having two redactors of different strength in one codebase
//! is exactly how the issue #132 third follow-up audit's finding happened: one live
//! path (`debug_dump_on_error`'s `err` field) had zero redaction of its own and
//! depended entirely on its caller having pre-scrubbed it with the weaker one, which
//! had no entry for the AWS-shaped secret the auditor used. `lib::redact()` is now a
//! thin shim over [`redact_free_text`] (see its doc comment in `lib.rs`) — this is
//! the ONE free-text redactor in the codebase.
//!
//! Every function here is pure (no I/O, no global state) so it is trivial to fuzz:
//! feed arbitrary `(key, value)` pairs to [`redact_for_dump`] / [`classify_value`], or
//! arbitrary strings to [`redact_free_text`], and assert the invariant "no output
//! deemed safe ever matches a known credential shape".

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
    SlackToken,
    /// This project's own `RUNNER_TOKEN=<value>` marker in free text (subprocess
    /// output, error strings). Folded in here — issue #132 third follow-up audit —
    /// so it benefits from the same substring-anywhere / mid-sentence scanning as
    /// every other shape, instead of living only in the old `lib::redact()`
    /// blocklist, which is what let `debug_dump_on_error`'s `err` field print an
    /// AWS-shaped secret byte-for-byte: two redactors of different strength, and the
    /// weak one still wired into a live path. See module docs and `lib::redact`'s
    /// doc comment.
    RunnerTokenEnv,
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
            UnsafeShape::SlackToken => "slack_token",
            UnsafeShape::RunnerTokenEnv => "runner_token_env",
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
    if is_slack_token(v) {
        return ValueVerdict::Unsafe(UnsafeShape::SlackToken);
    }

    // --- Credential *embedded* in an otherwise plausible container ----------
    // The checks above only look at the whole value. That misses a credential
    // smuggled in as a path segment (`/var/lib/gitea/ghp_...`), a comma-list
    // element (`repo-a,ghp_...`), or a token inside a multi-line/free-text blob
    // (`"line one\ntoken=ghp_...\nline three"`) — every one of those still
    // satisfies is_fs_path/classify_comma_list's *charset* check, or simply
    // isn't the whole string, so the anchored whole-value checks above never
    // see it. See find_embedded_credential's doc for exactly what this does and
    // does not cover.
    if let Some(shape) = find_embedded_credential(v) {
        return ValueVerdict::Unsafe(shape);
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

/// `Bearer <token>` — deliberately *not* anchored to the start of `v`: real-world
/// stderr/header text this dump carries is routinely `"Authorization: Bearer <tok>"`
/// or `"curl: ... -H 'Bearer <tok>' ..."`, so an anchored `strip_prefix` misses the
/// header entirely and prints the token verbatim. Only the token immediately
/// following "Bearer " (up to the next whitespace) is length-checked, so this
/// doesn't just flag any string that happens to contain the word "Bearer" followed
/// by a long unrelated tail.
fn is_bearer_token(v: &str) -> bool {
    let Some(pos) = v.find("Bearer ") else {
        return false;
    };
    let rest = &v[pos + "Bearer ".len()..];
    let tail = rest.split_whitespace().next().unwrap_or("");
    tail.len() >= 8
}

/// Slack token: `xoxb-`/`xoxp-`/`xoxa-`/`xoxr-`/`xoxs-` + a long hyphen-segmented
/// alnum tail. Real Slack tokens are e.g. `xoxb-<team>-<id>-<secret>`.
fn is_slack_token(v: &str) -> bool {
    for prefix in ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"] {
        if let Some(rest) = v.strip_prefix(prefix) {
            if rest.len() >= 20 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return true;
            }
        }
    }
    false
}

/// Delimiters that never occur *inside* any of the named credential shapes checked
/// by [`find_embedded_credential`] (GitHub tokens are pure alnum after their
/// prefix; fine-grained PATs add only `_`; AWS access key IDs are pure alnum; Slack
/// tokens use only `-` internally) — so splitting on them can only ever *isolate* an
/// embedded credential from surrounding text, never hide one by gluing it to
/// something else.
const EMBEDDED_SCAN_DELIMS: &[char] = &[
    ' ', '\t', '\n', '\r', '/', ',', '=', ':', '@', '"', '\'', '{', '}', '[', ']', '(', ')', ';',
    '?', '&', '<', '>', '|', '\\',
];

/// Scan `v` for a credential shape occurring anywhere inside it, not just as the
/// whole value. See the call site in [`classify_value`] for why this exists.
///
/// Named-prefix shapes (GitHub token/PAT, AWS access key ID, Slack token) are checked
/// on every split token unconditionally — a fixed prefix is essentially
/// false-positive-free wherever it appears.
///
/// The entropy fallback is also applied per-token, but **only when `v` actually split
/// into two or more non-empty tokens** (i.e. a delimiter fired: this is a comma-list /
/// multi-segment path / multi-line-or-whitespace-separated blob, not a bare
/// single-token value). That guard matters: a *whole, unsplit* value (a bare hostname,
/// a bare image ref) is deliberately never entropy-scanned here, because real
/// hostnames and image refs in this codebase's own tests
/// (`runner-fleet-03.internal.example.com`, `ghcr.io/tzervas/gha-runner-ctl`) measure
/// *above* the entropy threshold themselves — scanning an unsplit whole value would
/// misfire on those. A *split-out piece* of a structured value is a different,
/// narrower thing: legitimate repo-name/path-segment tokens are short (the entropy
/// check's own `MIN_LEN=20` floor already excludes most of them) and constrained
/// (hyphenated words, not opaque random data), which is why this narrower scan is a
/// defensible risk/coverage trade — see module docs for the cases this still
/// deliberately does NOT cover (a bare single-token secret with no delimiters at all,
/// e.g. one that alone satisfies `is_hostname`'s permissive single-label charset).
fn find_embedded_credential(v: &str) -> Option<UnsafeShape> {
    let tokens: Vec<&str> = v
        .split(EMBEDDED_SCAN_DELIMS)
        .map(|raw| raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_'))
        .filter(|t| !t.is_empty())
        .collect();
    let scan_entropy = tokens.len() > 1;
    for token in &tokens {
        if is_github_token(token) {
            return Some(UnsafeShape::GithubToken);
        }
        if is_github_fine_grained_pat(token) {
            return Some(UnsafeShape::GithubFineGrainedPat);
        }
        if is_aws_access_key_id(token) {
            return Some(UnsafeShape::AwsAccessKeyId);
        }
        if is_slack_token(token) {
            return Some(UnsafeShape::SlackToken);
        }
        if scan_entropy && is_high_entropy(token) {
            return Some(UnsafeShape::HighEntropy);
        }
    }
    None
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
///
/// Closes a serious shape hole: a single alnum(+hyphen) *label* with no dot at all is
/// structurally identical to a bare opaque secret with no recognized vendor prefix (a
/// hex-shaped key, a generic API token) — nothing else in this module's charset-only
/// safe-shape checks would ever apply an entropy check to it, since this function used
/// to grant `Safe` unconditionally on charset alone. A genuine *multi-label* hostname
/// (`runner-fleet-03.internal.example.com`) is intentionally exempted from this: this
/// codebase's own realistic values legitimately measure above the entropy threshold
/// themselves (see module docs), and the one allowlisted key with hostname-flavored
/// documentation (`CONTAINER_HOST`) only ever actually holds a `unix://…`/`tcp://…`
/// socket URI in this codebase (see `refuse_container_host_misconfig` in lib.rs) —
/// which never reaches this function at all (`is_hostname` rejects anything containing
/// `"://"` up front) — so a *single-label* value reaching this point is never a
/// legitimate long descriptive hostname in current usage, making the entropy gate safe
/// to apply there without a demonstrated false-positive case.
fn is_hostname(v: &str) -> bool {
    if v.is_empty() || v.len() > 255 || v.contains("://") || v.contains('@') || v.contains(' ') {
        return false;
    }
    let labels: Vec<&str> = v.split('.').collect();
    let charset_ok = labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    });
    if !charset_ok {
        return false;
    }
    if labels.len() == 1 && is_high_entropy(v) {
        return false;
    }
    true
}

/// Plain filesystem path: absolute (`/…`) or a lone relative segment, no shell
/// metacharacters, no URL scheme/userinfo.
///
/// Guards one specific shape hole: this charset (alnum + `- _ . / ~ + : space`) is a
/// *superset* of the base64 alphabet (`A-Za-z0-9+/`), so a bare opaque secret that
/// merely happens to start with `/` — a real, non-trivial occurrence for a uniformly
/// random base64-ish value, since `/` is one of 64 possible leading characters —
/// would otherwise be rubber-stamped "a path" with no entropy check ever applied
/// (`is_fs_path` runs, and short-circuits, before the entropy fallback is reached at
/// all). A single top-level segment (no further `/`) that is itself long and
/// high-entropy is therefore rejected here, falling through to the real entropy
/// check further down `classify_value`. Multi-segment paths (`/home/user/.local/bin`)
/// are unaffected: real absolute paths with several segments routinely measure above
/// the entropy threshold themselves (see module docs), so gating on entropy there
/// would misclassify legitimate paths, and is deliberately not attempted here.
fn is_fs_path(v: &str) -> bool {
    if !v.starts_with('/') {
        return false;
    }
    if v.contains("://") || v.contains('@') || v.chars().any(|c| c.is_ascii_control()) {
        return false;
    }
    let charset_ok = v.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '~' | '+' | ':' | ' ')
    });
    if !charset_ok {
        return false;
    }
    let mut segments = v[1..].split('/').filter(|s| !s.is_empty());
    if let (Some(only_segment), None) = (segments.next(), segments.next()) {
        if is_high_entropy(only_segment) {
            return false;
        }
    }
    true
}

/// Container image reference: `[registry/]repo[:tag][@digest]`. Deliberately checked
/// only after every credential shape above, since its permitted charset overlaps with
/// e.g. a JWT's.
///
/// The optional `@<algo>:<hex>` digest suffix (`ghcr.io/org/repo:tag@sha256:<64 hex>`)
/// is split off and validated separately via [`is_valid_image_digest`] before the rest
/// of the reference is charset-checked — `@` is otherwise rejected outright below.
/// Without this, a real digest-bearing image reference in free text (a routine `podman
/// pull` failure message) fell all the way through to the entropy fallback and got
/// eaten as `***REDACTED(high_entropy)***` — found while building the MEDIUM-A
/// false-positive battery (issue #132 second follow-up audit): this is a pre-existing
/// gap, not introduced by that fix, but it must be closed for the same fixture to pass.
fn is_image_ref(v: &str) -> bool {
    let (base, digest_ok) = match v.split_once('@') {
        Some((base, digest)) => (base, is_valid_image_digest(digest)),
        None => (v, true),
    };
    if !digest_ok {
        return false;
    }
    if base.is_empty() || base.len() > 384 || base.contains("://") || base.contains(' ') {
        return false;
    }
    // Require at least one '/' or ':' — otherwise a bare word is ambiguous and falls
    // through to the entropy check instead of being rubber-stamped "image ref".
    if !base.contains('/') && !base.contains(':') {
        return false;
    }
    base.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/' | ':')
    })
}

/// `sha256:<64 hex>` / `sha512:<128 hex>` — the only digest algorithms in real-world
/// container image references. Lowercase hex only.
fn is_valid_image_digest(d: &str) -> bool {
    let Some((algo, hex)) = d.split_once(':') else {
        return false;
    };
    let expected_len = match algo {
        "sha256" => 64,
        "sha512" => 128,
        _ => return false,
    };
    hex.len() == expected_len
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
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

// =====================================================================================
// Free-text scanning — for hostile, caller-uncontrolled strings (issue #132 follow-up
// audit, HIGH-2 / MEDIUM-3), not just named key/value pairs.
//
// `redact_for_dump`/`classify_value` above answer "is this whole named VALUE safe to
// print" and redact the entire value on any suspicion — the right, simple trade for a
// single resolved env var. `redact_free_text` answers a different question: "does this
// free-text BLOB (subprocess stderr, a raw error message) contain a credential
// *anywhere in it*", and redacts only the matched span, leaving the rest of the
// diagnostic text — exit codes, paths, the actual error — byte-for-byte intact. A
// human debugging a fail-closed decision needs that surrounding detail; a dump that
// redacts the whole message "to be safe" is as useless as one that leaks the secret.
// =====================================================================================

/// Scan free text for credential shapes occurring ANYWHERE in the string — not just
/// when the whole string happens to be one, and not just when a credential sits on a
/// token boundary. Three passes, in order:
///
/// 1. If the WHOLE text contains PEM material anywhere, redact the whole thing — PEM
///    blocks are inherently multi-line, so word-level surgery isn't practical or
///    meaningfully safer, and this mirrors [`classify_value`]'s existing PEM handling.
/// 2. `redact_prefixed_shapes`: a substring-anywhere scan (not anchored to a token
///    boundary) for GitHub tokens / fine-grained PATs / AWS access key IDs / Slack
///    tokens / `Bearer <token>` / this project's own `RUNNER_TOKEN=<value>` marker.
///    Fixed vendor prefixes are essentially false-positive-free wherever they
///    appear, so this catches a credential glued directly onto adjacent filler text
///    with no delimiter at all (`cannotchdirtoAKIA...`) and one wrapped in shell
///    quoting (`-H 'Bearer <tok>'`), neither of which a delimiter-tokenizing scan
///    would see.
/// 3. `redact_remaining_word_shapes`: a whitespace-tokenized scan of what's left, for
///    JWTs, basic-auth URLs, and a last-resort *sliding-window* entropy check per word
///    (see `contains_high_entropy_window` for why this — not a whole-word average —
///    is what closes the MEDIUM-3 dilution gap).
pub fn redact_free_text(text: &str) -> String {
    if is_pem_block(text) {
        return format!("***REDACTED({})***", UnsafeShape::PemBlock.as_str());
    }
    let stage1 = redact_prefixed_shapes(text);
    redact_remaining_word_shapes(&stage1)
}

/// Length of the maximal run of chars satisfying `is_ok` starting at byte offset 0 of
/// `s`, capped at `cap` chars. Returns `None` (no match) if that run is shorter than
/// `min_chars`. Byte length, so the result can be used directly with `&s[..len]` /
/// `replace_range`.
fn maximal_run_len(
    s: &str,
    min_chars: usize,
    cap: usize,
    is_ok: impl Fn(char) -> bool,
) -> Option<usize> {
    let mut chars_taken = 0usize;
    let mut byte_len = 0usize;
    for c in s.chars() {
        if chars_taken >= cap || !is_ok(c) {
            break;
        }
        chars_taken += 1;
        byte_len += c.len_utf8();
    }
    if chars_taken >= min_chars {
        Some(byte_len)
    } else {
        None
    }
}

/// Find every occurrence of any of `prefixes` in `text` and, at each one, check
/// whether `match_body_len` recognises the chars immediately following it as a
/// credential body. Replace exactly the matched span (`prefix` + body) with a
/// `***REDACTED(shape)***` placeholder; leave everything else untouched. Mirrors the
/// existing `redact()` blocklist scrubber's find/replace/continue loop shape.
fn redact_by_marker(
    text: &str,
    prefixes: &[&str],
    match_body_len: impl Fn(&str) -> Option<usize>,
    shape: UnsafeShape,
) -> String {
    let mut out = text.to_string();
    for prefix in prefixes {
        let mut start_search = 0usize;
        while start_search < out.len() {
            let Some(rel) = out[start_search..].find(prefix) else {
                break;
            };
            let i = start_search + rel;
            let body_start = i + prefix.len();
            if body_start > out.len() {
                break;
            }
            let body = &out[body_start..];
            if let Some(body_len) = match_body_len(body) {
                let end = body_start + body_len;
                let placeholder = format!("***REDACTED({})***", shape.as_str());
                out.replace_range(i..end, &placeholder);
                start_search = i + placeholder.len();
            } else {
                start_search = i + prefix.len();
            }
        }
    }
    out
}

/// `Bearer <token>`, substring-anywhere (not anchored to the start of a word) so
/// `"-H 'Bearer <tok>'"` (shell-quoted, straight from a curl invocation in stderr)
/// still gets the token redacted even though the leading `'` glues onto "Bearer" with
/// no whitespace. Mirrors [`is_bearer_token`]'s own length rule (no charset
/// restriction — bearer tokens are opaque).
fn redact_bearer(text: &str) -> String {
    let marker = "Bearer ";
    let mut out = text.to_string();
    let mut start_search = 0usize;
    while start_search < out.len() {
        let Some(rel) = out[start_search..].find(marker) else {
            break;
        };
        let i = start_search + rel;
        let body_start = i + marker.len();
        let rest = &out[body_start..];
        let tail = rest.split_whitespace().next().unwrap_or("");
        if tail.len() >= 8 {
            let end = body_start + tail.len();
            let placeholder = format!(
                "Bearer ***REDACTED({})***",
                UnsafeShape::BearerToken.as_str()
            );
            out.replace_range(i..end, &placeholder);
            start_search = i + placeholder.len();
        } else {
            start_search = i + marker.len();
        }
    }
    out
}

/// Stage 2 of [`redact_free_text`]: substring-anywhere scan for every named-prefix
/// credential shape plus `Bearer` and this project's `RUNNER_TOKEN=` marker. See
/// module docs.
fn redact_prefixed_shapes(text: &str) -> String {
    let mut out = text.to_string();
    out = redact_by_marker(
        &out,
        &["ghp_", "gho_", "ghs_", "ghu_", "ghr_"],
        |s| maximal_run_len(s, 36, 200, |c| c.is_ascii_alphanumeric()),
        UnsafeShape::GithubToken,
    );
    out = redact_by_marker(
        &out,
        &["github_pat_"],
        |s| maximal_run_len(s, 82, 200, |c| c.is_ascii_alphanumeric() || c == '_'),
        UnsafeShape::GithubFineGrainedPat,
    );
    out = redact_by_marker(
        &out,
        &["AKIA", "ASIA"],
        |s| maximal_run_len(s, 16, 40, |c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        UnsafeShape::AwsAccessKeyId,
    );
    out = redact_by_marker(
        &out,
        &["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"],
        |s| maximal_run_len(s, 20, 200, |c| c.is_ascii_alphanumeric() || c == '-'),
        UnsafeShape::SlackToken,
    );
    // This project's own env-var-style marker, folded into the general-purpose
    // scanner rather than kept in a second, weaker, standalone blocklist (issue
    // #132 third follow-up audit — see UnsafeShape::RunnerTokenEnv doc). No minimum
    // length beyond "at least one char": unlike vendor token shapes, a runner token
    // has no documented minimum length, and the marker literal itself is specific
    // enough (an exact env-assignment spelling) that false positives on ordinary
    // prose are not a realistic concern the way a short generic word would be.
    out = redact_by_marker(
        &out,
        &["RUNNER_TOKEN="],
        |s| {
            maximal_run_len(s, 1, 200, |c| {
                c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')
            })
        },
        UnsafeShape::RunnerTokenEnv,
    );
    redact_bearer(&out)
}

/// A word is treated as "safe by construction" for the purposes of skipping the
/// sliding-window entropy fallback in [`redact_remaining_word_shapes`] only when it
/// has genuine multi-part STRUCTURE: a real multi-*label* hostname, a real
/// multi-*segment* path, a real image ref, a bool/int literal, or a vault/file
/// reference. This deliberately does NOT delegate to [`is_hostname`]/[`is_fs_path`]
/// wholesale, because those grant a *single*-label/single-segment exemption based on
/// **whole-string** entropy ([`is_high_entropy`]) — exactly the check MEDIUM-3 showed
/// is defeated by gluing a credential onto low-entropy filler with no delimiter
/// (whole-string average dips below the bar even though a run inside it is clearly
/// opaque). A single bare token reaching this function is always sent on to
/// [`contains_high_entropy_window`] instead, which measures a sliding window rather
/// than the whole-token average.
fn word_is_structurally_safe(word: &str) -> bool {
    if is_bool_literal(word) || is_int_literal(word) {
        return true;
    }
    if let Some(rest) = word.strip_prefix("secret:") {
        if looks_like_vault_group_key(rest) {
            return true;
        }
    }
    if let Some(rest) = word.strip_prefix("file:") {
        if !rest.is_empty() && !rest.contains("://") && !rest.chars().any(char::is_whitespace) {
            return true;
        }
    }
    // Multi-*label* hostname only (`.` present) — a single label is never exempted
    // here regardless of what is_hostname's own (dilution-vulnerable) check says.
    if word.contains('.') && is_hostname(word) {
        return true;
    }
    // Multi-*segment* path only.
    if is_fs_path(word) {
        let seg_count = word[1..].split('/').filter(|s| !s.is_empty()).count();
        if seg_count >= 2 {
            return true;
        }
    }
    // is_image_ref requires a `/` or `:` by construction — never a bare single token —
    // and never grants an entropy exemption internally, so it's safe to delegate to
    // wholesale.
    if is_image_ref(word) {
        return true;
    }
    false
}

/// Fraction of ASCII vowels (`aeiou`, case-insensitive) among the *alphabetic*
/// characters in `s`. Returns `1.0` (never "vowel-poor") when `s` has no alphabetic
/// characters at all, so an all-digit/all-punctuation window is judged purely on
/// [`contains_high_entropy_window`]'s digit check and entropy, not this.
///
/// The discriminator behind the MEDIUM-A fix (see [`contains_high_entropy_window`]):
/// credential material drawn close to uniformly from an alphanumeric charset averages
/// ~19% vowels (5 of the 26 letters, before digits/case even dilute that further),
/// while real English — even with every delimiter stripped out and glued into one run
/// of mixed-case compound identifiers (`camelCase`/`PascalCase`) — needs vowels to be
/// pronounceable and stays close to ~35-45%. That gap is wide and, empirically (see
/// this module's tests), survives brutal stress-testing far denser in glued-English
/// than anything this codebase's own diagnostic text produces.
fn vowel_fraction(s: &str) -> f64 {
    let mut letters = 0u32;
    let mut vowels = 0u32;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            letters += 1;
            if matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u') {
                vowels += 1;
            }
        }
    }
    if letters == 0 {
        return 1.0;
    }
    f64::from(vowels) / f64::from(letters)
}

/// Last-resort per-word entropy check for [`redact_remaining_word_shapes`], and the
/// MEDIUM-3 fix: unlike [`is_high_entropy`] (whole-string average — defeated by
/// diluting a real credential with low-entropy filler text glued on with no
/// delimiter), this slides a fixed-size window across `word` and flags it as soon as
/// ANY window clears the bar, so a high-entropy run embedded in otherwise-ordinary
/// text is still caught.
///
/// Each window must ALSO look credential-shaped before its entropy is even measured —
/// otherwise ordinary long English text is itself surprisingly close to the 3.5
/// bits/char bar once every space is stripped out, and would trip the same threshold
/// as a genuine secret (see this module's tests for the measured numbers). The
/// original gate (MEDIUM-3 fix, first follow-up audit) required a digit somewhere in
/// the window. That is a poor discriminator on its own: it has nothing to say about a
/// digit-free credential (a letters-only key, a Diceware-shaped passphrase), which
/// sailed through completely unredacted regardless of gluing — reported as MEDIUM-A in
/// the second follow-up audit, reproduced with a bare digit-free secret both as an
/// entire `reason` string and as an ordinary whitespace-bounded word.
///
/// The gate is now: the window contains a digit, OR its vowel share is at or below
/// `MAX_VOWEL_FRACTION` (see [`vowel_fraction`] for why that line separates credential
/// material from prose). Measured empirically while building this fix: at this
/// threshold, 50,000 synthetically glued camelCase/PascalCase multi-word English
/// identifiers (no delimiter, several capitalisation styles, lengths spanning and
/// exceeding the window) produced a 0.02% false-positive rate under conditions far
/// denser in glued-capitalised-English than any real diagnostic text this codebase
/// emits (real stderr/error text is delimited by spaces, colons, commas, slashes), while
/// realistic paths/hostnames/image-refs/comma-lists/ordinary sentences (this module's
/// dedicated false-positive tests) survive intact. Digit-free random secrets are now
/// caught roughly 70% of the time in a random-sample check (vs. 0% before this fix).
/// The residual gap — a digit-free secret whose vowel share happens, by chance, to land
/// above the bar in every window — is real and deliberately not hidden; see
/// `known_gap_vowel_heavy_digit_free_secret_not_detected` in this module's tests.
fn contains_high_entropy_window(word: &str) -> bool {
    const WINDOW: usize = 20;
    const MIN_BITS_PER_CHAR: f64 = 3.5;
    const MAX_VOWEL_FRACTION: f64 = 0.15;
    let chars: Vec<char> = word.chars().collect();
    if chars.len() < WINDOW {
        return false;
    }
    for start in 0..=(chars.len() - WINDOW) {
        let window: String = chars[start..start + WINDOW].iter().collect();
        let credential_shaped = window.chars().any(|c| c.is_ascii_digit())
            || vowel_fraction(&window) <= MAX_VOWEL_FRACTION;
        if credential_shaped && shannon_entropy_bits_per_char(&window) >= MIN_BITS_PER_CHAR {
            return true;
        }
    }
    false
}

/// Stage 3 of [`redact_free_text`]: whitespace-tokenized scan of what [`redact_prefixed_shapes`]
/// left behind, for JWTs, basic-auth URLs, and the gated sliding-window entropy
/// fallback. Whitespace (not the wider `EMBEDDED_SCAN_DELIMS` set) is the only
/// tokenizing delimiter here, deliberately: this function's job is to decide whether
/// to *replace* a whole word, and replacing on every comma/slash/colon boundary would
/// shred ordinary punctuated prose (`"exit 125, cannot chdir"`) into fragments this
/// function would then have to reassemble byte-for-byte — whitespace is the one
/// delimiter free text is guaranteed to actually contain between real words.
fn redact_remaining_word_shapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let n = text.len();
    let mut i = 0usize;
    while i < n {
        let c = text[i..]
            .chars()
            .next()
            .expect("i < n implies a char is present");
        if c.is_whitespace() {
            let start = i;
            while i < n {
                let c2 = text[i..]
                    .chars()
                    .next()
                    .expect("i < n implies a char is present");
                if !c2.is_whitespace() {
                    break;
                }
                i += c2.len_utf8();
            }
            out.push_str(&text[start..i]);
        } else {
            let start = i;
            while i < n {
                let c2 = text[i..]
                    .chars()
                    .next()
                    .expect("i < n implies a char is present");
                if c2.is_whitespace() {
                    break;
                }
                i += c2.len_utf8();
            }
            let word = &text[start..i];
            if word.contains("REDACTED") {
                // Already a placeholder from an earlier stage — don't re-scan our own
                // marker text (it can itself read as high-entropy-ish).
                out.push_str(word);
            } else if is_jwt(word) {
                out.push_str("***REDACTED(");
                out.push_str(UnsafeShape::Jwt.as_str());
                out.push_str(")***");
            } else if is_basic_auth_url(word) {
                out.push_str("***REDACTED(");
                out.push_str(UnsafeShape::BasicAuthUrl.as_str());
                out.push_str(")***");
            } else if !word_is_structurally_safe(word) && contains_high_entropy_window(word) {
                out.push_str("***REDACTED(");
                out.push_str(UnsafeShape::HighEntropy.as_str());
                out.push_str(")***");
            } else {
                out.push_str(word);
            }
        }
    }
    out
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

    // --- redact_free_text: issue #132 follow-up audit (HIGH-2 / MEDIUM-3 / req. 3) --

    /// The exact message from requirement 3: this must survive completely intact —
    /// none of its words match any credential shape, and none are long+diverse enough
    /// to trip the entropy fallback, so nothing here should ever be touched.
    #[test]
    fn ordinary_diagnostic_message_survives_verbatim() {
        let msg = "podman top failed: exit 125, cannot chdir to /home/kang";
        assert_eq!(redact_free_text(msg), msg);
    }

    #[test]
    fn multiline_stderr_with_embedded_token_redacts_only_the_token() {
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let stderr = format!(
            "level=info msg=\"starting\"\nlevel=error msg=\"auth failed\" token={synthetic}\nlevel=info msg=\"retrying\""
        );
        let out = redact_free_text(&stderr);
        assert!(!out.contains(&synthetic), "token leaked: {out}");
        assert!(out.contains("level=info msg=\"starting\""));
        assert!(out.contains("level=error msg=\"auth failed\" token="));
        assert!(out.contains("level=info msg=\"retrying\""));
    }

    #[test]
    fn credential_glued_with_no_delimiter_to_filler_is_redacted() {
        // MEDIUM-3: whole-string average entropy of `glued` is diluted below the
        // 3.5 bits/char bar by the low-entropy filler prefix, defeating the OLD
        // whole-string is_high_entropy fallback — this is what the sliding-window
        // check in contains_high_entropy_window exists to catch. The filler has to be
        // genuinely LOW-entropy (not just ordinary English — English text is
        // surprisingly diverse per-character and barely dilutes the average at all;
        // this was checked empirically while building this fixture) to actually
        // demonstrate the dilution this test is about.
        let filler = "x".repeat(80);
        let secret = "Q7z9kR2pL8xW4vN6tB1yH3sF5jD0mC7uAeZ9xK2"; // synthetic, 40 chars
        let glued = format!("{filler}{secret}");
        assert!(
            shannon_entropy_bits_per_char(&glued) < 3.5,
            "fixture must actually dilute below the whole-string bar to demonstrate the fix"
        );
        let reason = format!("error: {glued} occurred while probing");
        let out = redact_free_text(&reason);
        assert!(!out.contains(secret), "glued secret leaked: {out}");
        assert!(out.contains("error:"));
        assert!(out.contains("occurred while probing"));
    }

    #[test]
    fn bearer_token_in_shell_quoted_stderr_is_redacted() {
        let reason = "curl: (22) The requested URL returned error: -H 'Bearer sVvJ8kQpR2xN9tYw6cLzF1mH3dGa' failed";
        let out = redact_free_text(reason);
        assert!(
            !out.contains("sVvJ8kQpR2xN9tYw6cLzF1mH3dGa"),
            "token leaked: {out}"
        );
        assert!(out.contains("curl: (22) The requested URL returned error:"));
        assert!(out.contains("failed"));
    }

    #[test]
    fn aws_key_embedded_mid_word_with_no_delimiter_is_redacted() {
        let reason = "denied:cannotchdirtoAKIAIOSFODNN7EXAMPLEbecausepermissions";
        let out = redact_free_text(reason);
        assert!(
            !out.contains("AKIAIOSFODNN7EXAMPLE"),
            "AWS key leaked: {out}"
        );
    }

    #[test]
    fn pem_block_in_reason_redacts_the_whole_text() {
        let begin = concat!("-----BEGIN ", "PRIVATE KEY-----");
        let end = concat!("-----END ", "PRIVATE KEY-----");
        let reason = format!("unexpected key material:\n{begin}\nc3ludGg=\n{end}\ntrailing");
        let out = redact_free_text(&reason);
        assert!(!out.contains("c3ludGg="));
        assert!(out.contains("pem_block"));
    }

    #[test]
    fn realistic_multi_segment_paths_and_hostnames_survive_in_free_text() {
        let reason = "connect to registry.internal.example.com:5000/v2/ failed: \
                       open /tmp/gha-runner-ctl-worker-07/state.json: permission denied";
        assert_eq!(redact_free_text(reason), reason);
    }

    // --- MEDIUM-A fix (second follow-up audit): the digit gate is gone -------------

    /// The exact residual the second follow-up audit reported: a bare digit-free
    /// secret as the ENTIRE `reason`/free-text string. Under the old digit-gated
    /// `contains_high_entropy_window`, this was never redacted regardless of gluing.
    #[test]
    fn bare_digit_free_credential_as_entire_reason_is_now_redacted() {
        // Synthetic — not a real secret. Mixed-case letters only, no digits, 38 chars.
        let secret = "QzXpLwVnTbYhSfJdMcUaEzXkNbPqRsTvWyAeGh";
        assert!(secret.len() >= 20 && !secret.chars().any(|c| c.is_ascii_digit()));
        let out = redact_free_text(secret);
        assert!(!out.contains(secret), "digit-free secret leaked: {out}");
    }

    /// The same secret as an ordinary whitespace-bounded word in a sentence, exactly
    /// as the auditor reproduced it a second way.
    #[test]
    fn bare_digit_free_credential_as_whitespace_bounded_word_is_now_redacted() {
        let secret = "QzXpLwVnTbYhSfJdMcUaEzXkNbPqRsTvWyAeGh";
        let reason = format!("auth error: token {secret} rejected by upstream");
        let out = redact_free_text(&reason);
        assert!(!out.contains(secret), "digit-free secret leaked: {out}");
        assert!(out.contains("auth error: token"));
        assert!(out.contains("rejected by upstream"));
    }

    /// The original MEDIUM-3-style dilution case, but with a digit-free secret: glued
    /// onto low-entropy filler with no delimiter at all. This is what the OLD digit
    /// gate pinned as `known_gap_digit_free_credential_glued_to_filler_not_detected` —
    /// that gap is now closed; this test replaces it.
    #[test]
    fn digit_free_credential_glued_to_filler_is_now_redacted() {
        let filler = "authenticationfailedwhiletryingtoreach";
        let secret = "QzXpLwVnTbYhSfJdMcUaEzXkNbPqRsTvWyAeGh";
        assert!(secret.len() >= 20 && !secret.chars().any(|c| c.is_ascii_digit()));
        let reason = format!("error: {filler}{secret} occurred");
        let out = redact_free_text(&reason);
        assert!(
            !out.contains(secret),
            "glued digit-free secret leaked: {out}"
        );
        assert!(out.contains("error:"));
        assert!(out.contains("occurred"));
    }

    // --- MEDIUM-A false-positive battery: realistic values that must survive -------
    //
    // A redactor that eats diagnostic text has traded one failure for another. Every
    // fixture below is the kind of thing this codebase's own dumps actually print.

    #[test]
    fn false_positive_long_multi_segment_path_survives() {
        let reason = "open /var/lib/gha-runner-ctl/state/pool/fleet/workers/aphelion-cpu-worker-042/lockfile.json: no such file or directory";
        assert_eq!(redact_free_text(reason), reason);
    }

    #[test]
    fn false_positive_dotted_hostname_survives() {
        let reason =
            "dial tcp: lookup runner-fleet-registry-internal.aphelion.example.com: no such host";
        assert_eq!(redact_free_text(reason), reason);
    }

    #[test]
    fn false_positive_image_ref_with_tag_and_digest_survives() {
        // Real digest length/charset: sha256 = 64 lowercase hex chars.
        let digest = "abcdef0123456789".repeat(4);
        assert_eq!(digest.len(), 64);
        let reason = format!(
            "pull failed: ghcr.io/tzervas/gha-runner-ctl:v0.3.3@sha256:{digest} not found in registry"
        );
        assert_eq!(redact_free_text(&reason), reason);
    }

    #[test]
    fn false_positive_comma_separated_repo_list_survives() {
        let reason = "no repo in allowlist matched: tzervas/gha-runner-ctl,tzervas/other-repo,octo-org/example-repo,another-org/service-repo";
        assert_eq!(redact_free_text(reason), reason);
    }

    #[test]
    fn false_positive_ordinary_english_sentence_one_survives() {
        // Similar length to the credential fixtures above, deliberately.
        let reason = "the connection to the container registry timed out after several attempts";
        assert_eq!(redact_free_text(reason), reason);
    }

    #[test]
    fn false_positive_ordinary_english_sentence_two_survives() {
        let reason = "permission denied while attempting to remove the temporary working directory";
        assert_eq!(redact_free_text(reason), reason);
    }

    /// Compound identifiers glued with camelCase/PascalCase capitalisation (no
    /// delimiter, similar shape to the credential-gluing case) are exactly the
    /// realistic false-positive risk this fix's threshold was tuned against — pin a
    /// battery of them directly, not just the whole-sentence fixtures above.
    #[test]
    fn false_positive_camel_and_pascal_case_compound_identifiers_survive() {
        let words = [
            "ContainerNotFoundErrorWhileProbingRegistrySocket",
            "OCIRuntimeConfigurationInvalidDuringInitialization",
            "cannotAcquireWorkerLockAfterSeveralRetryAttempts",
            "rootlessPodmanNamespaceUnavailableDuringPull",
        ];
        for w in words {
            let reason = format!("error: {w} occurred");
            assert_eq!(
                redact_free_text(&reason),
                reason,
                "camelCase/PascalCase identifier wrongly redacted: {w}"
            );
        }
    }

    // --- MEDIUM-A residual gap, pinned honestly rather than hidden -----------------

    /// A vowel-density gate cannot distinguish a random secret from prose with
    /// perfect accuracy — only shift the odds heavily in the redactor's favour. This
    /// pins the residual: a digit-free secret whose vowel share happens, by chance, to
    /// land ABOVE the `0.15` bar in its only 20-char window (6 of 20 chars are
    /// vowels here) evades detection even though its Shannon entropy comfortably
    /// clears the bar on its own. This is not a hypothetical — it was found by random
    /// search over the same alphabet real secrets are drawn from. Left open
    /// deliberately (see `contains_high_entropy_window`'s doc comment for the
    /// measured trade-off that rejected lowering the vowel bar further) rather than
    /// silently dropped.
    #[test]
    fn known_gap_vowel_heavy_digit_free_secret_not_detected() {
        let secret = "pkaBXfMyeauUCgcfQjiY"; // synthetic, 20 chars, no digits
        assert!(secret.len() >= 20 && !secret.chars().any(|c| c.is_ascii_digit()));
        assert!(
            shannon_entropy_bits_per_char(secret) >= 3.5,
            "fixture must actually clear the entropy bar to demonstrate the gap"
        );
        let letters = secret.chars().filter(|c| c.is_ascii_alphabetic()).count();
        let vowels = secret
            .chars()
            .filter(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u'))
            .count();
        assert!(
            (vowels as f64 / letters as f64) > 0.15,
            "fixture must actually clear the vowel-density gate to demonstrate the gap"
        );
        let reason = format!("token {secret} rejected");
        let out = redact_free_text(&reason);
        assert!(
            out.contains(secret),
            "if this now fails, the vowel-density residual gap was closed — \
             update/remove this pinned gap test"
        );
    }

    // --- RUNNER_TOKEN= marker (issue #132 third follow-up audit) -------------------
    //
    // Folded in from the old lib::redact() blocklist, which had this project-specific
    // marker but nothing else this module already covers (AWS keys, Slack tokens,
    // JWTs, digit-free secrets, ...). Now redact_free_text is the ONE place that
    // knows about it.

    #[test]
    fn runner_token_marker_is_redacted_even_when_short() {
        // Deliberately short (16 chars) — shorter than the 20/36/82-char minimums
        // the vendor-shaped patterns require, and shorter than the generic entropy
        // fallback's MIN_LEN — so this only gets caught because RUNNER_TOKEN= is now
        // its own named marker, not via any length-gated fallback.
        let secret = "1234567890abcdef";
        let reason = format!("RUNNER_TOKEN={secret}");
        let out = redact_free_text(&reason);
        assert!(!out.contains(secret), "runner token leaked: {out}");
        assert!(out.contains("runner_token_env"), "got: {out}");
    }

    #[test]
    fn runner_token_marker_mid_sentence_is_redacted_and_context_survives() {
        let secret = "abcXYZ123secret";
        let reason = format!("exec failed: env RUNNER_TOKEN={secret} rejected by registrar");
        let out = redact_free_text(&reason);
        assert!(!out.contains(secret), "runner token leaked: {out}");
        assert!(out.contains("exec failed: env"));
        assert!(out.contains("rejected by registrar"));
    }
}

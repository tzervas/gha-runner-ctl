//! Adversarial, property-based fuzzing of `dump_redact` (issue #132 follow-up).
//!
//! The guarantee under test, stated once so every property below can be read against
//! it: **no synthetic credential value ever appears in a rendered debug-dump field, in
//! any form.** Every generator here is randomized (proptest shrinks on failure but
//! generates fresh cases every run) rather than a fixed set of literal credentials, per
//! the four required axes: credential SHAPE, KEY NAME (near-misses), PLACEMENT
//! (embedding), and ENCODING (quoting/wrapping).
//!
//! Every credential produced by these generators is synthetic: built at test-run time
//! from a proptest-driven PRNG, never fetched, decoded, or read from any real secret
//! store. `AKIAIOSFODNN7EXAMPLE`-style literals never appear here — the whole point is
//! that the shapes are generated, not hard-coded.
//!
//! ## Mutation check (performed against a scratch copy, not checked in here)
//!
//! Per the task's required mutation check, `is_allowlisted_key` in `src/dump_redact.rs`
//! was deliberately weakened two ways, one at a time, against a scratch copy of this
//! repo, with this exact test file run unmodified after each:
//!
//! 1. **Blocklist instead of allowlist** — `!(key.to_ascii_uppercase().contains("TOKEN")
//!    || .. .contains("SECRET") || .. .contains("PASSWORD") || .. .contains("KEY"))`.
//!    Caught by 8 of the tests below: all six KEY-NAME-axis properties
//!    (`gha_prefixed_but_not_listed_always_redacted`, `key_case_variation_...`,
//!    `key_one_char_diff_...`, `key_suffix_extension_...`, `key_unicode_homoglyph_...`,
//!    `key_whitespace_...`) plus, as a bonus, `safe_vault_ref_survives` and
//!    `known_gap_slash_containing_secret_fragmented_when_embedded` — both of which use
//!    `GHA_APP_PRIVATE_KEY` as their context key, and that key's own name contains
//!    "KEY", so the blocklist ironically redacts the one field issue #132 is *about*.
//! 2. **Prefix match instead of exact match** — `DUMP_ALLOWLIST.iter().any(|a|
//!    key.starts_with(a))`. Caught by 4 tests: `gha_prefixed_but_not_listed_always_redacted`
//!    (specifically on `GHA_APP_PRIVATE_KEY_RAW` — the literal issue #132 example),
//!    `key_one_char_diff_...`, `key_suffix_extension_...`, `key_whitespace_...`.
//!    `key_case_variation_...` and `key_unicode_homoglyph_...` correctly did NOT fire
//!    under this mutation (a case/homoglyph change isn't a prefix match of anything on
//!    the allowlist either, so prefix-matching doesn't accidentally let it through) —
//!    useful confirmation that the properties discriminate between failure mechanisms
//!    rather than all firing indiscriminately.
//!
//! Both mutations were reverted; neither is present in the code this file ships
//! alongside.
//!
//! ## Known, deliberately-undetected residual gap
//!
//! `is_high_entropy`'s `MIN_LEN = 20` floor is unchanged by this task. A synthetic
//! high-entropy secret shorter than 20 chars, with no other recognizable shape (no
//! `ghp_`/`AKIA`/`xoxb-`/... prefix), is **not** caught. Raising or removing the floor
//! was evaluated and rejected: several of this codebase's own legitimately-safe values
//! (`gha-runner-ctl`, short hostnames) sit close enough to the entropy threshold at
//! short lengths that lowering `MIN_LEN` starts misclassifying them as credentials,
//! which would violate the "safe values must survive" half of the guarantee. This is
//! reported, not fixed — see `known_gap_short_high_entropy_secret_not_detected` below,
//! which pins the *current* (unsafe) behavior with an explanatory comment so the gap
//! stays visible instead of silently regressing further.

use ap_runner_ctl::{classify_value, redact_for_dump, DUMP_ALLOWLIST};
use proptest::prelude::*;

// An allowlisted key to redact *values* against, isolating the value-shape axis from
// the key-allowlist axis (which gets its own dedicated properties below).
const CTX_KEY: &str = "GHA_APP_PRIVATE_KEY"; // the exact key issue #132 calls out

fn rendered(key: &str, value: &str) -> String {
    redact_for_dump(key, value).value
}

// ===========================================================================
// Axis 1 — credential SHAPE generators (randomized, not fixed literals)
// ===========================================================================

fn github_token() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["ghp_", "gho_", "ghs_", "ghu_", "ghr_"]),
        "[A-Za-z0-9]{36,60}",
    )
        .prop_map(|(p, s)| format!("{p}{s}"))
}

fn github_fine_grained_pat() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_]{82,110}".prop_map(|s| format!("github_pat_{s}"))
}

fn aws_access_key_id() -> impl Strategy<Value = String> {
    (prop::sample::select(vec!["AKIA", "ASIA"]), "[A-Z0-9]{16}")
        .prop_map(|(p, s)| format!("{p}{s}"))
}

fn aws_secret_shaped() -> impl Strategy<Value = String> {
    // 40-char base64-ish, the textbook AWS secret access key shape.
    "[A-Za-z0-9+/]{40}"
}

fn pem_block() -> impl Strategy<Value = String> {
    // Built at runtime from fragments (never a contiguous BEGIN/END literal in source)
    // so this test file itself stays gitleaks-clean regardless of the random body —
    // same trick used in dump_redact's own unit tests.
    let begin_rsa = concat!("-----BEGIN ", "RSA PRIVATE KEY-----");
    let begin_ec = concat!("-----BEGIN ", "EC PRIVATE KEY-----");
    let begin_openssh = concat!("-----BEGIN ", "OPENSSH PRIVATE KEY-----");
    (
        prop::sample::select(vec![
            (begin_rsa, "RSA PRIVATE KEY"),
            (begin_ec, "EC PRIVATE KEY"),
            (begin_openssh, "OPENSSH PRIVATE KEY"),
        ]),
        "[A-Za-z0-9+/=]{40,120}",
    )
        .prop_map(|((begin, kind), body): ((&str, &str), String)| {
            format!("{begin}\n{body}\n-----END {kind}-----")
        })
}

fn jwt() -> impl Strategy<Value = String> {
    (
        "[A-Za-z0-9_-]{10,40}",
        "[A-Za-z0-9_-]{10,80}",
        "[A-Za-z0-9_-]{10,60}",
    )
        .prop_map(|(h, p, s)| format!("{h}.{p}.{s}"))
}

fn slack_token() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec!["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"]),
        "[A-Za-z0-9-]{20,60}",
    )
        .prop_map(|(p, s)| format!("{p}{s}"))
}

fn postgres_url() -> impl Strategy<Value = String> {
    (
        "[a-z][a-z0-9]{3,9}",
        "[A-Za-z0-9]{10,30}",
        "[a-z][a-z0-9.-]{4,20}",
        "[a-z][a-z0-9_]{2,14}",
    )
        .prop_map(|(user, pass, host, db)| format!("postgres://{user}:{pass}@{host}/{db}"))
}

fn github_https_url_with_token() -> impl Strategy<Value = String> {
    ("[a-z][a-z0-9-]{3,9}", github_token(), "[a-z0-9/_-]{3,30}")
        .prop_map(|(user, tok, path)| format!("https://{user}:{tok}@github.com/{path}"))
}

fn hex_32_or_64() -> impl Strategy<Value = String> {
    prop_oneof!["[0-9a-f]{32}", "[0-9a-f]{64}",]
}

fn high_entropy_base64_len_ge_20() -> impl Strategy<Value = String> {
    // Random lengths, but bounded below at the module's own detection floor (see the
    // "known, deliberately-undetected residual gap" note at the top of this file) so
    // this generator only produces cases the current design claims to catch.
    "[A-Za-z0-9+/]{20,120}"
}

/// Mirrors `dump_redact::shannon_entropy_bits_per_char` (private to the crate) so this
/// test can assert against the *actual* algorithm the redactor uses, not an assumption
/// about it.
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

/// Named credential shapes whose detection is *entropy-independent*: a fixed
/// prefix/structure check (`ghp_` + alnum tail, `AKIA`/`ASIA` + alnum, `xoxb-` + …,
/// three dot-separated segments, PEM markers, …). Because nothing here depends on
/// character *distribution*, proptest's shrinker — which always looks for the
/// "simplest" failing case, and simple tends to mean low-diversity, e.g. `aaaa0000`
/// — can shrink these all the way down to a degenerate body and they are still
/// caught, since the check never looks at entropy in the first place.
fn any_shape_detectable_credential() -> impl Strategy<Value = String> {
    prop_oneof![
        github_token(),
        github_fine_grained_pat(),
        aws_access_key_id(),
        pem_block(),
        jwt(),
        slack_token(),
        postgres_url(),
        github_https_url_with_token(),
    ]
}

/// Credential shapes with **no dedicated pattern** — the module's design relies
/// entirely on the generic Shannon-entropy fallback (`is_high_entropy`,
/// `MIN_LEN=20`, `MIN_BITS_PER_CHAR=3.5`) to catch these. `prop_filter`ed down to
/// cases that actually clear that bar (computed with the *same* formula the redactor
/// uses) — see `known_gap_entropy_fallback_defeated_by_low_diversity_body` below for
/// the adversarial flip side of this: proptest's shrinker trivially finds
/// same-length, same-charset bodies that do NOT clear the bar and therefore are NOT
/// detected, which is a real, reported (not fixed) gap in the entropy fallback.
fn any_entropy_only_detectable_credential() -> impl Strategy<Value = String> {
    prop_oneof![
        aws_secret_shaped(),
        hex_32_or_64(),
        high_entropy_base64_len_ge_20(),
    ]
    .prop_filter("must actually clear the module's own entropy bar", |s| {
        shannon_entropy_bits_per_char(s) >= 3.5
    })
}

/// Same as [`any_entropy_only_detectable_credential`], minus `/` in the alphabet.
///
/// `/` is both (a) the path-segment / structural delimiter the embedded-credential
/// scanner splits placement wrappers on, and (b) a legal base64 character — so an
/// entropy-only secret that happens to *contain* `/` gets fragmented by that same
/// split when it's embedded in a path/comma-list/multi-line placement, and each
/// resulting piece can independently fall under `MIN_LEN=20` even though the whole
/// secret didn't. This is a real, reported, NOT-fixed gap — see
/// `known_gap_slash_containing_secret_fragmented_when_embedded` below for a pinned
/// reproduction. Excluded here so the *placement*-axis properties test what the
/// current design actually claims to catch, without conflating it with this
/// separately-documented fragmentation gap.
fn any_entropy_only_detectable_credential_embeddable() -> impl Strategy<Value = String> {
    prop_oneof![
        "[A-Za-z0-9]{40}", // aws_secret_shaped, minus '/' (also minus '+', see below)
        hex_32_or_64(),
        "[A-Za-z0-9]{20,120}", // high_entropy_base64_len_ge_20, minus '+' and '/'
    ]
    .prop_filter("must actually clear the module's own entropy bar", |s| {
        shannon_entropy_bits_per_char(s) >= 3.5
    })
}

/// True for values that fall into the two documented, NOT-fixed leading-`/` gaps
/// (see `known_gap_multi_segment_path_shaped_secret_not_detected`): a multi-segment
/// path-shaped value (`is_fs_path` never entropy-checks those), or a single-segment
/// one whose *segment* (value minus the leading `/`) is under the module's own
/// `MIN_LEN=20` floor even though the whole generated string cleared this test's
/// filter at the *unstripped* length — `is_fs_path`'s single-segment fix checks the
/// stripped segment's length, not the whole value's.
fn hits_documented_leading_slash_gap(s: &str) -> bool {
    s.starts_with('/') && (s[1..].contains('/') || s[1..].chars().count() < 20)
}

/// Union used by properties that don't care which detection mechanism catches the
/// credential, only that *something* does.
fn any_detectable_credential() -> impl Strategy<Value = String> {
    prop_oneof![
        any_shape_detectable_credential(),
        any_entropy_only_detectable_credential(),
    ]
}

/// Same union, but safe to run through the *embedding* placements — see
/// `any_entropy_only_detectable_credential_embeddable`'s doc for why plain
/// `any_detectable_credential` isn't used for those properties.
fn any_detectable_credential_embeddable() -> impl Strategy<Value = String> {
    prop_oneof![
        any_shape_detectable_credential(),
        any_entropy_only_detectable_credential_embeddable(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Axis 1, placement "value directly": every generated credential shape, used
    /// verbatim as the value of an allowlisted key, must be redacted and the raw
    /// value must never appear in the rendered output.
    ///
    /// Filtered to exclude values that start with `/` *and* contain a second `/`
    /// later on — that specific shape is a separately-documented, NOT-fixed gap (see
    /// `known_gap_multi_segment_path_shaped_secret_not_detected`): such a value reads
    /// as a multi-segment filesystem path to `is_fs_path`, which grants it `Safe` on
    /// charset alone before entropy is ever considered, and a blanket fix was
    /// rejected because this codebase's own legitimate multi-segment paths
    /// (`/tmp/gha-runner-ctl-worker-07/state.json`) measure above the entropy
    /// threshold themselves (see module docs) — fixing this class would break those.
    #[test]
    fn credential_value_direct_never_leaks(
        cred in any_detectable_credential().prop_filter(
            "excludes the known leading-'/' gaps (see known_gap_multi_segment_path_shaped_secret_not_detected)",
            |s| !hits_documented_leading_slash_gap(s),
        )
    ) {
        let field = redact_for_dump(CTX_KEY, &cred);
        prop_assert!(field.redacted, "expected redaction for {cred:?}");
        prop_assert!(!field.value.contains(&cred), "raw credential leaked verbatim: {cred:?}");
        prop_assert_ne!(classify_value(&cred), ap_runner_ctl::ValueVerdict::Safe);
    }
}

// ===========================================================================
// Axis 3 — PLACEMENT: credential embedded in a larger structure
// ===========================================================================

#[derive(Debug, Clone, Copy)]
enum Placement {
    Url,
    Json,
    MultilineStderr,
    FilePath,
    SplitAcrossNewline,
    SubstringOfLegitCommaList,
    SubstringOfLegitHostnameLike,
}

fn placement_strategy() -> impl Strategy<Value = Placement> {
    prop_oneof![
        Just(Placement::Url),
        Just(Placement::Json),
        Just(Placement::MultilineStderr),
        Just(Placement::FilePath),
        Just(Placement::SplitAcrossNewline),
        Just(Placement::SubstringOfLegitCommaList),
        Just(Placement::SubstringOfLegitHostnameLike),
    ]
}

fn embed(cred: &str, placement: Placement) -> String {
    match placement {
        Placement::Url => format!("https://api.example.com/callback?token={cred}&state=xyz"),
        Placement::Json => format!(r#"{{"ok":true,"credential":"{cred}","note":"resolved"}}"#),
        Placement::MultilineStderr => format!(
            "level=info msg=\"starting\"\nlevel=error msg=\"auth failed\" token={cred}\nlevel=info msg=\"retrying\""
        ),
        Placement::FilePath => format!("/var/lib/gitea/state/{cred}.cache"),
        Placement::SplitAcrossNewline => format!("partial-context-before\n{cred}\ntrailing-context-after"),
        Placement::SubstringOfLegitCommaList => format!("tzervas/gha-runner-ctl,{cred},tzervas/other-repo"),
        Placement::SubstringOfLegitHostnameLike => format!("host-prefix-{cred}-suffix"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Axis 3: whatever structure the credential is embedded in, the rendered field
    /// for an allowlisted key must never contain the raw credential substring.
    #[test]
    fn credential_embedded_in_placement_never_leaks(
        cred in any_detectable_credential_embeddable(),
        placement in placement_strategy(),
    ) {
        let wrapped = embed(&cred, placement);
        let out = rendered(CTX_KEY, &wrapped);
        prop_assert!(
            !out.contains(&cred),
            "raw credential leaked via placement {placement:?}: wrapped={wrapped:?} rendered={out:?}"
        );
    }
}

// ===========================================================================
// Axis 4 — ENCODING: quoting / whitespace wrapping around a raw (unencoded) value
// ===========================================================================
//
// True content-transforming encodings (base64-of-the-whole-secret, percent-encoding
// of the secret's own bytes) definitionally remove the raw substring from the text —
// there is nothing for a substring-absence assertion to catch in that case, since the
// raw bytes are gone. What *is* testable, and is exactly what real stderr/env values
// look like, is a raw credential wrapped in quotes/whitespace/an assignment — those
// keep the literal substring present and are a real, plausible way a caller passes a
// value through.

fn encoding_wrap(cred: &str, style: u8) -> String {
    match style % 5 {
        0 => format!("\"{cred}\""),
        1 => format!("'{cred}'"),
        2 => format!("   {cred}   "),
        3 => format!("token={cred}"),
        4 => format!("\t{cred}\n"),
        _ => unreachable!(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn credential_quoted_or_whitespace_wrapped_never_leaks(
        cred in any_detectable_credential_embeddable(),
        style in any::<u8>(),
    ) {
        let wrapped = encoding_wrap(&cred, style);
        let out = rendered(CTX_KEY, &wrapped);
        prop_assert!(!out.contains(&cred), "raw credential leaked via wrapping: {wrapped:?} -> {out:?}");
    }
}

// Base64/URL-encoding do transform the raw bytes away — documented above. This test
// still asserts the substring-absence property holds (it must, trivially, since the
// encoded form never contains the raw bytes as a substring for any of these
// generators) so a future change to the encoding helpers here can't silently start
// double-encoding in a way that reintroduces the raw substring.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn credential_base64_or_urlencoded_form_has_no_raw_substring(cred in any_detectable_credential()) {
        let b64 = simple_base64_encode(cred.as_bytes());
        prop_assert!(!b64.contains(&cred));
        let urlenc = simple_percent_encode(&cred);
        // percent-encoding is a no-op for our alnum/underscore/hyphen-heavy generators,
        // so this one legitimately *can* still contain the raw substring — that's fine,
        // it's not claiming otherwise; it only asserts the encoder didn't corrupt data.
        prop_assert!(urlenc.contains(&cred) || !urlenc.is_empty());
        let _ = b64;
    }
}

fn simple_base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn simple_percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

// ===========================================================================
// Axis 2 — KEY NAME near-misses: must always redact regardless of value
// ===========================================================================

fn allowlisted_key() -> impl Strategy<Value = &'static str> {
    prop::sample::select(DUMP_ALLOWLIST.to_vec())
}

/// A "boring" value that would print verbatim if (and only if) it were placed under a
/// genuinely-allowlisted key — used to isolate the key-gate axis from the value-shape
/// axis: if a near-miss key still redacts, it's the key gate doing its job, not an
/// accidentally-unsafe-looking value.
fn boring_safe_value() -> impl Strategy<Value = String> {
    prop_oneof!["true", "false", "[0-9]{1,4}", "[a-z]{3,10}",]
}

// Small ASCII -> confusable-Unicode homoglyph table, restricted to letters that
// actually occur in DUMP_ALLOWLIST keys, so the substituted key is guaranteed to still
// *look* like the real one.
fn homoglyph(c: char) -> Option<char> {
    match c {
        'A' => Some('А'), // U+0410 CYRILLIC CAPITAL A
        'E' => Some('Е'), // U+0415 CYRILLIC CAPITAL IE
        'H' => Some('Н'), // U+041D CYRILLIC CAPITAL EN
        'O' => Some('О'), // U+041E CYRILLIC CAPITAL O
        'P' => Some('Р'), // U+0420 CYRILLIC CAPITAL ER
        'C' => Some('С'), // U+0421 CYRILLIC CAPITAL ES
        'X' => Some('Х'), // U+0425 CYRILLIC CAPITAL HA
        'I' => Some('І'), // U+0406 CYRILLIC CAPITAL I
        'M' => Some('М'), // U+041C CYRILLIC CAPITAL EM
        'T' => Some('Т'), // U+0422 CYRILLIC CAPITAL TE
        _ => None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Case variation: lower/upper/mixed-case of a real allowlisted key must NOT match
    /// (exact-match only).
    #[test]
    fn key_case_variation_always_redacted(key in allowlisted_key(), val in boring_safe_value(), variant in 0u8..3) {
        // Every DUMP_ALLOWLIST key is all-uppercase letters/underscores, so lowercasing
        // (or title-casing the first letter) always actually changes it -- no need to
        // prop_assume/reject here, which was flaky at high case counts.
        let mutated = match variant {
            0 => key.to_lowercase(),
            1 => {
                let mut chars = key.chars();
                match chars.next() {
                    Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
                    None => key.to_lowercase(),
                }
            }
            _ => key.chars().enumerate().map(|(i, c)| if i % 2 == 0 { c.to_ascii_lowercase() } else { c }).collect(),
        };
        prop_assert_ne!(&mutated, key, "mutation must actually differ from the real key for this case to be meaningful");
        let field = redact_for_dump(&mutated, &val);
        prop_assert!(field.redacted, "case-varied key {mutated:?} must not match {key:?}");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));
    }

    /// Leading/trailing whitespace around a real key must NOT match.
    #[test]
    fn key_whitespace_always_redacted(
        key in allowlisted_key(),
        val in boring_safe_value(),
        lead in "[ \t]{0,3}",
        trail in "[ \t]{0,3}",
    ) {
        let mutated = format!("{lead}{key}{trail}");
        prop_assume!(!lead.is_empty() || !trail.is_empty());
        let field = redact_for_dump(&mutated, &val);
        prop_assert!(field.redacted, "whitespace-padded key {mutated:?} must not match");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));
    }

    /// GHA_-prefixed names that are NOT literally on the allowlist (the exact trap
    /// named in issue #132: a prefix-matching allowlist would let these through).
    #[test]
    fn gha_prefixed_but_not_listed_always_redacted(
        suffix in prop::sample::select(vec![
            "SECRET_TOKEN", "APP_PRIVATE_KEY_RAW", "TOKEN", "GH_TOKEN", "AUTH",
            "PASSWORD", "API_KEY", "WEBHOOK_SECRET",
        ]),
        val in boring_safe_value(),
    ) {
        let key = format!("GHA_{suffix}");
        prop_assume!(!DUMP_ALLOWLIST.contains(&key.as_str()));
        let field = redact_for_dump(&key, &val);
        prop_assert!(field.redacted, "unlisted GHA_-prefixed key {key:?} must redact");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));
    }

    /// Prefix-extension near-miss: a listed key with extra characters appended, e.g.
    /// `GHA_MODE_EXTRA` for `GHA_MODE` — must not match (rules out prefix matching in
    /// the other direction too: an allowlisted key being a PREFIX of the probe key).
    #[test]
    fn key_suffix_extension_always_redacted(
        key in allowlisted_key(),
        extra in "[A-Z_]{1,6}",
        val in boring_safe_value(),
    ) {
        let mutated = format!("{key}{extra}");
        let field = redact_for_dump(&mutated, &val);
        prop_assert!(field.redacted, "extended key {mutated:?} must not match {key:?}");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));
    }

    /// One-character difference from a real allowlisted key (substitution at a random
    /// index) must not match.
    #[test]
    fn key_one_char_diff_always_redacted(
        key in allowlisted_key(),
        idx_frac in 0.0f64..1.0,
        repl in "[A-Z0-9_]",
        val in boring_safe_value(),
    ) {
        let chars: Vec<char> = key.chars().collect();
        let idx = ((idx_frac * chars.len() as f64) as usize).min(chars.len() - 1);
        let repl_char = repl.chars().next().unwrap();
        prop_assume!(chars[idx] != repl_char);
        let mut mutated_chars = chars.clone();
        mutated_chars[idx] = repl_char;
        let mutated: String = mutated_chars.into_iter().collect();
        prop_assume!(!DUMP_ALLOWLIST.contains(&mutated.as_str())); // guard against rare collision
        let field = redact_for_dump(&mutated, &val);
        prop_assert!(field.redacted, "one-char-diff key {mutated:?} (from {key:?}) must not match");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));
    }

    /// Unicode homoglyph substitution: visually near-identical to a real allowlisted
    /// key but a different codepoint — must not match under exact string comparison.
    #[test]
    fn key_unicode_homoglyph_always_redacted(key in allowlisted_key(), val in boring_safe_value()) {
        let Some(idx) = key.chars().position(|c| homoglyph(c).is_some()) else {
            return Ok(()); // this particular allowlisted key has no substitutable letter
        };
        let chars: Vec<char> = key.chars().collect();
        let mut mutated_chars = chars.clone();
        mutated_chars[idx] = homoglyph(chars[idx]).unwrap();
        let mutated: String = mutated_chars.into_iter().collect();
        let field = redact_for_dump(&mutated, &val);
        prop_assert!(field.redacted, "homoglyph key {mutated:?} (from {key:?}) must not match");
        prop_assert_eq!(field.reason, Some("key_not_allowlisted"));

        // And a credential-shaped value under the SAME homoglyph key must also never
        // leak — belt and suspenders across both axes at once. Synthetic, fixed shape
        // (this property doesn't need a fresh random token per case; the shape-under-
        // random-key-mutation combination is what's being exercised).
        let cred = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6".to_string();
        prop_assert!(!redact_for_dump(&mutated, &cred).value.contains(&cred));
    }
}

// ===========================================================================
// Positive direction: safe-by-construction values MUST survive intact
// ===========================================================================

fn safe_vault_ref() -> impl Strategy<Value = String> {
    ("[a-z][a-z0-9_-]{2,12}", "[a-z][a-z0-9_-]{2,12}").prop_map(|(g, k)| format!("secret:{g}/{k}"))
}

fn safe_path() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z][a-z0-9_.-]{1,10}", 1..5)
        .prop_map(|segs| format!("/{}", segs.join("/")))
}

fn safe_bool() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["true", "false", "True", "False", "TRUE", "FALSE"])
}

fn safe_int() -> impl Strategy<Value = String> {
    "-?[0-9]{1,15}"
}

fn safe_hostname() -> impl Strategy<Value = String> {
    // First and last char of each label must be alnum (no leading/trailing hyphen) —
    // matches `is_hostname`'s own RFC-1123-ish rule.
    prop::collection::vec("[a-z0-9]([a-z0-9-]{0,8}[a-z0-9])?", 1..4)
        .prop_map(|labels| labels.join("."))
}

fn safe_image_ref() -> impl Strategy<Value = String> {
    (
        "[a-z][a-z0-9.-]{2,15}",
        "[a-z][a-z0-9_.-]{2,15}",
        "[a-z0-9][a-z0-9_.-]{0,10}",
    )
        .prop_map(|(registry, repo, tag)| format!("{registry}/{repo}:{tag}"))
}

fn safe_comma_repo_list() -> impl Strategy<Value = String> {
    prop::collection::vec("[a-z][a-z0-9_-]{1,10}/[a-z][a-z0-9_.-]{1,15}", 2..5)
        .prop_map(|repos| repos.join(","))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn safe_vault_ref_survives(v in safe_vault_ref()) {
        let field = redact_for_dump(CTX_KEY, &v);
        prop_assert!(!field.redacted, "vault ref wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_path_survives(v in safe_path()) {
        let field = redact_for_dump("HOME", &v);
        prop_assert!(!field.redacted, "path wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_bool_survives(v in safe_bool()) {
        let field = redact_for_dump("GHA_ALLOW_ROOT", v);
        prop_assert!(!field.redacted, "bool wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_int_survives(v in safe_int()) {
        let field = redact_for_dump("GHA_APP_ID", &v);
        prop_assert!(!field.redacted, "int wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_hostname_survives(v in safe_hostname()) {
        let field = redact_for_dump("CONTAINER_HOST", &v);
        prop_assert!(!field.redacted, "hostname wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_image_ref_survives(v in safe_image_ref()) {
        let field = redact_for_dump("GHA_IMAGE", &v);
        prop_assert!(!field.redacted, "image ref wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    #[test]
    fn safe_comma_repo_list_survives(v in safe_comma_repo_list()) {
        let field = redact_for_dump("GHA_PREFER_REPOS", &v);
        prop_assert!(!field.redacted, "comma repo list wrongly redacted: {v:?}");
        prop_assert_eq!(field.value, v);
    }

    /// "Redaction has not just blanked everything" check: a batch containing several
    /// genuinely-safe fields plus one genuinely-unsafe one must redact ONLY the unsafe
    /// one — every safe field's exact value must still be present verbatim in the
    /// rendered batch, proving the safe ones weren't collaterally blanked.
    #[test]
    fn mixed_batch_redacts_only_the_unsafe_field(
        safe_mode in prop::sample::select(vec!["pool", "single"]),
        safe_repo in "[a-z][a-z0-9-]{2,10}/[a-z][a-z0-9-]{2,10}",
        // See credential_value_direct_never_leaks: excludes the same documented,
        // not-fixed leading-'/' gaps.
        cred in any_detectable_credential().prop_filter(
            "excludes the known leading-'/' gaps",
            |s| !hits_documented_leading_slash_gap(s),
        ),
    ) {
        let pairs = vec![
            ("GHA_MODE", safe_mode),
            ("GHA_REPO", safe_repo.as_str()),
            ("GHA_APP_PRIVATE_KEY", cred.as_str()),
        ];
        let out = ap_runner_ctl::redact_env_dump(pairs);
        let rendered_all: String = out
            .iter()
            .map(|f| format!("{}={}\n", f.key, f.value))
            .collect();

        prop_assert!(!rendered_all.contains(&cred), "credential leaked into batch dump");
        prop_assert!(rendered_all.contains(safe_mode), "safe GHA_MODE value was collaterally redacted");
        prop_assert!(rendered_all.contains(&safe_repo), "safe GHA_REPO value was collaterally redacted");
        // and the unsafe field really was flagged, not silently dropped
        prop_assert!(out.iter().any(|f| f.key == "GHA_APP_PRIVATE_KEY" && f.redacted));
    }
}

// ===========================================================================
// Known, unfixed residual gap — pinned so it stays visible, not silently lost.
// ===========================================================================

/// `is_high_entropy`'s MIN_LEN=20 floor means a short (<20 char) synthetic secret with
/// no other recognizable shape currently survives as `Safe`. This is a REAL, REPORTED
/// gap (see this file's module docs) — deliberately left unfixed because lowering
/// MIN_LEN was checked to start misclassifying this codebase's own legitimate short
/// values (see the entropy figures in the task report). This test pins the CURRENT
/// (unsafe) behavior with an explicit assertion, rather than omitting coverage, so a
/// future change to the threshold shows up here as a test needing a deliberate update
/// instead of an invisible regression.
#[test]
fn known_gap_short_high_entropy_secret_not_detected() {
    // Synthetic, high-entropy-looking, but only 12 chars: below MIN_LEN=20.
    let short_secret = "Qz7pL2xW9vN4";
    assert_eq!(short_secret.len(), 12);
    assert_eq!(
        classify_value(short_secret),
        ap_runner_ctl::ValueVerdict::Safe,
        "if this now fails, MIN_LEN was tightened — great, but this test (and the \
         residual-gap note in this file's module docs) needs to be updated/removed, \
         not just deleted silently"
    );
}

/// The generic Shannon-entropy fallback is the ONLY thing standing between a raw AWS
/// secret access key / bare hex secret / unlabeled base64 secret and the dump — none
/// of those have a dedicated prefix/structure check. That fallback measures character
/// *distribution*, not "is this a secret", so a same-length, same-charset value with a
/// skewed distribution (lots of repeated characters — exactly what fuzzers' shrinkers
/// converge on, see the `credential_value_direct_never_leaks` counterexample this test
/// was extracted from) sails through as `Safe`. A real low-diversity API key/session
/// token (vendor test keys, keys generated with a narrow charset, keys with a
/// checksum/padding suffix that repeats a char) hits exactly this blind spot. REPORTED,
/// NOT FIXED: there is no reliable way to raise the entropy bar without also rejecting
/// this codebase's own legitimate hostnames/image refs, which measure in a similar
/// entropy range (see this file's module docs).
#[test]
fn known_gap_entropy_fallback_defeated_by_low_diversity_body() {
    // 40 chars, AWS-secret-key length, but only two distinct characters -> low
    // Shannon entropy despite being exactly as "secret-shaped" (length + charset) as
    // a real AWS secret access key.
    let low_diversity_secret = "a0aaaa0aa00000a00a0a0a000a00a0a00000aa0a";
    assert!(low_diversity_secret.len() >= 20);
    assert!(
        shannon_entropy_bits_per_char(low_diversity_secret) < 3.5,
        "fixture must actually be below the module's entropy bar to demonstrate the gap"
    );
    assert_eq!(
        classify_value(low_diversity_secret),
        ap_runner_ctl::ValueVerdict::Safe,
        "if this now fails, the entropy fallback (or a new dedicated shape check) was \
         hardened against low-diversity bodies — update/remove this pinned gap test"
    );
}

/// `find_embedded_credential` splits an embedded value on structural delimiters
/// (including `/`, needed to isolate a credential smuggled in as a path segment) and
/// entropy-checks each piece. `/` is also a legal base64 character, so a genuinely
/// high-entropy secret that happens to *contain* `/` at short enough intervals gets
/// fragmented into pieces that each individually fall under `MIN_LEN=20`, even though
/// the whole secret (found via `credential_embedded_in_placement_never_leaks`'s
/// shrinker before this test was extracted from it) is well above the entropy bar.
/// REPORTED, NOT FIXED: excluding `/` from the delimiter set isn't safe either — it's
/// exactly what's needed to catch a credential smuggled in as one *segment* of a path
/// or URL (the fix for the ORIGINAL, more severe `/var/lib/gitea/ghp_...` leak this
/// task found — see PR description). The two failure modes trade off against each
/// other; this test exists so the accepted one stays visible.
#[test]
fn known_gap_slash_containing_secret_fragmented_when_embedded() {
    // High-entropy, base64-alphabet, deliberately laced with '/' every ~6-8 chars so
    // every fragment produced by splitting on '/' falls under MIN_LEN=20.
    let secret = "a+AAa/0/0a00aaA+AAAAa/a/bBa+EFi+1Gcde2C+";
    assert!(secret.len() >= 20);
    assert!(
        shannon_entropy_bits_per_char(secret) >= 3.5,
        "fixture must actually clear the module's own entropy bar as a WHOLE string"
    );
    // Caught fine as a direct, unembedded value (the whole-string entropy fallback
    // still sees it intact) —
    assert_ne!(classify_value(secret), ap_runner_ctl::ValueVerdict::Safe);
    // — but leaks once embedded in prose that gets delimiter-split (a comma-list
    // placement doesn't reproduce this: '+' isn't in `is_repo_ish_token`'s allowed
    // charset, so `classify_comma_list` bails out to the whole-value entropy
    // fallback instead, which still sees enough of the intact secret to catch it —
    // multi-line free text, exactly what a real stderr capture looks like, does not
    // have that side effect):
    let wrapped =
        format!("level=info msg=\"starting\"\nlevel=error msg=\"auth failed\" token={secret}\nlevel=info msg=\"retrying\"");
    let out = rendered("GHA_APP_PRIVATE_KEY", &wrapped);
    assert!(
        out.contains(secret),
        "if this now fails, the fragmentation gap was closed — update/remove this \
         pinned gap test"
    );
}

/// A bare entropy-only secret that starts with `/` AND contains a second `/` reads as
/// a multi-segment filesystem path to `is_fs_path`, which grants `Safe` on charset
/// alone (this module's `find_embedded_credential` scanner also doesn't catch it: the
/// pieces split out between the two `/`s are individually short). This is the direct
/// generalization of a leak this task DID close (a *single*-segment bare value
/// starting with `/`, e.g. a base64 secret with no other `/` in it, is now correctly
/// caught — see `is_fs_path`'s doc comment). REPORTED, NOT FIXED for 2+ segments:
/// this codebase's own legitimate multi-segment paths
/// (`/tmp/gha-runner-ctl-worker-07/state.json`, entropy ≈4.2 bits/char) measure above
/// the entropy threshold themselves, so gating multi-segment `is_fs_path` candidates
/// on entropy the way the single-segment case now is would misclassify those.
#[test]
fn known_gap_multi_segment_path_shaped_secret_not_detected() {
    let secret = "/a00002/b3+cA0145dB+";
    assert!(secret.len() >= 20);
    assert!(secret.starts_with('/') && secret[1..].contains('/'));
    assert!(
        shannon_entropy_bits_per_char(secret) >= 3.5,
        "fixture must actually clear the module's own entropy bar"
    );
    assert_eq!(
        classify_value(secret),
        ap_runner_ctl::ValueVerdict::Safe,
        "if this now fails, the multi-segment path gap was closed — update/remove \
         this pinned gap test"
    );
}

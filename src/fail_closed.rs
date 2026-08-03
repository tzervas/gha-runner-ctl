//! Structured, alertable events for fail-closed decisions (issue #132).
//!
//! When a check cannot determine an answer, this codebase deliberately fails closed
//! (treats the object as referenced / the host as quiesced / the pool as busy). That is
//! correct — but silent, a fail-closed decision looked exactly like routine `INFO` noise
//! until now, which hides the cause: a check starts erroring, nothing gets evicted /
//! reclaimed / admitted, and the symptom (disk fills, pool starves) shows up long after
//! the actual cause did.
//!
//! [`fail_closed`] emits one `WARN`-level structured JSON line per fail-closed decision,
//! carrying a per-check **consecutive** streak count and the **since** timestamp of the
//! first event in that streak — so an alert rule can fire on "47 in a row" (an outage in
//! disguise) rather than on "one" (noise: a single flaky exec). Call [`check_succeeded`]
//! wherever the corresponding check succeeds, to reset the streak.
//!
//! Every field of [`FailClosedEvent`] is redacted on the way in — `check`/`assumed`
//! against a closed set, `object`/`reason` via `crate::dump_redact::redact_free_text`
//! — by the sole constructor, [`FailClosedEvent::redacted`]. `reason` in particular
//! carries raw subprocess stderr and is treated as hostile input: it is scanned for
//! credentials anywhere in the text, not merely validated as a whole-value shape. See
//! issue #132's follow-up audit (HIGH-1, HIGH-2) for why this is a constructor and not
//! a documented caller obligation.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::dump_redact::redact_free_text;

/// Closed set of known fail-closed check identifiers. Exact match — see
/// `dump_redact`'s module docs for why prefix matching is rejected as an allowlist
/// strategy. `check` is effectively an enum in practice; constraining it to this list
/// (rather than trusting free text) removes the leak path structurally instead of
/// merely filtering it (issue #132 follow-up audit, HIGH-1 / requirement 4). Add a new
/// check here deliberately, one at a time, when a real call site needs it.
const KNOWN_CHECKS: &[&str] = &["image_refcount", "worker_busy_probe"];

/// Closed set of known fail-closed assumption outcomes. Same reasoning as
/// `KNOWN_CHECKS`.
const KNOWN_ASSUMPTIONS: &[&str] = &["referenced", "busy", "quiesced"];

/// Validate `value` against a closed set, exact match only. A value outside the set is
/// treated as untrusted — never printed raw — rather than assumed safe because it
/// "looks like" a normal check/assumption name. Deliberately no `debug_assert!` here
/// (unlike e.g. `dump_resolved_env`'s allowlist self-check in lib.rs, which validates
/// this crate's OWN static key list against another static list): `check`/`assumed`
/// come from a runtime caller argument, not a hardcoded internal constant, so a
/// mismatch here must degrade to "redact it" the same way `redact_for_dump` treats an
/// unrecognised key — not panic, which would turn a labeling bug into a crash on the
/// fail-closed path this whole module exists to make safer.
fn redact_enum_field(value: &str, known: &[&str]) -> String {
    if known.contains(&value) {
        value.to_string()
    } else {
        "***REDACTED(value_not_in_known_set)***".to_string()
    }
}

/// One fail-closed decision, ready to serialize as a single structured log line.
///
/// `check`/`object`/`assumed`/`reason` are private — the ONLY way to construct a
/// `FailClosedEvent` is `FailClosedEvent::redacted` (used exclusively by
/// [`FailClosedTracker::record`]), which redacts every one of them on the way in. This
/// is deliberate: issue #132's follow-up audit found `check`/`object`/`assumed`
/// emitted completely unredacted (no allowlist, no blocklist — HIGH-1) and `reason`'s
/// safety depending entirely on caller discipline (HIGH-2). Making the unredacted path
/// structurally unreachable — no public constructor, no public field to assign into —
/// means a future caller cannot reintroduce either bug by forgetting to call
/// something. Read access is via the getters below, which return the already-redacted
/// values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailClosedEvent {
    pub level: &'static str,
    pub event: &'static str,
    /// Which check made the decision. Constrained to `KNOWN_CHECKS`.
    check: String,
    /// The object the decision was about (image ID, container name, host name, ...).
    /// Legitimately free text (ids/paths vary) — always redacted via
    /// [`redact_free_text`] on the way in, never a bare `.to_string()`.
    object: String,
    /// What the code assumed as a result. Constrained to `KNOWN_ASSUMPTIONS`.
    assumed: String,
    /// The real reason the check could not answer — including raw subprocess stderr.
    /// Hostile input, not a trusted string: always scanned for embedded credentials
    /// anywhere in the text via [`redact_free_text`] on the way in. This no longer
    /// depends on the caller having pre-redacted it.
    reason: String,
    /// How many consecutive times this `check` has failed closed, back to `since`.
    pub consecutive: u64,
    /// RFC3339 UTC timestamp of the first event in the current streak.
    pub since: String,
}

impl FailClosedEvent {
    /// The only constructor. Redacts `check`/`assumed` against their closed sets and
    /// `object`/`reason` via [`redact_free_text`] — every field that reaches either
    /// output path (`to_json_line` / `crate::debug_dump_fail_closed`) is redacted here,
    /// once, structurally, rather than at each call site.
    fn redacted(
        check: &str,
        object: &str,
        assumed: &str,
        reason: &str,
        consecutive: u64,
        since: String,
    ) -> Self {
        FailClosedEvent {
            level: "warn",
            event: "fail_closed",
            check: redact_enum_field(check, KNOWN_CHECKS),
            object: redact_free_text(object),
            assumed: redact_enum_field(assumed, KNOWN_ASSUMPTIONS),
            reason: redact_free_text(reason),
            consecutive,
            since,
        }
    }

    /// The (redacted) check name.
    pub fn check(&self) -> &str {
        &self.check
    }

    /// The (redacted) object the decision was about.
    pub fn object(&self) -> &str {
        &self.object
    }

    /// The (redacted) assumption the code made.
    pub fn assumed(&self) -> &str {
        &self.assumed
    }

    /// The (redacted) reason text.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Render as a single JSON line. Falls back to Debug formatting in the
    /// (unreachable in practice — every field is a plain String) case that
    /// serialization itself fails, so a logging call can never panic.
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| format!("{self:?}"))
    }
}

#[derive(Debug, Clone)]
struct StreakState {
    consecutive: u64,
    since: String,
}

/// Per-check consecutive-failure tracker. Plain struct (not a bare global function) so
/// tests can each own an isolated instance instead of racing on process-global state.
pub struct FailClosedTracker {
    streaks: Mutex<BTreeMap<String, StreakState>>,
}

impl FailClosedTracker {
    pub const fn new() -> Self {
        FailClosedTracker {
            streaks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record one fail-closed decision for `check` at time `now`, bump its streak, and
    /// return the resulting event. Pure with respect to `now` (no hidden clock read) so
    /// it is fully unit-testable; [`fail_closed`] is the thin wrapper that supplies
    /// `SystemTime::now()` and prints the result.
    pub fn record(
        &self,
        check: &str,
        object: &str,
        assumed: &str,
        reason: &str,
        now: SystemTime,
    ) -> FailClosedEvent {
        let now_str = format_rfc3339_utc(unix_secs(now));
        let mut streaks = self.streaks.lock().unwrap_or_else(|e| e.into_inner());
        let state = streaks
            .entry(check.to_string())
            .or_insert_with(|| StreakState {
                consecutive: 0,
                since: now_str.clone(),
            });
        state.consecutive += 1;
        if state.consecutive == 1 {
            state.since = now_str;
        }
        FailClosedEvent::redacted(
            check,
            object,
            assumed,
            reason,
            state.consecutive,
            state.since.clone(),
        )
    }

    /// Reset the streak for `check` — call when the check succeeds again.
    pub fn reset(&self, check: &str) {
        let mut streaks = self.streaks.lock().unwrap_or_else(|e| e.into_inner());
        streaks.remove(check);
    }

    /// Current consecutive count for `check` (0 if no active streak). Test/inspection
    /// helper.
    #[cfg(test)]
    fn consecutive(&self, check: &str) -> u64 {
        let streaks = self.streaks.lock().unwrap_or_else(|e| e.into_inner());
        streaks.get(check).map_or(0, |s| s.consecutive)
    }
}

impl Default for FailClosedTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide tracker used by [`fail_closed`] / [`check_succeeded`]. Production call
/// sites share this so a streak survives across calls from anywhere in the process;
/// tests that need isolation construct their own [`FailClosedTracker`] instead.
pub static GLOBAL: FailClosedTracker = FailClosedTracker::new();

/// Record a fail-closed decision on the global tracker, print it as a structured `WARN`
/// JSON line to stderr, and return the event (so the caller can also feed it to a debug
/// dump — see `crate::debug_dump_fail_closed`).
pub fn fail_closed(check: &str, object: &str, assumed: &str, reason: &str) -> FailClosedEvent {
    let ev = GLOBAL.record(check, object, assumed, reason, SystemTime::now());
    eprintln!("{}", ev.to_json_line());
    ev
}

/// Reset the global streak for `check`. Call this wherever the check in question
/// succeeds, so an old failure streak does not linger and pollute the next real one.
pub fn check_succeeded(check: &str) {
    GLOBAL.reset(check);
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Format a Unix timestamp (seconds) as `YYYY-MM-DDTHH:MM:SSZ`. Pure, no I/O — the
/// inverse of `appauth::parse_rfc3339_utc`, reimplemented here rather than shared to
/// keep this module dependency-free of `appauth`'s (unrelated) auth-config concerns.
pub fn format_rfc3339_utc(unix_secs: u64) -> String {
    let secs = unix_secs as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = sod / 3600;
    let mi = (sod % 3600) / 60;
    let s = sod % 60;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch -> proleptic-Gregorian
/// (y, m, d). Correct for any representable date; the inverse of `days_from_civil` in
/// `appauth.rs`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn format_rfc3339_utc_known_instants() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1_785_326_400), "2026-07-29T12:00:00Z");
        assert_eq!(format_rfc3339_utc(951_868_800), "2000-03-01T00:00:00Z");
    }

    #[test]
    fn event_is_warn_level_and_named_fail_closed() {
        let t = FailClosedTracker::new();
        let ev = t.record(
            "image_refcount",
            "sha256:abc",
            "referenced",
            "podman inspect: exit 125",
            UNIX_EPOCH,
        );
        assert_eq!(ev.level, "warn");
        assert_eq!(ev.event, "fail_closed");
    }

    #[test]
    fn consecutive_count_increments_per_check() {
        let t = FailClosedTracker::new();
        let t0 = UNIX_EPOCH;
        let ev1 = t.record("image_refcount", "sha256:abc", "referenced", "boom", t0);
        assert_eq!(ev1.consecutive, 1);
        let ev2 = t.record("image_refcount", "sha256:abc", "referenced", "boom", t0);
        assert_eq!(ev2.consecutive, 2);
        let ev3 = t.record("image_refcount", "sha256:def", "referenced", "boom", t0);
        assert_eq!(ev3.consecutive, 3);
        assert_eq!(t.consecutive("image_refcount"), 3);
    }

    #[test]
    fn streaks_are_tracked_independently_per_check() {
        let t = FailClosedTracker::new();
        let t0 = UNIX_EPOCH;
        t.record("image_refcount", "x", "referenced", "e", t0);
        t.record("image_refcount", "x", "referenced", "e", t0);
        t.record("worker_busy_probe", "y", "busy", "e", t0);
        assert_eq!(t.consecutive("image_refcount"), 2);
        assert_eq!(t.consecutive("worker_busy_probe"), 1);
    }

    #[test]
    fn since_is_pinned_to_first_event_in_the_streak() {
        let t = FailClosedTracker::new();
        let t0 = UNIX_EPOCH;
        let t1 = UNIX_EPOCH + Duration::from_secs(3600);
        let ev1 = t.record("image_refcount", "x", "referenced", "e", t0);
        assert_eq!(ev1.since, "1970-01-01T00:00:00Z");
        let ev2 = t.record("image_refcount", "x", "referenced", "e", t1);
        assert_eq!(
            ev2.since, ev1.since,
            "since must stay pinned to the first event, not update on every call"
        );
        assert_eq!(ev2.consecutive, 2);
    }

    #[test]
    fn reset_clears_the_streak_so_the_next_failure_starts_a_new_one() {
        let t = FailClosedTracker::new();
        let t0 = UNIX_EPOCH;
        let t1 = UNIX_EPOCH + Duration::from_secs(60);
        t.record("image_refcount", "x", "referenced", "e", t0);
        t.record("image_refcount", "x", "referenced", "e", t0);
        assert_eq!(t.consecutive("image_refcount"), 2);

        t.reset("image_refcount");
        assert_eq!(t.consecutive("image_refcount"), 0);

        let ev = t.record("image_refcount", "x", "referenced", "e", t1);
        assert_eq!(ev.consecutive, 1, "streak must restart from 1 after reset");
        assert_eq!(
            ev.since, "1970-01-01T00:01:00Z",
            "since must move to the new streak's start"
        );
    }

    #[test]
    fn reset_on_a_check_with_no_streak_is_a_harmless_no_op() {
        let t = FailClosedTracker::new();
        t.reset("never_failed");
        assert_eq!(t.consecutive("never_failed"), 0);
    }

    #[test]
    fn to_json_line_round_trips_the_key_fields() {
        let t = FailClosedTracker::new();
        let ev = t.record(
            "image_refcount",
            "sha256:abc123",
            "referenced",
            "podman inspect: exit 125: no such image",
            UNIX_EPOCH,
        );
        let line = ev.to_json_line();
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["level"], "warn");
        assert_eq!(parsed["event"], "fail_closed");
        assert_eq!(parsed["check"], "image_refcount");
        assert_eq!(parsed["object"], "sha256:abc123");
        assert_eq!(parsed["assumed"], "referenced");
        assert_eq!(parsed["consecutive"], 1);
        assert_eq!(parsed["since"], "1970-01-01T00:00:00Z");
    }

    // --- process-global fail_closed()/check_succeeded() wrapper smoke test ------
    // `check` must be one of KNOWN_CHECKS now that it's a closed set (issue #132
    // follow-up audit, HIGH-1) — "worker_busy_probe" is safe to reuse here: it's only
    // otherwise touched by lib.rs's `container_worker_busy`, which unit tests never
    // reach (it short-circuits on `container_running`, which needs a real podman
    // container). GLOBAL is still process-wide and `cargo test` runs in parallel, so
    // this remains the only test allowed to drive that check name through the GLOBAL
    // wrapper.
    #[test]
    fn global_wrapper_records_and_resets() {
        let check = "worker_busy_probe";
        check_succeeded(check); // start from a clean slate regardless of test order
        let ev1 = fail_closed(check, "obj", "busy", "boom");
        assert_eq!(ev1.consecutive, 1);
        let ev2 = fail_closed(check, "obj", "busy", "boom");
        assert_eq!(ev2.consecutive, 2);
        check_succeeded(check);
        let ev3 = fail_closed(check, "obj", "busy", "boom");
        assert_eq!(
            ev3.consecutive, 1,
            "streak must restart after check_succeeded"
        );
    }

    // --- issue #132 follow-up audit: HIGH-1 / HIGH-2 -----------------------------
    // Every field must be redacted in the alertable WARN JSON event (to_json_line).
    // The plaintext debug-dump path is proven equivalently in lib.rs's
    // `write_debug_dump_fail_closed`-based tests, since debug_dump_fail_closed prints
    // nothing but these same getters — see that module's test module doc comment.

    /// A synthetic credential placed in `check` (HIGH-1): `check` is now a closed
    /// enum (`KNOWN_CHECKS`), so ANY value outside that set — credential-shaped or
    /// not — is redacted structurally, never printed raw. This is what "removes the
    /// leak path structurally" (requirement 4) means concretely.
    #[test]
    fn synthetic_credential_in_check_is_redacted_in_json_event() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let ev = t.record(&synthetic, "obj", "referenced", "boom", UNIX_EPOCH);
        assert_ne!(ev.check(), synthetic, "check must not pass through raw");
        let line = ev.to_json_line();
        assert!(
            !line.contains(&synthetic),
            "credential leaked in JSON: {line}"
        );
    }

    /// A synthetic credential placed in `object` (HIGH-1): `object` stays free text
    /// (ids/paths legitimately vary) but is now always redacted via
    /// `dump_redact::redact_free_text` on the way in.
    #[test]
    fn synthetic_credential_in_object_is_redacted_in_json_event() {
        let t = FailClosedTracker::new();
        let synthetic = "AKIAIOSFODNN7EXAMPLE"; // synthetic AWS-shaped key id
        let ev = t.record(
            "image_refcount",
            synthetic,
            "referenced",
            "boom",
            UNIX_EPOCH,
        );
        assert_ne!(ev.object(), synthetic);
        let line = ev.to_json_line();
        assert!(
            !line.contains(synthetic),
            "credential leaked in JSON: {line}"
        );
    }

    /// A synthetic credential placed in `assumed` (HIGH-1): closed enum, same
    /// structural argument as `check`.
    #[test]
    fn synthetic_credential_in_assumed_is_redacted_in_json_event() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let ev = t.record("image_refcount", "obj", &synthetic, "boom", UNIX_EPOCH);
        assert_ne!(ev.assumed(), synthetic);
        let line = ev.to_json_line();
        assert!(
            !line.contains(&synthetic),
            "credential leaked in JSON: {line}"
        );
    }

    /// HIGH-2, the highest-risk field: a synthetic credential embedded MID-SENTENCE
    /// in `reason` (exactly the raw-stderr shape this field is documented to carry —
    /// no pre-redaction by the caller here) must still be redacted, while the
    /// surrounding diagnostic text survives (requirement 3).
    #[test]
    fn synthetic_credential_embedded_mid_sentence_in_reason_is_redacted_in_json_event() {
        let t = FailClosedTracker::new();
        let synthetic = format!("ghp_{}", "a1B2c3D4".repeat(5));
        let reason = format!(
            "podman inspect failed: exit 125: authorization error, token={synthetic}, retry"
        );
        let ev = t.record("image_refcount", "obj", "referenced", &reason, UNIX_EPOCH);
        assert!(!ev.reason().contains(&synthetic));
        assert!(
            ev.reason().contains("podman inspect failed: exit 125"),
            "diagnostic detail must survive: {}",
            ev.reason()
        );
        let line = ev.to_json_line();
        assert!(
            !line.contains(&synthetic),
            "credential leaked in JSON: {line}"
        );
        assert!(line.contains("podman inspect failed"));
    }

    /// The un-pre-redacted case the auditor confirmed live: `reason` carrying raw,
    /// completely unscrubbed stderr (no caller-side `redact()` call at all) must
    /// still come out clean — this is HIGH-2's core claim, that safety no longer
    /// depends on caller discipline.
    #[test]
    fn reason_is_safe_even_when_caller_never_pre_redacts_it() {
        let t = FailClosedTracker::new();
        let synthetic = "AKIAIOSFODNN7EXAMPLE";
        let raw_stderr = format!(
            "Error: authenticating to registry.example.com: Get \"https://registry.example.com/v2/\": \
             x-amz-access-key: {synthetic}: connection refused"
        );
        let ev = t.record(
            "worker_busy_probe",
            "container-7",
            "busy",
            &raw_stderr,
            UNIX_EPOCH,
        );
        assert!(!ev.reason().contains(synthetic));
        assert!(!ev.to_json_line().contains(synthetic));
    }
}

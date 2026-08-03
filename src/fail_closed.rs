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

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// One fail-closed decision, ready to serialize as a single structured log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailClosedEvent {
    pub level: &'static str,
    pub event: &'static str,
    /// Which check made the decision, e.g. `"image_refcount"`, `"worker_busy_probe"`.
    pub check: String,
    /// The object the decision was about (image ID, container name, host name, ...).
    pub object: String,
    /// What the code assumed as a result, e.g. `"referenced"`, `"busy"`, `"quiesced"`.
    pub assumed: String,
    /// The real reason the check could not answer — including subprocess stderr where
    /// applicable. Free text; run it through `crate::redact` before handing it here if
    /// it might contain subprocess output.
    pub reason: String,
    /// How many consecutive times this `check` has failed closed, back to `since`.
    pub consecutive: u64,
    /// RFC3339 UTC timestamp of the first event in the current streak.
    pub since: String,
}

impl FailClosedEvent {
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
        FailClosedEvent {
            level: "warn",
            event: "fail_closed",
            check: check.to_string(),
            object: object.to_string(),
            assumed: assumed.to_string(),
            reason: reason.to_string(),
            consecutive: state.consecutive,
            since: state.since.clone(),
        }
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
    // Uses a check name unique to this test (not shared with any other test or call
    // site) since GLOBAL is process-wide and `cargo test` runs tests in parallel
    // within the same process.

    #[test]
    fn global_wrapper_records_and_resets() {
        let check = "test_only_global_wrapper_smoke_check";
        check_succeeded(check); // start from a clean slate regardless of test order
        let ev1 = fail_closed(check, "obj", "referenced", "boom");
        assert_eq!(ev1.consecutive, 1);
        let ev2 = fail_closed(check, "obj", "referenced", "boom");
        assert_eq!(ev2.consecutive, 2);
        check_succeeded(check);
        let ev3 = fail_closed(check, "obj", "referenced", "boom");
        assert_eq!(
            ev3.consecutive, 1,
            "streak must restart after check_succeeded"
        );
    }
}

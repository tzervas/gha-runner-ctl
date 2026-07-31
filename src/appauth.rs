//! GitHub App installation-token authentication — additive, opt-in alternative to
//! the long-lived `GH_TOKEN`/`GITHUB_TOKEN` PAT path resolved by [`crate::github_token`].
//!
//! ## Why
//!
//! `listen` re-scans every repo in `GHA_PRIORITY_REPOS` every tick. On the homelab
//! instance that is ~80 GETs/min ≈ 4,800/hour against a classic PAT's 5,000/hour cap —
//! ~96% of budget, which is why `listen: list_demand_jobs: budget exhausted mid-scan`
//! fires on nearly every tick. A GitHub App installation token gets 15,000 requests/hour
//! (3x headroom), which is what makes a faster, steadier poll interval sustainable. The
//! poll interval is not the bottleneck here — the credential is.
//!
//! ## Selection
//!
//! Selected only when all three of `GHA_APP_ID`, `GHA_APP_INSTALLATION_ID`, and
//! `GHA_APP_PRIVATE_KEY` are set (see [`app_auth_config_from_env`]). Any other
//! combination — including zero configured — falls back to the existing `GH_TOKEN`
//! discovery unchanged, so existing deployments need zero config change. Once fully
//! configured, App auth is authoritative: a bad key or a failed mint is a hard error,
//! never a silent downgrade back to the PAT path (which could mask a real
//! misconfiguration, or mint against the wrong identity).
//!
//! ## Private key handling
//!
//! `GHA_APP_PRIVATE_KEY` is a **path** to the PEM file (optionally prefixed `file:`),
//! never inline key material. Inline PEM in an env var is readable by anyone who can
//! read `/proc/<pid>/environ` for the process, is far more likely to leak into shell
//! history / `env` dumps / CI logs than a `0600` file, and cannot be `chmod`-restricted.
//! We never even read the PEM bytes into our own process memory: `openssl -sign <path>`
//! reads the key straight off disk.
//!
//! ## Signing without a new dependency
//!
//! RS256 signing shells out to `openssl dgst -sha256 -sign <path>` (already a runtime
//! dependency of this tool's host environment, and already used elsewhere in this
//! codebase's style — see `get_token_from_git_credential`/`store_token_in_git_credential`
//! for the same "pipe secrets over stdin/stdout, never argv" pattern). The signing input
//! goes over stdin and the raw signature comes back over stdout, so neither the JWT nor
//! the key ever appears in `/proc/<pid>/cmdline` (world-readable). This keeps the change
//! at **zero new crates** — no `jsonwebtoken`/`ring`/`rsa` needed just to mint a JWT every
//! ~55 minutes.
//!
//! ## Never logged
//!
//! Every error string that could carry response-body material is passed through
//! [`crate::redact`] (already hardened for `ghs_` App tokens). The JWT and the minted
//! installation token are never `eprintln!`'d; only non-secret identifiers (app id,
//! installation id, expiry) are logged.

use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// `iat` is backdated by this much to tolerate host/GitHub clock skew.
const JWT_SKEW_BACKDATE_SECS: i64 = 60;
/// `exp - iat`. GitHub hard-caps App JWT lifetime at 10 minutes and rejects anything
/// longer ("'Expiration time' claim ('exp') is too far in the future").
const JWT_LIFETIME_SECS: i64 = 600;
/// Re-mint this long before the cached installation token actually expires, so a
/// long `listen` run never presents a token that dies mid-request.
const REFRESH_MARGIN_SECS: i64 = 300;
/// Used only when GitHub's `expires_at` cannot be parsed: deliberately shorter than
/// the documented 1h TTL so the failure mode is an extra mint, not a 401.
const FALLBACK_TTL_SECS: i64 = 1800;
/// GitHub App installation tokens: 15,000 requests/hour vs. a classic PAT's 5,000/hour.
pub const APP_AUTH_HOURLY_BUDGET: u32 = 15_000;

// --- Configuration / selection -----------------------------------------------

/// Fully-resolved GitHub App configuration. Only constructed when all three env
/// vars are present and non-empty — see [`app_auth_config_from_env`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppAuthConfig {
    pub(crate) app_id: String,
    pub(crate) installation_id: String,
    pub(crate) key_path: PathBuf,
}

/// Read `GHA_APP_ID` / `GHA_APP_INSTALLATION_ID` / `GHA_APP_PRIVATE_KEY` from the
/// process environment. `None` means "use the existing `GH_TOKEN` path, unchanged" —
/// either nothing is configured, or it's a partial/typo'd configuration (a warning is
/// printed to stderr in that case so a typo doesn't silently look like "not using App
/// auth" forever).
pub(crate) fn app_auth_config_from_env() -> Option<AppAuthConfig> {
    match resolve_app_auth_config(|k| std::env::var(k).ok().filter(|v| !v.is_empty())) {
        Ok(cfg) => cfg,
        Err(warning) => {
            eprintln!("appauth: {warning}");
            None
        }
    }
}

/// Pure resolution over an injected lookup, so tests never need to mutate process-wide
/// env vars (which would race with other tests in the same binary).
///
/// - all three absent → `Ok(None)` (silent — this is the default, unconfigured case)
/// - all three present → `Ok(Some(cfg))`
/// - 1 or 2 present → `Err(..)` naming what's missing (caller logs it, then falls back)
pub(crate) fn resolve_app_auth_config(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<AppAuthConfig>, String> {
    let app_id = get("GHA_APP_ID");
    let installation_id = get("GHA_APP_INSTALLATION_ID");
    let key = get("GHA_APP_PRIVATE_KEY");

    let missing: Vec<&str> = [
        ("GHA_APP_ID", app_id.is_some()),
        ("GHA_APP_INSTALLATION_ID", installation_id.is_some()),
        ("GHA_APP_PRIVATE_KEY", key.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| (!present).then_some(name))
    .collect();

    if missing.len() == 3 {
        return Ok(None);
    }
    if !missing.is_empty() {
        return Err(format!(
            "GitHub App auth is partially configured (missing {}) — falling back to \
             GH_TOKEN/PAT discovery. Set all three (GHA_APP_ID, GHA_APP_INSTALLATION_ID, \
             GHA_APP_PRIVATE_KEY) to enable App auth, or unset all to silence this warning.",
            missing.join(", ")
        ));
    }

    let key_raw = key.expect("checked present above");
    let key_str = key_raw.strip_prefix("file:").unwrap_or(&key_raw).trim();
    if key_str.is_empty() {
        return Err("GHA_APP_PRIVATE_KEY resolved to an empty path".into());
    }

    Ok(Some(AppAuthConfig {
        app_id: app_id.expect("checked present above"),
        installation_id: installation_id.expect("checked present above"),
        key_path: PathBuf::from(key_str),
    }))
}

fn ensure_key_path_readable(key_path: &Path) -> Result<(), String> {
    let meta = std::fs::metadata(key_path).map_err(|e| {
        format!(
            "GHA_APP_PRIVATE_KEY path {} is not readable: {e}",
            key_path.display()
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "GHA_APP_PRIVATE_KEY path {} is not a regular file",
            key_path.display()
        ));
    }
    Ok(())
}

// --- JWT claims (pure, testable without openssl or the network) --------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JwtClaims {
    pub(crate) iat: i64,
    pub(crate) exp: i64,
}

/// `iat` backdated for clock skew; `exp` capped at GitHub's 10-minute JWT ceiling.
pub(crate) fn jwt_claims(now_unix: i64) -> JwtClaims {
    let iat = now_unix - JWT_SKEW_BACKDATE_SECS;
    JwtClaims {
        iat,
        exp: iat + JWT_LIFETIME_SECS,
    }
}

fn jwt_header_json() -> &'static str {
    r#"{"alg":"RS256","typ":"JWT"}"#
}

fn jwt_payload_json(app_id: &str, claims: JwtClaims) -> String {
    // app_id is validated ASCII-digits-only by the caller, so no escaping is needed.
    format!(
        r#"{{"iat":{},"exp":{},"iss":{}}}"#,
        claims.iat, claims.exp, app_id
    )
}

// --- base64url (RFC 4648 §5, unpadded) ----------------------------------------

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(B64URL[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64URL[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64URL[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(B64URL[(n & 0x3f) as usize] as char);
        }
    }
    out
}

// --- signing -------------------------------------------------------------------

/// RS256-sign `signing_input` with `openssl dgst -sha256 -sign <path> -binary`.
///
/// The key path is a `Command` argument (not secret; fine in argv). The signing input
/// is written over stdin and the raw signature is read back over stdout — neither ever
/// appears in `/proc/<pid>/cmdline`.
fn sign_rs256(signing_input: &str, key_path: &Path) -> Result<Vec<u8>, String> {
    let mut child = Command::new("openssl")
        .arg("dgst")
        .arg("-sha256")
        .arg("-sign")
        .arg(key_path)
        .arg("-binary")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "cannot run `openssl` to sign the App JWT: {e}. Install openssl, or unset \
                 GHA_APP_ID/GHA_APP_INSTALLATION_ID/GHA_APP_PRIVATE_KEY to use GH_TOKEN auth."
            )
        })?;

    {
        let stdin = child.stdin.as_mut().ok_or("openssl stdin unavailable")?;
        stdin
            .write_all(signing_input.as_bytes())
            .map_err(|e| format!("writing JWT signing input to openssl: {e}"))?;
    }

    let out = child
        .wait_with_output()
        .map_err(|e| format!("openssl dgst failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "openssl could not sign with the App private key (exit {}): {}",
            out.status,
            crate::redact(&String::from_utf8_lossy(&out.stderr))
        ));
    }
    if out.stdout.is_empty() {
        return Err("openssl produced an empty RS256 signature".to_string());
    }
    Ok(out.stdout)
}

fn encode_jwt(app_id: &str, key_path: &Path, now_unix: i64) -> Result<String, String> {
    if app_id.is_empty() || !app_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("GHA_APP_ID must be the App's numeric ID".to_string());
    }
    ensure_key_path_readable(key_path)?;
    let claims = jwt_claims(now_unix);
    let signing_input = format!(
        "{}.{}",
        b64url(jwt_header_json().as_bytes()),
        b64url(jwt_payload_json(app_id, claims).as_bytes())
    );
    let sig = sign_rs256(&signing_input, key_path)?;
    Ok(format!("{signing_input}.{}", b64url(&sig)))
}

// --- installation token minting -------------------------------------------------

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

/// The only network call in this module — kept isolated so everything else (claim
/// construction, refresh timing, fallback selection) is unit-testable without it.
fn mint_installation_token_http(jwt: &str, installation_id: &str) -> Result<(String, i64), String> {
    let url = format!("https://api.github.com/app/installations/{installation_id}/access_tokens");
    let result = crate::http_agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();

    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!(
                "installation-token mint failed: HTTP {code}: {}",
                crate::redact(&body)
            ));
        }
        Err(e) => {
            return Err(format!(
                "installation-token mint failed: {}",
                crate::redact(&e.to_string())
            ));
        }
    };

    let status = resp.status();
    if !(200..300).contains(&status) {
        return Err(format!("installation-token mint failed: HTTP {status}"));
    }
    let body: InstallationTokenResponse = resp
        .into_json()
        .map_err(|e| format!("installation-token response parse failed: {e}"))?;
    if body.token.is_empty() {
        return Err("installation-token mint returned an empty token".to_string());
    }
    let now = now_unix();
    let expires_at_unix = parse_rfc3339_utc(&body.expires_at).unwrap_or(now + FALLBACK_TTL_SECS);
    Ok((body.token, expires_at_unix))
}

// --- cache + refresh decision (pure, testable) ----------------------------------

struct CachedToken {
    token: String,
    expires_at_unix: i64,
}

static TOKEN_CACHE: Mutex<Option<CachedToken>> = Mutex::new(None);

/// True when `now` is within `margin_secs` of `expires_at` (or past it). Installation
/// tokens live ~1h; a `listen` process holds the cache for its whole run, so this is
/// the only thing standing between "mint once per hour" and "mint every request."
pub(crate) fn needs_remint(expires_at_unix: i64, now_unix: i64, margin_secs: i64) -> bool {
    now_unix + margin_secs >= expires_at_unix
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve a usable installation token: cached and fresh, or mint (and cache) a new
/// one. This is the App-auth entry point called from [`crate::github_token`].
pub(crate) fn installation_token(cfg: &AppAuthConfig) -> Result<String, String> {
    {
        let guard = TOKEN_CACHE
            .lock()
            .map_err(|_| "appauth: token cache lock poisoned".to_string())?;
        if let Some(cached) = guard.as_ref() {
            if !needs_remint(cached.expires_at_unix, now_unix(), REFRESH_MARGIN_SECS) {
                return Ok(cached.token.clone());
            }
        }
    }

    let now = now_unix();
    let jwt = encode_jwt(&cfg.app_id, &cfg.key_path, now)?;
    let (token, expires_at_unix) = mint_installation_token_http(&jwt, &cfg.installation_id)?;

    let mut guard = TOKEN_CACHE
        .lock()
        .map_err(|_| "appauth: token cache lock poisoned".to_string())?;
    *guard = Some(CachedToken {
        token: token.clone(),
        expires_at_unix,
    });
    eprintln!(
        "auth: minted GitHub App installation token (app_id={}, installation_id={}, \
         expires in {}s, ~{APP_AUTH_HOURLY_BUDGET}/hour budget)",
        cfg.app_id,
        cfg.installation_id,
        (expires_at_unix - now).max(0)
    );
    Ok(token)
}

// --- minimal RFC 3339 (UTC) parser, just enough for GitHub's `expires_at` -------

/// Parses `"2026-07-29T12:00:00Z"`-shaped timestamps (GitHub always sends `Z`, no
/// offset). Returns `None` on anything else so the caller falls back to
/// [`FALLBACK_TTL_SECS`] instead of failing the mint over a formatting surprise.
fn parse_rfc3339_utc(s: &str) -> Option<i64> {
    let s = s.trim().strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let time = time.split('.').next()?; // drop fractional seconds, if any
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let se: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Howard Hinnant's `days_from_civil`: proleptic-Gregorian y/m/d -> days since the Unix
/// epoch (1970-01-01). Well-known, correct for any (reasonable) calendar date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- resolve_app_auth_config: fallback / partial / full -------------------

    #[test]
    fn resolve_app_auth_config_all_absent_falls_back_silently() {
        let got = resolve_app_auth_config(|_| None).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn resolve_app_auth_config_all_present_selects_app_auth() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_INSTALLATION_ID" => Some("78901234".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("file:/etc/gha/app-key.pem".to_string()),
            _ => None,
        })
        .unwrap()
        .expect("all three present must select app auth");
        assert_eq!(got.app_id, "123456");
        assert_eq!(got.installation_id, "78901234");
        assert_eq!(got.key_path, PathBuf::from("/etc/gha/app-key.pem"));
    }

    #[test]
    fn resolve_app_auth_config_accepts_bare_path_without_file_prefix() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("1".to_string()),
            "GHA_APP_INSTALLATION_ID" => Some("2".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("/etc/gha/app-key.pem".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(got.key_path, PathBuf::from("/etc/gha/app-key.pem"));
    }

    #[test]
    fn resolve_app_auth_config_partial_is_an_error_naming_whats_missing() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("missing GHA_APP_INSTALLATION_ID"), "{err}");
        assert!(err.contains("GHA_APP_PRIVATE_KEY"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_two_of_three_is_an_error() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_INSTALLATION_ID" => Some("78901234".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("GHA_APP_PRIVATE_KEY"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_empty_string_env_counts_as_absent() {
        // app_auth_config_from_env filters empty strings before calling resolve_*;
        // resolve_* itself just trusts its input, so this proves the *filter* contract
        // by exercising the same predicate the real accessor uses.
        let get = |k: &str| -> Option<String> {
            let raw: Option<&str> = match k {
                "GHA_APP_ID" => Some(""),
                "GHA_APP_INSTALLATION_ID" => Some("78901234"),
                "GHA_APP_PRIVATE_KEY" => Some("/etc/gha/app-key.pem"),
                _ => None,
            };
            raw.map(str::to_string).filter(|v| !v.is_empty())
        };
        let err = resolve_app_auth_config(get).unwrap_err();
        assert!(err.contains("GHA_APP_ID"), "{err}");
    }

    // --- JWT claim construction: iat/exp/iss bounds ----------------------------

    #[test]
    fn jwt_claims_backdates_iat_and_caps_lifetime_at_ten_minutes() {
        let now = 1_785_326_400; // 2026-07-29T12:00:00Z (arbitrary fixed instant)
        let c = jwt_claims(now);
        assert_eq!(
            c.iat,
            now - 60,
            "iat must be backdated exactly 60s for skew"
        );
        assert_eq!(
            c.exp - c.iat,
            600,
            "exp - iat must be GitHub's 10-minute cap"
        );
        assert!(c.exp <= now + 600, "exp must not run away into the future");
    }

    #[test]
    fn jwt_payload_json_round_trips_iat_exp_iss() {
        let claims = jwt_claims(1_785_326_400);
        let payload = jwt_payload_json("123456", claims);
        let v: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(v["iat"], claims.iat);
        assert_eq!(v["exp"], claims.exp);
        // iss is emitted as a bare JSON integer (GitHub accepts numeric or string iss);
        // app_id is pre-validated ASCII-digits-only, so this is intentional, not a bug.
        assert_eq!(v["iss"], 123456);
    }

    #[test]
    fn jwt_header_is_rs256() {
        let v: serde_json::Value = serde_json::from_str(jwt_header_json()).unwrap();
        assert_eq!(v["alg"], "RS256");
        assert_eq!(v["typ"], "JWT");
    }

    #[test]
    fn encode_jwt_rejects_non_numeric_app_id_before_touching_the_key() {
        // Deliberately points at a path that doesn't exist: if app_id validation didn't
        // happen first, this would fail with a *different* (key-not-found) error.
        let err = encode_jwt("not-a-number", Path::new("/nonexistent/key.pem"), 0).unwrap_err();
        assert!(err.contains("GHA_APP_ID"), "{err}");
    }

    // --- refresh-when-near-expiry decision --------------------------------------

    #[test]
    fn needs_remint_false_well_before_expiry() {
        let now = 1_000_000;
        let expires_at = now + 3600; // a full hour out
        assert!(!needs_remint(expires_at, now, 300));
    }

    #[test]
    fn needs_remint_true_within_margin() {
        let now = 1_000_000;
        let expires_at = now + 200; // inside the 300s margin
        assert!(needs_remint(expires_at, now, 300));
    }

    #[test]
    fn needs_remint_true_exactly_at_margin_boundary() {
        let now = 1_000_000;
        let expires_at = now + 300; // exactly the margin: refresh a little early, not late
        assert!(needs_remint(expires_at, now, 300));
    }

    #[test]
    fn needs_remint_true_when_already_expired() {
        let now = 1_000_000;
        let expires_at = now - 1;
        assert!(needs_remint(expires_at, now, 300));
    }

    // --- base64url: RFC 4648 §10 test vectors -----------------------------------

    #[test]
    fn b64url_matches_rfc4648_test_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        assert_eq!(b64url(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url(b"foobar"), "Zm9vYmFy");
    }

    // --- RFC 3339 parsing --------------------------------------------------------

    #[test]
    fn parse_rfc3339_utc_known_instants() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc("2026-07-29T12:00:00Z"),
            Some(1_785_326_400)
        );
        assert_eq!(parse_rfc3339_utc("2000-03-01T00:00:00Z"), Some(951_868_800));
    }

    #[test]
    fn parse_rfc3339_utc_accepts_fractional_seconds() {
        assert_eq!(
            parse_rfc3339_utc("2026-07-29T12:00:00.123Z"),
            Some(1_785_326_400)
        );
    }

    #[test]
    fn parse_rfc3339_utc_rejects_non_utc_offsets() {
        assert_eq!(parse_rfc3339_utc("2026-07-29T12:00:00+02:00"), None);
    }

    #[test]
    fn parse_rfc3339_utc_rejects_garbage() {
        assert_eq!(parse_rfc3339_utc("not a timestamp"), None);
        assert_eq!(parse_rfc3339_utc(""), None);
    }

    // --- ensure_key_path_readable -------------------------------------------------

    #[test]
    fn ensure_key_path_readable_rejects_missing_file() {
        let err =
            ensure_key_path_readable(Path::new("/definitely/does/not/exist.pem")).unwrap_err();
        assert!(err.contains("not readable"), "{err}");
    }

    #[test]
    fn ensure_key_path_readable_rejects_a_directory() {
        let err = ensure_key_path_readable(Path::new("/tmp")).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
    }

    // --- real RS256 signing, no network: skipped if openssl is absent -----------

    fn openssl_available() -> bool {
        Command::new("openssl")
            .arg("version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn sign_rs256_missing_key_path_is_a_clear_error() {
        let err = sign_rs256("test-signing-input", Path::new("/nonexistent/key.pem")).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn sign_rs256_produces_a_signature_openssl_itself_verifies() {
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "gha-appauth-sign-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let key_path = dir.join("key.pem");
        let pub_path = dir.join("key.pub");
        let data_path = dir.join("data.txt");
        let sig_path = dir.join("sig.bin");

        let gen = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
            ])
            .arg(&key_path)
            .output()
            .expect("run openssl genpkey");
        assert!(
            gen.status.success(),
            "genpkey failed: {}",
            String::from_utf8_lossy(&gen.stderr)
        );

        let pubkey = Command::new("openssl")
            .arg("pkey")
            .arg("-in")
            .arg(&key_path)
            .arg("-pubout")
            .arg("-out")
            .arg(&pub_path)
            .output()
            .expect("run openssl pkey");
        assert!(pubkey.status.success());

        let signing_input = "gha-runner-ctl.appauth.test-signing-input";
        let sig = sign_rs256(signing_input, &key_path).expect("sign should succeed");
        assert!(!sig.is_empty());
        std::fs::write(&sig_path, &sig).expect("write signature");
        std::fs::write(&data_path, signing_input).expect("write data");

        let verify = Command::new("openssl")
            .args(["dgst", "-sha256", "-verify"])
            .arg(&pub_path)
            .arg("-signature")
            .arg(&sig_path)
            .arg(&data_path)
            .output()
            .expect("run openssl dgst -verify");
        assert!(
            verify.status.success(),
            "openssl did not verify our own signature: {}",
            String::from_utf8_lossy(&verify.stderr)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_jwt_end_to_end_produces_three_dot_separated_base64url_sections() {
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "gha-appauth-jwt-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let key_path = dir.join("key.pem");
        let gen = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out",
            ])
            .arg(&key_path)
            .output()
            .expect("run openssl genpkey");
        assert!(gen.status.success());

        let now = now_unix();
        let jwt = encode_jwt("123456", &key_path, now).expect("encode_jwt should succeed");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "JWT must have exactly 3 dot-separated sections: {jwt}"
        );
        assert!(!parts[0].is_empty() && !parts[1].is_empty() && !parts[2].is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

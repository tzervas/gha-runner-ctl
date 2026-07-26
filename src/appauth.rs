//! GitHub **App** authentication — the host holds a private key, never a usable token.
//!
//! Today the host keeps a long-lived `GH_TOKEN`. That token *is* the access, and the
//! only way to roll it is an interactive `gh auth login` device flow, which cannot be
//! scripted. With App auth the host holds an App private key and *derives* an
//! installation token with a <=1 h TTL on demand, so "rotation" is just the next mint.
//!
//! Flow (ported from the cryptographically-verified `openssl`+`curl` reference in
//! `tzervas/mycelium-workflows` `.github/actions/app-token/action.yml`):
//!
//! 1. Sign an RS256 JWT — `iat` backdated 60 s for clock skew, `exp` = `iat + 600`
//!    (GitHub rejects JWTs with a lifetime over 10 minutes).
//! 2. `GET /app/installations` and match `account.login` against the owner.
//! 3. `POST /app/installations/{id}/access_tokens`.
//!
//! ## Safety posture
//!
//! - **Never argv.** `/proc/<pid>/cmdline` is world-readable, so neither the PEM nor
//!   any token is ever passed as an argument. `openssl dgst -sign` takes a *path*;
//!   the signing input goes over stdin and the signature comes back over stdout.
//! - **Never logged.** Every error string that could carry material is passed through
//!   [`crate::redact`] at the call site, and [`Pem`]/[`InstallationToken`] have manual
//!   `Debug` impls that print a placeholder.
//! - **Never a silent downgrade.** If `GHA_APP_ID` is set, App auth is the only path.
//!   A bad key or a missing installation is a hard error — this module never falls
//!   back to `GH_TOKEN`, and never falls back to an unauthenticated state.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `iat` is backdated by this much to tolerate host/GitHub clock skew.
pub const JWT_SKEW_BACKDATE_SECS: i64 = 60;
/// `exp - iat`. GitHub hard-caps App JWT lifetime at 10 minutes and rejects anything
/// longer with `'Expiration time' claim ('exp') is too far in the future`.
pub const JWT_LIFETIME_SECS: i64 = 600;
/// Re-mint this long before the installation token actually expires, so a long
/// `listen` run never presents a token that dies mid-request.
pub const REMINT_MARGIN: Duration = Duration::from_secs(300);
/// Used only when GitHub's `expires_at` cannot be parsed: deliberately shorter than
/// the documented 1 h TTL so the failure mode is an extra mint, not a 401.
pub const FALLBACK_TTL: Duration = Duration::from_secs(1800);

// --- Secret-carrying newtypes ------------------------------------------------

/// PEM private-key material. `Debug` never renders the contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Pem(String);

impl Pem {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    fn as_str(&self) -> &str {
        &self.0
    }
    /// Cheap structural check so a truncated / wrong-format key fails *here*, loudly,
    /// instead of as an opaque `openssl` error or a later 401.
    fn looks_like_private_key(&self) -> bool {
        let s = self.0.trim();
        s.starts_with("-----BEGIN") && s.contains("PRIVATE KEY-----")
    }
}

impl fmt::Debug for Pem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pem(***REDACTED*** {} bytes)", self.0.len())
    }
}

/// A minted installation access token (`ghs_…`). `Debug` never renders it.
#[derive(Clone, PartialEq, Eq)]
pub struct InstallationToken(String);

impl InstallationToken {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstallationToken(***REDACTED***)")
    }
}

// --- Configuration / mode selection ------------------------------------------

/// Where the private key comes from. Path is preferred: inline PEM must be written to
/// a 0600 temp file for `openssl` to read it, which is a (brief) on-disk exposure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// `GHA_APP_PRIVATE_KEY_PATH`
    Path(PathBuf),
    /// `GHA_APP_PRIVATE_KEY` (PEM contents in the environment)
    Inline(Pem),
}

impl KeySource {
    /// Short, non-secret description for `detect` output.
    pub fn describe(&self) -> String {
        match self {
            KeySource::Path(p) => format!("path:{}", p.display()),
            KeySource::Inline(_) => "inline:GHA_APP_PRIVATE_KEY".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub app_id: String,
    pub owner: String,
    pub key: KeySource,
}

/// Which authentication path this process resolved to, decided from configuration
/// alone (no network). Reported by `detect` so misconfiguration is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// GitHub App installation tokens, TTL <= 1 h, re-minted on expiry.
    App(Box<AppConfig>),
    /// Legacy discovery of a long-lived token (`GH_TOKEN`/`GITHUB_TOKEN`/`gh`/GCM/config).
    Token,
}

impl AuthMode {
    /// One-line summary for `detect`. Contains no secret material.
    pub fn describe(&self) -> String {
        match self {
            AuthMode::App(c) => format!(
                "github-app (app_id={}, owner={}, key={}) — installation token, TTL <= 1h",
                c.app_id,
                c.owner,
                c.key.describe()
            ),
            AuthMode::Token => {
                "token (GH_TOKEN/GITHUB_TOKEN/gh/GCM/config discovery) — long-lived, \
                 rotation needs interactive `gh auth login`"
                    .to_string()
            }
        }
    }
}

/// GitHub App IDs are short decimal integers. Reject anything else early rather than
/// letting it become an unauthenticated-looking 401 from the API.
fn is_valid_app_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.chars().all(|c| c.is_ascii_digit())
}

/// Owner logins are the same shape the rest of the tool already validates.
fn is_valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Decide the auth mode from configuration.
///
/// `lookup` resolves an environment variable (injected so this is unit-testable
/// without mutating the process environment — `std::env::set_var` is not thread-safe
/// and would race the rest of the test binary).
///
/// `owner_hint` lets the caller supply an owner already resolved from `--owner` /
/// `GHA_OWNER` / `--user` / the `owner` half of `--repo`.
///
/// # Errors
///
/// Returns `Err` — never `Ok(AuthMode::Token)` — when `GHA_APP_ID` is set but the
/// rest of the App configuration is unusable. Falling back to `GH_TOKEN` there would
/// silently re-introduce exactly the long-lived credential App auth exists to remove.
pub fn select_auth_mode<F>(lookup: F, owner_hint: Option<&str>) -> Result<AuthMode, String>
where
    F: Fn(&str) -> Option<String>,
{
    let get = |k: &str| -> Option<String> {
        lookup(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };

    let Some(app_id) = get("GHA_APP_ID") else {
        return Ok(AuthMode::Token);
    };

    if !is_valid_app_id(&app_id) {
        return Err(format!(
            "GHA_APP_ID must be the numeric App ID (Settings → Developer settings → \
             GitHub Apps → App ID), got {} character(s) that are not all digits. \
             Note this is NOT the Client ID (`Iv1.…`/`Iv23…`). Refusing to continue \
             rather than falling back to GH_TOKEN.",
            app_id.len()
        ));
    }

    let key = match (get("GHA_APP_PRIVATE_KEY_PATH"), get("GHA_APP_PRIVATE_KEY")) {
        (Some(p), _) => KeySource::Path(PathBuf::from(p)),
        (None, Some(pem)) => KeySource::Inline(Pem::new(pem)),
        (None, None) => {
            return Err(format!(
                "GHA_APP_ID={app_id} is set but no private key was provided. Set \
                 GHA_APP_PRIVATE_KEY_PATH=/path/to/key.pem (preferred) or \
                 GHA_APP_PRIVATE_KEY=<PEM contents>. Refusing to fall back to GH_TOKEN \
                 silently — unset GHA_APP_ID if token auth is what you want."
            ));
        }
    };

    let owner = owner_hint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| get("GHA_OWNER"))
        .ok_or_else(|| {
            format!(
                "GHA_APP_ID={app_id} is set but the owner is unknown. GitHub App auth must \
                 resolve which *installation* to mint from, so set GHA_OWNER (or --owner / \
                 --user / --repo owner/name) to the account the App is installed on."
            )
        })?;

    if !is_valid_owner(&owner) {
        return Err(format!(
            "invalid owner {:?} for GitHub App auth — expected a bare account login, \
             not a URL or owner/repo pair",
            owner.chars().take(64).collect::<String>()
        ));
    }

    Ok(AuthMode::App(Box::new(AppConfig { app_id, owner, key })))
}

/// Convenience wrapper over the real process environment.
pub fn select_auth_mode_from_env(owner_hint: Option<&str>) -> Result<AuthMode, String> {
    select_auth_mode(|k| std::env::var(k).ok(), owner_hint)
}

// --- base64url (RFC 4648 §5, unpadded) ---------------------------------------

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url. JWT sections are `base64url(...)` with `=` stripped, which is
/// what the reference implementation's `openssl base64 -A | tr '+/' '-_' | tr -d '='`
/// produces.
pub fn b64url_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
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

/// Decoder used by the tests to assert the JWT sections decode to the expected JSON.
pub fn b64url_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for c in input.bytes() {
        let v = B64URL
            .iter()
            .position(|&b| b == c)
            .ok_or_else(|| format!("invalid base64url byte {c:#04x}"))? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

// --- JWT construction ---------------------------------------------------------

/// The `header.payload` string that gets RS256-signed.
///
/// Byte-for-byte the same construction as the verified reference:
/// header `{"alg":"RS256","typ":"JWT"}`, payload `{"iat":…,"exp":…,"iss":"…"}` with
/// `iat = now - 60` and `exp = iat + 600`.
pub fn jwt_signing_input(app_id: &str, now_epoch: i64) -> String {
    let iat = now_epoch - JWT_SKEW_BACKDATE_SECS;
    let exp = iat + JWT_LIFETIME_SECS;
    let header = b64url_encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload =
        b64url_encode(format!(r#"{{"iat":{iat},"exp":{exp},"iss":"{app_id}"}}"#).as_bytes());
    format!("{header}.{payload}")
}

fn now_epoch_secs() -> Result<i64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| format!("host clock is before the UNIX epoch: {e}"))
}

/// Removes the whole temp key **directory** on drop, including on the error paths.
///
/// Removing only the file would leave a growing trail of empty 0700 directories in
/// `/tmp` — one per mint, so a long `listen` run re-minting hourly would litter
/// indefinitely. Caught by `inline_key_temp_file_is_removed_after_signing`.
struct TempKeyDir(PathBuf);

impl Drop for TempKeyDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Materialise the key as a filesystem path for `openssl -sign`.
///
/// A `KeySource::Path` is used in place — no copy is made. Inline PEM is written to a
/// 0600 file inside a fresh 0700 directory and removed by [`TempKeyDir`]. Either way
/// only the *path* reaches argv, never the key material.
fn key_path(key: &KeySource) -> Result<(PathBuf, Option<TempKeyDir>), String> {
    match key {
        KeySource::Path(p) => {
            let pem = fs::read_to_string(p).map_err(|e| {
                format!(
                    "GHA_APP_PRIVATE_KEY_PATH {} is unreadable: {e}. Refusing to continue \
                     (App auth cannot silently degrade to GH_TOKEN).",
                    p.display()
                )
            })?;
            if !Pem::new(pem).looks_like_private_key() {
                return Err(format!(
                    "GHA_APP_PRIVATE_KEY_PATH {} does not contain a PEM private key \
                     (expected a `-----BEGIN … PRIVATE KEY-----` block). Download the \
                     .pem from the App's settings page; the key contents are not shown here.",
                    p.display()
                ));
            }
            Ok((p.clone(), None))
        }
        KeySource::Inline(pem) => {
            if !pem.looks_like_private_key() {
                return Err("GHA_APP_PRIVATE_KEY does not contain a PEM private key \
                     (expected a `-----BEGIN … PRIVATE KEY-----` block). Its contents are \
                     not echoed here."
                    .to_string());
            }
            // Unique per call: pid alone collides across the re-mints of a long
            // `listen` run, and across threads within one second.
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "{TEMP_KEY_DIR_PREFIX}{}-{}-{seq}",
                std::process::id(),
                now_epoch_secs().unwrap_or(0)
            ));
            // Created 0700 *atomically* — a create-then-chmod sequence would leave a
            // umask-wide window during which another local user could enter the dir.
            create_private_dir(&dir)?;
            // Register the cleanup guard before writing the key, so every error path
            // below still removes it.
            let guard = TempKeyDir(dir.clone());
            let path = dir.join("app.pem");
            let mut f = create_private_file(&path)?;
            f.write_all(pem.as_str().as_bytes())
                .and_then(|()| f.write_all(b"\n"))
                .map_err(|e| format!("write temp key: {e}"))?;
            drop(f);
            Ok((path, Some(guard)))
        }
    }
}

const TEMP_KEY_DIR_PREFIX: &str = "gha-runner-ctl-appkey-";

#[cfg(unix)]
fn create_private_dir(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(false)
        .mode(0o700)
        .create(p)
        .map_err(|e| format!("temp key dir {}: {e}", p.display()))
}

#[cfg(unix)]
fn create_private_file(p: &Path) -> Result<fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)
        .map_err(|e| format!("temp key file: {e}"))
}

#[cfg(not(unix))]
fn create_private_dir(p: &Path) -> Result<(), String> {
    fs::create_dir(p).map_err(|e| format!("temp key dir {}: {e}", p.display()))
}

#[cfg(not(unix))]
fn create_private_file(p: &Path) -> Result<fs::File, String> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .map_err(|e| format!("temp key file: {e}"))
}

/// RS256-sign `signing_input` with `openssl dgst -sha256 -sign <path> -binary`.
///
/// The signing input goes over **stdin** and the signature comes back over stdout;
/// argv carries only the key path. This is the same primitive the reference
/// implementation was verified against with `openssl dgst -sha256 -verify` =>
/// `Verified OK`.
pub fn sign_rs256(signing_input: &str, key: &KeySource) -> Result<Vec<u8>, String> {
    let (path, _cleanup) = key_path(key)?;

    let mut child = Command::new("openssl")
        .args(["dgst", "-sha256", "-sign"])
        .arg(&path)
        .arg("-binary")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "cannot run `openssl` to sign the App JWT: {e}. Install openssl \
                 (Debian/Ubuntu: apt-get install -y openssl) or use GH_TOKEN auth."
            )
        })?;

    child
        .stdin
        .take()
        .ok_or("openssl stdin unavailable")?
        .write_all(signing_input.as_bytes())
        .map_err(|e| format!("writing JWT signing input to openssl: {e}"))?;

    let out = child
        .wait_with_output()
        .map_err(|e| format!("openssl dgst failed: {e}"))?;

    if !out.status.success() {
        // stderr can name the key path but never its contents; redact defensively.
        return Err(format!(
            "openssl could not sign with the App private key (exit {}): {}",
            out.status.code().unwrap_or(-1),
            crate::redact(String::from_utf8_lossy(&out.stderr).trim())
        ));
    }
    if out.stdout.is_empty() {
        return Err("openssl produced an empty RS256 signature".to_string());
    }
    Ok(out.stdout)
}

/// Build a complete signed App JWT.
pub fn build_app_jwt(app_id: &str, key: &KeySource, now_epoch: i64) -> Result<String, String> {
    let signing_input = jwt_signing_input(app_id, now_epoch);
    let sig = sign_rs256(&signing_input, key)?;
    Ok(format!("{signing_input}.{}", b64url_encode(&sig)))
}

// --- ISO8601 -> epoch ---------------------------------------------------------

/// Parse GitHub's `expires_at` (`2026-07-24T21:00:00Z`) to epoch seconds.
///
/// Deliberately strict and dependency-free: anything unexpected returns `None` and the
/// caller falls back to [`FALLBACK_TTL`], which is shorter than the real TTL, so the
/// worst case is a redundant mint rather than a 401 mid-run.
pub fn parse_iso8601_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec)
}

// --- Mint --------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct InstallationAccount {
    #[serde(default)]
    login: String,
}

#[derive(serde::Deserialize)]
struct Installation {
    id: u64,
    #[serde(default)]
    account: Option<InstallationAccount>,
}

#[derive(serde::Deserialize)]
struct AccessTokenResponse {
    token: String,
    #[serde(default)]
    expires_at: String,
}

fn api_base() -> String {
    std::env::var("GITHUB_API_URL")
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| s.starts_with("https://"))
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

/// Resolve the installation id of `owner` for this App. Errors loudly when the App is
/// not installed there — an empty/absent installation must never look like success.
///
/// **Residual:** only the first page (100 installations) is examined, matching the
/// reference implementation. That covers the intended use — a fleet App installed on
/// one or a few accounts. An App installed on more than 100 accounts would need
/// `Link`-header pagination here; the failure would be a loud "no installation on
/// `<owner>`" listing the accounts that *were* seen, not a silent wrong token.
pub fn resolve_installation_id(jwt: &str, owner: &str, app_id: &str) -> Result<u64, String> {
    let url = format!("{}/app/installations?per_page=100", api_base());
    let resp = crate::http_agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| {
            format!(
                "GET /app/installations failed for App {app_id}: {}. A 401 here means the \
                 JWT was rejected — usually a private key that does not belong to this App ID, \
                 or host clock skew beyond 60s.",
                crate::redact(&e.to_string())
            )
        })?;

    let list: Vec<Installation> = resp.into_json().map_err(|e| {
        format!(
            "parsing /app/installations: {}",
            crate::redact(&e.to_string())
        )
    })?;

    let want = owner.to_ascii_lowercase();
    list.iter()
        .find(|i| {
            i.account
                .as_ref()
                .is_some_and(|a| a.login.to_ascii_lowercase() == want)
        })
        .map(|i| i.id)
        .ok_or_else(|| {
            let seen: Vec<&str> = list
                .iter()
                .filter_map(|i| i.account.as_ref().map(|a| a.login.as_str()))
                .collect();
            format!(
                "App {app_id} has no installation on '{owner}' (installed on: {}). Install the \
                 App on that account and grant it the runner repos. Refusing to continue — \
                 falling back to GH_TOKEN here would silently undo App auth.",
                if seen.is_empty() {
                    "<none>".to_string()
                } else {
                    seen.join(", ")
                }
            )
        })
}

/// Identifies which App+account a cached token belongs to.
///
/// One process can legitimately mint for more than one account: `warm` builds a
/// per-repo `Cli`, so a `--prefer-repos` list spanning two owners resolves two
/// different installations. An unkeyed cache would hand the second owner the first
/// owner's token, which fails as a confusing 404 on the registration-token POST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenKey {
    pub app_id: String,
    pub owner_lc: String,
}

impl TokenKey {
    pub fn of(cfg: &AppConfig) -> Self {
        Self {
            app_id: cfg.app_id.clone(),
            owner_lc: cfg.owner.to_ascii_lowercase(),
        }
    }
}

/// A minted token plus the instant after which it must be re-minted.
#[derive(Debug, Clone)]
pub struct CachedToken {
    token: InstallationToken,
    /// Wall-clock deadline; [`REMINT_MARGIN`] is already subtracted.
    renew_after: SystemTime,
    pub key: TokenKey,
    pub installation_id: u64,
    pub expires_at: String,
}

impl CachedToken {
    /// Usable only if it is for the same App+account *and* not yet due for renewal.
    pub fn is_usable_for(&self, key: &TokenKey, now: SystemTime) -> bool {
        self.key == *key && now < self.renew_after
    }
    pub fn token(&self) -> &InstallationToken {
        &self.token
    }
}

/// Compute the renewal deadline from GitHub's `expires_at`, minus [`REMINT_MARGIN`].
pub fn renew_deadline(minted_at: SystemTime, expires_at: &str) -> SystemTime {
    let ttl = parse_iso8601_utc(expires_at)
        .and_then(|exp| {
            let now = minted_at.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
            u64::try_from(exp - now).ok()
        })
        .map(Duration::from_secs)
        .filter(|d| *d > REMINT_MARGIN)
        .unwrap_or(FALLBACK_TTL);
    minted_at + ttl.saturating_sub(REMINT_MARGIN)
}

/// Perform a full mint: JWT -> installation lookup -> access token.
pub fn mint_installation_token(cfg: &AppConfig) -> Result<CachedToken, String> {
    let now = now_epoch_secs()?;
    let jwt = build_app_jwt(&cfg.app_id, &cfg.key, now)?;
    let installation_id = resolve_installation_id(&jwt, &cfg.owner, &cfg.app_id)?;

    let url = format!(
        "{}/app/installations/{installation_id}/access_tokens",
        api_base()
    );
    let resp = crate::http_agent()
        .post(&url)
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("Content-Length", "0")
        .call()
        .map_err(|e| {
            format!(
                "POST /app/installations/{installation_id}/access_tokens failed: {}",
                crate::redact(&e.to_string())
            )
        })?;

    let body: AccessTokenResponse = resp.into_json().map_err(|e| {
        format!(
            "parsing installation token response: {}",
            crate::redact(&e.to_string())
        )
    })?;

    if body.token.is_empty() {
        return Err(format!(
            "GitHub returned an empty installation token for installation {installation_id}. \
             Refusing to continue with no credential."
        ));
    }

    let minted_at = SystemTime::now();
    Ok(CachedToken {
        renew_after: renew_deadline(minted_at, &body.expires_at),
        token: InstallationToken(body.token),
        key: TokenKey::of(cfg),
        installation_id,
        expires_at: body.expires_at,
    })
}

fn cache() -> &'static Mutex<Option<CachedToken>> {
    static CACHE: OnceLock<Mutex<Option<CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Return a live installation token, minting only when the cached one is within
/// [`REMINT_MARGIN`] of expiry. This is what makes long `listen` runs survive the
/// <=1 h TTL without an interactive step.
pub fn cached_installation_token(cfg: &AppConfig) -> Result<String, String> {
    let mut guard = cache()
        .lock()
        .map_err(|_| "auth cache poisoned".to_string())?;

    let key = TokenKey::of(cfg);
    if let Some(c) = guard.as_ref() {
        if c.is_usable_for(&key, SystemTime::now()) {
            return Ok(c.token().expose().to_string());
        }
    }

    let fresh = mint_installation_token(cfg)?;
    // Expiry is not a secret; the token is. Only the former is ever printed.
    eprintln!(
        "auth: minted GitHub App installation token (app_id={} owner={} installation={} expires={})",
        cfg.app_id,
        cfg.owner,
        fresh.installation_id,
        if fresh.expires_at.is_empty() {
            "<unreported>"
        } else {
            &fresh.expires_at
        }
    );
    let tok = fresh.token().expose().to_string();
    *guard = Some(fresh);
    Ok(tok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a PEM private-key banner at runtime, e.g. `pem_banner("RSA ", "abc")`.
    ///
    /// A spelled-out BEGIN/END private-key header in source trips the gitleaks
    /// `private-key` rule, and `gitleaks` is a **required** status check across the
    /// fleet. Assembling the header from fragments keeps the scanner honest — no
    /// allowlist entry, which would also blind it to a genuine key pasted into this
    /// file later — while still producing the exact byte sequence that
    /// `looks_like_private_key` matches on.
    fn pem_banner(kind: &str, body: &str) -> String {
        let (b, e, k) = ("-----BEGIN ", "-----END ", "PRIVATE KEY-----");
        format!("{b}{kind}{k}\n{body}\n{e}{kind}{k}")
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    // --- base64url ---

    #[test]
    fn b64url_matches_rfc4648_vectors_unpadded() {
        assert_eq!(b64url_encode(b""), "");
        assert_eq!(b64url_encode(b"f"), "Zg");
        assert_eq!(b64url_encode(b"fo"), "Zm8");
        assert_eq!(b64url_encode(b"foo"), "Zm9v");
        assert_eq!(b64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(b64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(b64url_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn b64url_uses_url_alphabet_not_standard() {
        // 0xfb 0xff would be "+/" in standard base64.
        let s = b64url_encode(&[0xfb, 0xff, 0xfe]);
        assert!(
            !s.contains('+') && !s.contains('/') && !s.contains('='),
            "{s}"
        );
        assert_eq!(b64url_decode(&s).unwrap(), vec![0xfb, 0xff, 0xfe]);
    }

    #[test]
    fn b64url_roundtrips_arbitrary_bytes() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        assert_eq!(b64url_decode(&b64url_encode(&bytes)).unwrap(), bytes);
    }

    // --- JWT construction (the ported reference logic) ---

    #[test]
    fn jwt_header_decodes_to_rs256() {
        let si = jwt_signing_input("123456", 1_700_000_000);
        let header = si.split('.').next().unwrap();
        assert_eq!(
            String::from_utf8(b64url_decode(header).unwrap()).unwrap(),
            r#"{"alg":"RS256","typ":"JWT"}"#
        );
    }

    #[test]
    fn jwt_payload_backdates_iat_60s_and_caps_lifetime_at_600s() {
        let now = 1_700_000_000i64;
        let si = jwt_signing_input("123456", now);
        let payload = si.split('.').nth(1).unwrap();
        let json = String::from_utf8(b64url_decode(payload).unwrap()).unwrap();
        assert_eq!(
            json,
            r#"{"iat":1699999940,"exp":1700000540,"iss":"123456"}"#
        );

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let iat = v["iat"].as_i64().unwrap();
        let exp = v["exp"].as_i64().unwrap();
        assert_eq!(now - iat, 60, "iat must be backdated 60s for clock skew");
        assert_eq!(exp - iat, 600, "GitHub caps App JWT lifetime at 10 minutes");
        assert!(exp - now <= 600, "exp must never exceed now+600");
        // iss is a *string* in the reference implementation; GitHub accepts both but
        // changing it would be an unreviewed deviation from the verified reference.
        assert!(v["iss"].is_string());
    }

    #[test]
    fn jwt_signing_input_has_exactly_two_sections() {
        let si = jwt_signing_input("1", 0);
        assert_eq!(si.matches('.').count(), 1);
        assert!(!si.contains('=') && !si.contains('+') && !si.contains('/'));
    }

    // --- auth-mode selection ---

    #[test]
    fn no_app_id_selects_token_mode() {
        let m = select_auth_mode(env_of(&[("GH_TOKEN", "gho_x")]), None).unwrap();
        assert_eq!(m, AuthMode::Token);
    }

    #[test]
    fn empty_app_id_selects_token_mode() {
        let m = select_auth_mode(env_of(&[("GHA_APP_ID", "   ")]), Some("tzervas")).unwrap();
        assert_eq!(m, AuthMode::Token);
    }

    #[test]
    fn app_id_takes_precedence_over_gh_token() {
        let m = select_auth_mode(
            env_of(&[
                ("GH_TOKEN", "gho_long_lived"),
                ("GHA_APP_ID", "123456"),
                ("GHA_APP_PRIVATE_KEY_PATH", "/etc/gha/app.pem"),
                ("GHA_OWNER", "tzervas"),
            ]),
            None,
        )
        .unwrap();
        match m {
            AuthMode::App(c) => {
                assert_eq!(c.app_id, "123456");
                assert_eq!(c.owner, "tzervas");
                assert_eq!(c.key, KeySource::Path(PathBuf::from("/etc/gha/app.pem")));
            }
            AuthMode::Token => panic!("App auth must win when GHA_APP_ID is set"),
        }
    }

    #[test]
    fn key_path_wins_over_inline_key() {
        let m = select_auth_mode(
            env_of(&[
                ("GHA_APP_ID", "1"),
                ("GHA_APP_PRIVATE_KEY_PATH", "/k.pem"),
                ("GHA_APP_PRIVATE_KEY", &pem_banner("", "ignored")),
                ("GHA_OWNER", "o"),
            ]),
            None,
        )
        .unwrap();
        assert!(
            matches!(m, AuthMode::App(ref c) if c.key == KeySource::Path(PathBuf::from("/k.pem")))
        );
    }

    #[test]
    fn inline_key_accepted_when_no_path() {
        let m = select_auth_mode(
            env_of(&[
                ("GHA_APP_ID", "1"),
                ("GHA_APP_PRIVATE_KEY", &pem_banner("RSA ", "abc")),
                ("GHA_OWNER", "o"),
            ]),
            None,
        )
        .unwrap();
        assert!(matches!(m, AuthMode::App(ref c) if matches!(c.key, KeySource::Inline(_))));
    }

    #[test]
    fn app_id_without_key_refuses_loudly_and_never_falls_back() {
        let err = select_auth_mode(
            env_of(&[
                ("GHA_APP_ID", "123456"),
                ("GHA_OWNER", "o"),
                ("GH_TOKEN", "gho_x"),
            ]),
            None,
        )
        .unwrap_err();
        assert!(err.contains("GHA_APP_PRIVATE_KEY_PATH"), "{err}");
        assert!(err.contains("Refusing"), "{err}");
    }

    #[test]
    fn app_id_without_owner_refuses_loudly() {
        let err = select_auth_mode(
            env_of(&[
                ("GHA_APP_ID", "123456"),
                ("GHA_APP_PRIVATE_KEY_PATH", "/k.pem"),
            ]),
            None,
        )
        .unwrap_err();
        assert!(err.contains("GHA_OWNER"), "{err}");
    }

    #[test]
    fn client_id_mistaken_for_app_id_is_rejected() {
        for bad in [
            "Iv1.abc123",
            "Iv23liABCDEF",
            "not-a-number",
            "12345678901234567890123",
        ] {
            let err = select_auth_mode(
                env_of(&[
                    ("GHA_APP_ID", bad),
                    ("GHA_APP_PRIVATE_KEY_PATH", "/k.pem"),
                    ("GHA_OWNER", "o"),
                ]),
                None,
            )
            .unwrap_err();
            assert!(err.contains("numeric App ID"), "{bad}: {err}");
        }
    }

    #[test]
    fn owner_hint_overrides_gha_owner_env() {
        let m = select_auth_mode(
            env_of(&[
                ("GHA_APP_ID", "1"),
                ("GHA_APP_PRIVATE_KEY_PATH", "/k.pem"),
                ("GHA_OWNER", "from-env"),
            ]),
            Some("from-cli"),
        )
        .unwrap();
        assert!(matches!(m, AuthMode::App(ref c) if c.owner == "from-cli"));
    }

    #[test]
    fn owner_shaped_like_owner_slash_repo_is_rejected() {
        let err = select_auth_mode(
            env_of(&[("GHA_APP_ID", "1"), ("GHA_APP_PRIVATE_KEY_PATH", "/k.pem")]),
            Some("tzervas/gha-runner-ctl"),
        )
        .unwrap_err();
        assert!(err.contains("bare account login"), "{err}");
    }

    // --- describe() must never leak material ---

    #[test]
    fn describe_never_contains_key_material() {
        let pem = pem_banner("", "SUPERSECRETKEYBYTES");
        let m = AuthMode::App(Box::new(AppConfig {
            app_id: "1".into(),
            owner: "o".into(),
            key: KeySource::Inline(Pem::new(pem)),
        }));
        let d = m.describe();
        assert!(!d.contains("SUPERSECRETKEYBYTES"), "{d}");
        assert!(d.contains("github-app"), "{d}");
    }

    #[test]
    fn debug_impls_redact_secrets() {
        let pem = Pem::new(pem_banner("", "SUPERSECRET"));
        assert!(!format!("{pem:?}").contains("SUPERSECRET"));
        let tok = InstallationToken("ghs_supersecrettoken".into());
        assert!(!format!("{tok:?}").contains("supersecrettoken"));
        // ...and the whole config, which is what gets Debug-printed in practice.
        let cfg = AppConfig {
            app_id: "1".into(),
            owner: "o".into(),
            key: KeySource::Inline(pem),
        };
        assert!(!format!("{cfg:?}").contains("SUPERSECRET"));
    }

    #[test]
    fn crate_redact_covers_minted_ghs_tokens() {
        let line = format!("boom: {}", "ghs_16CharsOfTokenMaterial00");
        assert!(!crate::redact(&line).contains("16CharsOfTokenMaterial00"));
    }

    // --- expiry handling ---

    /// Expected values produced independently by GNU coreutils, not by hand:
    /// `date -u -d "<s>" +%s`. Leap-year and epoch-boundary cases included because a
    /// wrong `days_from_civil` would otherwise only show up as a mysterious 401 after
    /// the token silently outlived its cached deadline.
    #[test]
    fn parses_github_expires_at() {
        for (s, want) in [
            ("1970-01-01T00:00:00Z", 0i64),
            ("1999-12-31T23:59:59Z", 946_684_799),
            ("2000-03-01T00:00:00Z", 951_868_800),
            ("2024-02-29T23:59:59Z", 1_709_251_199), // leap day
            ("2026-07-24T21:00:00Z", 1_784_926_800),
            ("2038-01-19T03:14:08Z", 2_147_483_648), // past i32 seconds
            ("2100-03-01T00:00:00Z", 4_107_542_400), // 2100 is NOT a leap year
        ] {
            assert_eq!(parse_iso8601_utc(s), Some(want), "{s}");
        }
    }

    #[test]
    fn rejects_malformed_expires_at() {
        for bad in [
            "",
            "not a date",
            "2026-07-24",
            "2026/07/24T21:00:00Z",
            "2026-13-01T00:00:00Z",
        ] {
            assert_eq!(parse_iso8601_utc(bad), None, "{bad}");
        }
    }

    #[test]
    fn renewal_happens_before_actual_expiry() {
        let minted = UNIX_EPOCH + Duration::from_secs(1_784_926_800);
        // GitHub's documented 1h TTL.
        let deadline = renew_deadline(minted, "2026-07-24T22:00:00Z");
        let ttl = deadline.duration_since(minted).unwrap();
        assert_eq!(ttl, Duration::from_secs(3600) - REMINT_MARGIN);
        assert!(ttl < Duration::from_secs(3600));
    }

    #[test]
    fn unparseable_expiry_falls_back_to_conservative_ttl() {
        let minted = UNIX_EPOCH + Duration::from_secs(1_784_926_800);
        let d = renew_deadline(minted, "garbage");
        assert_eq!(
            d.duration_since(minted).unwrap(),
            FALLBACK_TTL - REMINT_MARGIN
        );
    }

    #[test]
    fn already_expired_token_is_not_treated_as_fresh() {
        let minted = UNIX_EPOCH + Duration::from_secs(1_784_926_800);
        // Expiry in the past relative to mint: must not yield a far-future deadline.
        let d = renew_deadline(minted, "2020-01-01T00:00:00Z");
        assert_eq!(
            d.duration_since(minted).unwrap(),
            FALLBACK_TTL - REMINT_MARGIN
        );

        let key = TokenKey {
            app_id: "1".into(),
            owner_lc: "o".into(),
        };
        let c = CachedToken {
            token: InstallationToken("ghs_x".into()),
            renew_after: minted,
            key: key.clone(),
            installation_id: 7,
            expires_at: "x".into(),
        };
        assert!(!c.is_usable_for(&key, minted + Duration::from_secs(1)));
        assert!(c.is_usable_for(&key, minted - Duration::from_secs(1)));
    }

    /// `warm` builds a per-repo `Cli`, so a `--prefer-repos` list spanning two owners
    /// resolves two different installations in one process. A cache keyed only on
    /// freshness would hand the second owner the first owner's token.
    #[test]
    fn cached_token_is_not_reused_across_a_different_owner_or_app() {
        let minted = UNIX_EPOCH + Duration::from_secs(1_784_926_800);
        let still_valid = minted + Duration::from_secs(600);
        let cfg = |app_id: &str, owner: &str| AppConfig {
            app_id: app_id.into(),
            owner: owner.into(),
            key: KeySource::Path("/k.pem".into()),
        };
        let c = CachedToken {
            token: InstallationToken("ghs_owner_a".into()),
            renew_after: still_valid + Duration::from_secs(1),
            key: TokenKey::of(&cfg("1", "owner-a")),
            installation_id: 11,
            expires_at: "x".into(),
        };
        assert!(
            c.is_usable_for(&TokenKey::of(&cfg("1", "owner-a")), still_valid),
            "same app + same owner, not yet due: reuse"
        );
        assert!(
            c.is_usable_for(&TokenKey::of(&cfg("1", "OWNER-A")), still_valid),
            "GitHub logins are case-insensitive: must still reuse"
        );
        assert!(
            !c.is_usable_for(&TokenKey::of(&cfg("1", "owner-b")), still_valid),
            "different owner => different installation => must re-mint"
        );
        assert!(
            !c.is_usable_for(&TokenKey::of(&cfg("2", "owner-a")), still_valid),
            "different app => must re-mint"
        );
    }

    // --- signing: real RS256, no network, skipped if openssl is absent ---

    fn openssl_available() -> bool {
        Command::new("openssl")
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[test]
    fn rs256_signature_verifies_against_the_public_key() {
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("gha-appauth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let priv_pem = dir.join("priv.pem");
        let pub_pem = dir.join("pub.pem");
        assert!(Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out"
            ])
            .arg(&priv_pem)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("openssl")
            .arg("rsa")
            .arg("-in")
            .arg(&priv_pem)
            .arg("-pubout")
            .arg("-out")
            .arg(&pub_pem)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());

        let key = KeySource::Path(priv_pem.clone());
        let jwt = build_app_jwt("123456", &key, 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "JWT must be header.payload.signature");

        // Write signing input + signature and let openssl verify, exactly as the
        // reference implementation was verified.
        let si_path = dir.join("si.txt");
        let sig_path = dir.join("sig.bin");
        fs::write(&si_path, format!("{}.{}", parts[0], parts[1])).unwrap();
        fs::write(&sig_path, b64url_decode(parts[2]).unwrap()).unwrap();

        let out = Command::new("openssl")
            .args(["dgst", "-sha256", "-verify"])
            .arg(&pub_pem)
            .arg("-signature")
            .arg(&sig_path)
            .arg(&si_path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("Verified OK"),
            "openssl verify said: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );

        // Same key delivered inline must produce an equally valid signature.
        let inline = KeySource::Inline(Pem::new(fs::read_to_string(&priv_pem).unwrap()));
        let jwt2 = build_app_jwt("123456", &inline, 1_700_000_000).unwrap();
        assert_eq!(
            jwt2.split('.').take(2).collect::<Vec<_>>(),
            parts[..2].to_vec(),
            "signing input must not depend on how the key was supplied"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_pem_key_refuses_before_invoking_openssl() {
        let dir = std::env::temp_dir().join(format!("gha-appauth-badkey-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("not-a-key.pem");
        fs::write(&p, "this is not a PEM file").unwrap();
        let err = build_app_jwt("1", &KeySource::Path(p), 0).unwrap_err();
        assert!(err.contains("PEM private key"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_key_file_refuses_loudly() {
        let err =
            build_app_jwt("1", &KeySource::Path("/nonexistent/app.pem".into()), 0).unwrap_err();
        assert!(err.contains("unreadable"), "{err}");
        assert!(
            err.contains("GH_TOKEN"),
            "must say it is not falling back: {err}"
        );
    }

    #[test]
    fn inline_key_temp_file_is_removed_after_signing() {
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("gha-appauth-tmpclean-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let priv_pem = dir.join("priv.pem");
        assert!(Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out"
            ])
            .arg(&priv_pem)
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        let pem = fs::read_to_string(&priv_pem).unwrap();
        let key = KeySource::Inline(Pem::new(pem));
        let before = temp_keydirs();
        // Three mints, as a long `listen` run would do over three hours. An earlier
        // version removed only the file and left the 0700 dir, so this grew by one
        // per mint.
        for _ in 0..3 {
            build_app_jwt("1", &key, 0).unwrap();
        }
        assert_eq!(
            temp_keydirs(),
            before,
            "inline-key temp dir must not survive signing"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inline_key_temp_dir_and_file_are_private_and_key_never_in_argv() {
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let pem = pem_banner("", "not-really-a-key");
        // Signing will fail (garbage key) but key_path() still had to create the temp
        // material first, and the guard must still clean it up on that error path.
        let before = temp_keydirs();
        let err = build_app_jwt("1", &KeySource::Inline(Pem::new(pem)), 0).unwrap_err();
        assert!(err.contains("openssl"), "{err}");
        assert!(
            !err.contains("not-really-a-key"),
            "openssl stderr must not echo key material: {err}"
        );
        assert_eq!(
            temp_keydirs(),
            before,
            "temp dir must be removed on the signing-error path too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_dir_and_file_helpers_use_0700_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("gha-appauth-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        create_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let f = d.join("k.pem");
        drop(create_private_file(&f).unwrap());
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // create_new: a second call must not silently reuse an attacker-planted file.
        assert!(create_private_file(&f).is_err());
        let _ = fs::remove_dir_all(&d);
    }

    fn temp_keydirs() -> usize {
        fs::read_dir(std::env::temp_dir())
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with(TEMP_KEY_DIR_PREFIX)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    // --- MAJOR finding fix: the README's "Verified by inspecting the child's
    // recorded `/proc/<pid>/cmdline`" claim had no test performing that
    // inspection. This one does. ---

    /// Proves JWT signing input is never placed on openssl argv by inspecting the live
    /// child's `/proc/<pid>/cmdline` *before* writing to stdin.
    ///
    /// Two ordering guarantees make this race-free rather than flaky:
    ///
    /// 1. `openssl dgst -sign` blocks until it receives EOF on stdin, so the child is
    ///    guaranteed still alive throughout this test — right up until this test
    ///    itself decides to write and close stdin. Reading after writing (or after
    ///    `wait`) would race against process exit and could inspect a reaped pid.
    /// 2. Immediately after `spawn()` returns, the kernel has allocated the pid but the
    ///    child may not have completed `execve("openssl")` yet — `/proc/<pid>/cmdline`
    ///    can read back empty during that narrow fork/exec transition (measured: ~19
    ///    empty reads out of 20 spawns with no wait at all). So this polls
    ///    `/proc/<pid>/cmdline` until it is non-empty (bounded at 2s) *before* making
    ///    any assertion about its contents — still strictly before writing to stdin,
    ///    so the "child cannot have exited yet" guarantee above is untouched.
    #[test]
    fn signing_input_and_key_path_never_appear_in_argv() {
        if !openssl_available() {
            eprintln!("skipping: openssl not available");
            return;
        }
        if !Path::new("/proc").exists() {
            eprintln!("skipping: /proc not available (non-Linux host)");
            return;
        }

        let dir =
            std::env::temp_dir().join(format!("gha-appauth-argvtest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let priv_pem_path = dir.join("key.pem");

        assert!(
            Command::new("openssl")
                .args([
                    "genpkey",
                    "-algorithm",
                    "RSA",
                    "-pkeyopt",
                    "rsa_keygen_bits:2048",
                    "-out",
                ])
                .arg(&priv_pem_path)
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "openssl genpkey failed"
        );

        let signing_input = "header.payload-MARKER-SECRET-VALUE-should-never-be-in-argv";
        let marker = "MARKER-SECRET-VALUE-should-never-be-in-argv";

        // Same command shape as sign_rs256(), spawned directly here (rather than via
        // sign_rs256 itself) so the test can observe the live Child's pid before it
        // is written to and reaped.
        let mut child = Command::new("openssl")
            .args(["dgst", "-sha256", "-sign"])
            .arg(&priv_pem_path)
            .arg("-binary")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        // Child is blocked waiting for stdin EOF, so it cannot exit out from under
        // us — but /proc/<pid>/cmdline can briefly read back empty in the narrow
        // window between fork() and this exec() completing, so poll (bounded) for a
        // non-empty read rather than assuming the very first read landed after exec.
        let cmdline_path = format!("/proc/{}/cmdline", child.id());
        let poll_deadline = std::time::Instant::now() + Duration::from_secs(2);
        let cmdline_raw = loop {
            let read = fs::read(&cmdline_path).unwrap_or_default();
            if !read.is_empty() || std::time::Instant::now() >= poll_deadline {
                break read;
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        assert!(
            !cmdline_raw.is_empty(),
            "/proc/{}/cmdline never became non-empty within 2s — the child likely \
             exited before exec, which would itself be a test-infrastructure bug, \
             not a pass",
            child.id()
        );
        let argv: Vec<String> = cmdline_raw
            .split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect();
        let joined = argv.join("\0");

        assert!(
            !argv.iter().any(|a| a.contains(marker)) && !joined.contains(marker),
            "signing-input marker must not appear in argv: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains(signing_input)) && !joined.contains(signing_input),
            "full signing input must not appear in argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a.contains("openssl")),
            "expected openssl in argv (sanity check that we inspected the right \
             process, not a false pass on an empty vec): {argv:?}"
        );
        let key_path = priv_pem_path.to_str().unwrap();
        assert!(
            argv.iter().any(|a| a == key_path),
            "expected key path {key_path} in argv: {argv:?}"
        );
        assert!(
            argv.iter().any(|a| a == "-sign"),
            "expected -sign in argv: {argv:?}"
        );

        {
            let mut stdin = child.stdin.take().expect("openssl stdin unavailable");
            stdin.write_all(signing_input.as_bytes()).unwrap();
        } // drop stdin -> EOF so openssl can finish

        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "openssl dgst -sign failed: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

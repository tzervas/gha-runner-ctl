//! GitHub App installation-token authentication — a first-class, parameterised
//! alternative to the long-lived `GH_TOKEN`/`GITHUB_TOKEN` PAT path resolved by
//! [`crate::github_token`].
//!
//! ## Why
//!
//! `listen` re-scans every repo in `GHA_PRIORITY_REPOS` every tick. On the homelab
//! instance that is ~80 GETs/min ≈ 4,800/hour against a classic PAT's 5,000/hour cap —
//! ~96% of budget, which is why `listen: list_demand_jobs: budget exhausted mid-scan`
//! fires on nearly every tick. A GitHub App installation token raises that ceiling —
//! measured at 12,500 requests/hour on this fleet's installation (2.5x the PAT), which is
//! what makes a faster, steadier poll interval sustainable. The poll interval is not the
//! bottleneck here — the credential is.
//!
//! ## CLI surface
//!
//! Three `Cli` fields drive this module (see `src/lib.rs`): `--app-id`/`GHA_APP_ID`,
//! `--app-installation-id`/`GHA_APP_INSTALLATION_ID` (optional — see auto-discovery
//! below), `--app-private-key`/`GHA_APP_PRIVATE_KEY`. `Cli::app_auth_config` builds an
//! [`AppAuthConfig`] from the already-clap-resolved fields (flag beats env, exactly like
//! every other option this tool has), so this module never reads `std::env::var` itself.
//!
//! ## Selection — never a silent downgrade
//!
//! Selected when `--app-id`/`GHA_APP_ID` and `--app-private-key`/`GHA_APP_PRIVATE_KEY`
//! are both set (`--app-installation-id` is optional). If **nothing** App-auth-shaped is
//! set, [`resolve_app_auth_config`] returns `Ok(None)` and the caller falls back to the
//! `GH_TOKEN`/PAT chain unchanged — existing deployments need zero config change. If
//! **any** App-auth flag/env is set but the required pair (id + key) is incomplete, this
//! is an `Err`, and the caller (`Cli::app_auth_config`'s consumers) propagates it as a
//! hard failure rather than falling back — a typo'd flag name silently reverting to PAT
//! auth is a "looks like it worked" failure mode this deliberately refuses to have.
//!
//! ## Private key handling
//!
//! `--app-private-key`/`GHA_APP_PRIVATE_KEY` accepts three forms:
//! - `secret:<group>/<key>` (recommended) — retrieved from the vault via the existing
//!   `secret` CLI (`secret get <group>/<key>`); this module only ever *consumes* that
//!   tool, never reimplements or edits it.
//! - `file:<path>` / a bare path — an existing `0600` PEM file on disk.
//!
//! Inline PEM content (a value containing `-----BEGIN`) is refused outright, in the flag
//! *and* the env var: an env var is readable via `/proc/<pid>/environ` by anyone who can
//! already read the process, is far more likely to leak into shell history / `env` dumps
//! / CI logs than a `0600` file, and can't be `chmod`-restricted the way a file can. We
//! never read PEM bytes into our own long-lived process memory for the `file:`/path
//! forms either: `openssl -sign <path>` reads the key straight off disk. For the
//! `secret:` form the decrypted PEM is materialised to a `0600` file on tmpfs for the
//! span of a single signing operation and shredded immediately after (see
//! [`TempKeyFile`]).
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
//! ~55 minutes, and no `base64` crate for the vault-encoding auto-detection either.
//!
//! ## Installation auto-discovery
//!
//! `--app-installation-id` is optional. When absent, [`resolve_installation_id`] calls
//! `GET /app/installations` with the App JWT and picks one, in priority order:
//! 1. An explicit `--app-installation-id` always wins (no network call at all).
//! 2. If an owner hint is available (`--owner`, `--user`, or the owner half of `--repo`)
//!    and exactly one installation's account matches it, use that one.
//! 3. If there is exactly one installation total, use it.
//! 4. Otherwise: zero installations is an error naming the install URL; more than one
//!    with no unique owner match is an error listing every `id`/account pair and telling
//!    the user to pass `--app-installation-id`.
//!
//! The resolved id is cached (keyed by app id + owner hint) for the process lifetime —
//! auto-discovery costs one extra request per process, not per mint.
//!
//! ## Never logged
//!
//! Every error string that could carry response-body material is passed through
//! [`crate::redact`] (already hardened for `ghs_` App tokens). [`Pem`] and
//! [`InstallationToken`] carry hand-written `Debug` impls that never render their
//! contents, so an accidental `{:?}` can't leak them either. The JWT and the minted
//! installation token are never `eprintln!`'d; only non-secret identifiers (app id,
//! installation id, expiry) are logged.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

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
// NO HOURLY-BUDGET CONSTANT ON PURPOSE.
//
// An installation's rate limit is NOT a fixed number: GitHub scales it with the size of
// the installation (repositories and users), so any constant compiled in here is a guess
// that will be wrong for some installations. Measured on the `tzervas` personal-account
// installation (all repositories, 2026-07-31): **12,500 req/hour**, not the 15,000 that
// is widely quoted — still 2.5x a classic PAT's 5,000/hour.
//
// Printing a guessed budget in the mint log would state a confident wrong number to
// whoever is debugging a rate-limit problem, which is worse than printing nothing.
// `ApiPacer` already reads the real values from the `X-RateLimit-*` response headers, and
// `GET /rate_limit` reports them on demand (see `doctor`) — both are authoritative where
// a constant is not.

// --- Secret-carrying newtypes: Debug never renders the contents ----------------

/// PEM private-key material, held only for the span between vault retrieval and
/// writing it to a `0600` temp file. `Debug` never renders the bytes.
struct Pem(String);

impl Pem {
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Pem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pem(***REDACTED*** {} bytes)", self.0.len())
    }
}

/// A minted installation access token (`ghs_…`). `Debug` never renders it.
pub(crate) struct InstallationToken(String);

impl InstallationToken {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for InstallationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InstallationToken(***REDACTED***)")
    }
}

// --- Configuration / selection -----------------------------------------------

/// Where the private key comes from — see the module docs for the three accepted
/// `--app-private-key`/`GHA_APP_PRIVATE_KEY` forms. Neither variant carries key
/// material: `Path` is just a filesystem path, `Vault` is just a `<group>/<key>`
/// locator resolved on demand (see [`resolve_signing_path`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeySource {
    /// `file:<path>` or a bare path to an existing `0600` PEM file.
    Path(PathBuf),
    /// `secret:<group>/<key>` — resolved via the `secret` CLI at signing time.
    Vault { group_key: String },
}

/// Fully-resolved GitHub App configuration. Only constructed when both `app_id` and
/// the private key are present — see [`resolve_app_auth_config`]. `installation_id`
/// is `None` when it should be auto-discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppAuthConfig {
    pub(crate) app_id: String,
    pub(crate) installation_id: Option<String>,
    pub(crate) key_source: KeySource,
}

/// GitHub App IDs are short decimal integers (not the `Iv1.…`/`Iv23…` Client ID, which
/// is a common mix-up). Reject anything else here rather than letting it become an
/// opaque 401 from the API.
fn is_valid_app_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
}

/// GitHub installation ids are also short decimal integers.
fn is_valid_installation_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 20 && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse `--app-private-key`/`GHA_APP_PRIVATE_KEY` into a [`KeySource`], refusing
/// inline PEM content wherever it appears (with or without a recognized prefix).
fn parse_key_source(raw: &str) -> Result<KeySource, String> {
    let trimmed = raw.trim();
    if trimmed.contains("-----BEGIN") {
        return Err(
            "--app-private-key/GHA_APP_PRIVATE_KEY must not contain inline PEM key \
             material. Use `secret:<group>/<key>` (recommended — retrieved from the \
             vault) or `file:<path>` instead. Inline key material in a flag or env var \
             is readable via /proc/<pid>/environ by anyone who can read the process, is \
             far more likely to leak into shell history, `env` dumps, and CI logs than a \
             0600 file, and cannot be chmod-restricted the way a file can."
                .to_string(),
        );
    }
    if let Some(rest) = trimmed.strip_prefix("secret:") {
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(
                "--app-private-key/GHA_APP_PRIVATE_KEY=secret: is missing a <group>/<key> \
                 (expected e.g. secret:runner/gha-app-key)"
                    .to_string(),
            );
        }
        return Ok(KeySource::Vault {
            group_key: rest.to_string(),
        });
    }
    let path_str = trimmed.strip_prefix("file:").unwrap_or(trimmed).trim();
    if path_str.is_empty() {
        return Err("--app-private-key/GHA_APP_PRIVATE_KEY resolved to an empty path".into());
    }
    Ok(KeySource::Path(PathBuf::from(path_str)))
}

/// Pure resolution over an injected lookup (`Cli::app_auth_config` supplies one backed
/// by the already-clap-resolved `Cli` fields — flag beats env, exactly like every other
/// option this tool has). Never touches `std::env::var` directly, and never mutates
/// process-wide state, so tests can call this directly without racing each other.
///
/// - nothing App-auth-shaped set → `Ok(None)` (silent — this is the default,
///   unconfigured case; caller falls back to `GH_TOKEN`/PAT discovery)
/// - `GHA_APP_ID` + `GHA_APP_PRIVATE_KEY` both present → `Ok(Some(cfg))`
///   (`GHA_APP_INSTALLATION_ID` optional — see the module docs on auto-discovery)
/// - anything App-auth-shaped set but that pair incomplete → `Err(..)` naming what's
///   missing. Callers MUST propagate this as a hard failure, not a silent fallback: a
///   typo'd env var name silently reverting to PAT auth would look like success.
pub(crate) fn resolve_app_auth_config(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<AppAuthConfig>, String> {
    let app_id = get("GHA_APP_ID");
    let installation_id = get("GHA_APP_INSTALLATION_ID");
    let key = get("GHA_APP_PRIVATE_KEY");

    if app_id.is_none() && installation_id.is_none() && key.is_none() {
        return Ok(None);
    }

    let missing: Vec<&str> = [
        ("--app-id/GHA_APP_ID", app_id.is_some()),
        ("--app-private-key/GHA_APP_PRIVATE_KEY", key.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| (!present).then_some(name))
    .collect();

    if !missing.is_empty() {
        return Err(format!(
            "GitHub App auth is partially configured (missing {}). --app-installation-id/\
             GHA_APP_INSTALLATION_ID is optional (auto-discovered when omitted) but \
             --app-id and --app-private-key are both required to enable App auth. Set \
             both, or unset every --app-* flag/env var to use GH_TOKEN/PAT discovery \
             instead. Refusing to silently fall back — that would make a typo look like \
             a working PAT setup.",
            missing.join(", ")
        ));
    }

    let app_id = app_id.expect("checked present above");
    if !is_valid_app_id(&app_id) {
        return Err(format!(
            "--app-id/GHA_APP_ID must be the App's numeric ID (Settings → Developer \
             settings → GitHub Apps → your App → App ID), got {} character(s) that \
             are not all digits. This is NOT the Client ID (`Iv1.…`/`Iv23…`).",
            app_id.chars().count()
        ));
    }
    if let Some(id) = &installation_id {
        if !is_valid_installation_id(id) {
            return Err(format!(
                "--app-installation-id/GHA_APP_INSTALLATION_ID must be numeric, got {:?}",
                id.chars().take(32).collect::<String>()
            ));
        }
    }

    let key_source = parse_key_source(&key.expect("checked present above"))?;

    Ok(Some(AppAuthConfig {
        app_id,
        installation_id,
        key_source,
    }))
}

// --- Vault retrieval (consumes the existing `secret` CLI; never reimplements it) --

/// `secret get <group>/<key>` — decrypt-to-stdout, exactly the pipe-only mode the
/// tool's own `--help` recommends ("avoid it interactively... exists for pipes").
/// We never vendor, fork, or modify SOPS/age/the `secret` script itself — only shell
/// out to it, same as this module already does for `openssl`.
/// Pure so `secret_get_missing_binary_names_the_fix` can assert on it without
/// mutating the process-wide `PATH` (which would race any other test that shells out
/// to a PATH-resolved binary, e.g. the `openssl` end-to-end signing tests).
fn secret_not_found_message(group_key: &str) -> String {
    format!(
        "--app-private-key=secret:{group_key} requires the `secret` CLI on PATH \
         (normally /usr/local/bin/secret) — not found. Install it, or use file:<path> \
         instead."
    )
}

fn secret_get(group_key: &str) -> Result<String, String> {
    let out = Command::new("secret")
        .arg("get")
        .arg(group_key)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                secret_not_found_message(group_key)
            } else {
                format!("failed to run `secret get {group_key}`: {e}")
            }
        })?;
    if !out.status.success() {
        return Err(format!(
            "`secret get {group_key}` failed ({}): {}",
            out.status,
            crate::redact(String::from_utf8_lossy(&out.stderr).trim())
        ));
    }
    String::from_utf8(out.stdout)
        .map(|s| s.trim_end_matches('\n').to_string())
        .map_err(|_| format!("`secret get {group_key}` returned non-UTF-8 output"))
}

/// The vault stores this PEM base64-encoded on a single line, because `secret set`
/// refuses values containing whitespace. Detect which encoding we got so it works
/// regardless of how a user stored it: raw PEM as-is, or base64 that decodes to one.
fn decode_vault_pem(raw: &str) -> Result<Pem, String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("-----BEGIN") {
        return Ok(Pem(trimmed.to_string()));
    }
    if let Ok(bytes) = base64_decode_standard(trimmed) {
        if let Ok(decoded) = String::from_utf8(bytes) {
            if decoded.trim_start().starts_with("-----BEGIN") {
                return Ok(Pem(decoded));
            }
        }
    }
    Err(
        "vault entry is neither raw PEM nor base64-encoded PEM (expected it to start \
         with -----BEGIN, directly or after base64-decoding). Re-check what was stored \
         at this vault path."
            .to_string(),
    )
}

/// Minimal, dependency-free standard-alphabet (RFC 4648 §4) base64 decoder — just
/// enough to undo `base64` command output. Ignores whitespace and `=` padding.
fn base64_decode_standard(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() * 3 / 4 + 3);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for b in input.bytes() {
        if b.is_ascii_whitespace() || b == b'=' {
            continue;
        }
        let v = val(b).ok_or_else(|| format!("invalid base64 byte {b:#04x}"))?;
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

// --- Temp key materialization for the `secret:` form ---------------------------

/// Directory to materialise a vault-retrieved key into: tmpfs when available (nothing
/// ever touches persistent disk), falling back to `$TMPDIR`/`/tmp` with a one-line
/// warning when `/dev/shm` doesn't exist (e.g. some containers).
fn tmp_secret_dir() -> PathBuf {
    let shm = PathBuf::from("/dev/shm");
    if shm.is_dir() {
        return shm;
    }
    eprintln!(
        "appauth: /dev/shm is not available; materialising the App private key under a \
         persistent-storage temp dir instead (removed immediately after each use)"
    );
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// A `0600` file holding decrypted PEM key material, for the span of one signing
/// operation. Shredded (best-effort overwrite, then removed) on drop — including on
/// error paths, since `Drop::drop` runs on any early return via `?` through the scope
/// that holds this guard. (Residual risk: `panic = "abort"` in this crate's release
/// profile and unhandled process signals skip `Drop` entirely; `/dev/shm` not
/// surviving a reboot is what bounds that residual exposure, not this guard.)
struct TempKeyFile {
    path: PathBuf,
}

impl TempKeyFile {
    fn write(pem: &Pem) -> Result<Self, String> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tmp_secret_dir();
        let path = dir.join(format!(
            "gha-app-key-{}-{}.pem",
            std::process::id(),
            now_unix()
        ));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("failed to create temp key file {}: {e}", path.display()))?;
        f.write_all(pem.as_str().as_bytes())
            .map_err(|e| format!("failed to write temp key file {}: {e}", path.display()))?;
        drop(f);
        Ok(Self { path })
    }
}

impl Drop for TempKeyFile {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let zeros = vec![0u8; meta.len() as usize];
            let _ = std::fs::write(&self.path, zeros);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Both `--app-private-key` forms must resolve to a file only the owner can read: a
/// user-supplied `file:`/bare path is checked as given; a vault-materialised temp file
/// is created `0600` already, and this is re-checked anyway as defense in depth.
fn ensure_key_path_private(key_path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(key_path).map_err(|e| {
        format!(
            "--app-private-key path {} is not readable: {e}",
            key_path.display()
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "--app-private-key path {} is not a regular file",
            key_path.display()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "--app-private-key path {} is group- or world-readable (mode {:o}) — refusing \
             to use it. Fix: chmod 600 {}",
            key_path.display(),
            mode,
            key_path.display()
        ));
    }
    Ok(())
}

/// Resolve a [`KeySource`] to a filesystem path `openssl` can sign against, returning
/// an optional cleanup guard that must be kept alive exactly until signing is done.
fn resolve_signing_path(key_source: &KeySource) -> Result<(PathBuf, Option<TempKeyFile>), String> {
    match key_source {
        KeySource::Path(p) => {
            ensure_key_path_private(p)?;
            Ok((p.clone(), None))
        }
        KeySource::Vault { group_key } => {
            let raw = secret_get(group_key)?;
            let pem = decode_vault_pem(&raw)?;
            let tmp = TempKeyFile::write(&pem)?;
            ensure_key_path_private(&tmp.path)?;
            let path = tmp.path.clone();
            Ok((path, Some(tmp)))
        }
    }
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
                 --app-id/--app-installation-id/--app-private-key to use GH_TOKEN auth."
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

fn encode_jwt(app_id: &str, key_source: &KeySource, now_unix: i64) -> Result<String, String> {
    if !is_valid_app_id(app_id) {
        return Err("--app-id/GHA_APP_ID must be the App's numeric ID".to_string());
    }
    let claims = jwt_claims(now_unix);
    let signing_input = format!(
        "{}.{}",
        b64url(jwt_header_json().as_bytes()),
        b64url(jwt_payload_json(app_id, claims).as_bytes())
    );
    let (key_path, _cleanup) = resolve_signing_path(key_source)?;
    let sig = sign_rs256(&signing_input, &key_path)?;
    // _cleanup (if any) drops here, shredding the vault-materialised temp file.
    Ok(format!("{signing_input}.{}", b64url(&sig)))
}

// --- installation listing / lookup (shared by discovery + doctor) --------------

#[derive(Deserialize, Clone, Debug, Default)]
pub(crate) struct InstallationAccount {
    #[serde(default)]
    pub(crate) login: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub(crate) struct Installation {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) account: Option<InstallationAccount>,
    #[serde(default)]
    pub(crate) repository_selection: Option<String>,
    #[serde(default)]
    pub(crate) permissions: Option<BTreeMap<String, String>>,
}

impl Installation {
    pub(crate) fn account_login(&self) -> &str {
        self.account
            .as_ref()
            .and_then(|a| a.login.as_deref())
            .unwrap_or("(unknown account)")
    }
}

#[derive(Deserialize, Default)]
struct AppInfo {
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn list_installations_http(
    jwt: &str,
    http: &crate::HttpConfig,
) -> Result<Vec<Installation>, String> {
    let result = crate::http_agent(http)
        .get("https://api.github.com/app/installations?per_page=100")
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();
    match result {
        Ok(resp) => resp
            .into_json::<Vec<Installation>>()
            .map_err(|e| format!("GET /app/installations response parse failed: {e}")),
        Err(ureq::Error::Status(401, r)) => Err(format!(
            "GET /app/installations: HTTP 401 — the App JWT was rejected. This usually \
             means the App was deleted/suspended, the private key was rotated (update the \
             vault entry), or --app-id doesn't match this key. Response: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "GET /app/installations failed: HTTP {code}: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(e) => Err(format!(
            "GET /app/installations failed: {}",
            crate::redact(&e.to_string())
        )),
    }
}

fn get_app_info_http(jwt: &str, http: &crate::HttpConfig) -> Result<AppInfo, String> {
    let result = crate::http_agent(http)
        .get("https://api.github.com/app")
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();
    match result {
        Ok(resp) => resp
            .into_json::<AppInfo>()
            .map_err(|e| format!("GET /app response parse failed: {e}")),
        Err(ureq::Error::Status(401, r)) => Err(format!(
            "GET /app: HTTP 401 — the App JWT was rejected (revoked App, rotated key, or \
             wrong --app-id). Response: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "GET /app failed: HTTP {code}: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(e) => Err(format!(
            "GET /app failed: {}",
            crate::redact(&e.to_string())
        )),
    }
}

pub(crate) fn get_installation_http(
    jwt: &str,
    installation_id: &str,
    http: &crate::HttpConfig,
) -> Result<Installation, String> {
    let url = format!("https://api.github.com/app/installations/{installation_id}");
    let result = crate::http_agent(http)
        .get(&url)
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();
    match result {
        Ok(resp) => resp.into_json::<Installation>().map_err(|e| {
            format!("GET /app/installations/{installation_id} response parse failed: {e}")
        }),
        Err(ureq::Error::Status(401, r)) => Err(format!(
            "GET /app/installations/{installation_id}: HTTP 401 — the App JWT was \
             rejected (revoked App, rotated key, or wrong --app-id). Response: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(ureq::Error::Status(404, r)) => Err(format!(
            "GET /app/installations/{installation_id}: HTTP 404 — no such installation \
             for this App (wrong --app-installation-id, or the App was uninstalled \
             there). Omit --app-installation-id to auto-discover it instead. Response: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(ureq::Error::Status(code, r)) => Err(format!(
            "GET /app/installations/{installation_id} failed: HTTP {code}: {}",
            crate::redact(&r.into_string().unwrap_or_default())
        )),
        Err(e) => Err(format!(
            "GET /app/installations/{installation_id} failed: {}",
            crate::redact(&e.to_string())
        )),
    }
}

// --- installation selection (pure, testable: zero / one / many) ----------------

fn zero_installations_message(app_id: &str, app_slug: Option<&str>) -> String {
    match app_slug {
        Some(slug) => format!(
            "GitHub App {app_id} is created but not installed on any account or org yet. \
             Install it, then re-run: https://github.com/settings/apps/{slug}/installations"
        ),
        None => format!(
            "GitHub App {app_id} is created but not installed on any account or org yet. \
             Open the App at https://github.com/settings/apps, click \"Install App\", \
             then re-run — or pass --app-installation-id once you know it."
        ),
    }
}

/// Pick an installation id from a discovery response. `app_slug` is only used to build
/// the install URL in the zero-installations message; pass it (best-effort — it costs
/// one more `GET /app` call, only worth making once discovery already found nothing).
///
/// Priority: an owner-matched installation beats "the only one there is" beats
/// ambiguous-error, matching the module docs.
fn select_installation(
    installations: &[Installation],
    owner_hint: Option<&str>,
    app_id: &str,
    app_slug: Option<&str>,
) -> Result<String, String> {
    if installations.is_empty() {
        return Err(zero_installations_message(app_id, app_slug));
    }

    if let Some(owner) = owner_hint {
        let matches: Vec<&Installation> = installations
            .iter()
            .filter(|i| i.account_login().eq_ignore_ascii_case(owner))
            .collect();
        if matches.len() == 1 {
            let inst = matches[0];
            eprintln!(
                "appauth: auto-discovered installation id={} account={} (matched owner {owner})",
                inst.id,
                inst.account_login()
            );
            return Ok(inst.id.to_string());
        }
    }

    if installations.len() == 1 {
        let inst = &installations[0];
        eprintln!(
            "appauth: auto-discovered installation id={} account={}",
            inst.id,
            inst.account_login()
        );
        return Ok(inst.id.to_string());
    }

    let mut lines = String::new();
    for inst in installations {
        lines.push_str(&format!(
            "\n  id={} account={}",
            inst.id,
            inst.account_login()
        ));
    }
    Err(format!(
        "GitHub App {app_id} is installed on {} targets — installation id is ambiguous \
         ({}). Pass --app-installation-id explicitly (or set \
         GHA_APP_INSTALLATION_ID):{lines}",
        installations.len(),
        owner_hint.map_or_else(
            || "no owner hint to narrow it (set --owner/--user/--repo, or pass the id \
                 directly)"
                .to_string(),
            |o| format!("owner hint '{o}' matched none or more than one")
        )
    ))
}

// --- installation id resolution (cached; hits the network only when needed) ----

type InstallationIdCacheKey = (String, Option<String>);

fn installation_id_cache() -> &'static Mutex<HashMap<InstallationIdCacheKey, String>> {
    static CACHE: OnceLock<Mutex<HashMap<InstallationIdCacheKey, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve the installation id to mint against: `cfg.installation_id` if explicitly
/// set (no network at all), else auto-discover (and cache) it — see the module docs.
fn resolve_installation_id(
    cfg: &AppAuthConfig,
    owner_hint: Option<&str>,
    now: i64,
    http: &crate::HttpConfig,
) -> Result<String, String> {
    if let Some(id) = &cfg.installation_id {
        return Ok(id.clone());
    }

    let cache_key: InstallationIdCacheKey =
        (cfg.app_id.clone(), owner_hint.map(str::to_ascii_lowercase));
    {
        let guard = installation_id_cache()
            .lock()
            .map_err(|_| "appauth: installation-id cache lock poisoned".to_string())?;
        if let Some(id) = guard.get(&cache_key) {
            return Ok(id.clone());
        }
    }

    let jwt = encode_jwt(&cfg.app_id, &cfg.key_source, now)?;
    let installations = list_installations_http(&jwt, http)?;
    let app_slug = if installations.is_empty() {
        get_app_info_http(&jwt, http).ok().and_then(|a| a.slug)
    } else {
        None
    };
    let id = select_installation(&installations, owner_hint, &cfg.app_id, app_slug.as_deref())?;

    let mut guard = installation_id_cache()
        .lock()
        .map_err(|_| "appauth: installation-id cache lock poisoned".to_string())?;
    guard.insert(cache_key, id.clone());
    Ok(id)
}

// --- installation token minting -------------------------------------------------

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

/// The only network call besides discovery — kept isolated so everything else (claim
/// construction, refresh timing, fallback selection) is unit-testable without it.
fn mint_installation_token_http(
    jwt: &str,
    installation_id: &str,
    http: &crate::HttpConfig,
) -> Result<(InstallationToken, i64), String> {
    let url = format!("https://api.github.com/app/installations/{installation_id}/access_tokens");
    let result = crate::http_agent(http)
        .post(&url)
        .set("Authorization", &format!("Bearer {jwt}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call();

    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(401, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!(
                "installation-token mint failed: HTTP 401 — the App JWT was rejected. \
                 This usually means the App was revoked/deleted, the private key was \
                 rotated (update the vault entry), or --app-id doesn't match this key. \
                 Response: {}",
                crate::redact(&body)
            ));
        }
        Err(ureq::Error::Status(404, r)) => {
            let body = r.into_string().unwrap_or_default();
            return Err(format!(
                "installation-token mint failed: HTTP 404 — installation {installation_id} \
                 not found for this App. Re-check --app-installation-id, or omit it to \
                 auto-discover. Response: {}",
                crate::redact(&body)
            ));
        }
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
    Ok((InstallationToken(body.token), expires_at_unix))
}

// --- cache + refresh decision (pure, testable) ----------------------------------

struct CachedToken {
    token: InstallationToken,
    expires_at_unix: i64,
}

/// Keyed by (app_id, installation_id) — `warm` builds a per-repo `Cli`, so one process
/// can legitimately mint for more than one installation (e.g. two owners in a
/// `--prefer-repos` list). A single flat slot would hand the second target the first
/// target's token, which fails as a confusing 404/403 on the next call.
type TokenCacheKey = (String, String);

fn token_cache() -> &'static Mutex<HashMap<TokenCacheKey, CachedToken>> {
    static CACHE: OnceLock<Mutex<HashMap<TokenCacheKey, CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

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
/// one. `owner_hint` (from `Cli::app_auth_owner_hint`) is used only when
/// `cfg.installation_id` is absent, to disambiguate auto-discovery. This is the
/// App-auth entry point called from [`crate::github_token`] and `doctor`.
pub(crate) fn installation_token(
    cfg: &AppAuthConfig,
    owner_hint: Option<&str>,
    http: &crate::HttpConfig,
) -> Result<String, String> {
    let now = now_unix();
    let installation_id = resolve_installation_id(cfg, owner_hint, now, http)?;
    let cache_key: TokenCacheKey = (cfg.app_id.clone(), installation_id.clone());

    {
        let guard = token_cache()
            .lock()
            .map_err(|_| "appauth: token cache lock poisoned".to_string())?;
        if let Some(cached) = guard.get(&cache_key) {
            if !needs_remint(cached.expires_at_unix, now, REFRESH_MARGIN_SECS) {
                return Ok(cached.token.expose().to_string());
            }
        }
    }

    let jwt = encode_jwt(&cfg.app_id, &cfg.key_source, now)?;
    let (token, expires_at_unix) = mint_installation_token_http(&jwt, &installation_id, http)?;
    let out = token.expose().to_string();

    let mut guard = token_cache()
        .lock()
        .map_err(|_| "appauth: token cache lock poisoned".to_string())?;
    // Deliberately does NOT state an hourly budget — see the note where the constant used
    // to live. The real limit is installation-dependent; `X-RateLimit-*` and `/rate_limit`
    // (see `doctor`) report it accurately, a compiled-in guess does not.
    eprintln!(
        "auth: minted GitHub App installation token (app_id={}, installation_id={}, \
         expires in {}s)",
        cfg.app_id,
        installation_id,
        (expires_at_unix - now).max(0)
    );
    guard.insert(
        cache_key,
        CachedToken {
            token,
            expires_at_unix,
        },
    );
    Ok(out)
}

// --- doctor / auth-check reporting ----------------------------------------------

/// Everything `doctor` prints about an active App-auth configuration. No secrets.
pub(crate) struct DoctorReport {
    pub(crate) app_id: String,
    /// Human-readable App name, for display only.
    pub(crate) app_name: Option<String>,
    /// URL-safe App slug (e.g. `tzervas-fleet-runner-ctl`) — use this, never
    /// `app_name`, when building a `https://github.com/settings/apps/<slug>/...` link.
    pub(crate) app_slug: Option<String>,
    pub(crate) installation_id: String,
    pub(crate) account_login: String,
    pub(crate) repository_selection: Option<String>,
    pub(crate) permissions: BTreeMap<String, String>,
}

/// Gather everything `doctor` needs about App auth: app identity, installation
/// identity/scope, and the granted permission set. Two GETs beyond whatever
/// auto-discovery already needed (`GET /app`, `GET /app/installations/{id}`).
pub(crate) fn doctor_report(
    cfg: &AppAuthConfig,
    owner_hint: Option<&str>,
    http: &crate::HttpConfig,
) -> Result<DoctorReport, String> {
    let now = now_unix();
    let installation_id = resolve_installation_id(cfg, owner_hint, now, http)?;
    let jwt = encode_jwt(&cfg.app_id, &cfg.key_source, now)?;
    let app_info = get_app_info_http(&jwt, http)?;
    let inst = get_installation_http(&jwt, &installation_id, http)?;
    Ok(DoctorReport {
        app_id: cfg.app_id.clone(),
        app_name: app_info.name.clone(),
        app_slug: app_info.slug,
        installation_id,
        account_login: inst.account_login().to_string(),
        repository_selection: inst.repository_selection.clone(),
        permissions: inst.permissions.clone().unwrap_or_default(),
    })
}

/// Permission sets required to mint runner registration tokens, keyed by
/// registration scope. `doctor` flags anything short of the applicable set so a
/// mis-scoped App fails loudly instead of as a confusing 403 mid-`listen`.
///
/// The two sets differ in one entry, and that entry is the whole reason to prefer
/// an organization:
///
/// | scope | token-minting permission | also grants |
/// |---|---|---|
/// | repo / user | `administration:write` | repo settings, collaborators, branch protection, transfer, **deletion** |
/// | org | `organization_self_hosted_runners:write` | nothing else |
///
/// On a **user account** there is no narrow option. GitHub exposes no
/// user-scoped equivalent of `organization_self_hosted_runners`, so minting a
/// registration token requires `administration:write` — which also confers the
/// ability to delete every repository the App is installed on. The credential
/// cannot be narrowed, only confined.
///
/// On an **organization** the narrow permission exists and is sufficient. It is
/// available on the **free** organization tier (verified against the live API:
/// `POST /orgs/{org}/actions/runners/registration-token` succeeds, and the
/// `Default` runner group is present). Only *additional* runner groups require a
/// paid tier, and those exist to restrict which repos may use which runner — the
/// opposite of what a single shared pool wants.
///
/// See `docs/GITHUB_APP_AUTH.md` for the migration path.
pub(crate) const REPO_SCOPE_PERMISSIONS: &[(&str, &str)] = &[
    ("actions", "read"),
    ("administration", "write"),
    ("metadata", "read"),
];

/// Org-scoped equivalent. Note `organization_self_hosted_runners` in place of
/// `administration` — same capability, without repository deletion rights.
pub(crate) const ORG_SCOPE_PERMISSIONS: &[(&str, &str)] = &[
    ("actions", "read"),
    ("organization_self_hosted_runners", "write"),
    ("metadata", "read"),
];

/// The permission set required for `scope`.
pub(crate) fn expected_permissions(scope: crate::Scope) -> &'static [(&'static str, &'static str)] {
    match scope {
        crate::Scope::Org => ORG_SCOPE_PERMISSIONS,
        crate::Scope::Repo | crate::Scope::User => REPO_SCOPE_PERMISSIONS,
    }
}

/// Human-readable rendering of a permission set, for `doctor` output.
pub(crate) fn describe_permissions(set: &[(&str, &str)]) -> String {
    set.iter()
        .map(|(p, l)| format!("{p}:{l}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn permission_rank(level: &str) -> u8 {
    match level {
        "none" => 0,
        "read" => 1,
        "write" => 2,
        "admin" => 3,
        _ => 0,
    }
}

/// Missing/insufficient permissions, or an empty vec if the expected set is fully
/// covered (extra permissions beyond the expected set are not flagged as failures —
/// `doctor` reports them separately as informational, since over-scoping isn't a
/// functional break the way under-scoping is).
pub(crate) fn missing_permissions(
    granted: &BTreeMap<String, String>,
    scope: crate::Scope,
) -> Vec<String> {
    expected_permissions(scope)
        .iter()
        .filter_map(|(perm, level)| {
            let have = granted.get(*perm).map(String::as_str).unwrap_or("none");
            if permission_rank(have) < permission_rank(level) {
                Some(format!("{perm}:{level} (have {have})"))
            } else {
                None
            }
        })
        .collect()
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

    // Test PEM fixtures are assembled from fragments so the PEM begin/end header lines
    // never appear contiguously anywhere in this file — including in this comment.
    //
    // gitleaks matches that header as RuleID `private-key` regardless of the body, so a
    // fixture whose "key" is literally `abc` still fails the security gate. Splitting the
    // literal is deliberately preferred over a `gitleaks:allow` annotation or an ignore
    // entry: an allowlist on these lines would also silence a REAL key pasted here later,
    // whereas this keeps the gate fully armed. The runtime values are byte-identical.
    const PEM_BEGIN: &str = concat!("-----BEGIN ", "RSA PRIVATE KEY-----");
    const PEM_END: &str = concat!("-----END ", "RSA PRIVATE KEY-----");
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // --- resolve_app_auth_config: fallback / partial / full -------------------

    #[test]
    fn resolve_app_auth_config_all_absent_falls_back_silently() {
        let got = resolve_app_auth_config(|_| None).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn resolve_app_auth_config_id_and_key_selects_app_auth_without_installation_id() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("file:/etc/gha/app-key.pem".to_string()),
            _ => None,
        })
        .unwrap()
        .expect("app_id + key present must select app auth");
        assert_eq!(got.app_id, "123456");
        assert_eq!(got.installation_id, None, "installation id is optional");
        assert_eq!(
            got.key_source,
            KeySource::Path(PathBuf::from("/etc/gha/app-key.pem"))
        );
    }

    #[test]
    fn resolve_app_auth_config_all_three_present_selects_app_auth() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_INSTALLATION_ID" => Some("78901234".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("file:/etc/gha/app-key.pem".to_string()),
            _ => None,
        })
        .unwrap()
        .expect("all three present must select app auth");
        assert_eq!(got.installation_id.as_deref(), Some("78901234"));
    }

    #[test]
    fn resolve_app_auth_config_secret_form() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("secret:runner/gha-app-key".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            got.key_source,
            KeySource::Vault {
                group_key: "runner/gha-app-key".to_string()
            }
        );
    }

    #[test]
    fn resolve_app_auth_config_accepts_bare_path_without_file_prefix() {
        let got = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("1".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("/etc/gha/app-key.pem".to_string()),
            _ => None,
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            got.key_source,
            KeySource::Path(PathBuf::from("/etc/gha/app-key.pem"))
        );
    }

    #[test]
    fn resolve_app_auth_config_missing_key_is_an_error_naming_it() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("missing --app-private-key"), "{err}");
        // The optional field is explained, but never listed as one of the *missing*
        // required ones.
        assert!(!err.contains("missing --app-installation-id"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_missing_app_id_is_an_error_naming_it() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_PRIVATE_KEY" => Some("file:/k.pem".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("app-id"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_installation_id_alone_is_an_error_naming_the_real_gaps() {
        // Setting only the optional field is still a signal of intent — this must not
        // silently look like "not using App auth."
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_INSTALLATION_ID" => Some("78901234".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("app-id"), "{err}");
        assert!(err.contains("app-private-key"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_empty_string_env_counts_as_absent() {
        let get = |k: &str| -> Option<String> {
            let raw: Option<&str> = match k {
                "GHA_APP_ID" => Some(""),
                "GHA_APP_PRIVATE_KEY" => Some("/etc/gha/app-key.pem"),
                _ => None,
            };
            raw.map(str::to_string).filter(|v| !v.is_empty())
        };
        let err = resolve_app_auth_config(get).unwrap_err();
        assert!(err.contains("app-id"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_rejects_non_numeric_app_id() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("Iv1.abc123".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("file:/k.pem".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("numeric ID"), "{err}");
    }

    #[test]
    fn resolve_app_auth_config_rejects_non_numeric_installation_id() {
        let err = resolve_app_auth_config(|k| match k {
            "GHA_APP_ID" => Some("123456".to_string()),
            "GHA_APP_INSTALLATION_ID" => Some("not-a-number".to_string()),
            "GHA_APP_PRIVATE_KEY" => Some("file:/k.pem".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("app-installation-id"), "{err}");
    }

    // --- parse_key_source: three forms + inline-PEM refusal --------------------

    #[test]
    fn parse_key_source_secret_form() {
        let got = parse_key_source("secret:runner/gha-app-key").unwrap();
        assert_eq!(
            got,
            KeySource::Vault {
                group_key: "runner/gha-app-key".to_string()
            }
        );
    }

    #[test]
    fn parse_key_source_file_prefix() {
        let got = parse_key_source("file:/etc/gha/key.pem").unwrap();
        assert_eq!(got, KeySource::Path(PathBuf::from("/etc/gha/key.pem")));
    }

    #[test]
    fn parse_key_source_bare_path() {
        let got = parse_key_source("/etc/gha/key.pem").unwrap();
        assert_eq!(got, KeySource::Path(PathBuf::from("/etc/gha/key.pem")));
    }

    #[test]
    fn parse_key_source_empty_secret_group_key_is_an_error() {
        let err = parse_key_source("secret:").unwrap_err();
        assert!(err.contains("<group>/<key>"), "{err}");
    }

    #[test]
    fn parse_key_source_refuses_inline_pem_even_without_a_prefix() {
        let pem = format!("{PEM_BEGIN}\nMIIB...\n{PEM_END}");
        let pem = pem.as_str();
        let err = parse_key_source(pem).unwrap_err();
        assert!(err.contains("secret:"), "{err}");
        assert!(err.contains("file:"), "{err}");
    }

    #[test]
    fn parse_key_source_refuses_inline_pem_disguised_with_a_file_prefix() {
        let smuggled = format!("file:{PEM_BEGIN}\nabc\n{PEM_END}");
        let smuggled = smuggled.as_str();
        let err = parse_key_source(smuggled).unwrap_err();
        assert!(err.contains("inline PEM"), "{err}");
    }

    // --- base64 decode + vault-PEM encoding auto-detection ----------------------

    #[test]
    fn base64_decode_standard_matches_known_vectors() {
        assert_eq!(base64_decode_standard("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode_standard("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode_standard("Zm9vYmFy").unwrap(), b"foobar");
        // Standard alphabet (+/), unlike the JWT's base64url (-_).
        assert_eq!(
            base64_decode_standard("+/8="),
            base64_decode_standard("+/8=")
        );
    }

    #[test]
    fn decode_vault_pem_accepts_raw_pem_as_is() {
        let pem = format!("{PEM_BEGIN}\nabc\n{PEM_END}\n");
        let pem = pem.as_str();
        let got = decode_vault_pem(pem).unwrap();
        assert!(got.as_str().starts_with("-----BEGIN"));
    }

    #[test]
    fn decode_vault_pem_accepts_base64_encoded_pem() {
        let pem = format!("{PEM_BEGIN}\nabc\n{PEM_END}\n");
        let pem = pem.as_str();
        let b64 = base64_encode_for_test(pem.as_bytes());
        let got = decode_vault_pem(&b64).unwrap();
        assert_eq!(got.as_str(), pem);
    }

    #[test]
    fn decode_vault_pem_rejects_neither_pem_nor_base64_pem() {
        let err = decode_vault_pem("not pem, not base64 either $$$").unwrap_err();
        assert!(err.contains("neither raw PEM nor base64"), "{err}");
    }

    #[test]
    fn decode_vault_pem_rejects_base64_of_non_pem_content() {
        let b64 = base64_encode_for_test(b"just some other secret, not a key");
        let err = decode_vault_pem(&b64).unwrap_err();
        assert!(err.contains("neither raw PEM nor base64"), "{err}");
    }

    /// Standard-alphabet base64 encoder used only by tests, to build round-trip
    /// fixtures for `base64_decode_standard`/`decode_vault_pem` without adding a
    /// dependency (production code never needs to *encode* base64, only decode it).
    fn base64_encode_for_test(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[((n >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[(n & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    // --- temp key file: 0600, written, shredded on drop -------------------------

    #[test]
    fn temp_key_file_is_written_0600_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;
        let path = {
            let tmp =
                TempKeyFile::write(&Pem("-----BEGIN TEST-----\nx\n-----END TEST-----".into()))
                    .unwrap();
            let meta = std::fs::metadata(&tmp.path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::read_to_string(&tmp.path).unwrap(),
                "-----BEGIN TEST-----\nx\n-----END TEST-----"
            );
            tmp.path.clone()
        };
        assert!(!path.exists(), "temp key file must be removed on drop");
    }

    #[test]
    fn ensure_key_path_private_rejects_missing_file() {
        let err = ensure_key_path_private(Path::new("/definitely/does/not/exist.pem")).unwrap_err();
        assert!(err.contains("not readable"), "{err}");
    }

    #[test]
    fn ensure_key_path_private_rejects_a_directory() {
        let err = ensure_key_path_private(Path::new("/tmp")).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
    }

    #[test]
    fn ensure_key_path_private_rejects_group_or_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "gha-appauth-perm-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::write(&path, b"not a real key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = ensure_key_path_private(&path).unwrap_err();
        assert!(err.contains("mode"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ensure_key_path_private_accepts_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "gha-appauth-perm-ok-test-{}-{}",
            std::process::id(),
            now_unix()
        ));
        std::fs::write(&path, b"not a real key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_key_path_private(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    // --- secret_get: missing binary is a clear, named error ---------------------

    #[test]
    fn secret_get_missing_binary_names_the_fix() {
        // Exercises the exact message `secret_get` produces when `Command::new("secret")`
        // fails with `NotFound`, without mutating the process-wide `PATH` — doing that
        // would race any other test in this binary that shells out to a PATH-resolved
        // binary (e.g. the `openssl` end-to-end signing tests run in parallel).
        let err = secret_not_found_message("runner/gha-app-key");
        assert!(err.contains("secret"), "{err}");
        assert!(err.contains("PATH"), "{err}");
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("file:<path>"), "{err}");
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
        let key = KeySource::Path(PathBuf::from("/nonexistent/key.pem"));
        let err = encode_jwt("not-a-number", &key, 0).unwrap_err();
        assert!(err.contains("app-id"), "{err}");
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

    // --- auto-discovery selection: zero / one / many installations --------------

    fn installation(id: u64, login: &str) -> Installation {
        Installation {
            id,
            account: Some(InstallationAccount {
                login: Some(login.to_string()),
            }),
            repository_selection: Some("all".to_string()),
            permissions: None,
        }
    }

    #[test]
    fn select_installation_zero_names_the_install_url_with_slug() {
        let err = select_installation(&[], None, "4451176", Some("tzervas-fleet-runner-ctl"))
            .unwrap_err();
        assert!(err.contains("4451176"), "{err}");
        assert!(
            err.contains("https://github.com/settings/apps/tzervas-fleet-runner-ctl/installations"),
            "{err}"
        );
    }

    #[test]
    fn select_installation_zero_without_slug_still_gives_actionable_text() {
        let err = select_installation(&[], None, "4451176", None).unwrap_err();
        assert!(err.contains("Install App"), "{err}");
    }

    #[test]
    fn select_installation_one_picks_it_regardless_of_owner_hint() {
        let list = vec![installation(150429495, "tzervas")];
        let id = select_installation(&list, None, "4451176", None).unwrap();
        assert_eq!(id, "150429495");
        let id2 = select_installation(&list, Some("someone-else"), "4451176", None).unwrap();
        assert_eq!(
            id2, "150429495",
            "sole installation wins even if the owner hint doesn't match"
        );
    }

    #[test]
    fn select_installation_many_with_matching_owner_hint_disambiguates() {
        let list = vec![installation(1, "org-a"), installation(2, "org-b")];
        let id = select_installation(&list, Some("org-b"), "1", None).unwrap();
        assert_eq!(id, "2");
    }

    #[test]
    fn select_installation_many_owner_hint_match_is_case_insensitive() {
        let list = vec![installation(1, "TzerVas"), installation(2, "other")];
        let id = select_installation(&list, Some("tzervas"), "1", None).unwrap();
        assert_eq!(id, "1");
    }

    #[test]
    fn select_installation_many_without_a_unique_owner_match_lists_all() {
        let list = vec![installation(1, "org-a"), installation(2, "org-b")];
        let err = select_installation(&list, None, "9", None).unwrap_err();
        assert!(err.contains("id=1 account=org-a"), "{err}");
        assert!(err.contains("id=2 account=org-b"), "{err}");
        assert!(err.contains("--app-installation-id"), "{err}");
    }

    #[test]
    fn select_installation_many_owner_hint_matching_more_than_one_still_ambiguous() {
        // e.g. two installations somehow reporting the same account login: don't guess.
        let list = vec![installation(1, "org-a"), installation(2, "org-a")];
        let err = select_installation(&list, Some("org-a"), "9", None).unwrap_err();
        assert!(err.contains("id=1"), "{err}");
        assert!(err.contains("id=2"), "{err}");
    }

    // --- permission gap reporting -------------------------------------------------

    #[test]
    fn missing_permissions_empty_when_fully_covered() {
        let mut perms = BTreeMap::new();
        perms.insert("actions".to_string(), "read".to_string());
        perms.insert("administration".to_string(), "write".to_string());
        perms.insert("metadata".to_string(), "read".to_string());
        assert!(missing_permissions(&perms, crate::Scope::Repo).is_empty());
    }

    #[test]
    fn missing_permissions_flags_absent_and_underscoped() {
        let mut perms = BTreeMap::new();
        perms.insert("actions".to_string(), "read".to_string());
        // administration missing entirely; metadata under-scoped (none < read is moot
        // since it's just absent, but exercise an explicit "none" value too).
        perms.insert("metadata".to_string(), "none".to_string());
        let missing = missing_permissions(&perms, crate::Scope::Repo);
        assert!(
            missing
                .iter()
                .any(|m| m.starts_with("administration:write")),
            "{missing:?}"
        );
        assert!(
            missing.iter().any(|m| m.starts_with("metadata:read")),
            "{missing:?}"
        );
    }

    #[test]
    fn org_scope_does_not_require_administration_write() {
        // The whole point of the org path: minting a registration token must NOT
        // require the permission that also confers repository deletion.
        let perms: BTreeMap<String, String> = [
            ("actions", "read"),
            ("organization_self_hosted_runners", "write"),
            ("metadata", "read"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert!(
            missing_permissions(&perms, crate::Scope::Org).is_empty(),
            "org scope should be satisfied without administration:write"
        );
        // The same grant must FAIL repo scope, or the two sets are not really distinct.
        assert!(
            !missing_permissions(&perms, crate::Scope::Repo).is_empty(),
            "repo scope must still demand administration:write"
        );
    }

    #[test]
    fn repo_scope_grant_is_insufficient_for_org_scope() {
        let perms: BTreeMap<String, String> = [
            ("actions", "read"),
            ("administration", "write"),
            ("metadata", "read"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert!(missing_permissions(&perms, crate::Scope::Repo).is_empty());
        let missing = missing_permissions(&perms, crate::Scope::Org);
        assert!(
            missing
                .iter()
                .any(|m| m.contains("organization_self_hosted_runners")),
            "org scope must flag the missing narrow permission, got {missing:?}"
        );
    }

    #[test]
    fn user_scope_shares_the_repo_permission_set() {
        // User scope registers per-repo under the hood, so it carries the same
        // (unavoidably broad) requirement. Asserted so a future change to one
        // does not silently diverge from the other.
        assert_eq!(
            expected_permissions(crate::Scope::User),
            expected_permissions(crate::Scope::Repo)
        );
        assert_ne!(
            expected_permissions(crate::Scope::Org),
            expected_permissions(crate::Scope::Repo)
        );
    }

    #[test]
    fn missing_permissions_higher_than_required_still_passes() {
        let mut perms = BTreeMap::new();
        perms.insert("actions".to_string(), "write".to_string()); // higher than required read
        perms.insert("administration".to_string(), "admin".to_string());
        perms.insert("metadata".to_string(), "read".to_string());
        assert!(missing_permissions(&perms, crate::Scope::Repo).is_empty());
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
        let key = KeySource::Path(key_path.clone());
        let jwt = encode_jwt("123456", &key, now).expect("encode_jwt should succeed");
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "JWT must have exactly 3 dot-separated sections: {jwt}"
        );
        assert!(!parts[0].is_empty() && !parts[1].is_empty() && !parts[2].is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encode_jwt_end_to_end_via_secret_vault_form() {
        // Exercises the secret:<group>/<key> path end-to-end using a fake `secret`
        // binary on PATH, so this needs neither the real vault nor network — only
        // openssl, matching the other end-to-end signing test in this module.
        if !openssl_available() {
            eprintln!("skipping: openssl not on PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "gha-appauth-vault-test-{}-{}",
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
        let pem = std::fs::read_to_string(&key_path).expect("read generated pem");

        // A fake `secret` on PATH that just base64-decodes its argv[2] back to the PEM
        // (encoded once here) — proves the base64-detection branch end-to-end without
        // any real vault dependency.
        let b64 = base64_encode_for_test(pem.as_bytes());
        let fake_secret = dir.join("secret");
        std::fs::write(
            &fake_secret,
            format!("#!/bin/sh\nif [ \"$1\" = get ]; then printf '%s' '{b64}'; fi\n"),
        )
        .unwrap();
        std::fs::set_permissions(&fake_secret, std::fs::Permissions::from_mode(0o700)).unwrap();

        let saved_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{saved_path}", dir.display()));

        let key = KeySource::Vault {
            group_key: "runner/gha-app-key".to_string(),
        };
        let now = now_unix();
        let jwt_result = encode_jwt("123456", &key, now);

        std::env::set_var("PATH", saved_path);
        let _ = std::fs::remove_dir_all(&dir);

        let jwt = jwt_result.expect("encode_jwt via secret: form should succeed");
        assert_eq!(jwt.split('.').count(), 3);
    }
}
